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

use crate::jsobject::{AlpineObject, first_object, method_member, typed_property};
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
pub(crate) fn is_js_non_handler(name: &str) -> bool {
    name.starts_with('$') || JS_CALL_BUILTINS.contains(&name)
}

/// The JavaScript global constructors instantiated from Alpine expressions that
/// are never project-defined classes, so an `Instantiates` reference to one
/// never resolves and is pure noise. Skipped when collecting instantiations,
/// the constructor analogue of [`JS_CALL_BUILTINS`].
const JS_NEW_BUILTINS: &[&str] = &[
    "AbortController",
    "Array",
    "Audio",
    "Blob",
    "CustomEvent",
    "DOMParser",
    "Date",
    "Error",
    "Event",
    "File",
    "FileReader",
    "FormData",
    "Image",
    "IntersectionObserver",
    "Map",
    "MutationObserver",
    "Object",
    "Promise",
    "Proxy",
    "RegExp",
    "ResizeObserver",
    "Set",
    "URL",
    "URLSearchParams",
    "WeakMap",
    "WeakSet",
    "WebSocket",
    "XMLHttpRequest",
];

/// Whether an instantiated name is a non-project JavaScript constructor: an
/// Alpine magic (`$`-prefixed) or a builtin in [`JS_NEW_BUILTINS`]. Such a
/// `new` never names a project-defined class, so it must not become an
/// `Instantiates` reference or type a component property.
pub(crate) fn is_js_builtin_constructor(name: &str) -> bool {
    name.starts_with('$') || JS_NEW_BUILTINS.contains(&name)
}

/// A fail-fast bound on the node-walk loop.
pub(crate) const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single node.
pub(crate) const CHILDREN_MAX: u32 = 1_000_000;

/// A fail-fast bound on the descent through an expression's single-child levels,
/// far past the nesting a wrapped Alpine attribute value can produce.
const EXPRESSION_DEPTH_MAX: u32 = 1_000;

