use std::cell::RefCell;
use std::rc::Rc;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span, Visibility,
};
use constellation_resolution::{
    COLLECTION_CONTEXT, ImportMapping, QUERYSET_DISPATCH, SERVICE_DISPATCH, TYPED_RECEIVER,
    UnresolvedRef,
};
use rustc_hash::FxHashSet;
use tree_sitter::{Node as TsNode, Parser};

use crate::{ExtractionOutput, Extractor, NODES_PER_FILE_MAX, SOURCE_BYTES_MAX};

/// A fail-fast bound on the walk loop, far above the node count of any file
/// that fits under [`SOURCE_BYTES_MAX`].
const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single tree node.
const CHILDREN_MAX: u32 = 1_000_000;

/// The provenance tag on edges and references this extractor produces.
const PROVENANCE: &str = "extraction:python";

/// A cap on the rendered length of a string-list binding's signature
/// (`INSTALLED_APPS`, `MIDDLEWARE`), enough to convey the contents without
/// bloating the node row.
const STRING_LIST_SIGNATURE_BYTES_MAX: usize = 600;

/// The Django callables whose string template argument constellation links as a
/// `Renders` edge from the enclosing view to the template.
const RENDER_FUNCTIONS: &[&str] = &[
    "render",
    "render_to_string",
    "get_template",
    "select_template",
    "TemplateResponse",
];

/// The file extensions that mark a string argument as a template name.
const TEMPLATE_EXTENSIONS: &[&str] = &[".html", ".htm", ".txt", ".xml"];

/// The Django URL functions whose call constellation promotes to a `Route` node.
const ROUTE_FUNCTIONS: &[&str] = &["path", "re_path", "url"];

/// The Django functions that resolve a URL name to a route; their first string
/// argument is the route name (`reverse("article-detail")`).
const URL_RESOLVE_FUNCTIONS: &[&str] = &["reverse", "reverse_lazy", "redirect"];

/// The class-based-view (and DRF) class attributes that bind the view to another
/// symbol (a model, form, or serializer) by name or expression.
const VIEW_ATTRIBUTES: &[&str] = &[
    "filter_backends",
    "form_class",
    "model",
    "permission_classes",
    "queryset",
    "serializer_class",
    "table_class",
];

/// The Django model field constructors that declare a relation to another model;
/// their first argument names the related model. `GenericRelation` is a reverse
/// generic accessor (no own column) but still names a related model in its first
/// argument and exposes a queryable attribute, so it is surfaced like the
/// concrete relation fields rather than dropped.
const RELATION_FIELDS: &[&str] = &["ForeignKey", "ManyToManyField", "OneToOneField", "GenericRelation"];

/// The capitalized names from `typing` and the builtins that are type constructors,
/// not domain classes; excluded from type-annotation edges so only user types
/// (`Article`, `ArticleService`) become `Returns`/`TypeOf` references.
const TYPING_NAMES: &[&str] = &[
    "Annotated",
    "Any",
    "AsyncGenerator",
    "AsyncIterable",
    "AsyncIterator",
    "Awaitable",
    "Callable",
    "ClassVar",
    "Coroutine",
    "Counter",
    "DefaultDict",
    "Deque",
    "Dict",
    "Final",
    "FrozenSet",
    "Generator",
    "Generic",
    "Iterable",
    "Iterator",
    "List",
    "Literal",
    "Mapping",
    "MutableMapping",
    "NamedTuple",
    "Never",
    "NoReturn",
    "None",
    "Optional",
    "OrderedDict",
    "Protocol",
    "Self",
    "Sequence",
    "Set",
    "Tuple",
    "Type",
    "TypeVar",
    "TypedDict",
    "Union",
];

/// The lowercase Python builtin functions, dispatched by the interpreter with no
/// project-local definition to bind to. A bare `print(...)` / `len(...)` /
/// `isinstance(...)` call can never resolve to an indexed node, so emitting a
/// `Calls` reference for it only manufactures a permanently-unresolved row. Skipped
/// at extraction. Sorted for lookup-by-eye; matched only against a bare-identifier
/// callee, so an attribute call (`queryset.filter(...)`, `value.get(...)`) is
/// untouched and still routes through method dispatch. Capitalized builtins
/// (`ValueError`, `KeyError`) read as constructors and take the `Instantiates`
/// path instead, so they are intentionally absent here.
const CALL_BUILTINS: &[&str] = &[
    "abs", "all", "any", "ascii", "bin", "bool", "bytearray", "bytes", "callable", "chr",
    "classmethod", "complex", "delattr", "dict", "dir", "divmod", "enumerate", "eval", "exec",
    "filter", "float", "format", "frozenset", "getattr", "globals", "hasattr", "hash", "hex", "id",
    "input", "int", "isinstance", "issubclass", "iter", "len", "list", "locals", "map", "max",
    "memoryview", "min", "next", "object", "oct", "open", "ord", "pow", "print", "property",
    "range", "repr", "reversed", "round", "set", "setattr", "slice", "sorted", "staticmethod",
    "str", "sum", "super", "tuple", "type", "vars", "zip",
];

/// The decorator names that are Python descriptor builtins or well-known framework
/// decorators, none of which name a project-local symbol, so a `Decorates`
/// reference to one never resolves and is pure noise. `@property` / `@classmethod`
/// / `@x.setter` decorate in-language; `@pytest.fixture` / `@pytest.mark.django_db`
/// / `@transaction.atomic` come from test and ORM libraries. A custom decorator
/// (`@ai`, a project `@action`) is not listed: it can resolve to its definition.
const DECORATOR_BUILTINS: &[&str] = &[
    "abstractmethod", "abstractproperty", "atomic", "cached_property", "classmethod", "deleter",
    "django_db", "final", "fixture", "override", "parametrize", "property", "setter",
    "staticmethod", "wraps",
];

/// A bound on receiver-chain links walked looking for `.objects`, far past any
/// real queryset chain, so the walk is provably finite.
const CHAIN_DEPTH_MAX: u32 = 32;

/// The queryset-yielding method names that, like `.objects`, start a dispatchable
/// queryset chain when they appear as a receiver-chain hop. `get_queryset` is the
/// Django manager/view override returning the base queryset; the `_set` spelling
/// is its legacy alias.
const QUERYSET_SOURCE_METHODS: &[&str] = &["get_queryset", "get_query_set"];

/// The object-fetch helper that binds a single model instance; its first
/// argument names the model.
const INSTANCE_GET_FUNCTION: &str = "get_object_or_404";

/// The object-fetch helper that binds a list of a model; its first argument
/// names the model and the local is a collection.
const COLLECTION_GET_FUNCTION: &str = "get_list_or_404";

/// The queryset terminal methods that return a single model instance rather than a
/// queryset, so `x = Model.objects.get(...)` types `x` as an instance, not a
/// collection. Tuple-returning terminals (`get_or_create`) are excluded entirely.
const QUERYSET_INSTANCE_METHODS: &[&str] = &["get", "first", "last", "latest", "earliest"];

/// A fail-fast bound on the queryset receiver-chain descent.
const QUERYSET_CHAIN_MAX: u32 = 1_000;

/// A fail-fast bound on the parenthesis-unwrapping descent.
const PARENTHESES_DEPTH_MAX: u32 = 1_000;

/// An extractor of Python source into graph nodes, containment edges, and the
/// unresolved references (calls, imports, inheritance, decorators) that
/// resolution later turns into edges.
pub struct PythonExtractor;

thread_local! {
    /// The per-thread Python parser, reused across files. Extraction runs over
    /// files on rayon workers, so one parser per thread is reuse-maximal with no
    /// cross-thread sharing: a file pays for its parse, never for parser
    /// construction.
    static PARSER: RefCell<Parser> = RefCell::new(new_parser());
}

/// A Python parser with the grammar loaded. It panics only on a grammar against
/// tree-sitter ABI mismatch, a build error that cannot arise at runtime in a
/// correctly linked binary.
fn new_parser() -> Parser {
    let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();

    assert!(language.node_kind_count() > 0, "python grammar must expose node kinds");

    let mut parser = Parser::new();

    parser
        .set_language(&language)
        .expect("the bundled python grammar is ABI-compatible with tree-sitter");

    parser
}

impl PythonExtractor {
    /// The extractor; the grammar loads per worker thread on first use.
    pub fn new() -> Self {
        Self
    }
}

impl Default for PythonExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for PythonExtractor {
    fn language(&self) -> Language {
        Language::Python
    }

    fn extract(&self, project: &ProjectId, file_path: &str, source: &str) -> ExtractionOutput {
        assert!(!file_path.is_empty(), "file_path must not be empty");

        let mut output = ExtractionOutput::empty();

        if source.len() > SOURCE_BYTES_MAX {
            return output;
        }

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(source, None)) else {
            return output;
        };

        let bytes = source.as_bytes();
        let root = tree.root_node();

        let file_id = NodeId::new(project, file_path);
        output.nodes.push(make_file_node(project, file_path, &file_id, root));

        let file_scope = Scope {
            prefix: Rc::from(file_path),
            parent_id: Rc::new(file_id.clone()),
            parent_kind: ParentKind::File,
            enclosing_class: None,
            local_types: None,
            class_attribute_types: None,
        };

        let mut stack: Vec<Frame> = Vec::new();
        push_named_children(root, &file_scope, &mut stack);

        let mut iterations: u32 = 0;

        while let Some(frame) = stack.pop() {
            iterations += 1;

            assert!(
                iterations <= WALK_ITERATIONS_MAX,
                "walk exceeded {WALK_ITERATIONS_MAX} iterations",
            );

            if output.nodes.len() >= NODES_PER_FILE_MAX as usize {
                break;
            }

            process_frame(project, file_path, bytes, &file_id, frame, &mut stack, &mut output);
        }

        let exported = collect_dunder_all(bytes, root);
        apply_exports(&mut output, &exported);

        output
    }
}

/// Whether a node's direct children are file-, class-, or function-scoped,
/// determining the qualified-name separator and whether a function is a method.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentKind {
    Class,
    File,
    Function,
}

/// The scope a tree node lives in: its parent's qualified name, the parent
/// node that contains it, and the parent's kind. `Rc` keeps cloning the scope
/// onto every child frame O(1).
#[derive(Clone)]
struct Scope {
    prefix: Rc<str>,
    parent_id: Rc<NodeId>,
    parent_kind: ParentKind,
    /// The qualified name of the nearest enclosing class, for resolving `self`/`cls`
    /// method calls back to that class.
    enclosing_class: Option<Rc<str>>,
    /// The type-annotated locals in scope: parameter name to the domain type named in
    /// its annotation, for resolving `local.method()` to that type's class. `None`
    /// when the scope has no typed locals (every file/class scope and any
    /// param-less or untyped function), so the common case allocates nothing; the
    /// `Rc` keeps cloning a populated scope onto child frames O(1).
    local_types: Option<Rc<Vec<(String, String)>>>,
    /// The type-annotated attributes of the nearest enclosing class: attribute name
    /// to the domain type named in its annotation (`repository: ArticleRepository`),
    /// for resolving `self.<attr>.method()` to that type's class. `None` when the
    /// class declares no annotated attributes; shared by `Rc` onto every method
    /// frame, so a populated class costs one clone per method, not per call.
    class_attribute_types: Option<Rc<Vec<(String, String)>>>,
}

/// A single unit of pending work: a tree node plus the scope it lives in and any
/// decorators lifted from an enclosing `decorated_definition`.
struct Frame<'tree> {
    node: TsNode<'tree>,
    scope: Scope,
    decorators: Vec<TsNode<'tree>>,
}

