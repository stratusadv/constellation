use std::cell::RefCell;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{EventRecord, EventRole, UnresolvedRef};
use tree_sitter::{Node as TsNode, Parser};

use crate::django::{self, AstNode};
use crate::jsexpr::AlpineExpr;
use crate::tsutil::{line_1based, node_text};
use crate::{ExtractionOutput, Extractor};

/// A fail-fast bound on the HTML node-walk loop.
const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single HTML node.
const CHILDREN_MAX: u32 = 1_000_000;

/// The provenance tag on edges this extractor produces for Alpine component methods.
const ALPINE_PROVENANCE: &str = "alpine";

/// Django's built-in template tags (and the `{% end... %}` closers and clause
/// words a leaf/block tag node can surface), none of which a project defines, so a
/// `UsesTag` reference to one would only be permanent noise. A tag not listed is a
/// custom `{% my_tag %}` from a `{% load %}`-ed library, resolved to its
/// `@register.simple_tag`/`inclusion_tag` function by name.
const TEMPLATE_BUILTIN_TAGS: &[&str] = &[
    "autoescape", "block", "blocktrans", "blocktranslate", "comment", "csrf_token", "cycle",
    "debug", "elif", "else", "empty", "endautoescape", "endblock", "endblocktrans",
    "endblocktranslate", "endcomment", "endfilter", "endfor", "endif", "endifchanged",
    "endspaceless", "endverbatim", "endwith", "extends", "filter", "firstof", "for", "if",
    "ifchanged", "include", "load", "lorem", "now", "plural", "regroup", "resetcycle", "spaceless",
    "static", "templatetag", "trans", "translate", "url", "verbatim", "widthratio", "with",
];

/// Django's built-in template filters plus the `humanize` contrib set, excluded
/// from `UsesTag` references for the same reason as the built-in tags. A filter not
/// listed is a custom `@register.filter`.
const TEMPLATE_BUILTIN_FILTERS: &[&str] = &[
    "add", "addslashes", "apnumber", "capfirst", "center", "cut", "date", "default",
    "default_if_none", "dictsort", "dictsortreversed", "divisibleby", "escape", "escapejs",
    "filesizeformat", "first", "floatformat", "force_escape", "get_digit", "intcomma", "intword",
    "iriencode", "join", "json_script", "last", "length", "length_is", "linebreaks",
    "linebreaksbr", "linenumbers", "ljust", "lower", "make_list", "naturalday", "naturaltime",
    "ordinal", "phone2numeric", "pluralize", "pprint", "random", "rjust", "safe", "safeseq",
    "slice", "slugify", "stringformat", "striptags", "time", "timesince", "timeuntil", "title",
    "truncatechars", "truncatechars_html", "truncatewords", "truncatewords_html", "unordered_list",
    "upper", "urlencode", "urlize", "urlizetrunc", "wordcount", "wordwrap", "yesno",
];

/// An extractor of Django templates across three proper parsers, replacing the former
/// hand-rolled byte scanners: the `django` front end reads `{% extends %}` /
/// `{% include %}` / `{% url %}` into cross-template references; tree-sitter-html
/// reads element attributes (`class`, `src`, `href`, and Alpine directives); and
/// `AlpineExpr` reads the JavaScript embedded in those Alpine attribute values.
pub struct TemplateExtractor;

thread_local! {
    /// The per-thread HTML parser, reused across template files so each file pays
    /// only for its parse, not for parser construction. One parser per rayon
    /// worker thread, no cross-thread sharing. The embedded Alpine expressions go
    /// through a separate per-thread JavaScript parser in [`crate::jsexpr`].
    static PARSER: RefCell<Parser> = RefCell::new(new_parser());
}

