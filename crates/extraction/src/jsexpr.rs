//! Shared JavaScript-expression analysis over tree-sitter, used by both the
//! JavaScript extractor (for real `.js` files) and the template extractor (for
//! the JavaScript embedded in Alpine attribute values). It replaces the byte
//! scanners the template extractor used to read Alpine expressions with a real
//! parse of the same grammar the JavaScript files go through.
//!
//! Alpine attribute values are expression fragments (`save()`, `cartItem()`,
//! `{ count: 0 }`), not whole programs. Parsing them bare is ambiguous (a
//! leading `{` lexes as a block, not an object), so every fragment is wrapped in
//! parentheses to force expression context before parsing.

use std::cell::RefCell;

use tree_sitter::{Node as TsNode, Parser};

use crate::tsutil::{node_text, to_u32};

/// The JavaScript global functions called from Alpine expressions that are not
/// project-defined handlers, so a `Handles` reference to one never resolves and
/// is pure noise. Skipped when collecting call identifiers, the analogue of the
/// Python builtin-call filter. Alpine magics (`$dispatch`, `$nextTick`, `$id`,
/// `$refs`, ...) are excluded by their `$` prefix, not listed here.
const JS_CALL_BUILTINS: &[&str] = &[
    "Array",
    "Boolean",
    "Date",
    "JSON",
    "Math",
    "Number",
    "Object",
    "String",
    "alert",
    "clearInterval",
    "clearTimeout",
    "confirm",
    "decodeURIComponent",
    "encodeURIComponent",
    "fetch",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "prompt",
    "setInterval",
    "setTimeout",
    "structuredClone",
];

/// Whether a called name is a non-handler JavaScript global: an Alpine magic
/// (`$`-prefixed) or a builtin in [`JS_CALL_BUILTINS`]. Such a call is never a
/// project-defined handler, so it must not become a `Handles` reference.
fn is_js_non_handler(name: &str) -> bool {
    name.starts_with('$') || JS_CALL_BUILTINS.contains(&name)
}

/// A fail-fast bound on the node-walk loop.
const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single node.
const CHILDREN_MAX: u32 = 1_000_000;

/// A fail-fast bound on the Django-tag scan: far past the number of `{% %}`/`{{ }}`
/// tags any single Alpine attribute value could hold.
const DJANGO_TAG_SCAN_MAX: u32 = 1_000_000;

/// A copy of an Alpine attribute value with its Django template tags (`{% ... %}`,
/// `{{ ... }}`) replaced by same-length whitespace, so the JavaScript parser
/// sees valid filler instead of the stray `{`/`}` a `{% for %}`-built object map
/// injects (those braces corrupt the parse and drop every method defined after
/// them). Newlines inside a tag are preserved, so each method keeps its original
/// line; identifiers outside the tags are untouched at their original offsets.
fn blank_django_tags(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    let mut iterations: u32 = 0;

    while !rest.is_empty() {
        iterations += 1;

        assert!(iterations <= DJANGO_TAG_SCAN_MAX, "django-tag scan exceeded {DJANGO_TAG_SCAN_MAX}");

        let block = rest.find("{%").map(|start| (start, "%}"));
        let variable = rest.find("{{").map(|start| (start, "}}"));

        let next = match (block, variable) {
            (Some(block), Some(variable)) => Some(block.min(variable)),
            (Some(block), None) => Some(block),
            (None, Some(variable)) => Some(variable),
            (None, None) => None,
        };

        let Some((start, close)) = next else {
            out.push_str(rest);

            break;
        };

        out.push_str(&rest[..start]);

        let tail = &rest[start..];
        let end = tail.find(close).map_or(tail.len(), |offset| offset + close.len());

        // Fill per byte, not per char, so a multibyte char inside a tag keeps the
        // byte length (and every later offset) identical to the source.
        for byte in tail[..end].bytes() {
            out.push(if byte == b'\n' { '\n' } else { ' ' });
        }

        rest = &tail[end..];
    }

    assert!(out.len() == value.len(), "blanking preserves the byte length");

    out
}