/// The dispatch of one popped frame: emit its node/edges/references and push the
/// children that must still be visited.
fn process_frame<'tree>(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    frame: Frame<'tree>,
    stack: &mut Vec<Frame<'tree>>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    match frame.node.kind() {
        "decorated_definition" => push_decorated(frame, stack),
        "class_definition" => {
            if let Some(scope) = handle_class(project, file_path, bytes, &frame, output)
                && let Some(body) = frame.node.child_by_field_name("body")
            {
                push_named_children(body, &scope, stack);
            }
        }
        "function_definition" => {
            if let Some(scope) = handle_function(project, file_path, bytes, &frame, output)
                && let Some(body) = frame.node.child_by_field_name("body")
            {
                push_named_children(body, &scope, stack);
            }
        }
        "import_statement" | "import_from_statement" => {
            handle_import(file_path, bytes, file_id, frame.node, output);
        }
        "call" => {
            if route_call(project, file_path, bytes, &frame, file_id, output) {
            } else if let Some(reference) = url_resolve_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            } else if let Some(reference) = render_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            } else if let Some(reference) = signal_connect_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            } else if let Some(reference) = call_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            if let Some(reference) = template_kwarg_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            if let Some(reference) = glue_register_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            push_named_children(frame.node, &frame.scope, stack);
        }
        "attribute" => {
            if let Some(reference) = settings_read_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            push_named_children(frame.node, &frame.scope, stack);
        }
        "assignment" => {
            annotated_type_refs(file_path, bytes, &frame, output);

            if let Some(reference) = context_type_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            let handled = class_field(project, file_path, bytes, &frame, output)
                || view_template(file_path, bytes, &frame, output)
                || view_attribute(file_path, bytes, &frame, output);

            if !handled {
                module_or_class_binding(project, file_path, bytes, &frame, output);
                push_named_children(frame.node, &frame.scope, stack);
            }
        }
        _ => push_named_children(frame.node, &frame.scope, stack),
    }
}

/// The scope for a class body, after creating the class node, its containment
/// edge, and inheritance/decorator references.
fn handle_class(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> Option<Scope> {
    let name_node = frame.node.child_by_field_name("name")?;
    let name = node_text(bytes, name_node);

    assert!(!name.is_empty(), "class name must not be empty");

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);

    let kind = class_kind(bytes, frame.node);
    let decorators = decorator_texts(bytes, &frame.decorators);
    let mut node = make_node(project, kind, name, &qualified_name, file_path, frame.node);
    node.visibility = Some(visibility_of(name));
    apply_decorators(&mut node, decorators);

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(node);

    extends_refs(file_path, bytes, frame.node, &id, output);
    decorates_refs(file_path, bytes, &id, &frame.decorators, frame.node, output);
    admin_register_refs(file_path, bytes, &id, frame.node, output);
    test_subject_ref(file_path, name, &id, frame.node, output);

    Some(Scope {
        prefix: Rc::from(qualified_name.as_str()),
        parent_id: Rc::new(id),
        parent_kind: ParentKind::Class,
        enclosing_class: Some(Rc::from(qualified_name.as_str())),
        local_types: None,
        class_attribute_types: class_attribute_types(bytes, frame.node),
    })
}

/// The scope for a function/method body, after creating its node and containment
/// edge, so nested definitions and calls attribute correctly.
fn handle_function(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> Option<Scope> {
    let name_node = frame.node.child_by_field_name("name")?;
    let name = node_text(bytes, name_node);

    assert!(!name.is_empty(), "function name must not be empty");

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);

    let decorators = decorator_texts(bytes, &frame.decorators);
    let kind = function_kind(frame.scope.parent_kind, &decorators, bytes, frame.node);

    let mut node = make_node(project, kind, name, &qualified_name, file_path, frame.node);
    node.signature = signature_of(bytes, frame.node);
    node.visibility = Some(visibility_of(name));
    node.is_async = is_async_function(frame.node);
    apply_decorators(&mut node, decorators);

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(node);

    decorates_refs(file_path, bytes, &id, &frame.decorators, frame.node, output);
    signature_type_refs(file_path, bytes, &id, frame.node, output);

    if !frame.decorators.is_empty() {
        signal_receiver_refs(file_path, bytes, &id, frame.node, output);
        drf_route(project, file_path, bytes, &id, kind, frame, output);
    }

    Some(Scope {
        prefix: Rc::from(qualified_name.as_str()),
        parent_id: Rc::new(id),
        parent_kind: ParentKind::Function,
        enclosing_class: frame.scope.enclosing_class.clone(),
        local_types: typed_locals(bytes, frame.node),
        class_attribute_types: frame.scope.class_attribute_types.clone(),
    })
}

/// The type-annotated locals of a function as a scope-shareable map, or `None`
/// when it has none, so a param-less or untyped function adds no allocation.
fn typed_locals(bytes: &[u8], def_node: TsNode<'_>) -> Option<Rc<Vec<(String, String)>>> {
    let pairs = parameter_types(bytes, def_node);

    if pairs.is_empty() {
        return None;
    }

    Some(Rc::new(pairs))
}

/// The type-annotated attributes a class body declares (`repository:
/// ArticleRepository`, `service: OrderService = ...`), as a scope-shareable
/// `(name, type)` map, or `None` when it declares none, so a class with no
/// annotated attributes adds no allocation. Backs `self.<attr>.method()`
/// typed-receiver dispatch for the class's methods; only the first domain type of
/// each annotation is kept, matching `parameter_types`.
fn class_attribute_types(bytes: &[u8], class_node: TsNode<'_>) -> Option<Rc<Vec<(String, String)>>> {
    let body = class_node.child_by_field_name("body")?;

    let mut cursor = body.walk();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut count: u32 = 0;

    for statement in body.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "class-body fan-out exceeded {CHILDREN_MAX}");

        let assignment = if statement.kind() == "expression_statement" {
            statement.named_child(0)
        } else {
            Some(statement)
        };

        let Some(assignment) = assignment else {
            continue;
        };

        if assignment.kind() != "assignment" {
            continue;
        }

        let Some(left) = assignment.child_by_field_name("left") else {
            continue;
        };

        let Some(type_node) = assignment.child_by_field_name("type") else {
            continue;
        };

        if left.kind() != "identifier" {
            continue;
        }

        let name = node_text(bytes, left);

        if let Some(type_name) = annotation_type_names(bytes, type_node).into_iter().next() {
            pairs.push((name.to_string(), type_name));
        }
    }

    if pairs.is_empty() {
        return None;
    }

    Some(Rc::new(pairs))
}

/// The type-annotated parameters of a function, as `(name, type)` pairs: the
/// first domain type named in each annotation (`order: PurchaseOrder` ->
/// `("order", "PurchaseOrder")`, `order: Optional[PurchaseOrder]` likewise).
/// Untyped parameters (`self`, `request`) and builtin/typing annotations
/// contribute nothing, so only domain-typed locals seed typed-receiver dispatch.
fn parameter_types(bytes: &[u8], def_node: TsNode<'_>) -> Vec<(String, String)> {
    let Some(parameters) = def_node.child_by_field_name("parameters") else {
        return Vec::new();
    };

    let mut cursor = parameters.walk();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut count: u32 = 0;

    for parameter in parameters.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "parameter fan-out exceeded {CHILDREN_MAX}");

        if !matches!(parameter.kind(), "typed_parameter" | "typed_default_parameter") {
            continue;
        }

        let Some(type_node) = parameter.child_by_field_name("type") else {
            continue;
        };

        let Some(name) = parameter_identifier(bytes, parameter) else {
            continue;
        };

        if let Some(type_name) = annotation_type_names(bytes, type_node).into_iter().next() {
            pairs.push((name.to_string(), type_name));
        }
    }

    pairs
}

/// The parameter name of a `typed_parameter`/`typed_default_parameter`: its
/// first identifier child (`order: PurchaseOrder` -> `order`).
fn parameter_identifier<'tree>(bytes: &'tree [u8], parameter: TsNode<'tree>) -> Option<&'tree str> {
    let mut cursor = parameter.walk();

    parameter
        .named_children(&mut cursor)
        .find(|child| child.kind() == "identifier")
        .map(|node| node_text(bytes, node))
}

/// The lift of decorators off a `decorated_definition`, pushing its inner definition
/// for normal handling and carrying the decorators along.
fn push_decorated<'tree>(frame: Frame<'tree>, stack: &mut Vec<Frame<'tree>>) {
    let mut decorators: Vec<TsNode<'tree>> = Vec::new();
    let mut cursor = frame.node.walk();
    let mut count: u32 = 0;

    for child in frame.node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "decorator fan-out exceeded {CHILDREN_MAX}");

        if child.kind() == "decorator" {
            decorators.push(child);
        }
    }

    if let Some(definition) = frame.node.child_by_field_name("definition") {
        stack.push(Frame {
            node: definition,
            scope: frame.scope.clone(),
            decorators,
        });
    }
}

/// The routing of an import statement to the matching builder.
fn handle_import(
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    match node.kind() {
        "import_statement" => import_plain(file_path, bytes, file_id, node, output),
        "import_from_statement" => import_from(file_path, bytes, file_id, node, output),
        _ => {}
    }
}

/// An `Imports` reference per module of an `import a.b.c [as d]` statement.
fn import_plain(
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "import fan-out exceeded {CHILDREN_MAX}");

        let (module_node, alias) = match child.kind() {
            "dotted_name" => (child, None),
            "aliased_import" => match child.child_by_field_name("name") {
                Some(name) => (name, child.child_by_field_name("alias")),
                None => continue,
            },
            _ => continue,
        };

        let module = node_text(bytes, module_node);

        if module.is_empty() {
            continue;
        }

        add_import(file_path, file_id, module, None, child, output);

        let local = alias.map_or_else(|| module.to_string(), |node| node_text(bytes, node).to_string());

        output.import_mappings.push(ImportMapping {
            local_name: local,
            exported_name: module.to_string(),
            source: module.to_string(),
            is_default: false,
            is_namespace: true,
            resolved_path: None,
        });
    }
}

/// An `Imports` reference per name of a `from x.y import a, b as c` statement.
fn import_from(
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let module = node
        .child_by_field_name("module_name")
        .map_or(String::new(), |module_node| node_text(bytes, module_node).to_string());

    let mut cursor = node.walk();
    let mut count: u32 = 0;

    if !cursor.goto_first_child() {
        return;
    }

    loop {
        count += 1;

        assert!(count <= CHILDREN_MAX, "from-import fan-out exceeded {CHILDREN_MAX}");

        if cursor.field_name() == Some("name")
            && let Some((local, exported)) = imported_binding(bytes, cursor.node())
        {
            add_import(file_path, file_id, &exported, Some(&module), cursor.node(), output);

            output.import_mappings.push(ImportMapping {
                local_name: local,
                exported_name: exported,
                source: module.clone(),
                is_default: false,
                is_namespace: false,
                resolved_path: None,
            });
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// The `Imports` reference the resolver later ties to a definition (or to
/// an `External` node for a third-party module). No import *node* is created:
/// "this file imports symbol Y" is the `Imports` edge from the file to Y's
/// definition, so Y is never shadowed by a same-named import node in search and
/// the name-keyed tools.
fn add_import(
    file_path: &str,
    file_id: &NodeId,
    reference_name: &str,
    candidate_module: Option<&str>,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!reference_name.is_empty(), "import name must not be empty");
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let position = node.start_position();
    let mut reference = UnresolvedRef::new(
        file_id.clone(),
        reference_name,
        EdgeKind::Imports,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    );

    if let Some(module) = candidate_module
        && !module.is_empty()
    {
        reference.candidates.push(module.to_string());
    }

    output.unresolved_refs.push(reference);
}

/// An `Extends` reference per superclass of a class definition.
fn extends_refs(
    file_path: &str,
    bytes: &[u8],
    class_node: TsNode<'_>,
    class_id: &NodeId,
    output: &mut ExtractionOutput,
) {
    let Some(superclasses) = class_node.child_by_field_name("superclasses") else {
        return;
    };

    let mut cursor = superclasses.walk();
    let mut count: u32 = 0;

    for base in superclasses.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "base-class fan-out exceeded {CHILDREN_MAX}");

        if base.kind() == "keyword_argument" {
            continue;
        }

        if let Some(name) = dotted_last_name(bytes, base) {
            let position = base.start_position();

            output.unresolved_refs.push(UnresolvedRef::new(
                class_id.clone(),
                name,
                EdgeKind::Extends,
                line_1based(position.row),
                to_u32(position.column),
                file_path,
                Language::Python,
            ));
        }
    }
}