/// The property a glue object exposes its field map under, in each django-glue
/// version this indexes: `glue_fields` in 0.x, `$fields` in 1.x. Both spell the
/// same access, `<glue_name>.<map>.<field>`, and both versions are installed
/// across the projects here, so both are read.
pub(crate) const GLUE_FIELD_MAPS: &[&str] = &["$fields", "glue_fields"];

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

    /// The single bare identifier the expression consists of (`printQrCode`), or
    /// `None` when it is anything more: a call, an assignment, a member access, a
    /// literal, a comparison.
    ///
    /// Alpine invokes a listener expression whose value is a function, so
    /// `@click="printQrCode"` binds the same handler `@click="printQrCode()"`
    /// does. A bare identifier naming state rather than a method
    /// (`@click="open"`) has no function of that name to bind to and stays
    /// pending, so reading one can add no false edge.
    pub(crate) fn bare_identifier(&mut self, value: &str) -> Option<String> {
        let sanitized = blank_django_tags(value);
        let trimmed = sanitized.trim();

        if trimmed.is_empty() {
            return None;
        }

        let wrapped = format!("({trimmed})");
        let tree = PARSER.with(|parser| parser.borrow_mut().parse(&wrapped, None))?;

        let identifier = sole_identifier(tree.root_node())?;
        let name = node_text(wrapped.as_bytes(), identifier);

        if name.is_empty() || is_js_non_handler(name) {
            return None;
        }

        Some(name.to_string())
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

    /// The django-glue field accesses in the expression, in either version's
    /// spelling: the 1.x proxy (`Glue.model.task.title`, `Glue.form.contact.email`)
    /// and the field map both versions expose on a glue object
    /// (`record.glue_fields.quantity` in 0.x, `record.$fields.quantity` in 1.x).
    /// Each yields a `(glue_name, field)` pair, where `glue_name` is the unique
    /// name the object was registered under and `field` is the field read on it.
    /// Deduplicated, source order preserved. Drives the `AccessesMember` edges
    /// (template -> model member).
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

    /// The members of an Alpine `x-data` object literal: its methods, each with
    /// its 1-based line (so `@event` handlers resolve to a real node) and the
    /// calls its body makes, plus its `new`-initialized data properties
    /// (`rows: new QuerySetGlue('rows')`), whose classes type the
    /// `this.<property>.<method>()` calls the method bodies make. Covers method
    /// shorthand (`save() {}`) and function-valued properties
    /// (`save: () => {}`). A non-object value yields nothing.
    pub(crate) fn x_data_object(&mut self, value: &str, base_line: u32) -> AlpineObject {
        if !value.trim_start().starts_with('{') {
            return AlpineObject::empty();
        }

        let sanitized = blank_django_tags(value);
        let wrapped = format!("({sanitized})");

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(&wrapped, None)) else {
            return AlpineObject::empty();
        };

        let bytes = wrapped.as_bytes();

        let Some(object) = first_object(tree.root_node()) else {
            return AlpineObject::empty();
        };

        let mut component = AlpineObject::empty();
        let mut cursor = object.walk();
        let mut count: u32 = 0;

        for child in object.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "object fan-out exceeded {CHILDREN_MAX}");

            if let Some(method) = method_member(bytes, child, base_line) {
                component.methods.push(method);
            } else if let Some(property) = typed_property(bytes, child) {
                component.typed_properties.push(property);
            }
        }

        component
    }

    /// The class names instantiated in the expression (`new QuerySetGlue('x')`
    /// -> `QuerySetGlue`), each with the 1-based line of its first
    /// instantiation. Covers the bare constructor and the trailing property of
    /// a member access (`new glue.QuerySetGlue()`); builtin constructors are
    /// skipped. Deduplicated, ordered by line. Drives the `Instantiates`
    /// references from a template's Alpine attributes to the JavaScript
    /// classes its component state is built from.
    pub(crate) fn instantiations(&mut self, value: &str, base_line: u32) -> Vec<(String, u32)> {
        let mut classes: Vec<(String, u32)> = Vec::new();

        self.with_tree(value, |bytes, node| {
            if node.kind() != "new_expression" {
                return;
            }

            let Some(constructor) = node.child_by_field_name("constructor") else {
                return;
            };

            let Some(name) = callee_name(bytes, constructor) else {
                return;
            };

            if name.is_empty() || is_js_builtin_constructor(name) {
                return;
            }

            let line = base_line.saturating_add(to_u32(node.start_position().row));

            // The walk visits nodes in reverse source order, so an already-seen
            // name keeps the smallest line rather than the first visited.
            match classes.iter_mut().find(|(seen, _)| seen == name) {
                Some((_, seen_line)) => *seen_line = (*seen_line).min(line),
                None => classes.push((name.to_string(), line)),
            }
        });

        classes.sort_by_key(|(_, line)| *line);

        classes
    }

    /// The `(store, member)` Alpine store method calls in the expression
    /// (`$store.theme.families()` -> `("theme", "families")`). Only the exact
    /// three-link chain rooted at the `$store` magic matches, so a deeper
    /// `$store.theme.config.load()` contributes nothing rather than a wrong
    /// pair. Deduplicated, source order preserved.
    pub(crate) fn store_calls(&mut self, value: &str) -> Vec<(String, String)> {
        let mut calls: Vec<(String, String)> = Vec::new();

        self.with_tree(value, |bytes, node| {
            if node.kind() != "call_expression" {
                return;
            }

            let Some(function) = node.child_by_field_name("function") else {
                return;
            };

            if let Some((store, member)) = store_member(bytes, function)
                && !calls.iter().any(|(seen_store, seen_member)| {
                    seen_store == store && seen_member == member
                })
            {
                calls.push((store.to_string(), member.to_string()));
            }
        });

        calls
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

/// The lone `identifier` an expression tree reduces to, following the single
/// named child down from the wrapped root. `None` the moment a level holds
/// anything other than exactly one named child, or the descent ends on a node
/// that is not an identifier: either means the expression is more than a name.
fn sole_identifier(root: TsNode<'_>) -> Option<TsNode<'_>> {
    let mut node = root;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= EXPRESSION_DEPTH_MAX, "descent exceeded {EXPRESSION_DEPTH_MAX} levels");

        if node.kind() == "identifier" {
            return Some(node);
        }

        let only = {
            let mut cursor = node.walk();
            let mut children = node.named_children(&mut cursor);

            match (children.next(), children.next()) {
                (Some(only), None) => only,
                _ => return None,
            }
        };

        node = only;
    }
}

/// The `(store, member)` of a `$store.<store>.<member>` chain. The chain is
/// exactly three links (the `$store` magic, the store name, and the member),
/// so a deeper access does not match.
fn store_member<'bytes>(
    bytes: &'bytes [u8],
    node: TsNode<'_>,
) -> Option<(&'bytes str, &'bytes str)> {
    if node.kind() != "member_expression" {
        return None;
    }

    let member = node_text(bytes, node.child_by_field_name("property")?);

    let store_node = node.child_by_field_name("object")?;

    if store_node.kind() != "member_expression" {
        return None;
    }

    let store = node_text(bytes, store_node.child_by_field_name("property")?);

    let magic = store_node.child_by_field_name("object")?;

    if magic.kind() != "identifier" || node_text(bytes, magic) != "$store" {
        return None;
    }

    Some((store, member))
}