/// The callee name of a call: a bare identifier, or the trailing property of a
/// member access (`obj.method` -> `method`).
pub(crate) fn callee_name<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    match node.kind() {
        "identifier" => Some(node_text(bytes, node)),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|property| node_text(bytes, property)),
        _ => None,
    }
}

/// The static value of a JavaScript string literal: the text of its
/// `string_fragment`, with the quotes excluded by the grammar. `None` for a
/// non-string node, a template literal carrying a `${...}` substitution, or a
/// string split by an escape (no single clean fragment); an empty literal
/// yields `Some("")`.
pub(crate) fn string_literal<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }

    let mut cursor = node.walk();
    let mut fragment: Option<&str> = None;

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_fragment" => {
                if fragment.is_some() {
                    return None;
                }

                fragment = Some(node_text(bytes, child));
            }
            "template_substitution" => return None,
            _ => {}
        }
    }

    Some(fragment.unwrap_or(""))
}

/// The unquoted value of the `index`-th positional argument when it is a string
/// literal, else `None`.
pub(crate) fn string_argument(bytes: &[u8], call: TsNode<'_>, index: usize) -> Option<String> {
    let arguments = call.child_by_field_name("arguments")?;

    let mut cursor = arguments.walk();
    let argument = arguments.named_children(&mut cursor).nth(index)?;

    let text = string_literal(bytes, argument)?;

    if text.is_empty() { None } else { Some(text.to_string()) }
}

/// The name of the `index`-th positional argument when it is a bare identifier
/// (a named handler), else `None`.
pub(crate) fn identifier_argument<'bytes>(
    bytes: &'bytes [u8],
    call: TsNode<'_>,
    index: usize,
) -> Option<&'bytes str> {
    let arguments = call.child_by_field_name("arguments")?;

    let mut cursor = arguments.walk();
    let argument = arguments.named_children(&mut cursor).nth(index)?;

    if argument.kind() == "identifier" {
        Some(node_text(bytes, argument))
    } else {
        None
    }
}

thread_local! {
    /// The per-thread JavaScript parser for the embedded Alpine expressions,
    /// reused across attributes and files. One parser per rayon worker thread,
    /// no cross-thread sharing; an attribute value pays only for its parse.
    static PARSER: RefCell<Parser> = RefCell::new(new_parser());
}

/// A JavaScript parser with the grammar loaded. It panics only on a grammar
/// against tree-sitter ABI mismatch, a build error that cannot arise at runtime
/// in a correctly linked binary.
fn new_parser() -> Parser {
    let language: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();

    let mut parser = Parser::new();

    parser
        .set_language(&language)
        .expect("the bundled javascript grammar is ABI-compatible with tree-sitter");

    parser
}

/// An analyzer of the JavaScript fragments embedded in Alpine attribute values,
/// parsing each through a per-thread parser shared across attributes and files.
pub(crate) struct AlpineExpr;

impl AlpineExpr {
    /// The embedded-expression analyzer. Always `Some`; the `Option` return is
    /// kept so existing call sites and tests need no change.
    pub(crate) fn new() -> Option<Self> {
        Some(Self)
    }

    /// The bare function identifiers called in the expression (`save()` ->
    /// `save`), excluding method calls (`obj.save()`). Deduplicated, source
    /// order preserved. Drives the `Handles` edges from an Alpine directive to
    /// the JavaScript it invokes.
    pub(crate) fn call_identifiers(&mut self, value: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();

        self.with_tree(value, |bytes, node| {
            if node.kind() != "call_expression" {
                return;
            }

            if let Some(function) = node.child_by_field_name("function")
                && function.kind() == "identifier"
            {
                let name = node_text(bytes, function);

                if !name.is_empty() && !is_js_non_handler(name) && !names.iter().any(|seen| seen == name) {
                    names.push(name.to_string());
                }
            }
        });

        names
    }