/// A `Decorates` reference from a symbol to each decorator applied to it.
fn decorates_refs(
    file_path: &str,
    bytes: &[u8],
    symbol_id: &NodeId,
    decorators: &[TsNode<'_>],
    def_node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    let position = def_node.start_position();
    let mut count: u32 = 0;

    for decorator in decorators {
        count += 1;

        assert!(count <= CHILDREN_MAX, "decorator fan-out exceeded {CHILDREN_MAX}");

        if let Some(name) = decorator_base_name(bytes, *decorator)
            && !DECORATOR_BUILTINS.contains(&name.as_str())
        {
            output.unresolved_refs.push(UnresolvedRef::new(
                symbol_id.clone(),
                name,
                EdgeKind::Decorates,
                line_1based(position.row),
                to_u32(position.column),
                file_path,
                Language::Python,
            ));
        }
    }
}

/// A function's signature type references: a `Returns` reference to each
/// domain type named in its return annotation, and a `TypeOf` reference to each
/// domain type named in a parameter annotation. Local variable annotations
/// inside the body are deliberately skipped; only the signature is linked.
fn signature_type_refs(
    file_path: &str,
    bytes: &[u8],
    function_id: &NodeId,
    def_node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    if let Some(return_type) = def_node.child_by_field_name("return_type") {
        let line = line_1based(def_node.start_position().row);

        for name in annotation_type_names(bytes, return_type) {
            output.unresolved_refs.push(UnresolvedRef::new(
                function_id.clone(),
                name,
                EdgeKind::Returns,
                line,
                0,
                file_path,
                Language::Python,
            ));
        }
    }

    let Some(parameters) = def_node.child_by_field_name("parameters") else {
        return;
    };

    let mut cursor = parameters.walk();
    let mut count: u32 = 0;

    for parameter in parameters.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "parameter fan-out exceeded {CHILDREN_MAX}");

        if !matches!(parameter.kind(), "typed_parameter" | "typed_default_parameter") {
            continue;
        }

        let Some(type_node) = parameter.child_by_field_name("type") else {
            continue;
        };

        let line = line_1based(parameter.start_position().row);

        for name in annotation_type_names(bytes, type_node) {
            output.unresolved_refs.push(UnresolvedRef::new(
                function_id.clone(),
                name,
                EdgeKind::TypeOf,
                line,
                0,
                file_path,
                Language::Python,
            ));
        }
    }
}

/// A `TypeOf` reference for an annotated assignment at class or module
/// scope (`repository: ArticleRepository`, `config: Settings`), anchored at the
/// enclosing class or file. Function-local annotations are skipped as noise.
fn annotated_type_refs(
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) {
    if frame.scope.parent_kind == ParentKind::Function {
        return;
    }

    let Some(type_node) = frame.node.child_by_field_name("type") else {
        return;
    };

    let line = line_1based(frame.node.start_position().row);

    for name in annotation_type_names(bytes, type_node) {
        output.unresolved_refs.push(UnresolvedRef::new(
            frame.scope.parent_id.as_ref().clone(),
            name,
            EdgeKind::TypeOf,
            line,
            0,
            file_path,
            Language::Python,
        ));
    }
}

/// The domain type names referenced in a type annotation, with builtins and
/// typing constructs filtered out: `Optional[Article]` -> `[Article]`,
/// `dict[str, Author]` -> `[Author]`, `models.User` -> `[User]`. A stack walk,
/// no recursion.
fn annotation_type_names(bytes: &[u8], type_node: TsNode<'_>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut stack: Vec<TsNode> = vec![type_node];
    let mut guard: u32 = 0;

    while let Some(node) = stack.pop() {
        guard += 1;

        assert!(guard <= CHILDREN_MAX, "annotation walk exceeded {CHILDREN_MAX}");

        match node.kind() {
            "identifier" => push_type_name(node_text(bytes, node), &mut names),
            "attribute" => {
                if let Some(name) = dotted_last_name(bytes, node) {
                    push_type_name(name, &mut names);
                }
            }
            _ => {
                let mut cursor = node.walk();

                for child in node.named_children(&mut cursor) {
                    stack.push(child);
                }
            }
        }
    }

    names
}

/// The recording of a type name when it looks like a user-defined class (capitalized
/// and not a `typing`/builtin construct) and is not already present.
fn push_type_name(name: &str, names: &mut Vec<String>) {
    let class_like = name.chars().next().is_some_and(|first| first.is_ascii_uppercase());

    if class_like && !TYPING_NAMES.contains(&name) && !names.iter().any(|seen| seen == name) {
        names.push(name.to_string());
    }
}

/// A `Receives` reference from a `@receiver(signal, sender=Model)` handler
/// to the model named by its `sender=` argument, the decorator form of Django
/// signal wiring that connects a handler function to the model whose changes
/// trigger it.
fn signal_receiver_refs(
    file_path: &str,
    bytes: &[u8],
    handler_id: &NodeId,
    def_node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    let Some(decorated) = def_node.parent() else {
        return;
    };

    if decorated.kind() != "decorated_definition" {
        return;
    }

    let mut cursor = decorated.walk();
    let mut count: u32 = 0;

    for child in decorated.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "decorator fan-out exceeded {CHILDREN_MAX}");

        if child.kind() != "decorator" {
            continue;
        }

        let Some(model) = receiver_sender_model(bytes, child) else {
            continue;
        };

        let position = child.start_position();

        output.unresolved_refs.push(UnresolvedRef::new(
            handler_id.clone(),
            model,
            EdgeKind::Receives,
            line_1based(position.row),
            to_u32(position.column),
            file_path,
            Language::Python,
        ));
    }
}

/// The model named by the `sender=` argument of a `@receiver(...)` decorator, or
/// `None` when the decorator is not a `receiver` call or names no static model.
fn receiver_sender_model(bytes: &[u8], decorator: TsNode<'_>) -> Option<String> {
    let call = decorator_call(decorator)?;
    let callee = call.child_by_field_name("function").and_then(|node| callee_name(bytes, node))?;

    if callee != "receiver" {
        return None;
    }

    keyword_arg_node(bytes, call, "sender").and_then(|value| relation_target(bytes, value))
}

/// An `AdminOf` reference from a `@admin.register(Model)` / `@register(Model)`
/// decorated `ModelAdmin` class to the model it administers, the decorator form of
/// Django admin registration dominant in this codebase. The admin class node is
/// known here; the model is named by the decorator's first positional argument.
fn admin_register_refs(
    file_path: &str,
    bytes: &[u8],
    admin_id: &NodeId,
    class_node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    let Some(decorated) = class_node.parent() else {
        return;
    };

    if decorated.kind() != "decorated_definition" {
        return;
    }

    let mut cursor = decorated.walk();
    let mut count: u32 = 0;

    for child in decorated.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "decorator fan-out exceeded {CHILDREN_MAX}");

        if child.kind() != "decorator" {
            continue;
        }

        let Some(model) = admin_register_model(bytes, child) else {
            continue;
        };

        let position = child.start_position();

        output.unresolved_refs.push(UnresolvedRef::new(
            admin_id.clone(),
            model,
            EdgeKind::AdminOf,
            line_1based(position.row),
            to_u32(position.column),
            file_path,
            Language::Python,
        ));
    }
}

/// The model named by the first positional argument of an `@admin.register(Model)`
/// / `@register(Model)` decorator, or `None` when the decorator is not a register
/// call or names no static model. A `register` decorator on a class is admin
/// registration; DRF's `router.register` is a call, never a class decorator.
fn admin_register_model(bytes: &[u8], decorator: TsNode<'_>) -> Option<String> {
    let call = decorator_call(decorator)?;
    let callee = call.child_by_field_name("function").and_then(|node| callee_name(bytes, node))?;

    if callee != "register" {
        return None;
    }

    positional_args(call).first().and_then(|node| relation_target(bytes, *node))
}

/// Whether a file is a test module: under a `tests/` package, or named
/// `test_*.py` / `*_test.py` / `tests.py`.
fn is_test_file(file_path: &str) -> bool {
    let normalized = file_path.replace('\\', "/");

    if normalized.contains("/tests/") || normalized.contains("/test/") {
        return true;
    }

    let stem = normalized.rsplit('/').next().unwrap_or(normalized.as_str());

    stem.starts_with("test_") || stem.ends_with("_test.py") || stem == "tests.py"
}

/// A `Tests` reference from a `TestCase` class in a test module to the symbol
/// it covers, inferred from the class name: `PurchaseOrderTestCase` ->
/// `PurchaseOrder`, `CompanyModelTests` -> `Company`. The `Test`/`TestCase`/`Tests`
/// suffix is stripped, then a trailing layer noun (`Model`/`View`/`Form`/...) the
/// test name often carries. Resolution binds the stripped name to a definition;
/// a name that resolves to nothing emits no edge, the no-false-edge discipline.
fn test_subject_ref(
    file_path: &str,
    name: &str,
    class_id: &NodeId,
    class_node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    if !is_test_file(file_path) {
        return;
    }

    let trimmed = name
        .strip_suffix("TestCase")
        .or_else(|| name.strip_suffix("Tests"))
        .or_else(|| name.strip_suffix("Test"));

    let Some(mut base) = trimmed else {
        return;
    };

    for layer in ["Model", "View", "Form", "Service", "QuerySet"] {
        if let Some(stripped) = base.strip_suffix(layer)
            && !stripped.is_empty()
        {
            base = stripped;

            break;
        }
    }

    if base.is_empty() {
        return;
    }

    let position = class_node.start_position();

    output.unresolved_refs.push(UnresolvedRef::new(
        class_id.clone(),
        base,
        EdgeKind::Tests,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    ));
}

/// A `Reads` reference from the enclosing symbol to a Django setting it reads
/// (`settings.AUTH_USER_MODEL`). Only an upper-initial attribute on a bare
/// `settings` object qualifies, the SCREAMING_SNAKE convention every setting
/// follows, so `obj.method()` and lowercase attributes do not match; resolution
/// binds the name to a `Constant`/`Variable` node, and an unknown setting emits no
/// edge.
fn settings_read_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    let object = frame.node.child_by_field_name("object")?;

    if object.kind() != "identifier" || node_text(bytes, object) != "settings" {
        return None;
    }

    let attribute = frame.node.child_by_field_name("attribute")?;
    let name = node_text(bytes, attribute);

    if !name.chars().next().is_some_and(|character| character.is_ascii_uppercase()) {
        return None;
    }

    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        name,
        EdgeKind::Reads,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    ))
}

/// The `call` expression inside a `decorator` node (`@receiver(...)`), or `None`
/// for a bare-name decorator (`@property`).
fn decorator_call<'tree>(decorator: TsNode<'tree>) -> Option<TsNode<'tree>> {
    let mut cursor = decorator.walk();

    decorator.named_children(&mut cursor).find(|child| child.kind() == "call")
}