/// An HTML parser with the grammar loaded. It panics only on a grammar against
/// tree-sitter ABI mismatch, a build error that cannot arise at runtime in a
/// correctly linked binary.
fn new_parser() -> Parser {
    let html_language: tree_sitter::Language = tree_sitter_html::LANGUAGE.into();

    assert!(html_language.node_kind_count() > 0, "html grammar must expose node kinds");

    let mut parser = Parser::new();

    parser
        .set_language(&html_language)
        .expect("the bundled html grammar is ABI-compatible with tree-sitter");

    parser
}

impl TemplateExtractor {
    /// The extractor; the grammar loads per worker thread on first use.
    pub fn new() -> Self {
        Self
    }
}

impl Default for TemplateExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for TemplateExtractor {
    fn language(&self) -> Language {
        Language::HtmlDjango
    }

    fn extract(&self, project: &ProjectId, file_path: &str, source: &str) -> ExtractionOutput {
        assert!(!file_path.is_empty(), "file_path must not be empty");

        let mut output = ExtractionOutput::empty();

        let logical = template_name(file_path);

        assert!(!logical.is_empty(), "a template name is never empty");

        let template_id = NodeId::new(project, &logical);

        let identity = NodeIdentity {
            name: logical.clone(),
            qualified_name: logical.clone(),
            file_path: file_path.to_string(),
            language: Language::HtmlDjango,
        };

        output.nodes.push(Node::new(
            template_id.clone(),
            project.clone(),
            NodeKind::Template,
            identity,
            Span::new(1, 1, 0, 0),
            0,
        ));

        let tree = django::parse(source);
        collect_template_links(&tree, &template_id, file_path, &mut output);

        let context = HtmlContext { project, file_path, logical: &logical, template_id: &template_id };
        self.scan_html(&context, source, &mut output);

        output
    }
}

/// The template name Django would use to reference this file: the path after
/// the last `templates/` directory, or the bare file name when there is none.
fn template_name(file_path: &str) -> String {
    const MARKER: &str = "templates/";

    assert!(!file_path.is_empty(), "file_path must not be empty");

    if let Some(index) = file_path.rfind(MARKER) {
        return file_path[index + MARKER.len()..].to_string();
    }

    file_path.rsplit('/').next().unwrap_or(file_path).to_string()
}