    /// The event names dispatched by `$dispatch('event')` calls in the
    /// expression.
    pub(crate) fn dispatched_events(&mut self, value: &str) -> Vec<String> {
        let mut events: Vec<String> = Vec::new();

        self.with_tree(value, |bytes, node| {
            if node.kind() != "call_expression" {
                return;
            }

            let Some(function) = node.child_by_field_name("function") else {
                return;
            };

            if callee_name(bytes, function) == Some("$dispatch")
                && let Some(event) = string_argument(bytes, node, 0)
                && !events.contains(&event)
            {
                events.push(event);
            }
        });

        events
    }

    /// The django-glue field accesses in the expression: `Glue.model.task.title`
    /// / `Glue.form.contact.email` -> `(glue_name, field)` pairs, where
    /// `glue_name` is the unique name the proxy was registered under and `field`
    /// is the first field read on it. Only the field-bearing proxy kinds (`model`,
    /// `form`) match; `querySet`, `template`, and `function` are skipped.
    /// Deduplicated, source order preserved. Drives the rewrite's frontend
    /// `AccessesMember` edges (template -> model member).
    pub(crate) fn glue_member_accesses(&mut self, value: &str) -> Vec<(String, String)> {
        let mut accesses: Vec<(String, String)> = Vec::new();

        self.with_tree(value, |bytes, node| {
            if node.kind() != "member_expression" {
                return;
            }

            if let Some((name, field)) = glue_field_access(bytes, node)
                && !accesses.iter().any(|(seen_name, seen_field)| seen_name == name && seen_field == field)
            {
                accesses.push((name.to_string(), field.to_string()));
            }
        });

        accesses
    }

    /// The CSS class names written as string literals inside a class-binding
    /// expression, split on whitespace. `{ 'btn primary': on }` -> `btn`,
    /// `primary`.
    pub(crate) fn quoted_classes(&mut self, value: &str) -> Vec<String> {
        let mut classes: Vec<String> = Vec::new();

        self.with_tree(value, |bytes, node| {
            let Some(literal) = string_literal(bytes, node) else {
                return;
            };

            for token in literal.split_whitespace() {
                if !token.is_empty() && !classes.iter().any(|seen| seen == token) {
                    classes.push(token.to_string());
                }
            }
        });

        classes
    }

    /// The methods of an Alpine `x-data` object literal, each with its 1-based
    /// line, so `@event` handlers resolve to a real node. Covers method
    /// shorthand (`save() {}`) and function-valued properties
    /// (`save: () => {}`). A non-object value yields nothing.
    pub(crate) fn object_methods(&mut self, value: &str, base_line: u32) -> Vec<(String, u32)> {
        if !value.trim_start().starts_with('{') {
            return Vec::new();
        }

        let sanitized = blank_django_tags(value);
        let wrapped = format!("({sanitized})");

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(&wrapped, None)) else {
            return Vec::new();
        };

        let bytes = wrapped.as_bytes();

        let Some(object) = first_object(tree.root_node()) else {
            return Vec::new();
        };

        let mut methods: Vec<(String, u32)> = Vec::new();
        let mut cursor = object.walk();
        let mut count: u32 = 0;

        for child in object.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "object fan-out exceeded {CHILDREN_MAX}");

            if let Some((name, name_node)) = method_member(bytes, child) {
                let line = base_line.saturating_add(to_u32(name_node.start_position().row));

                methods.push((name.to_string(), line));
            }
        }

        methods
    }

    /// The walk that parses `value` as a parenthesized expression and invokes
    /// `visit` on every node of the resulting tree, depth-first.
    fn with_tree(&mut self, value: &str, mut visit: impl FnMut(&[u8], TsNode<'_>)) {
        let sanitized = blank_django_tags(value);
        let wrapped = format!("({sanitized})");

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(&wrapped, None)) else {
            return;
        };

        let bytes = wrapped.as_bytes();
        let mut stack: Vec<TsNode> = vec![tree.root_node()];
        let mut iterations: u32 = 0;

        while let Some(node) = stack.pop() {
            iterations += 1;

            assert!(iterations <= WALK_ITERATIONS_MAX, "walk exceeded {WALK_ITERATIONS_MAX}");

            visit(bytes, node);

            let mut cursor = node.walk();
            let mut count: u32 = 0;

            for child in node.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "child fan-out exceeded {CHILDREN_MAX}");

                stack.push(child);
            }
        }
    }
}