/// A `Receives` reference when the call is `signal.connect(handler,
/// sender=Model)`, Django's imperative signal wiring. The edge is anchored at
/// the wiring site and points at the model named by `sender=`; the handler is
/// recorded as a candidate, since its node is not known at the call. Without a
/// `sender=` model there is nothing to link, so the call falls to generic
/// handling.
fn signal_connect_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let function = frame.node.child_by_field_name("function")?;
    let name = callee_name(bytes, function)?;

    if name != "connect" {
        return None;
    }

    let model = keyword_arg_node(bytes, frame.node, "sender").and_then(|value| relation_target(bytes, value))?;

    let position = frame.node.start_position();

    let mut reference = UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        model,
        EdgeKind::Receives,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    );

    if let Some(handler) = positional_args(frame.node).first().and_then(|node| dotted_last_name(bytes, *node)) {
        reference.candidates.push(handler.to_string());
    }

    Some(reference)
}

/// A `Calls` reference from the enclosing symbol to a call's callee.
///
/// When the call is `self.method()` / `cls.method()`, the enclosing class
/// qualified name is attached as a candidate so resolution can bind it to that
/// class's method (instance-method resolution).
fn call_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    let function = frame.node.child_by_field_name("function")?;
    let name = callee_name(bytes, function)?;

    assert!(!name.is_empty(), "callee name must not be empty");

    if function.kind() == "identifier" && CALL_BUILTINS.contains(&name) {
        return None;
    }

    let position = frame.node.start_position();

    let mut reference = UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        name,
        EdgeKind::Calls,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    );

    if is_self_call(bytes, function)
        && let Some(class) = &frame.scope.enclosing_class
    {
        reference.candidates.push(class.to_string());
    } else if is_objects_manager_call(bytes, function) {
        reference.candidates.push(QUERYSET_DISPATCH.to_string());
    } else if is_services_call(bytes, function) {
        reference.candidates.push(SERVICE_DISPATCH.to_string());
    } else if let Some(type_name) = typed_receiver_type(bytes, function, &frame.scope) {
        reference.candidates.push(TYPED_RECEIVER.to_string());
        reference.candidates.push(type_name);
    } else if is_class_like(name) {
        // A bare `Article(...)` / `ArticleForm(...)` constructs a class; record it
        // as an instantiation, not a call. Resolves only to a Class/Model of that
        // name, so a rare uppercase-named function drops rather than misbinds.
        reference.reference_kind = EdgeKind::Instantiates;
    }

    Some(reference)
}

/// The annotated type of a call's receiver, so resolution can bind the method to
/// that class. Two receiver shapes carry a known type: a bare type-annotated
/// local (`order.recalculate()` with `order: PurchaseOrder` in scope), and a
/// type-annotated attribute on `self`/`cls` (`self.repository.get()` with
/// `repository: ArticleRepository` declared on the class). `None` for any other
/// receiver; deeper chains or computed receivers (`order.lines.first().x()`)
/// are left to the import-scoped path.
fn typed_receiver_type(bytes: &[u8], function: TsNode<'_>, scope: &Scope) -> Option<String> {
    if function.kind() != "attribute" {
        return None;
    }

    let object = function.child_by_field_name("object")?;

    match object.kind() {
        "identifier" => {
            let name = node_text(bytes, object);

            lookup_type(scope.local_types.as_deref(), name)
        }
        "attribute" => {
            let receiver = object.child_by_field_name("object")?;

            if !matches!(node_text(bytes, receiver), "self" | "cls") {
                return None;
            }

            let attribute = object.child_by_field_name("attribute")?;
            let name = node_text(bytes, attribute);

            lookup_type(scope.class_attribute_types.as_deref(), name)
        }
        _ => None,
    }
}

/// The domain type bound to `name` in a scope-shared `(name, type)` table, or
/// `None` when the table is absent or has no such entry.
fn lookup_type(table: Option<&Vec<(String, String)>>, name: &str) -> Option<String> {
    table?
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, type_name)| type_name.clone())
}

/// Whether a call's receiver chain passes through a queryset source: a `.objects`
/// manager (`Article.objects.by_year()`), a chained
/// `Article.objects.active().by_year()` whose later hop lands on the
/// queryset an earlier one returned, or a `get_queryset()` hop
/// (`self.get_queryset().by_year()`, the manager/view pattern that `.objects`
/// does not name). Routed to queryset-method dispatch. Over-matching is harmless:
/// dispatch only binds a sole `*QuerySet`/`*Manager`-owned method and drops
/// builtins, so a non-queryset call on such a chain resolves nothing.
fn is_objects_manager_call(bytes: &[u8], function: TsNode<'_>) -> bool {
    if function.kind() != "attribute" {
        return false;
    }

    let Some(mut node) = function.child_by_field_name("object") else {
        return false;
    };

    let mut depth: u32 = 0;

    loop {
        depth += 1;

        if depth > CHAIN_DEPTH_MAX {
            return false;
        }

        match node.kind() {
            "attribute" => {
                if node
                    .child_by_field_name("attribute")
                    .is_some_and(|attribute| node_text(bytes, attribute) == "objects")
                {
                    return true;
                }

                let Some(next) = node.child_by_field_name("object") else {
                    return false;
                };

                node = next;
            }
            "call" => {
                let Some(callee) = node.child_by_field_name("function") else {
                    return false;
                };

                // A get_queryset() hop yields a queryset just as .objects does, so a
                // method chained off it dispatches the same way.
                if callee.kind() == "attribute"
                    && callee.child_by_field_name("attribute").is_some_and(|attribute| {
                        QUERYSET_SOURCE_METHODS.contains(&node_text(bytes, attribute))
                    })
                {
                    return true;
                }

                let Some(next) = callee.child_by_field_name("object") else {
                    return false;
                };

                node = next;
            }
            _ => return false,
        }
    }
}

/// Whether a call's function is `<x>.services.<method>` or
/// `<x>.services.<sub>.<method>`, this codebase's service dispatch
/// (`order.services.processor.recalculate_totals()`), routed to service-method
/// resolution. `services` is matched as the receiver attribute one or two levels
/// above the called method; deeper chains are left to the import-scoped path.
fn is_services_call(bytes: &[u8], function: TsNode<'_>) -> bool {
    if function.kind() != "attribute" {
        return false;
    }

    let Some(object) = function.child_by_field_name("object") else {
        return false;
    };

    if object.kind() != "attribute" {
        return false;
    }

    let is_services =
        |node: TsNode<'_>| node.child_by_field_name("attribute").is_some_and(|attribute| node_text(bytes, attribute) == "services");

    if is_services(object) {
        return true;
    }

    object
        .child_by_field_name("object")
        .filter(|inner| inner.kind() == "attribute")
        .is_some_and(is_services)
}

/// A `Renders` reference when the call is a Django render call carrying a
/// string template argument, linking the enclosing view to the template.
fn render_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let function = frame.node.child_by_field_name("function")?;
    let name = callee_name(bytes, function)?;

    if !RENDER_FUNCTIONS.contains(&name) {
        return None;
    }

    let template = template_argument(bytes, frame.node)?;
    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        template,
        EdgeKind::Renders,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    ))
}

/// A `Renders` reference for a `template=` / `template_name=` string
/// keyword argument on any call, the dominant convention in wrapper-based view
/// layers (`workspace_views.list_view(request, ..., template='page.html')`), which
/// the direct-`render()` rule misses. Emitted alongside the call's own edge.
fn template_kwarg_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let template = keyword_arg(bytes, frame.node, "template")
        .or_else(|| keyword_arg(bytes, frame.node, "template_name"))
        .filter(|value| is_template_name(value))?;

    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        template,
        EdgeKind::Renders,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    ))
}

/// A `ContextType` reference from the enclosing function to the model a
/// function-body assignment binds a local to (an instance or collection),
/// carrying the local's name in `candidates` (plus the [`COLLECTION_CONTEXT`]
/// marker for a collection), the type the template member synthesis gives
/// `{{ <local>.attr }}` (instance) or `{% for x in <local> %}{{ x.attr }}`
/// (collection). Handles `get_object_or_404(Model, ...)` (instance),
/// `get_list_or_404(Model, ...)` (collection), and `Model.objects.<chain>`
/// (collection, or instance when the chain terminates in `.get()`/`.first()`/...).
/// Scoped to function bodies; resolution leaves `ContextType` pending for the
/// member-synthesis pass to consume.
fn context_type_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    if !matches!(frame.scope.parent_kind, ParentKind::Function) {
        return None;
    }

    let assignment = frame.node;
    let left = assignment.child_by_field_name("left")?;

    if left.kind() != "identifier" {
        return None;
    }

    let variable = node_text(bytes, left);

    let right = unwrap_parentheses(assignment.child_by_field_name("right")?);

    if right.kind() != "call" {
        return None;
    }

    let position = assignment.start_position();

    if let Some((model, is_collection)) = context_call_type(bytes, right) {
        let mut reference = UnresolvedRef::new(
            frame.scope.parent_id.as_ref().clone(),
            model,
            EdgeKind::ContextType,
            line_1based(position.row),
            to_u32(position.column),
            file_path,
            Language::Python,
        );

        reference.candidates.push(variable.to_string());

        if is_collection {
            reference.candidates.push(COLLECTION_CONTEXT.to_string());
        }

        return Some(reference);
    }

    // `x = base.accessor.<collection-chain>` (a reverse-relation queryset): the
    // local is a collection whose element model the synthesis derives from
    // `base`'s type and the `accessor`'s reverse relation. `base` and `accessor`
    // ride in `reference_name` + `candidates` for that later join.
    if let Some((base_local, accessor)) = reverse_collection(bytes, right) {
        let mut reference = UnresolvedRef::new(
            frame.scope.parent_id.as_ref().clone(),
            base_local,
            EdgeKind::DerivedCollection,
            line_1based(position.row),
            to_u32(position.column),
            file_path,
            Language::Python,
        );

        reference.candidates.push(variable.to_string());
        reference.candidates.push(accessor.to_string());

        return Some(reference);
    }

    None
}

/// The `(model, is_collection)` a context-binding call yields, or `None` when the
/// call is not a recognized model-binding form.
fn context_call_type<'bytes>(bytes: &'bytes [u8], call: TsNode<'_>) -> Option<(&'bytes str, bool)> {
    let function = call.child_by_field_name("function")?;

    if function.kind() == "identifier" {
        let name = node_text(bytes, function);
        let collection = match name {
            INSTANCE_GET_FUNCTION => false,
            COLLECTION_GET_FUNCTION => true,
            _ => return None,
        };

        let model = positional_args(call).first().and_then(|node| type_name_of(bytes, *node))?;

        return Some((model, collection));
    }

    // A `Model.objects.<chain>` queryset: the outermost method decides whether the
    // chain yields one instance or a collection; the receiver chain's base names
    // the model.
    let terminal = function.child_by_field_name("attribute").map(|attribute| node_text(bytes, attribute))?;

    if terminal == "get_or_create" || terminal == "update_or_create" {
        return None;
    }

    let model = queryset_model(bytes, call)?;
    let is_collection = !QUERYSET_INSTANCE_METHODS.contains(&terminal);

    Some((model, is_collection))
}

