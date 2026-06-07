//! Django template parser: folds a token stream into a nesting syntax tree.
//!
//! The parser is iterative (it carries an explicit stack of open block frames),
//! so the call graph stays acyclic and the nesting depth is a hard, asserted
//! bound. Tags constellation does not model keep their bodies via a generic
//! container, so nested links remain reachable no matter the surrounding
//! structure.

use super::ast::{AstNode, Binding, IfArm};
use super::lexer::{Token, TokenKind};

/// A fail-fast bound on block nesting depth.
const NESTING_DEPTH_MAX: u32 = 256;

/// A fail-fast bound on the byte scan within a single tag's argument string.
const ARGUMENT_SCAN_MAX: u32 = 1_000_000;

/// The block tags that open a body closed by a matching `{% endtag %}`. A tag
/// outside this set is treated as a leaf, so an unknown third-party block tag
/// never swallows the rest of the template.
const CONTAINER_TAGS: &[&str] = &[
    "autoescape",
    "block",
    "blocktrans",
    "blocktranslate",
    "cache",
    "comment",
    "filter",
    "for",
    "if",
    "ifchanged",
    "language",
    "localize",
    "localtime",
    "spaceless",
    "timezone",
    "utc",
    "with",
];

/// A forest of top-level nodes parsed from a token stream. The tree borrows the
/// source text the tokens point at, so it outlives the token vector.
pub fn parse<'source>(tokens: &[Token<'source>]) -> Vec<AstNode<'source>> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Vec<AstNode> = Vec::new();

    for token in tokens {
        match token.kind {
            TokenKind::Text(text) => push_node(&mut stack, &mut root, AstNode::Text(text)),
            TokenKind::Variable { expression } => {
                push_node(&mut stack, &mut root, AstNode::Variable { expression, line: token.line });
            }
            TokenKind::Comment(text) => push_node(&mut stack, &mut root, AstNode::Comment(text)),
            TokenKind::Verbatim(content) => {
                push_node(&mut stack, &mut root, AstNode::Verbatim { content });
            }
            TokenKind::BlockEnd { tag } => close_frame(&mut stack, &mut root, tag),
            TokenKind::BlockStart { tag, arguments } => {
                handle_block_start(&mut stack, &mut root, tag, arguments, token.line);
            }
        }
    }

    let mut guard: u32 = 0;

    while let Some(frame) = stack.pop() {
        guard += 1;

        assert!(guard <= NESTING_DEPTH_MAX + 1, "flush exceeded nesting depth");

        let node = frame.finish();
        push_node(&mut stack, &mut root, node);
    }

    root
}

/// An open block awaiting its closing tag, accumulating child nodes.
enum Frame<'source> {
    Block {
        name: &'source str,
        body: Vec<AstNode<'source>>,
    },
    If {
        arms: Vec<IfArm<'source>>,
        else_body: Option<Vec<AstNode<'source>>>,
    },
    For {
        variable: &'source str,
        iterable: &'source str,
        body: Vec<AstNode<'source>>,
        empty_body: Option<Vec<AstNode<'source>>>,
    },
    With {
        bindings: Vec<Binding<'source>>,
        body: Vec<AstNode<'source>>,
    },
    Autoescape {
        argument: &'source str,
        body: Vec<AstNode<'source>>,
    },
    Filter {
        specification: &'source str,
        body: Vec<AstNode<'source>>,
    },
    Spaceless {
        body: Vec<AstNode<'source>>,
    },
    Container {
        name: &'source str,
        arguments: &'source str,
        body: Vec<AstNode<'source>>,
    },
}