/// The walk of the parsed template tree, emitting a reference for every `{% extends %}`
/// / `{% include %}` / `{% url %}` / `{% static %}` whose target is a string
/// literal. The walk is an explicit stack (no recursion) over the node bodies.
fn collect_template_links(
    nodes: &[AstNode<'_>],
    template_id: &NodeId,
    file_path: &str,
    output: &mut ExtractionOutput,
) {
    let mut stack: Vec<&[AstNode]> = vec![nodes];
    let mut iterations: u32 = 0;

    while let Some(slice) = stack.pop() {
        iterations += 1;

        assert!(iterations <= WALK_ITERATIONS_MAX, "template walk exceeded {WALK_ITERATIONS_MAX}");

        for node in slice {
            if let Some((kind, target, line)) = link_of(node) {
                output.unresolved_refs.push(UnresolvedRef::new(
                    template_id.clone(),
                    target,
                    kind,
                    line,
                    0,
                    file_path,
                    Language::HtmlDjango,
                ));
            }

            if let AstNode::Variable { expression, line } = node
                && let Some((variable, member)) = variable_member(expression)
            {
                let mut reference = UnresolvedRef::new(
                    template_id.clone(),
                    member,
                    EdgeKind::AccessesMember,
                    *line,
                    0,
                    file_path,
                    Language::HtmlDjango,
                );

                reference.candidates.push(variable.to_string());

                output.unresolved_refs.push(reference);
            }

            if let AstNode::For { variable, iterable, .. } = node
                && let Some((loop_variable, source, accessor)) = loop_binding(variable, iterable)
            {
                let mut reference = UnresolvedRef::new(
                    template_id.clone(),
                    source,
                    EdgeKind::LoopBinding,
                    1,
                    0,
                    file_path,
                    Language::HtmlDjango,
                );

                reference.candidates.push(loop_variable.to_string());

                if let Some(accessor) = accessor {
                    reference.candidates.push(accessor.to_string());
                }

                output.unresolved_refs.push(reference);
            }

            if let AstNode::Include { bindings, line, .. } = node {
                for binding in bindings {
                    let Some((variable, field)) = glue_field_binding(binding.name, binding.value) else {
                        continue;
                    };

                    let mut reference = UnresolvedRef::new(
                        template_id.clone(),
                        field,
                        EdgeKind::AccessesMember,
                        *line,
                        0,
                        file_path,
                        Language::HtmlDjango,
                    );

                    reference.candidates.push(variable.to_string());

                    output.unresolved_refs.push(reference);
                }
            }

            for (name, line) in template_tag_filter_uses(node) {
                output.unresolved_refs.push(UnresolvedRef::new(
                    template_id.clone(),
                    name,
                    EdgeKind::UsesTag,
                    line,
                    0,
                    file_path,
                    Language::HtmlDjango,
                ));
            }

            node.push_child_slices(&mut stack);
        }
    }
}

/// The custom template-tag and filter names a node invokes that are not Django
/// built-ins: a leaf/block `{% my_tag %}`, a `{% filter my_filter %}` block, or the
/// `|my_filter` segments of a `{{ value|my_filter }}` variable. Each resolves to its
/// `@register.simple_tag`/`filter` function by name. Built-ins are excluded so a
/// `{% if %}` or a `|date` never becomes a dangling reference. Tag/block nodes carry
/// no line, so those reference line 1.
fn template_tag_filter_uses(node: &AstNode<'_>) -> Vec<(String, u32)> {
    let mut uses: Vec<(String, u32)> = Vec::new();

    match node {
        AstNode::Tag { name, .. } | AstNode::Container { name, .. } => {
            let name = *name;

            if !name.is_empty() && !TEMPLATE_BUILTIN_TAGS.contains(&name) {
                uses.push((name.to_string(), 1));
            }
        }
        AstNode::Filter { specification, .. } => {
            let name = specification.split([':', ' ']).next().unwrap_or(specification);

            if !name.is_empty() && !TEMPLATE_BUILTIN_FILTERS.contains(&name) {
                uses.push((name.to_string(), 1));
            }
        }
        AstNode::Variable { expression, line } => {
            for name in filter_names(expression) {
                if !TEMPLATE_BUILTIN_FILTERS.contains(&name) {
                    uses.push((name.to_string(), *line));
                }
            }
        }
        _ => {}
    }

    uses
}

/// The filter names applied in a variable expression: each `|`-separated segment
/// after the variable itself, truncated at its argument
/// (`value|truncatewords:30|money` -> `["truncatewords", "money"]`).
fn filter_names(expression: &str) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    let mut segments = expression.split('|');

    segments.next();

    for segment in segments {
        let name = segment.trim().split([':', ' ']).next().unwrap_or("").trim();

        if !name.is_empty() {
            names.push(name);
        }
    }

    names
}

/// The cross-template reference a node carries, if any: a literal `extends`
/// parent, a literal `include` target, a literal `url` route name, or a literal
/// `static` asset (linked by basename, like a raw `src`/`href`).
fn link_of<'tree>(node: &'tree AstNode<'_>) -> Option<(EdgeKind, &'tree str, u32)> {
    match node {
        AstNode::Extends { path, is_literal: true, line } if !path.is_empty() => {
            Some((EdgeKind::ExtendsTemplate, path, *line))
        }
        AstNode::Include { path, is_literal: true, line, .. } if !path.is_empty() => {
            Some((EdgeKind::IncludesTemplate, path, *line))
        }
        AstNode::Url { name, is_literal: true, line, .. } if !name.is_empty() => {
            Some((EdgeKind::Resolves, name, *line))
        }
        AstNode::Static { path, is_literal: true, line, .. } if !path.is_empty() => {
            let asset = asset_basename(path);

            if asset.is_empty() {
                None
            } else {
                Some((EdgeKind::References, asset, *line))
            }
        }
        _ => None,
    }
}