/// The model whose default manager a queryset call chains off:
/// `Model.objects.filter(...).order_by(...)` -> `Model`. Descends the receiver
/// chain through each `.method(...)` hop to the base, requiring it to be exactly
/// `<Identifier>.objects` (the default manager). `None` for any other receiver
/// (a custom manager, a local queryset variable, a reverse accessor
/// `obj.records.all()`), so only an unambiguous model collection is typed.
fn queryset_model<'bytes>(bytes: &'bytes [u8], call: TsNode<'_>) -> Option<&'bytes str> {
    let mut node = call;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= QUERYSET_CHAIN_MAX, "queryset chain exceeded {QUERYSET_CHAIN_MAX} hops");

        let function = node.child_by_field_name("function")?;

        if function.kind() != "attribute" {
            return None;
        }

        let object = function.child_by_field_name("object")?;

        match object.kind() {
            "call" => node = object,
            "attribute" => {
                let manager = object.child_by_field_name("attribute")?;

                if node_text(bytes, manager) != "objects" {
                    return None;
                }

                let model = object.child_by_field_name("object")?;

                if model.kind() != "identifier" {
                    return None;
                }

                return Some(node_text(bytes, model));
            }
            _ => return None,
        }
    }
}

/// The `(base_local, accessor)` of a reverse-relation queryset call:
/// `record.events.all()` / `order.lines.filter(...)` -> `("record", "events")`.
/// Descends the receiver chain to a base `<identifier>.<accessor>` whose accessor
/// is not the default manager `objects` (a direct model queryset is
/// `queryset_model`'s job), and only for a collection-returning terminal so a
/// `.first()`/`.get()` (a single instance) is not taken as a collection. `None`
/// otherwise.
fn reverse_collection<'bytes>(bytes: &'bytes [u8], call: TsNode<'_>) -> Option<(&'bytes str, &'bytes str)> {
    let terminal = call
        .child_by_field_name("function")?
        .child_by_field_name("attribute")
        .map(|attribute| node_text(bytes, attribute))?;

    if QUERYSET_INSTANCE_METHODS.contains(&terminal)
        || terminal == "get_or_create"
        || terminal == "update_or_create"
    {
        return None;
    }

    let mut node = call;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= QUERYSET_CHAIN_MAX, "reverse-queryset chain exceeded {QUERYSET_CHAIN_MAX} hops");

        let function = node.child_by_field_name("function")?;

        if function.kind() != "attribute" {
            return None;
        }

        let object = function.child_by_field_name("object")?;

        match object.kind() {
            "call" => node = object,
            "attribute" => {
                let accessor = node_text(bytes, object.child_by_field_name("attribute")?);

                if accessor == "objects" {
                    return None;
                }

                let base = object.child_by_field_name("object")?;

                if base.kind() != "identifier" {
                    return None;
                }

                return Some((node_text(bytes, base), accessor));
            }
            _ => return None,
        }
    }
}

/// The expression inside any wrapping parentheses (e.g., a multi-line queryset
/// wrapped as `( Model.objects.filter(...) )`). Multi-line querysets are routinely
/// parenthesized, which makes the assignment's right-hand side a
/// `parenthesized_expression` rather than the call itself; unwrapping lets the
/// queryset typing see through it.
fn unwrap_parentheses(node: TsNode<'_>) -> TsNode<'_> {
    let mut node = node;
    let mut depth: u32 = 0;

    while node.kind() == "parenthesized_expression" {
        depth += 1;

        assert!(depth <= PARENTHESES_DEPTH_MAX, "parenthesis nesting exceeded {PARENTHESES_DEPTH_MAX}");

        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }

    node
}

/// The model name a class reference names: the rightmost identifier of a `Model`
/// / `models.Model` / `app.models.Model` reference. `None` when the node is not a
/// plain or dotted name (a queryset expression, a call, a string).
fn type_name_of<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    match node.kind() {
        "identifier" => Some(node_text(bytes, node)),
        "attribute" => node.child_by_field_name("attribute").map(|attribute| node_text(bytes, attribute)),
        _ => None,
    }
}

/// A `ContextType` reference for a django-glue registration that binds a
/// model instance or queryset collection under a unique name, keyed by that name
/// (the same name a `glue_*field='name.field'` template binding and the
/// rewrite's `Glue.model.name.field` JS accesses). Handles the function API
/// (`glue_model_object`/`glue_query_set`, plain or `dg.`-aliased) and the
/// rewrite's proxy API (`Glue.model`/`Glue.queryset`/`Glue.form`), with
/// `unique_name`/`target` passed positionally or by keyword. Types the target
/// only when it is an inline queryset/instance call; a bare local is left to its
/// own assignment's `ContextType` (the glue name conventionally matches the
/// local).
fn glue_register_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    let call = frame.node;
    let function = call.child_by_field_name("function")?;

    let registers_collection = glue_registrar_kind(bytes, function)?;

    let unique_name = keyword_arg(bytes, call, "unique_name")
        .or_else(|| positional_args(call).get(1).and_then(|node| string_value(bytes, *node)))?;

    if unique_name.is_empty() {
        return None;
    }

    let target = keyword_arg_node(bytes, call, "target")
        .or_else(|| positional_args(call).get(2).copied())
        .map(|node| unwrap_parentheses(node))?;

    if target.kind() != "call" {
        return None;
    }

    let (model, target_collection) = context_call_type(bytes, target)?;
    let position = call.start_position();

    let mut reference = UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        model,
        EdgeKind::ContextType,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    );

    reference.candidates.push(unique_name);

    if registers_collection || target_collection {
        reference.candidates.push(COLLECTION_CONTEXT.to_string());
    }

    Some(reference)
}

/// Whether a call's callee is a django-glue registration and, if so, whether it
/// binds a queryset collection (`true`) or a single instance (`false`). Covers
/// the function API (`glue_model_object`/`glue_query_set`, plain or
/// attribute-qualified) and the rewrite's proxy API on the `Glue` class.
fn glue_registrar_kind(bytes: &[u8], function: TsNode<'_>) -> Option<bool> {
    match function.kind() {
        "identifier" => glue_function_kind(node_text(bytes, function)),
        "attribute" => {
            let method = node_text(bytes, function.child_by_field_name("attribute")?);
            let object = function.child_by_field_name("object")?;

            if object.kind() == "identifier" && node_text(bytes, object) == "Glue" {
                return match method {
                    "model" | "form" => Some(false),
                    "queryset" => Some(true),
                    _ => None,
                };
            }

            glue_function_kind(method)
        }
        _ => None,
    }
}

/// The collection-ness of a django-glue function-API registrar name, or `None`
/// when the name is not one.
fn glue_function_kind(name: &str) -> Option<bool> {
    match name {
        "glue_model_object" => Some(false),
        "glue_query_set" => Some(true),
        _ => None,
    }
}

/// A `Resolves` reference when the call resolves a URL name to a route
/// (`reverse`/`reverse_lazy`/`redirect` with a string first argument).
fn url_resolve_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let function = frame.node.child_by_field_name("function")?;
    let name = callee_name(bytes, function)?;

    if !URL_RESOLVE_FUNCTIONS.contains(&name) {
        return None;
    }

    let raw = positional_args(frame.node).first().and_then(|node| string_value(bytes, *node))?;

    // Keep the full namespaced target (`app:page:detail`). A bare name still
    // resolves against route names as before; a namespaced one stays pending for
    // the route-namespace pass, which binds it to the route under that exact
    // include chain instead of guessing among same-named routes across apps.
    let url_name = raw;

    if url_name.is_empty() {
        return None;
    }

    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        url_name,
        EdgeKind::Resolves,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::Python,
    ))
}

/// The first string-literal argument of a call that names a template.
fn template_argument(bytes: &[u8], call_node: TsNode<'_>) -> Option<String> {
    let arguments = call_node.child_by_field_name("arguments")?;

    let mut cursor = arguments.walk();
    let mut count: u32 = 0;

    for argument in arguments.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "argument fan-out exceeded {CHILDREN_MAX}");

        if let Some(value) = string_content_text(bytes, argument)
            && is_template_name(value)
        {
            return Some(value.to_string());
        }
    }

    None
}

/// Whether a string value looks like a template path by its extension.
fn is_template_name(value: &str) -> bool {
    TEMPLATE_EXTENSIONS.iter().any(|extension| value.ends_with(extension))
}

/// The recognition of a Django URL declaration call (`path`/`re_path`/`url` or a DRF
/// `router.register`), emitting its `Route` node and handler reference. Returns
/// true when the call was a route, so the caller skips generic call handling.
fn route_call(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    file_id: &NodeId,
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let Some(function) = frame.node.child_by_field_name("function") else {
        return false;
    };
    let Some(callee) = callee_name(bytes, function) else {
        return false;
    };

    let positional = positional_args(frame.node);

    if ROUTE_FUNCTIONS.contains(&callee) {
        return url_route(project, file_path, bytes, frame, file_id, &positional, output);
    }

    if callee == "register" {
        return router_route(project, file_path, bytes, frame, file_id, &positional, output);
    }

    false
}

/// The handling of `path("url", handler, ...)`.
fn url_route(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    file_id: &NodeId,
    positional: &[TsNode<'_>],
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let Some(url) = positional.first().and_then(|node| string_value(bytes, *node)) else {
        return false;
    };

    let display = keyword_arg(bytes, frame.node, "name")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| if url.is_empty() { "/".to_string() } else { url.clone() });

    let namespace = positional.get(1).and_then(|node| include_namespace(bytes, *node));

    let line = line_1based(frame.node.start_position().row);
    let spec = RouteSpec {
        qualified_suffix: &format!("route::{url}"),
        display: &display,
        line,
        signature: namespace,
    };
    let route_id = emit_route(project, file_path, spec, file_id, output);

    if let Some((name, kind, receiver)) = positional.get(1).and_then(|node| handler_reference(bytes, *node)) {
        let mut reference =
            UnresolvedRef::new(route_id, name, kind, line, 0, file_path, Language::Python);

        if let Some(receiver) = receiver {
            reference.candidates.push(receiver);
        }

        output.unresolved_refs.push(reference);
    }

    true
}

/// The handling of DRF `router.register("prefix", ViewSet)`. The string first argument
/// distinguishes it from `admin.site.register(Model, Admin)`.
fn router_route(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    file_id: &NodeId,
    positional: &[TsNode<'_>],
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let Some(prefix) = positional.first().and_then(|node| string_value(bytes, *node)) else {
        return false;
    };
    let Some(viewset_node) = positional.get(1) else {
        return false;
    };

    let Some(viewset) = dotted_last_name(bytes, *viewset_node) else {
        return false;
    };

    if !viewset.ends_with("View") && !viewset.ends_with("ViewSet") {
        return false;
    }

    let clean = prefix.trim_matches('^').trim_end_matches('$').trim_matches('/');
    let line = line_1based(frame.node.start_position().row);
    let spec = RouteSpec {
        qualified_suffix: &format!("route::viewset::{clean}"),
        display: &format!("VIEWSET /{clean}"),
        line,
        signature: None,
    };
    let route_id = emit_route(project, file_path, spec, file_id, output);

    output.unresolved_refs.push(UnresolvedRef::new(
        route_id,
        viewset.to_string(),
        EdgeKind::RoutesTo,
        line,
        0,
        file_path,
        Language::Python,
    ));

    true
}

/// The descriptive fields of a route node, grouped so [`emit_route`] stays within
/// the argument bound: the suffix that qualifies its id, the display name, its
/// 1-based line, and an optional `namespace=` carried on the node's signature for
/// the reverse-namespace pass (`None` for a leaf route).
struct RouteSpec<'a> {
    qualified_suffix: &'a str,
    display: &'a str,
    line: u32,
    signature: Option<String>,
}