impl<'source> Frame<'source> {
    /// The `{% endtag %}` name that closes this frame.
    fn end_tag(&self) -> &'source str {
        match self {
            Frame::Block { .. } => "block",
            Frame::If { .. } => "if",
            Frame::For { .. } => "for",
            Frame::With { .. } => "with",
            Frame::Autoescape { .. } => "autoescape",
            Frame::Filter { .. } => "filter",
            Frame::Spaceless { .. } => "spaceless",
            Frame::Container { name, .. } => name,
        }
    }

    /// The body that newly parsed children append to, tracking the active arm
    /// of a multi-branch block.
    fn sink(&mut self) -> &mut Vec<AstNode<'source>> {
        match self {
            Frame::Block { body, .. } => body,
            Frame::If { arms, else_body } => match else_body {
                Some(body) => body,
                None => &mut arms.last_mut().expect("an if frame always has its leading arm").body,
            },
            Frame::For { body, empty_body, .. } => match empty_body {
                Some(body) => body,
                None => body,
            },
            Frame::With { body, .. } => body,
            Frame::Autoescape { body, .. } => body,
            Frame::Filter { body, .. } => body,
            Frame::Spaceless { body } => body,
            Frame::Container { body, .. } => body,
        }
    }

    /// The finished node a closed frame converts into.
    fn finish(self) -> AstNode<'source> {
        match self {
            Frame::Block { name, body } => AstNode::Block { name, body },
            Frame::If { arms, else_body } => AstNode::If { arms, else_body },
            Frame::For { variable, iterable, body, empty_body } => {
                AstNode::For { variable, iterable, body, empty_body }
            }
            Frame::With { bindings, body } => AstNode::With { bindings, body },
            Frame::Autoescape { argument, body } => AstNode::Autoescape { argument, body },
            Frame::Filter { specification, body } => AstNode::Filter { specification, body },
            Frame::Spaceless { body } => AstNode::Spaceless { body },
            Frame::Container { name, arguments, body } => {
                AstNode::Container { name, arguments, body }
            }
        }
    }
}

/// The append of a node to the innermost open frame, or to the root when none is open.
fn push_node<'source>(
    stack: &mut [Frame<'source>],
    root: &mut Vec<AstNode<'source>>,
    node: AstNode<'source>,
) {
    match stack.last_mut() {
        Some(frame) => frame.sink().push(node),
        None => root.push(node),
    }
}

/// The close of the nearest open frame matching `end_tag`, finishing any unclosed
/// inner frames into their parents on the way. A stray end tag is ignored.
fn close_frame<'source>(
    stack: &mut Vec<Frame<'source>>,
    root: &mut Vec<AstNode<'source>>,
    end_tag: &str,
) {
    if !stack.iter().any(|frame| frame.end_tag() == end_tag) {
        return;
    }

    let mut guard: u32 = 0;

    loop {
        guard += 1;

        assert!(guard <= NESTING_DEPTH_MAX + 1, "close exceeded nesting depth");

        let frame = stack.pop().expect("a matching frame was just confirmed present");
        let matched = frame.end_tag() == end_tag;
        let node = frame.finish();

        push_node(stack, root, node);

        if matched {
            break;
        }
    }
}

/// The dispatch of a `{% tag arguments %}`: a branch marker mutates the open frame, a
/// container tag opens a new frame, a link tag becomes a typed leaf, and
/// anything else becomes a generic leaf tag.
fn handle_block_start<'source>(
    stack: &mut Vec<Frame<'source>>,
    root: &mut Vec<AstNode<'source>>,
    tag: &'source str,
    arguments: &'source str,
    line: u32,
) {
    if apply_branch_marker(stack, tag, arguments) {
        return;
    }

    if CONTAINER_TAGS.contains(&tag) {
        assert!((stack.len() as u32) < NESTING_DEPTH_MAX, "template nesting exceeds {NESTING_DEPTH_MAX}");

        stack.push(open_container(tag, arguments));

        return;
    }

    let leaf = match tag {
        "extends" => extends_node(arguments, line),
        "include" => include_node(arguments, line),
        "url" => url_node(arguments, line),
        "static" => static_node(arguments, line),
        "load" => AstNode::Load { libraries: arguments.split_whitespace().collect() },
        _ => AstNode::Tag { name: tag, arguments },
    };

    push_node(stack, root, leaf);
}