/// The first `object` node in a tree, depth-first: the outermost object of a
/// wrapped `({ ... })` value.
fn first_object(root: TsNode<'_>) -> Option<TsNode<'_>> {
    let mut stack: Vec<TsNode> = vec![root];
    let mut iterations: u32 = 0;

    while let Some(node) = stack.pop() {
        iterations += 1;

        assert!(iterations <= WALK_ITERATIONS_MAX, "object search exceeded {WALK_ITERATIONS_MAX}");

        if node.kind() == "object" {
            return Some(node);
        }

        let mut cursor = node.walk();
        let mut count: u32 = 0;

        for child in node.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "object-search child fan-out exceeded {CHILDREN_MAX}");

            stack.push(child);
        }
    }

    None
}

/// The name and name-node of a direct child of an object literal that defines a
/// method: a `method_definition` (`save() {}`) or a `pair` whose value is
/// a function or arrow (`save: () => {}`). `None` for a non-method child.
fn method_member<'bytes, 'tree>(
    bytes: &'bytes [u8],
    child: TsNode<'tree>,
) -> Option<(&'bytes str, TsNode<'tree>)> {
    match child.kind() {
        "method_definition" => {
            let name_node = child.child_by_field_name("name")?;

            Some((node_text(bytes, name_node), name_node))
        }
        "pair" => {
            let value = child.child_by_field_name("value")?;

            if !matches!(value.kind(), "function" | "function_expression" | "arrow_function" | "generator_function") {
                return None;
            }

            let name_node = child.child_by_field_name("key")?;

            let name = match name_node.kind() {
                "property_identifier" => node_text(bytes, name_node),
                "string" => string_literal(bytes, name_node)?,
                _ => return None,
            };

            if name.is_empty() {
                return None;
            }

            Some((name, name_node))
        }
        _ => None,
    }
}

/// The `(glue_name, field)` of a `Glue.<kind>.<name>.<field>` member access on a
/// field-bearing proxy kind (`model`/`form`). The chain is exactly
/// four links (the `Glue` identifier, the proxy kind, the unique name, and the
/// field), so a deeper `Glue.model.task.address.city` matches only at `.address`
/// (its first field), whose own type the synthesis does not track.
fn glue_field_access<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<(&'bytes str, &'bytes str)> {
    let field = node_text(bytes, node.child_by_field_name("property")?);

    let name_node = node.child_by_field_name("object")?;

    if name_node.kind() != "member_expression" {
        return None;
    }

    let name = node_text(bytes, name_node.child_by_field_name("property")?);

    let kind_node = name_node.child_by_field_name("object")?;

    if kind_node.kind() != "member_expression" {
        return None;
    }

    let kind = node_text(bytes, kind_node.child_by_field_name("property")?);

    if kind != "model" && kind != "form" {
        return None;
    }

    let glue = kind_node.child_by_field_name("object")?;

    if glue.kind() != "identifier" || node_text(bytes, glue) != "Glue" {
        return None;
    }

    Some((name, field))
}

#[cfg(test)]
mod tests {
    use super::{AlpineExpr, blank_django_tags};

    #[test]
    fn blank_django_tags_leaves_tagless_text_untouched() {
        assert_eq!(blank_django_tags("save()"), "save()", "an expression with no tags is returned unchanged");
    }

    #[test]
    fn blank_django_tags_fills_a_variable_tag_with_spaces() {
        let source = "a {{ x }} b";
        let blanked = blank_django_tags(source);

        assert_eq!(blanked, "a         b", "the tag becomes spaces, the surrounding text is preserved");
        assert_eq!(blanked.len(), source.len(), "the byte length is preserved so later offsets hold");
    }