/// The id of a route node, after creating it and its file-containment edge.
fn emit_route(
    project: &ProjectId,
    file_path: &str,
    spec: RouteSpec<'_>,
    file_id: &NodeId,
    output: &mut ExtractionOutput,
) -> NodeId {
    assert!(!file_path.is_empty(), "file_path must not be empty");
    assert!(spec.line >= 1, "route line is 1-based");

    let qualified_name = format!("{file_path}::{}", spec.qualified_suffix);
    let id = NodeId::new(project, &qualified_name);

    let identity = NodeIdentity {
        name: spec.display.to_string(),
        qualified_name,
        file_path: file_path.to_string(),
        language: Language::Python,
    };

    let mut node = Node::new(
        id.clone(),
        project.clone(),
        NodeKind::Route,
        identity,
        Span::new(spec.line, spec.line, 0, 0),
        0,
    );

    // An include route carries its `namespace=` here so the route-namespace pass
    // can rebuild the `reverse('app:ns:name')` chain; leaf routes pass None.
    node.signature = spec.signature;

    output.edges.push(contains_edge(file_id, &id));
    output.nodes.push(node);

    id
}

/// The `namespace=` of an `include('module', namespace='ns')` URL handler, when
/// the handler is such an include. Routes reached through it carry `ns` as one
/// segment of their reverse name (`reverse('app:ns:detail')`), so capturing it
/// lets the route-namespace pass bind a reverse to the exact route rather than a
/// same-named route in a sibling app.
fn include_namespace(bytes: &[u8], node: TsNode<'_>) -> Option<String> {
    if node.kind() != "call" {
        return None;
    }

    let function = node.child_by_field_name("function")?;

    if callee_name(bytes, function)? != "include" {
        return None;
    }

    keyword_arg(bytes, node, "namespace").filter(|namespace| !namespace.is_empty())
}

/// A DRF route for an `@action` viewset method or an `@api_view` function
/// view: a `Route` node contained by the file, with a direct `RoutesTo` edge to
/// the decorated symbol (its id is known here, so no resolution is needed). Any
/// other decorator set emits nothing.
fn drf_route(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    symbol_id: &NodeId,
    kind: NodeKind,
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) {
    let is_action = kind == NodeKind::Method && has_decorator(bytes, &frame.decorators, "action");
    let is_api_view = matches!(kind, NodeKind::Function | NodeKind::View)
        && has_decorator(bytes, &frame.decorators, "api_view");

    if !is_action && !is_api_view {
        return;
    }

    let Some(name) = frame.node.child_by_field_name("name").map(|node| node_text(bytes, node)) else {
        return;
    };

    assert!(!name.is_empty(), "decorated symbol name must not be empty");

    let line = line_1based(frame.node.start_position().row);
    let file_id = NodeId::new(project, file_path);
    let display = if is_action { format!("ACTION {name}") } else { format!("API {name}") };

    let spec = RouteSpec {
        qualified_suffix: &format!("route::drf::{name}::{line}"),
        display: &display,
        line,
        signature: None,
    };
    let route_id = emit_route(project, file_path, spec, &file_id, output);

    output.edges.push(
        Edge::new(route_id, symbol_id.clone(), EdgeKind::RoutesTo)
            .at(line, 0)
            .with_provenance(PROVENANCE),
    );
}

/// Whether any decorator's base name (its final dotted segment, call arguments
/// ignored) equals `target` (`@action(detail=True)` and `@app.action` both
/// match "action").
fn has_decorator(bytes: &[u8], decorators: &[TsNode<'_>], target: &str) -> bool {
    decorators
        .iter()
        .any(|decorator| decorator_base_name(bytes, *decorator).as_deref() == Some(target))
}

/// The positional arguments of a call, in order (keyword arguments and
/// comments excluded).
fn positional_args<'tree>(call_node: TsNode<'tree>) -> Vec<TsNode<'tree>> {
    let Some(arguments) = call_node.child_by_field_name("arguments") else {
        return Vec::new();
    };

    let mut cursor = arguments.walk();
    let mut out: Vec<TsNode<'tree>> = Vec::new();
    let mut count: u32 = 0;

    for argument in arguments.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "argument fan-out exceeded {CHILDREN_MAX}");

        if argument.kind() != "keyword_argument" && argument.kind() != "comment" {
            out.push(argument);
        }
    }

    out
}

/// The literal value of a string-typed argument node.
fn string_value(bytes: &[u8], node: TsNode<'_>) -> Option<String> {
    string_content_text(bytes, node).map(str::to_string)
}

/// The static value of a Python string literal node: the text of its
/// `string_content`, with the quotes and any prefix excluded by the grammar.
/// `None` for a non-string node or an f-string carrying an interpolation (no
/// single static value); an empty literal (`""`) yields `Some("")`.
fn string_content_text<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    if node.kind() != "string" {
        return None;
    }

    let mut cursor = node.walk();
    let mut content: Option<&str> = None;

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_content" => {
                if content.is_some() {
                    return None;
                }

                content = Some(node_text(bytes, child));
            }
            "interpolation" => return None,
            _ => {}
        }
    }

    Some(content.unwrap_or(""))
}

/// The value node of a `key=` keyword argument of a call, if present.
fn keyword_arg_node<'tree>(bytes: &[u8], call_node: TsNode<'tree>, key: &str) -> Option<TsNode<'tree>> {
    let arguments = call_node.child_by_field_name("arguments")?;

    let mut cursor = arguments.walk();
    let mut count: u32 = 0;

    for argument in arguments.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "keyword-arg fan-out exceeded {CHILDREN_MAX}");

        if argument.kind() == "keyword_argument"
            && argument.child_by_field_name("name").is_some_and(|name| node_text(bytes, name) == key)
        {
            return argument.child_by_field_name("value");
        }
    }

    None
}

/// The string value of a `key=` keyword argument of a call, if present.
fn keyword_arg(bytes: &[u8], call_node: TsNode<'_>, key: &str) -> Option<String> {
    keyword_arg_node(bytes, call_node, key).and_then(|value| string_value(bytes, value))
}

/// The symbol to link and the edge kind a Django URL handler argument node parses
/// into: `include('app.urls')` imports a module; everything else routes to
/// a view, reduced to the view name with any `.as_view(...)` unwrapped.
fn handler_reference(bytes: &[u8], node: TsNode<'_>) -> Option<(String, EdgeKind, Option<String>)> {
    match node.kind() {
        "call" => {
            let function = node.child_by_field_name("function")?;
            let callee = callee_name(bytes, function)?;

            if callee == "include" {
                let module =
                    positional_args(node).first().and_then(|argument| string_value(bytes, *argument))?;

                return Some((module, EdgeKind::Imports, None));
            }

            let view = if callee == "as_view" {
                function.child_by_field_name("object")?
            } else {
                function
            };

            let name = dotted_last_name(bytes, view)?;

            Some((name.to_string(), EdgeKind::RoutesTo, handler_receiver(bytes, view)))
        }
        "identifier" | "attribute" | "dotted_name" => {
            let name = dotted_last_name(bytes, node)?;

            Some((name.to_string(), EdgeKind::RoutesTo, handler_receiver(bytes, node)))
        }
        _ => None,
    }
}

/// The receiver module of a `module.view` handler expression (for example,
/// `page_views` in `page_views.detail_view`), so resolution can bind the view
/// inside the module the URL file imports, not a same-named view in a sibling app.
/// `None` for a bare `view` reference (the view itself is imported, scoped by its
/// own name).
fn handler_receiver(bytes: &[u8], node: TsNode<'_>) -> Option<String> {
    if node.kind() != "attribute" {
        return None;
    }

    let object = node.child_by_field_name("object")?;

    dotted_last_name(bytes, object).map(str::to_string)
}

/// The classification of a class by its base classes: a Django model, a view, or plain.
fn class_kind(bytes: &[u8], class_node: TsNode<'_>) -> NodeKind {
    let Some(superclasses) = class_node.child_by_field_name("superclasses") else {
        return NodeKind::Class;
    };

    let mut cursor = superclasses.walk();
    let mut count: u32 = 0;
    let mut view = false;

    for base in superclasses.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "base-class fan-out exceeded {CHILDREN_MAX}");

        if base.kind() == "keyword_argument" {
            continue;
        }

        if let Some(name) = dotted_last_name(bytes, base) {
            if is_model_base(name) {
                return NodeKind::Model;
            }

            if is_view_base(name) {
                view = true;
            }
        }
    }

    if view {
        return NodeKind::View;
    }

    if class_declares_model_field(bytes, class_node) {
        return NodeKind::Model;
    }

    NodeKind::Class
}

/// Whether a class body declares at least one Django model field (a relation
/// field `ForeignKey`/`ManyToManyField`/`OneToOneField` or a `models.*Field(...)`
/// call). This distinguishes a model with a non-"Model" base (a project mixin)
/// from a form or pydantic class, which use `forms.*` or bare `Field(...)`.
fn class_declares_model_field(bytes: &[u8], class_node: TsNode<'_>) -> bool {
    let Some(body) = class_node.child_by_field_name("body") else {
        return false;
    };

    let mut cursor = body.walk();
    let mut count: u32 = 0;

    for statement in body.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "class-body fan-out exceeded {CHILDREN_MAX}");

        let assignment = if statement.kind() == "expression_statement" {
            statement.named_child(0)
        } else {
            Some(statement)
        };

        let Some(assignment) = assignment else {
            continue;
        };

        if assignment.kind() != "assignment" {
            continue;
        }

        let Some(right) = assignment.child_by_field_name("right") else {
            continue;
        };

        if right.kind() == "call" && is_model_field_call(bytes, right) {
            return true;
        }
    }

    false
}

/// Whether a call constructs a Django model field: a relation field by any path,
/// or a `models.`-qualified `*Field(...)`. Bare `*Field` other than the relation
/// fields is excluded because forms and serializers reuse those names.
fn is_model_field_call(bytes: &[u8], call: TsNode<'_>) -> bool {
    let Some(function) = call.child_by_field_name("function") else {
        return false;
    };

    match function.kind() {
        "attribute" => {
            let Some(attribute) = function.child_by_field_name("attribute") else {
                return false;
            };

            let name = node_text(bytes, attribute);

            if RELATION_FIELDS.contains(&name) {
                return true;
            }

            let object =
                function.child_by_field_name("object").and_then(|node| dotted_last_name(bytes, node));

            name.ends_with("Field") && object == Some("models")
        }
        "identifier" => RELATION_FIELDS.contains(&node_text(bytes, function)),
        _ => false,
    }
}

/// Whether a base class name marks a Django model (`Model`, `TimeStampedModel`).
/// `BaseModel` is excluded because it is overwhelmingly pydantic, not Django; a
/// Django class that happens to extend one is still caught by structural field
/// detection.
fn is_model_base(name: &str) -> bool {
    name != "BaseModel" && (name == "Model" || name.ends_with("Model"))
}

/// Whether a base class name marks a Django/DRF view.
fn is_view_base(name: &str) -> bool {
    name.ends_with("View") || name.ends_with("ViewSet") || name == "APIView"
}