/// The application of an `{% elif %}` / `{% else %}` / `{% empty %}` marker to the
/// open frame it belongs to. Returns true when the token was a marker that landed.
fn apply_branch_marker<'source>(
    stack: &mut [Frame<'source>],
    tag: &str,
    arguments: &'source str,
) -> bool {
    match tag {
        "elif" => {
            if let Some(Frame::If { arms, else_body }) = stack.last_mut()
                && else_body.is_none()
            {
                arms.push(IfArm { condition: arguments, body: Vec::new() });

                return true;
            }
        }
        "else" => {
            if let Some(Frame::If { else_body, .. }) = stack.last_mut()
                && else_body.is_none()
            {
                *else_body = Some(Vec::new());

                return true;
            }
        }
        "empty" => {
            if let Some(Frame::For { empty_body, .. }) = stack.last_mut()
                && empty_body.is_none()
            {
                *empty_body = Some(Vec::new());

                return true;
            }
        }
        _ => {}
    }

    false
}

/// The open frame for a container tag.
fn open_container<'source>(tag: &'source str, arguments: &'source str) -> Frame<'source> {
    match tag {
        "block" => Frame::Block {
            name: arguments.split_whitespace().next().unwrap_or(""),
            body: Vec::new(),
        },
        "if" => Frame::If {
            arms: vec![IfArm { condition: arguments, body: Vec::new() }],
            else_body: None,
        },
        "for" => {
            let (variable, iterable) = split_for(arguments);

            Frame::For { variable, iterable, body: Vec::new(), empty_body: None }
        }
        "with" => Frame::With { bindings: parse_bindings(arguments).0, body: Vec::new() },
        "autoescape" => Frame::Autoescape { argument: arguments, body: Vec::new() },
        "filter" => Frame::Filter { specification: arguments, body: Vec::new() },
        "spaceless" => Frame::Spaceless { body: Vec::new() },
        _ => Frame::Container { name: tag, arguments, body: Vec::new() },
    }
}

/// An `{% extends %}` node.
fn extends_node(arguments: &str, line: u32) -> AstNode<'_> {
    let (token, _) = first_token(arguments);

    AstNode::Extends {
        path: strip_quotes(token),
        is_literal: is_quoted(token),
        line,
    }
}

/// An `{% include %}` node, parsing any `with`/`only` tail.
fn include_node(arguments: &str, line: u32) -> AstNode<'_> {
    let (token, rest) = first_token(arguments);
    let (bindings, only) = parse_include_tail(rest.trim());

    AstNode::Include {
        path: strip_quotes(token),
        is_literal: is_quoted(token),
        bindings,
        only,
        line,
    }
}

/// A `{% url %}` node, capturing the route name and any `as` target.
fn url_node(arguments: &str, line: u32) -> AstNode<'_> {
    let (token, rest) = first_token(arguments);

    AstNode::Url {
        name: strip_quotes(token),
        is_literal: is_quoted(token),
        as_variable: trailing_as(rest),
        line,
    }
}

/// A `{% static %}` node.
fn static_node(arguments: &str, line: u32) -> AstNode<'_> {
    let (token, rest) = first_token(arguments);

    AstNode::Static {
        path: strip_quotes(token),
        is_literal: is_quoted(token),
        as_variable: trailing_as(rest),
        line,
    }
}

/// The loop target and source a `{% for x, y in iterable %}` header splits into.
fn split_for(arguments: &str) -> (&str, &str) {
    match arguments.split_once(" in ") {
        Some((variable, iterable)) => (variable.trim(), iterable.trim()),
        None => (arguments.trim(), ""),
    }
}

/// The trailing `as variable` capture of a tag, if present.
fn trailing_as(rest: &str) -> Option<&str> {
    let mut words = rest.split_whitespace().rev();
    let last = words.next()?;
    let preceding = words.next()?;

    if preceding == "as" { Some(last) } else { None }
}