/// The identifiers a single HTML scan needs to attribute its findings.
struct HtmlContext<'context> {
    project: &'context ProjectId,
    file_path: &'context str,
    logical: &'context str,
    template_id: &'context NodeId,
}

impl TemplateExtractor {
    /// The HTML parse and per-attribute walk, emitting style/asset
    /// references, Alpine `Handles` references, dispatch/listen event records,
    /// and a function node per Alpine `x-data` method.
    fn scan_html(&self, context: &HtmlContext<'_>, source: &str, output: &mut ExtractionOutput) {
        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(source, None)) else {
            return;
        };

        let bytes = source.as_bytes();
        let mut alpine = AlpineExpr::new();
        let mut stack: Vec<TsNode> = vec![tree.root_node()];
        let mut iterations: u32 = 0;

        while let Some(node) = stack.pop() {
            iterations += 1;

            assert!(iterations <= WALK_ITERATIONS_MAX, "html walk exceeded {WALK_ITERATIONS_MAX}");

            if node.kind() == "attribute"
                && let Some(attribute) = attribute_parts(bytes, node)
            {
                process_attribute(context, &mut alpine, &attribute, output);
            }

            let mut cursor = node.walk();
            let mut count: u32 = 0;

            for child in node.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "html child fan-out exceeded {CHILDREN_MAX}");

                stack.push(child);
            }
        }
    }
}

/// The pieces of one HTML attribute: its name, its unquoted value, and the
/// 1-based line that value begins on.
struct Attribute<'source> {
    name: &'source str,
    value: &'source str,
    value_line: u32,
}

/// An attribute node's name and value. `None` when the attribute has no
/// value (`x-cloak`), which carries nothing to link.
fn attribute_parts<'source>(bytes: &'source [u8], node: TsNode<'_>) -> Option<Attribute<'source>> {
    let mut name: Option<&str> = None;
    let mut value: Option<(&str, u32)> = None;
    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "attribute_name" => name = Some(node_text(bytes, child)),
            "attribute_value" => {
                value = Some((node_text(bytes, child), line_1based(child.start_position().row)));
            }
            "quoted_attribute_value" => {
                value = Some(quoted_value(bytes, child));
            }
            _ => {}
        }
    }

    let name = name?;
    let (value, value_line) = value?;

    Some(Attribute { name, value, value_line })
}

/// The unquoted text of a `quoted_attribute_value` and its line; an empty quoted
/// value (`class=""`) has no inner node, so the value is the empty string.
fn quoted_value<'source>(bytes: &'source [u8], node: TsNode<'_>) -> (&'source str, u32) {
    let line = line_1based(node.start_position().row);
    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_value" {
            return (node_text(bytes, child), line_1based(child.start_position().row));
        }
    }

    ("", line)
}

/// The references, event records, and Alpine method nodes one attribute turns
/// into, dispatching on the attribute name.
fn process_attribute(
    context: &HtmlContext<'_>,
    alpine: &mut Option<AlpineExpr>,
    attribute: &Attribute<'_>,
    output: &mut ExtractionOutput,
) {
    let Attribute { name, value, value_line } = *attribute;

    assert!(!name.is_empty(), "attribute name must not be empty");

    // The rewrite's frontend reads model fields as `Glue.model.<name>.<field>` /
    // `Glue.form.<name>.<field>` in any Alpine attribute value (an x-data object
    // included), so extract those accesses up front. The cheap substring gate
    // keeps non-glue attributes (the common case) from being parsed.
    if value.contains("Glue.") {
        for (glue_name, field) in glue_member_accesses(alpine, value) {
            push_glue_access(context, &glue_name, &field, value_line, output);
        }
    }

    if name == "x-data" {
        for (method, line) in object_methods(alpine, value, value_line) {
            emit_alpine_method(context, &method, line, output);
        }

        if !value.trim_start().starts_with('{') {
            classify_attribute(context, alpine, name, value, value_line, output);
        }

        return;
    }

    classify_attribute(context, alpine, name, value, value_line, output);
}