/// A `Field` node contained by the model when a class-body assignment declares a
/// Django model field (RHS is a call to a `*Field` constructor), plus, for
/// relation fields, a `RelatesTo` reference to the related model. Returns true
/// when handled, so the caller skips walking the field constructor call.
fn class_field(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    if frame.scope.parent_kind != ParentKind::Class {
        return false;
    }

    let Some(left) = frame.node.child_by_field_name("left") else {
        return false;
    };

    if left.kind() != "identifier" {
        return false;
    }

    let Some(right) = frame.node.child_by_field_name("right") else {
        return false;
    };

    if right.kind() != "call" {
        return false;
    }

    let Some(callee) = right.child_by_field_name("function").and_then(|node| callee_name(bytes, node))
    else {
        return false;
    };

    if !callee.ends_with("Field") && !RELATION_FIELDS.contains(&callee) {
        return false;
    }

    let field_name = node_text(bytes, left);
    let qualified_name = join_qualified(&frame.scope, field_name);
    let field_id = NodeId::new(project, &qualified_name);

    output.edges.push(contains_edge(&frame.scope.parent_id, &field_id));
    output
        .nodes
        .push(make_node(project, NodeKind::Field, field_name, &qualified_name, file_path, frame.node));

    if RELATION_FIELDS.contains(&callee)
        && let Some(target) = positional_args(right).first().and_then(|node| relation_target(bytes, *node))
        && target != "self"
    {
        let position = frame.node.start_position();
        let related_name = keyword_arg(bytes, right, "related_name").filter(|name| !name.is_empty());

        output.unresolved_refs.push(UnresolvedRef::new(
            frame.scope.parent_id.as_ref().clone(),
            target.clone(),
            EdgeKind::RelatesTo,
            line_1based(position.row),
            to_u32(position.column),
            file_path,
            Language::Python,
        ));

        // A `related_name` is the reverse accessor the *target* model exposes back
        // to this one (`Article.comments` for a `Comment.article` FK).
        // Record it so the template member synthesis can type a
        // `{% for comment in article.comments %}` loop element as this model. The
        // ref runs to the target (resolved to the target model); its own model
        // (the reverse accessor's element type) is the ref's `from` node.
        if let Some(related_name) = related_name {
            let mut reverse = UnresolvedRef::new(
                frame.scope.parent_id.as_ref().clone(),
                target,
                EdgeKind::ReverseAccessor,
                line_1based(position.row),
                to_u32(position.column),
                file_path,
                Language::Python,
            );

            reverse.candidates.push(related_name);

            output.unresolved_refs.push(reverse);
        }
    }

    true
}

/// A `Constant` or `Variable` node for a module- or class-scope assignment
/// the Django-specific handlers did not claim (`urlpatterns`, `app_name`, a
/// `logger`, a `LIST_FILTERING_SESSION_KEY`, or a `TextChoices` member). A name in
/// SCREAMING_SNAKE_CASE is a constant; anything else a variable. Function-local
/// assignments are skipped as noise (mirroring `annotated_type_refs`), as are
/// non-identifier targets (tuple unpacking, attribute or subscript writes) and
/// the `__all__` export list, which is consumed as metadata, not a symbol.
fn module_or_class_binding(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    if frame.scope.parent_kind == ParentKind::Function {
        return;
    }

    let Some(left) = frame.node.child_by_field_name("left") else {
        return;
    };

    if left.kind() != "identifier" {
        return;
    }

    let name = node_text(bytes, left);

    if name.is_empty() || name == "__all__" {
        return;
    }

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);
    let kind = if is_screaming_snake(name) { NodeKind::Constant } else { NodeKind::Variable };

    let mut node = make_node(project, kind, name, &qualified_name, file_path, frame.node);
    node.visibility = Some(visibility_of(name));
    node.signature = string_list_signature(bytes, frame.node);

    // The application namespace `app_name = 'django_spire'` declares: stored as the
    // node's signature so route reverse-name resolution can fold it into the
    // namespace chain (Django's app namespace, which the `include(namespace=...)`
    // chain alone does not carry).
    if name == "app_name"
        && node.signature.is_none()
        && let Some(right) = frame.node.child_by_field_name("right")
        && let Some(value) = string_value(bytes, right)
    {
        node.signature = Some(value);
    }

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(node);
}

/// A compact one-line rendering of a binding whose value is a list/tuple/set of
/// string literals (`INSTALLED_APPS = [...]`, `MIDDLEWARE = [...]`), so a
/// `constellation_node` lookup surfaces the contents. `None` for any other RHS (a
/// call, a number, a list of dicts). Capped at [`STRING_LIST_SIGNATURE_BYTES_MAX`].
fn string_list_signature(bytes: &[u8], assignment: TsNode<'_>) -> Option<String> {
    let right = assignment.child_by_field_name("right")?;

    if !matches!(right.kind(), "list" | "tuple" | "set") {
        return None;
    }

    let mut rendered = String::from("[");
    let mut cursor = right.walk();
    let mut count: u32 = 0;
    let mut written: u32 = 0;

    for child in right.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "string-list fan-out exceeded {CHILDREN_MAX}");

        let Some(value) = string_content_text(bytes, child) else {
            continue;
        };

        if value.is_empty() {
            continue;
        }

        if written > 0 {
            rendered.push_str(", ");
        }

        rendered.push_str(value);
        written += 1;

        if rendered.len() > STRING_LIST_SIGNATURE_BYTES_MAX {
            rendered.push_str(", …");
            break;
        }
    }

    if written == 0 {
        return None;
    }

    rendered.push(']');

    Some(rendered)
}

/// Whether a name is SCREAMING_SNAKE_CASE (only uppercase letters, digits, and
/// underscores, with at least one letter), distinguishing a constant
/// (`MAX_RETRIES`, `DRAFT`) from a variable (`urlpatterns`, `app_name`).
fn is_screaming_snake(name: &str) -> bool {
    let mut has_letter = false;

    for character in name.chars() {
        if character.is_ascii_uppercase() {
            has_letter = true;
        } else if !character.is_ascii_digit() && character != '_' {
            return false;
        }
    }

    has_letter
}

/// The related model named by a relation field's first argument: a string
/// `'auth.User'` -> `User`, an identifier or attribute -> its final segment.
fn relation_target(bytes: &[u8], node: TsNode<'_>) -> Option<String> {
    match node.kind() {
        "string" => {
            let value = string_content_text(bytes, node)?;
            let last = value.rsplit('.').next().unwrap_or(value).trim();

            if last.is_empty() { None } else { Some(last.to_string()) }
        }
        "identifier" | "attribute" | "dotted_name" => {
            dotted_last_name(bytes, node).map(str::to_string)
        }
        _ => None,
    }
}

/// A `Renders` reference from the view to each template a class-body assignment
/// names when it sets `template_name`/`template_names` (the class-based-view
/// template declaration). Returns true when the assignment was a
/// template declaration, so the caller skips it.
fn view_template(
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    if frame.scope.parent_kind != ParentKind::Class {
        return false;
    }

    let Some(left) = frame.node.child_by_field_name("left") else {
        return false;
    };

    if left.kind() != "identifier" {
        return false;
    }

    let name = node_text(bytes, left);

    if name != "template_name" && name != "template_names" {
        return false;
    }

    if let Some(right) = frame.node.child_by_field_name("right") {
        let line = line_1based(frame.node.start_position().row);

        for template in template_strings(bytes, right) {
            output.unresolved_refs.push(UnresolvedRef::new(
                frame.scope.parent_id.as_ref().clone(),
                template,
                EdgeKind::Renders,
                line,
                0,
                file_path,
                Language::Python,
            ));
        }
    }

    true
}

/// The string literals on the right of a `template_name(s)` binding: a single
/// string, or the strings inside a list/tuple/set.
fn template_strings(bytes: &[u8], node: TsNode<'_>) -> Vec<String> {
    match node.kind() {
        "string" => match string_content_text(bytes, node) {
            Some(value) if !value.is_empty() => vec![value.to_string()],
            _ => Vec::new(),
        },
        "list" | "tuple" | "set" => {
            let mut cursor = node.walk();
            let mut templates: Vec<String> = Vec::new();
            let mut count: u32 = 0;

            for child in node.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "template list fan-out exceeded {CHILDREN_MAX}");

                if let Some(value) = string_content_text(bytes, child)
                    && !value.is_empty()
                {
                    templates.push(value.to_string());
                }
            }

            templates
        }
        _ => Vec::new(),
    }
}

/// A `RelatesTo` reference from the view to the symbol a CBV/DRF attribute
/// (`model`, `form_class`, `serializer_class`, `queryset`, ...) binds to, when a
/// view class-body assignment makes such a binding. Returns true when the
/// assignment was such a binding.
fn view_attribute(
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> bool {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    if frame.scope.parent_kind != ParentKind::Class {
        return false;
    }

    let Some(left) = frame.node.child_by_field_name("left") else {
        return false;
    };

    if left.kind() != "identifier" || !VIEW_ATTRIBUTES.contains(&node_text(bytes, left)) {
        return false;
    }

    if let Some(right) = frame.node.child_by_field_name("right") {
        let line = line_1based(frame.node.start_position().row);

        for symbol in rhs_symbols(bytes, right) {
            output.unresolved_refs.push(UnresolvedRef::new(
                frame.scope.parent_id.as_ref().clone(),
                symbol,
                EdgeKind::RelatesTo,
                line,
                0,
                file_path,
                Language::Python,
            ));
        }
    }

    true
}

/// The referenced symbol name(s) on the right of a view-attribute binding: a
/// single symbol, or the symbols in a list/tuple/set.
fn rhs_symbols(bytes: &[u8], node: TsNode<'_>) -> Vec<String> {
    match node.kind() {
        "list" | "tuple" | "set" => {
            let mut cursor = node.walk();
            let mut symbols: Vec<String> = Vec::new();
            let mut count: u32 = 0;

            for child in node.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "attribute list fan-out exceeded {CHILDREN_MAX}");

                if let Some(name) = rhs_root_name(bytes, child) {
                    symbols.push(name.to_string());
                }
            }

            symbols
        }
        _ => rhs_root_name(bytes, node).map(|name| vec![name.to_string()]).unwrap_or_default(),
    }
}

/// The leftmost identifier of an expression: `Article` from `Article`,
/// `Article.objects.all()`, or `Article.objects[0]`. Walks down the
/// object/function/value field without recursion.
fn rhs_root_name<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    let mut node = node;
    let mut guard: u32 = 0;

    loop {
        guard += 1;

        assert!(guard <= CHILDREN_MAX, "expression depth exceeded {CHILDREN_MAX}");

        match node.kind() {
            "identifier" => return Some(node_text(bytes, node)),
            "attribute" => node = node.child_by_field_name("object")?,
            "call" => node = node.child_by_field_name("function")?,
            "subscript" => node = node.child_by_field_name("value")?,
            _ => return None,
        }
    }
}

/// The push of every named child of `node` as fresh work in the same scope.
fn push_named_children<'tree>(node: TsNode<'tree>, scope: &Scope, stack: &mut Vec<Frame<'tree>>) {
    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "child fan-out exceeded {CHILDREN_MAX}");

        stack.push(Frame {
            node: child,
            scope: scope.clone(),
            decorators: Vec::new(),
        });
    }
}

/// The file node for the parsed source.
fn make_file_node(project: &ProjectId, file_path: &str, file_id: &NodeId, root: TsNode<'_>) -> Node {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let name = file_basename(file_path);

    assert!(!name.is_empty(), "file node name must not be empty");

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: file_path.to_string(),
        file_path: file_path.to_string(),
        language: Language::Python,
    };

    Node::new(file_id.clone(), project.clone(), NodeKind::File, identity, span_of(root), 0)
}

/// A symbol node built from its identity and tree node.
fn make_node(
    project: &ProjectId,
    kind: NodeKind,
    name: &str,
    qualified_name: &str,
    file_path: &str,
    node: TsNode<'_>,
) -> Node {
    assert!(!name.is_empty(), "node name must not be empty");
    assert!(!qualified_name.is_empty(), "qualified_name must not be empty");

    let id = NodeId::new(project, qualified_name);

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: qualified_name.to_string(),
        file_path: file_path.to_string(),
        language: Language::Python,
    };

    Node::new(id, project.clone(), kind, identity, span_of(node), 0)
}