/// The `[with k=v ...] [only]` tail of an `{% include %}`, parsed into bindings.
fn parse_include_tail(rest: &str) -> (Vec<Binding<'_>>, bool) {
    if rest == "only" {
        return (Vec::new(), true);
    }

    let Some(after) = strip_keyword(rest, "with") else {
        return (Vec::new(), false);
    };

    parse_bindings(after)
}

/// The trimmed remainder of `input` after a leading `keyword` that stands as a
/// whole word, or `None` when no such keyword leads.
fn strip_keyword<'source>(input: &'source str, keyword: &str) -> Option<&'source str> {
    let rest = input.strip_prefix(keyword)?;

    if rest.is_empty() || rest.starts_with([' ', '\t']) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// The first whitespace-delimited token, honoring a leading quoted
/// string, paired with the trimmed remainder.
fn first_token(input: &str) -> (&str, &str) {
    let bytes = input.as_bytes();
    let length = bytes.len();

    let mut position: usize = 0;

    while position < length && matches!(bytes[position], b' ' | b'\t') {
        position += 1;
    }

    let start = position;

    if position < length && matches!(bytes[position], b'\'' | b'"') {
        let quote = bytes[position];
        position += 1;

        while position < length && bytes[position] != quote {
            position += 1;
        }

        if position < length {
            position += 1;
        }
    } else {
        while position < length && !matches!(bytes[position], b' ' | b'\t') {
            position += 1;
        }
    }

    (&input[start..position], input[position..].trim_start())
}

/// The `k=v k2="a b" [only]` bindings parsed from `input`, honoring quoted values
/// that contain spaces. The bool is the trailing `only` flag.
fn parse_bindings(input: &str) -> (Vec<Binding<'_>>, bool) {
    let bytes = input.as_bytes();
    let length = bytes.len();

    let mut bindings: Vec<Binding> = Vec::new();
    let mut only = false;
    let mut position: usize = 0;
    let mut guard: u32 = 0;

    while position < length {
        guard += 1;

        assert!(guard <= ARGUMENT_SCAN_MAX, "binding scan exceeded {ARGUMENT_SCAN_MAX}");

        while position < length && matches!(bytes[position], b' ' | b'\t') {
            position += 1;
        }

        if position >= length {
            break;
        }

        if input[position..].starts_with("only") && word_ends_at(bytes, position + 4) {
            only = true;
            break;
        }

        let key_start = position;

        while position < length && !matches!(bytes[position], b'=' | b' ' | b'\t') {
            position += 1;
        }

        if position >= length || bytes[position] != b'=' {
            while position < length && !matches!(bytes[position], b' ' | b'\t') {
                position += 1;
            }

            continue;
        }

        let name = &input[key_start..position];
        position += 1;

        let value_start = position;

        while position < length && !matches!(bytes[position], b' ' | b'\t') {
            if matches!(bytes[position], b'\'' | b'"') {
                let quote = bytes[position];
                position += 1;

                while position < length && bytes[position] != quote {
                    position += 1;
                }
            }

            if position < length {
                position += 1;
            }
        }

        bindings.push(Binding { name, value: &input[value_start..position] });
    }

    (bindings, only)
}

/// Whether byte index `index` is the end of input or a whitespace boundary.
fn word_ends_at(bytes: &[u8], index: usize) -> bool {
    index >= bytes.len() || matches!(bytes[index], b' ' | b'\t')
}

/// Whether a tag argument token is a quoted string literal.
fn is_quoted(token: &str) -> bool {
    token.starts_with(['\'', '"'])
}

/// The token with its surrounding quotes stripped; a bareword is unchanged.
fn strip_quotes(token: &str) -> &str {
    if is_quoted(token) {
        token.trim_matches(['\'', '"'])
    } else {
        token
    }
}