/// The classification of a non-`x-data` attribute, emitting what it carries: plain
/// `class` styles, `src`/`href` asset references, or, for an Alpine directive, the
/// `Handles` references and dispatch/listen events of its expression.
fn classify_attribute(
    context: &HtmlContext<'_>,
    alpine: &mut Option<AlpineExpr>,
    name: &str,
    value: &str,
    line: u32,
    output: &mut ExtractionOutput,
) {
    if name == "class" {
        for class in value.split_whitespace() {
            if !class.is_empty() && !class.contains(['{', '}']) {
                push_attribute_ref(context, EdgeKind::Styles, class, line, output);
            }
        }
    } else if name == "src" {
        emit_asset(context, value, line, output);
    } else if name == "href" {
        if asset_basename(value).ends_with(".css") {
            emit_asset(context, value, line, output);
        }
    } else if is_class_binding(name) {
        for class in quoted_classes(alpine, value) {
            push_attribute_ref(context, EdgeKind::Styles, &class, line, output);
        }

        for callee in call_identifiers(alpine, value) {
            push_attribute_ref(context, EdgeKind::Handles, &callee, line, output);
        }
    } else if is_alpine(name) {
        emit_alpine_directive(context, alpine, name, value, line, output);
    }
}

/// The `Handles` references and dispatch/listen event records of one Alpine
/// directive's expression.
fn emit_alpine_directive(
    context: &HtmlContext<'_>,
    alpine: &mut Option<AlpineExpr>,
    name: &str,
    value: &str,
    line: u32,
    output: &mut ExtractionOutput,
) {
    let callees = call_identifiers(alpine, value);

    for callee in &callees {
        push_attribute_ref(context, EdgeKind::Handles, callee, line, output);
    }

    if let Some(event) = listener_event(name) {
        for handler in &callees {
            output.events.push(EventRecord {
                role: EventRole::Listen,
                event: event.clone(),
                symbol: handler.clone(),
                line,
                column: 0,
            });
        }
    }

    for event in dispatched_events(alpine, value) {
        output.events.push(EventRecord {
            role: EventRole::Dispatch,
            event,
            symbol: context.template_id.as_str().to_string(),
            line,
            column: 0,
        });
    }
}

/// A `References` edge to the asset a `src`/`href` points at, skipping
/// values that hold a Django tag (`{% static %}`), which the template front end
/// owns.
fn emit_asset(context: &HtmlContext<'_>, value: &str, line: u32, output: &mut ExtractionOutput) {
    if value.contains("{%") || value.contains("{{") {
        return;
    }

    let asset = asset_basename(value);

    if !asset.is_empty() {
        push_attribute_ref(context, EdgeKind::References, asset, line, output);
    }
}

/// A function node for one Alpine `x-data` method plus its containment edge
/// from the template, so an `@event` handler resolves to it. The node id mirrors
/// an `Alpine.data` component (`<template>::alpine::<method>`).
fn emit_alpine_method(
    context: &HtmlContext<'_>,
    method: &str,
    line: u32,
    output: &mut ExtractionOutput,
) {
    assert!(!method.is_empty(), "alpine method name must not be empty");

    let raw = format!("{}::alpine::{method}", context.logical);
    let qualified = format!("{}::{method}", context.logical);
    let method_id = NodeId::new(context.project, &raw);

    output.nodes.push(Node::new(
        method_id.clone(),
        context.project.clone(),
        NodeKind::Function,
        NodeIdentity {
            name: method.to_string(),
            qualified_name: qualified,
            file_path: context.file_path.to_string(),
            language: Language::JavaScript,
        },
        Span::new(line, line, 0, 0),
        0,
    ));

    output.edges.push(
        Edge::new(context.template_id.clone(), method_id, EdgeKind::Contains)
            .at(line, 0)
            .with_provenance(ALPINE_PROVENANCE),
    );
}