    #[test]
    fn blank_django_tags_preserves_a_newline_inside_a_block_tag() {
        let source = "{% if\n x %}z";
        let blanked = blank_django_tags(source);

        assert_eq!(blanked.len(), source.len(), "the byte length is preserved");
        assert_eq!(blanked.matches('\n').count(), 1, "the newline inside the tag survives so lines stay aligned");
        assert!(!blanked.contains('{') && !blanked.contains('}'), "the tag braces are blanked away");
        assert!(blanked.ends_with('z'), "the text after the tag is preserved");
    }

    #[test]
    fn blank_django_tags_blanks_multibyte_content_byte_for_byte() {
        let source = "{{ café }}";
        let blanked = blank_django_tags(source);

        assert_eq!(blanked.len(), source.len(), "a multibyte char keeps its byte length");
        assert!(blanked.bytes().all(|byte| byte == b' '), "the whole tag is blanked to ascii spaces");
    }

    #[test]
    fn blank_django_tags_blanks_an_unterminated_tag_through_the_end() {
        assert_eq!(blank_django_tags("{{ x"), "    ", "an unclosed tag is blanked to the end of input");
    }

    #[test]
    fn call_identifiers_collects_bare_calls_and_skips_member_calls() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        let mut names = expressions.call_identifiers("save(); other.method(); compute()");
        names.sort();

        assert_eq!(
            names,
            vec!["compute".to_string(), "save".to_string()],
            "bare function calls are collected; a member call is not",
        );
    }

    #[test]
    fn call_identifiers_deduplicates_repeated_calls() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.call_identifiers("save(); save()"),
            vec!["save".to_string()],
            "a repeated call appears once",
        );
    }

    #[test]
    fn dispatched_events_reads_dispatch_string_arguments() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.dispatched_events("$dispatch('refresh'); $dispatch('refresh')"),
            vec!["refresh".to_string()],
            "the dispatched event name is read once",
        );

        assert!(
            expressions.dispatched_events("save()").is_empty(),
            "an expression with no $dispatch yields no events",
        );
    }

    #[test]
    fn glue_member_accesses_matches_model_and_form_proxies_only() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.glue_member_accesses("Glue.model.task.title"),
            vec![("task".to_string(), "title".to_string())],
            "a model proxy yields (unique_name, field)",
        );

        assert_eq!(
            expressions.glue_member_accesses("Glue.form.contact.email"),
            vec![("contact".to_string(), "email".to_string())],
            "a form proxy yields (unique_name, field)",
        );

        assert!(
            expressions.glue_member_accesses("Glue.querySet.items.name").is_empty(),
            "a non-field-bearing proxy kind (querySet) is skipped",
        );
    }

    #[test]
    fn quoted_classes_splits_string_literals_on_whitespace() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.quoted_classes("{ 'btn primary': active }"),
            vec!["btn".to_string(), "primary".to_string()],
            "a quoted class string splits into tokens; the identifier value is not a class",
        );
    }

    #[test]
    fn object_methods_finds_shorthand_and_function_valued_properties() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.object_methods("{ save() {}, count: 0, load: () => {} }", 1),
            vec![("save".to_string(), 1), ("load".to_string(), 1)],
            "method shorthand and arrow-valued properties are methods; a data field is not",
        );
    }

    #[test]
    fn object_methods_offsets_each_method_line_by_the_base() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.object_methods("{\n  alpha() {},\n  beta() {}\n}", 10),
            vec![("alpha".to_string(), 11), ("beta".to_string(), 12)],
            "each method's line is the base line plus its row within the value",
        );
    }

    #[test]
    fn object_methods_ignores_a_non_object_value() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert!(
            expressions.object_methods("save()", 1).is_empty(),
            "an expression that is not an object literal has no methods",
        );
    }
}