/// The `(glue_name, field)` of a `Glue.<kind>.<name>.<field>` member access on a
/// field-bearing proxy kind (`model`/`form`). The chain is exactly
/// four links (the `Glue` identifier, the proxy kind, the unique name, and the
/// field), so a deeper `Glue.model.task.address.city` matches only at `.address`
/// (its first field), whose own type the synthesis does not track.
fn glue_field_access<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<(&'bytes str, &'bytes str)> {
    proxy_field_access(bytes, node).or_else(|| field_map_access(bytes, node))
}

/// The `(glue_name, field)` of a 1.x proxy access, `Glue.model.task.title` /
/// `Glue.form.contact.email`. Only the field-bearing proxy kinds (`model`,
/// `form`) match; `querySet`, `template`, `function`, and `json` are skipped,
/// the first because its fields belong to the elements rather than the
/// collection and the rest because they carry no model fields at all.
fn proxy_field_access<'bytes>(
    bytes: &'bytes [u8],
    node: TsNode<'_>,
) -> Option<(&'bytes str, &'bytes str)> {
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

/// The `(glue_name, field)` of a field-map access, `record.glue_fields.quantity`
/// (0.x) or `record.$fields.quantity` (1.x), which is how a template reads a
/// field off a glue object it was handed rather than off the `Glue` root. The
/// dominant spelling in the 0.x portals, where the proxy API does not exist.
///
/// The owner may itself be a member expression (`this.record.$fields.quantity`
/// inside an `x-data` method), in which case its trailing property is the glue
/// unique name.
fn field_map_access<'bytes>(
    bytes: &'bytes [u8],
    node: TsNode<'_>,
) -> Option<(&'bytes str, &'bytes str)> {
    let field = node_text(bytes, node.child_by_field_name("property")?);

    let map_node = node.child_by_field_name("object")?;

    if map_node.kind() != "member_expression" {
        return None;
    }

    let map = node_text(bytes, map_node.child_by_field_name("property")?);

    if !GLUE_FIELD_MAPS.contains(&map) {
        return None;
    }

    let owner = map_node.child_by_field_name("object")?;
    let name = match owner.kind() {
        "identifier" => node_text(bytes, owner),
        "member_expression" => node_text(bytes, owner.child_by_field_name("property")?),
        _ => return None,
    };

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
    fn bare_identifier_reads_a_lone_name_and_refuses_anything_more() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.bare_identifier("printQrCode"),
            Some("printQrCode".to_string()),
            "a lone name is the handler Alpine would invoke",
        );
        assert_eq!(
            expressions.bare_identifier("  close_modal  "),
            Some("close_modal".to_string()),
            "surrounding whitespace does not change the name",
        );

        let not_handlers = [
            "save()",
            "open = !open",
            "obj.method",
            "$dispatch",
            "a && b",
            "'text'",
            "",
        ];

        for expression in not_handlers {
            assert_eq!(
                expressions.bare_identifier(expression),
                None,
                "{expression:?} is more than a bare name, or is not a handler",
            );
        }
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
    fn instantiations_reads_project_constructors_and_skips_builtins() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        let value = "{\n  contract: new ModelObjectGlue('contract'),\n  rows: new QuerySetGlue('rows'),\n  \
                     again: new ModelObjectGlue('again'),\n  when: new Date(),\n}";

        assert_eq!(
            expressions.instantiations(value, 5),
            vec![("ModelObjectGlue".to_string(), 6), ("QuerySetGlue".to_string(), 7)],
            "each project class appears once at its first line; a builtin (Date) is skipped",
        );
    }

    #[test]
    fn store_calls_reads_the_exact_store_chain_only() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        assert_eq!(
            expressions.store_calls("$store.theme.families().then(f => families = f)"),
            vec![("theme".to_string(), "families".to_string())],
            "a $store.<name>.<member>() call yields (store, member)",
        );

        assert!(
            expressions.store_calls("$store.theme.config.load()").is_empty(),
            "a deeper chain does not guess at a (store, member) pair",
        );

        assert!(
            expressions.store_calls("store.theme.families()").is_empty(),
            "a chain not rooted at the $store magic is not a store call",
        );
    }
}