/// The push of one cross-layer attribute reference from the template.
fn push_attribute_ref(
    context: &HtmlContext<'_>,
    kind: EdgeKind,
    name: &str,
    line: u32,
    output: &mut ExtractionOutput,
) {
    assert!(!name.is_empty(), "reference name must not be empty");

    output.unresolved_refs.push(UnresolvedRef::new(
        context.template_id.clone(),
        name,
        kind,
        line,
        0,
        context.file_path,
        Language::HtmlDjango,
    ));
}

/// The push of a django-glue field access (`Glue.model.<glue_name>.<field>`) as an
/// `AccessesMember` reference keyed by the glue unique name, for the member
/// synthesis to type-scope through the view that registered it (reachable from
/// this template up the rendering/include chain).
fn push_glue_access(
    context: &HtmlContext<'_>,
    glue_name: &str,
    field: &str,
    line: u32,
    output: &mut ExtractionOutput,
) {
    if glue_name.is_empty() || field.is_empty() {
        return;
    }

    let mut reference = UnresolvedRef::new(
        context.template_id.clone(),
        field,
        EdgeKind::AccessesMember,
        line,
        0,
        context.file_path,
        Language::HtmlDjango,
    );

    reference.candidates.push(glue_name.to_string());

    output.unresolved_refs.push(reference);
}

/// The bare function names called in an Alpine expression, parsed with the
/// JavaScript grammar; empty when the parser is unavailable.
fn call_identifiers(alpine: &mut Option<AlpineExpr>, value: &str) -> Vec<String> {
    alpine.as_mut().map(|parser| parser.call_identifiers(value)).unwrap_or_default()
}

/// The `(glue_name, field)` django-glue member accesses in an Alpine expression
/// (`Glue.model.task.title`); empty when the parser is unavailable.
fn glue_member_accesses(alpine: &mut Option<AlpineExpr>, value: &str) -> Vec<(String, String)> {
    alpine.as_mut().map(|parser| parser.glue_member_accesses(value)).unwrap_or_default()
}

/// The `$dispatch('event')` event names in an Alpine expression.
fn dispatched_events(alpine: &mut Option<AlpineExpr>, value: &str) -> Vec<String> {
    alpine.as_mut().map(|parser| parser.dispatched_events(value)).unwrap_or_default()
}

/// The CSS class string literals inside an Alpine class-binding expression.
fn quoted_classes(alpine: &mut Option<AlpineExpr>, value: &str) -> Vec<String> {
    alpine.as_mut().map(|parser| parser.quoted_classes(value)).unwrap_or_default()
}

/// The methods of an Alpine `x-data` object literal with their 1-based lines.
fn object_methods(alpine: &mut Option<AlpineExpr>, value: &str, base_line: u32) -> Vec<(String, u32)> {
    alpine.as_mut().map(|parser| parser.object_methods(value, base_line)).unwrap_or_default()
}

/// The event name an Alpine listener attribute binds: `@click.prevent` ->
/// `click`, `x-on:cart-updated.window` -> `cart-updated`. `None` for non-listener
/// directives (`x-data`, `:class`, ...).
fn listener_event(name: &str) -> Option<String> {
    let raw = name.strip_prefix('@').or_else(|| name.strip_prefix("x-on:"))?;
    let event = raw.split('.').next().unwrap_or(raw);

    if event.is_empty() { None } else { Some(event.to_string()) }
}

/// Whether an attribute is an Alpine class binding (`:class`, `x-bind:class`),
/// whose value is a JS expression that may also name CSS classes as string
/// literals.
fn is_class_binding(name: &str) -> bool {
    name == ":class" || name == "x-bind:class"
}

/// Whether an attribute name is an Alpine.js directive (`x-data`, `@click`,
/// `:class`, `x-on:submit.prevent`, ...).
fn is_alpine(name: &str) -> bool {
    name.starts_with("x-") || name.starts_with('@') || name.starts_with(':')
}