/// A containment edge from a parent node to a child node, tagged with provenance.
fn contains_edge(parent: &NodeId, child: &NodeId) -> Edge {
    Edge::new(parent.clone(), child.clone(), EdgeKind::Contains).with_provenance(PROVENANCE)
}

/// The qualified name for a child: `parent::name` at file scope, `parent.name`
/// within a symbol.
fn join_qualified(scope: &Scope, name: &str) -> String {
    assert!(!name.is_empty(), "name must not be empty");
    assert!(!scope.prefix.is_empty(), "scope prefix must not be empty");

    match scope.parent_kind {
        ParentKind::File => format!("{}::{name}", scope.prefix),
        _ => format!("{}.{name}", scope.prefix),
    }
}

/// The display strings of a definition's decorator nodes, with the leading `@`
/// stripped, dropping any that are empty.
fn decorator_texts(bytes: &[u8], decorators: &[TsNode<'_>]) -> Vec<String> {
    decorators
        .iter()
        .filter_map(|decorator| decorator_expression(*decorator))
        .map(|expression| node_text(bytes, expression).to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

/// The move of decorator strings onto a node, deriving the static/abstract flags. Takes
/// the vector by value so the strings `decorator_texts` already allocated move
/// into the node rather than being cloned again.
fn apply_decorators(node: &mut Node, decorators: Vec<String>) {
    node.is_static = decorators.iter().any(|decorator| decorator.contains("staticmethod"));
    node.is_abstract = decorators.iter().any(|decorator| decorator.contains("abstractmethod"));
    node.decorators = decorators;
}

/// The node kind for a function definition: a class body's `@property` /
/// `@cached_property` (or a `@x.setter` / `@x.deleter`) is a `Property`, any
/// other class-body def a `Method`, and a module or nested def a `Function`.
fn function_kind(
    parent_kind: ParentKind,
    decorators: &[String],
    bytes: &[u8],
    def_node: TsNode<'_>,
) -> NodeKind {
    match parent_kind {
        ParentKind::Class if is_property_decorated(decorators) => NodeKind::Property,
        ParentKind::Class => NodeKind::Method,
        ParentKind::File if is_function_view(bytes, def_node, decorators) => NodeKind::View,
        _ => NodeKind::Function,
    }
}

/// Whether a module-level function is a Django function-based view: its first
/// parameter is `request` (the view calling convention) and it is not a pytest
/// fixture. Catches the `def list_view(request, pk): ...` delegating views the
/// workspaces use, which carry no view base class for `class_kind` to classify, so
/// generic extraction would otherwise file them as plain functions.
fn is_function_view(bytes: &[u8], def_node: TsNode<'_>, decorators: &[String]) -> bool {
    let is_fixture = decorators
        .iter()
        .any(|decorator| decorator == "fixture" || decorator.ends_with(".fixture"));

    if is_fixture {
        return false;
    }

    let Some(parameters) = def_node.child_by_field_name("parameters") else {
        return false;
    };

    let mut cursor = parameters.walk();

    let Some(first) = parameters.named_children(&mut cursor).next() else {
        return false;
    };

    let name = match first.kind() {
        "identifier" => Some(node_text(bytes, first)),
        "typed_parameter" | "typed_default_parameter" | "default_parameter" => {
            parameter_identifier(bytes, first)
        }
        _ => None,
    };

    name == Some("request")
}

/// Whether a method's decorators make it a property accessor: `@property`,
/// `@cached_property`, or a `@<name>.setter` / `@<name>.deleter`, matched on the
/// final dotted segment, so `@functools.cached_property` counts too.
fn is_property_decorated(decorators: &[String]) -> bool {
    decorators.iter().any(|decorator| {
        decorator == "property"
            || decorator.ends_with(".property")
            || decorator == "cached_property"
            || decorator.ends_with(".cached_property")
            || decorator.ends_with(".setter")
            || decorator.ends_with(".deleter")
    })
}

/// The Python access intent inferred from naming: leading double underscore (not
/// dunder) is private, a single leading underscore is protected, otherwise public.
fn visibility_of(name: &str) -> Visibility {
    assert!(!name.is_empty(), "name must not be empty");

    if name.starts_with("__") && !name.ends_with("__") {
        Visibility::Private
    } else if name.starts_with('_') {
        Visibility::Protected
    } else {
        Visibility::Public
    }
}

/// Whether a `function_definition` is `async def`.
fn is_async_function(node: TsNode<'_>) -> bool {
    node.child(0).is_some_and(|first| first.kind() == "async")
}

/// The parenthesized parameter list, plus return type when present, collapsed
/// to a single line. A multi-line `def` keeps its source newlines and indent
/// in `node_text`, which would otherwise render a four-line signature.
fn signature_of(bytes: &[u8], node: TsNode<'_>) -> Option<String> {
    let parameters = node.child_by_field_name("parameters")?;
    let mut signature = collapse_whitespace(node_text(bytes, parameters));

    if let Some(return_type) = node.child_by_field_name("return_type") {
        signature.push_str(" -> ");
        signature.push_str(&collapse_whitespace(node_text(bytes, return_type)));
    }

    Some(signature)
}

/// A (possibly multi-line) signature collapsed to one normalized line: runs of
/// whitespace become single spaces, and the spacing/trailing comma a multi-line
/// `def` leaves around the parentheses are tidied, so the same signature written
/// across lines or on one line renders identically across overloads.
fn collapse_whitespace(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());

    for word in text.split_whitespace() {
        if !joined.is_empty() {
            joined.push(' ');
        }

        joined.push_str(word);
    }

    joined
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ,", ",")
        .replace(",)", ")")
        .replace("[ ", "[")
        .replace(" ]", "]")
}

/// The callee name of a call: a bare identifier, or the trailing attribute of
/// an attribute access (`obj.method` -> `method`).
fn callee_name<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    match node.kind() {
        "identifier" => Some(node_text(bytes, node)),
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|attribute| node_text(bytes, attribute)),
        _ => None,
    }
}

/// Whether a call's function is `self.x` / `cls.x`, a method call on the
/// instance.
fn is_self_call(bytes: &[u8], function: TsNode<'_>) -> bool {
    if function.kind() != "attribute" {
        return false;
    }

    function
        .child_by_field_name("object")
        .is_some_and(|object| matches!(node_text(bytes, object), "self" | "cls"))
}

/// Whether a callee name reads as a class constructor (first character uppercase),
/// so `Article()` records an `Instantiates`, while a lowercase callee
/// (`render()`, `get_object()`) stays a `Calls`.
fn is_class_like(name: &str) -> bool {
    name.chars().next().is_some_and(|first| first.is_ascii_uppercase())
}

/// The final identifier of a (possibly dotted) name used as a base class.
fn dotted_last_name<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> Option<&'bytes str> {
    match node.kind() {
        "identifier" => Some(node_text(bytes, node)),
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|attribute| node_text(bytes, attribute)),
        "dotted_name" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).last().map(|last| node_text(bytes, last))
        }
        _ => None,
    }
}

/// The (local name, exported name) introduced by one entry in a
/// `from ... import` list. `X` -> `(X, X)`; `X as Y` -> `(Y, X)`.
fn imported_binding(bytes: &[u8], node: TsNode<'_>) -> Option<(String, String)> {
    match node.kind() {
        "identifier" | "dotted_name" => {
            let name = dotted_last_name(bytes, node)?.to_string();

            Some((name.clone(), name))
        }
        "aliased_import" => {
            let exported = node
                .child_by_field_name("name")
                .and_then(|name| dotted_last_name(bytes, name))?
                .to_string();

            let local = node
                .child_by_field_name("alias")
                .map_or_else(|| exported.clone(), |alias| node_text(bytes, alias).to_string());

            Some((local, exported))
        }
        _ => None,
    }
}

/// The base symbol name of a `decorator` node: the final dotted segment of its
/// expression, with any call arguments ignored. `@app.task(bind=True)` ->
/// `task`, `@property` -> `property`.
fn decorator_base_name(bytes: &[u8], decorator: TsNode<'_>) -> Option<String> {
    let expression = decorator_expression(decorator)?;

    let target = match expression.kind() {
        "call" => expression.child_by_field_name("function")?,
        _ => expression,
    };

    dotted_last_name(bytes, target).map(str::to_string)
}

/// The expression a `decorator` node applies, the part after the `@`.
fn decorator_expression(decorator: TsNode<'_>) -> Option<TsNode<'_>> {
    let mut cursor = decorator.walk();
    decorator.named_children(&mut cursor).next()
}

/// The final path segment of a file path.
fn file_basename(file_path: &str) -> &str {
    let basename = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);

    assert!(!basename.is_empty(), "file basename must not be empty");

    basename
}

/// A 1-based [`Span`] covering a tree node.
fn span_of(node: TsNode<'_>) -> Span {
    let start = node.start_position();
    let end = node.end_position();

    Span::new(
        line_1based(start.row),
        line_1based(end.row),
        to_u32(start.column),
        to_u32(end.column),
    )
}

/// A node's source text, falling back to empty on the (UTF-8-invalid)
/// error path that valid source never takes.
fn node_text<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> &'bytes str {
    node.utf8_text(bytes).unwrap_or("")
}

/// A saturating `usize` -> `u32` cast; source positions fit comfortably under the cap.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The 1-based line a 0-based tree-sitter row converts to.
fn line_1based(row: usize) -> u32 {
    let line = to_u32(row).saturating_add(1);

    assert!(line >= 1, "a 1-based line is at least one");

    line
}

/// The names listed in a module-level `__all__` (Python's explicit export
/// allowlist), gathered for [`apply_exports`]. Scans only top-level statements
/// (`__all__` is module scope) for `__all__ = [...]` / `(...)` and the augmented
/// `__all__ += [...]`; a dynamically built `__all__` contributes nothing.
fn collect_dunder_all(bytes: &[u8], root: TsNode<'_>) -> FxHashSet<String> {
    let mut exported: FxHashSet<String> = FxHashSet::default();
    let mut cursor = root.walk();
    let mut count: u32 = 0;

    for child in root.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "module fan-out exceeded {CHILDREN_MAX}");

        let statement = if child.kind() == "expression_statement" {
            child.named_child(0)
        } else {
            Some(child)
        };

        let Some(statement) = statement else {
            continue;
        };

        if !matches!(statement.kind(), "assignment" | "augmented_assignment") {
            continue;
        }

        let Some(left) = statement.child_by_field_name("left") else {
            continue;
        };

        if left.kind() != "identifier" || node_text(bytes, left) != "__all__" {
            continue;
        }

        if let Some(right) = statement.child_by_field_name("right") {
            collect_string_list(bytes, right, &mut exported);
        }
    }

    exported
}

/// The insertion of each string-literal element of a list/tuple/set into `out`.
fn collect_string_list(bytes: &[u8], node: TsNode<'_>, out: &mut FxHashSet<String>) {
    if !matches!(node.kind(), "list" | "tuple" | "set") {
        return;
    }

    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "export list fan-out exceeded {CHILDREN_MAX}");

        if let Some(value) = string_content_text(bytes, child)
            && !value.is_empty()
        {
            out.insert(value.to_string());
        }
    }
}

/// The flagging of the file-scope nodes whose name appears in the module's `__all__`.
/// Membership is by name among this file's top-level symbols (extraction output
/// holds one file), so a nested method sharing an exported name is left alone.
fn apply_exports(output: &mut ExtractionOutput, exported: &FxHashSet<String>) {
    if exported.is_empty() {
        return;
    }

    for node in &mut output.nodes {
        let file_scoped = node.qualified_name.rsplit("::").next() == Some(node.name.as_str());

        if file_scoped && exported.contains(&node.name) {
            node.is_exported = true;
        }
    }
}