/// The file name an asset URL points at, dropping any query or fragment and
/// the leading path.
fn asset_basename(value: &str) -> &str {
    let without_query = value.split(['?', '#']).next().unwrap_or(value);
    let basename = without_query.rsplit('/').next().unwrap_or(without_query);

    assert!(basename.len() <= value.len(), "basename is a slice of the value");

    basename
}

/// The `(variable, attribute)` of a template variable expression's leading
/// `head.attr` access (e.g., `"record.available_quantity|default:'N/A'"` ->
/// `("record", "available_quantity")`). `None` when there is no dotted access or
/// either part is not a plain identifier (a numeric list index `a.0`, a quoted
/// key, a filter argument). Only the FIRST attribute is taken (`a.b.c` yields
/// `("a", "b")`); a deeper member needs the type of `a.b`, which the type-scoped
/// member synthesis does not infer, so it is left for that pass to drop.
fn variable_member(expression: &str) -> Option<(&str, &str)> {
    // The head is everything before the first filter pipe, so a filter argument
    // (`x|default:other.attr`) is never mistaken for the accessed variable.
    let head = expression.split('|').next().unwrap_or(expression).trim();

    let (variable, rest) = head.split_once('.')?;

    let attribute_end = rest.find(['.', '(', '[', ' ', '\t']).unwrap_or(rest.len());
    let attribute = &rest[..attribute_end];

    if is_template_identifier(variable) && is_template_identifier(attribute) {
        Some((variable, attribute))
    } else {
        None
    }
}

/// The `(loop_variable, source, accessor)` of a `{% for x in xs %}` whose loop
/// binds a single variable over either a bare source (`{% for record in records %}`
/// -> `("record", "records", None)`) or one reverse-relation accessor
/// (`{% for record in inventory.records %}` -> `("record", "inventory", Some("records"))`).
/// `None` for a tuple target (`{% for k, v in items %}`) or a deeper/filtered
/// source, so only a loop the member synthesis can type (over a collection
/// context var or a typed instance's reverse accessor) is recorded.
fn loop_binding<'source>(
    variable: &'source str,
    iterable: &'source str,
) -> Option<(&'source str, &'source str, Option<&'source str>)> {
    let loop_variable = variable.trim();

    if !is_template_identifier(loop_variable) {
        return None;
    }

    let source = iterable.split('|').next().unwrap_or(iterable).trim();

    match source.split_once('.') {
        None => is_template_identifier(source).then_some((loop_variable, source, None)),
        Some((object, rest)) => {
            let accessor_end = rest.find(['.', '(', '[', ' ', '\t']).unwrap_or(rest.len());
            let accessor = &rest[..accessor_end];

            (is_template_identifier(object) && is_template_identifier(accessor))
                .then_some((loop_variable, object, Some(accessor)))
        }
    }
}

/// The `(glue_name, field)` a django-glue form-field include binds via a
/// `glue_*field='glue_name.field'` value (e.g.,
/// `glue_model_field='inventory.estimate_cost'` -> `("inventory", "estimate_cost")`).
/// django-glue binds a registered model instance to a form widget by a
/// `glue_name.field` string; the glue unique name before the dot is the rendering
/// view's typed local (the same name it was registered under), so it resolves
/// like a `{{ glue_name.field }}` member access. `None` for a non-glue binding
/// name or a non-dotted value.
fn glue_field_binding<'source>(name: &str, value: &'source str) -> Option<(&'source str, &'source str)> {
    if !name.starts_with("glue") || !name.ends_with("field") {
        return None;
    }

    let inner = value.trim().trim_matches(['\'', '"']);

    variable_member(inner)
}

/// Whether a string is a plain template identifier: a non-empty run of ASCII
/// letters, digits, and underscores that does not start with a digit. Excludes
/// the numeric attribute Django uses for list/tuple indexing (`row.0`), so an
/// index is never mistaken for a model member.
fn is_template_identifier(text: &str) -> bool {
    let mut characters = text.chars();

    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }

    text.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')
}
