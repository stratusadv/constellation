use constellation_extraction::{ExtractionOutput, Extractor, PythonExtractor};
use constellation_graph::{EdgeKind, NodeKind, ProjectId};
use constellation_resolution::SUPER_DISPATCH;

const SOURCE: &str = "from django.db import models
from .utils import helper as do_help

class Article(models.Model):
    title = 1
    @property
    def publish(self):
        do_help()
        return self.title

def make():
    return Article()
";

fn run() -> ExtractionOutput {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    extractor.extract(&project, "blog/models.py", SOURCE)
}

fn names_of(output: &ExtractionOutput, kind: NodeKind) -> Vec<String> {
    output
        .nodes
        .iter()
        .filter(|node| node.kind == kind)
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn extracts_class_method_and_function() {
    let output = run();

    assert_eq!(names_of(&output, NodeKind::File), vec!["models.py".to_string()]);
    assert_eq!(names_of(&output, NodeKind::Model), vec!["Article".to_string()]);
    assert!(names_of(&output, NodeKind::Method).is_empty(), "publish is a property, not a method");
    assert_eq!(names_of(&output, NodeKind::Property), vec!["publish".to_string()]);
    assert_eq!(names_of(&output, NodeKind::Function), vec!["make".to_string()]);
}

#[test]
fn detects_model_with_mixin_base_and_excludes_pydantic() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("app");
    let source = "from __future__ import annotations

from django.db import models


class Inventory(HistoryModelMixin, ActivityMixin):
    # All Types (Base Fields) - shared across all inventory types
    name = models.CharField(max_length=255)
    brand = models.ForeignKey(
        'configuration_brand.Brand',
        on_delete=models.SET_NULL,
        null=True,
    )


class LoopCTT(BaseModel):
    quantity = Field(default=0)
";

    let output = extractor.extract(&project, "app/inventory/models.py", source);
    let found = names_of(&output, NodeKind::Model);

    assert!(found.contains(&"Inventory".to_string()), "mixin-based model detected, got {found:?}");
    assert!(!found.contains(&"LoopCTT".to_string()), "pydantic BaseModel excluded, got {found:?}");
}

#[test]
fn classifies_function_based_view_by_request_parameter() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("workspace");
    let source = "from django.template.response import TemplateResponse


def list_view(request):
    return workspace_views.list_view(request, template='app/list.html')


def detail_view(request: WSGIRequest, pk: int) -> TemplateResponse:
    return workspace_views.detail_view(request, template='app/detail.html')


def helper(value):
    return value
";

    let output = extractor.extract(&project, "app/views/page_views.py", source);
    let views = names_of(&output, NodeKind::View);
    let functions = names_of(&output, NodeKind::Function);

    assert!(views.contains(&"list_view".to_string()), "untyped request view, got {views:?}");
    assert!(views.contains(&"detail_view".to_string()), "typed request view, got {views:?}");
    assert!(functions.contains(&"helper".to_string()), "non-request fn stays a function, got {functions:?}");
    assert!(!views.contains(&"helper".to_string()), "helper must not be a view, got {views:?}");
}

#[test]
fn captures_include_namespace_and_full_reverse_target() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("workspace");
    let source = "from django.urls import include, path, reverse
from django.shortcuts import redirect


app_name = 'partner'

urlpatterns = [
    path('page/', include('app.partner.urls.page_urls', namespace='page')),
]


def go(request):
    return redirect(reverse('partner:page:detail'))
";

    let output = extractor.extract(&project, "app/partner/urls/__init__.py", source);

    let include_namespace = output
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::Route)
        .and_then(|node| node.signature.clone());

    assert_eq!(include_namespace.as_deref(), Some("page"), "include namespace captured on the route");

    let reverse_target = output
        .unresolved_refs
        .iter()
        .find(|reference| reference.reference_kind == EdgeKind::Resolves)
        .map(|reference| reference.reference_name.clone());

    assert_eq!(
        reverse_target.as_deref(),
        Some("partner:page:detail"),
        "reverse keeps the full namespaced target, not just the last segment",
    );
}

#[test]
fn method_is_contained_by_its_class() {
    let output = run();

    let article = output.nodes.iter().find(|node| node.name == "Article").unwrap();
    let publish = output.nodes.iter().find(|node| node.name == "publish").unwrap();

    let contained = output.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Contains
            && edge.source == article.id
            && edge.target == publish.id
    });

    assert!(contained, "Article must contain publish via a contains edge");
    assert_eq!(publish.qualified_name, "blog/models.py::Article.publish");
}

#[test]
fn records_inheritance_call_and_import_references() {
    let output = run();

    let extends = output
        .unresolved_refs
        .iter()
        .any(|reference| reference.reference_kind == EdgeKind::Extends && reference.reference_name == "Model");

    assert!(extends, "Article should extend Model");

    let calls: Vec<&str> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Calls)
        .map(|reference| reference.reference_name.as_str())
        .collect();

    assert!(calls.contains(&"do_help"), "expected a call to do_help, got {calls:?}");

    let instantiates: Vec<&str> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Instantiates)
        .map(|reference| reference.reference_name.as_str())
        .collect();

    assert!(instantiates.contains(&"Article"), "expected Article instantiation, got {instantiates:?}");

    let imports: Vec<&str> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Imports)
        .map(|reference| reference.reference_name.as_str())
        .collect();

    assert!(imports.contains(&"models"), "expected import of models, got {imports:?}");
    assert!(imports.contains(&"helper"), "expected import of helper, got {imports:?}");
}

#[test]
fn skips_bare_builtin_calls_but_keeps_attribute_calls() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "def handle(request, queryset):
    print(len(request))
    value = isinstance(request, dict)
    text = str(value)
    row = queryset.get(id=1)
    queryset.filter(active=True)
    return text
";

    let output = extractor.extract(&project, "blog/views.py", source);

    let calls: Vec<&str> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Calls)
        .map(|reference| reference.reference_name.as_str())
        .collect();

    for builtin in ["print", "len", "isinstance", "str"] {
        assert!(!calls.contains(&builtin), "bare builtin {builtin} should not emit a call, got {calls:?}");
    }

    assert!(calls.contains(&"get"), "attribute call queryset.get should still emit, got {calls:?}");
    assert!(calls.contains(&"filter"), "attribute call queryset.filter should still emit, got {calls:?}");
}

#[test]
fn module_and_class_bindings_become_constants_and_variables() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "LIST_FILTERING_SESSION_KEY = 'lfk'
app_name = 'blog'
urlpatterns = []


class Status(models.TextChoices):
    DRAFT = 'draft', 'Draft'
    PUBLISHED = 'published', 'Published'
";

    let output = extractor.extract(&project, "blog/urls.py", source);

    let constants = names_of(&output, NodeKind::Constant);
    let variables = names_of(&output, NodeKind::Variable);

    assert!(constants.contains(&"LIST_FILTERING_SESSION_KEY".to_string()), "got {constants:?}");
    assert!(constants.contains(&"DRAFT".to_string()), "TextChoices member is a constant, got {constants:?}");
    assert!(constants.contains(&"PUBLISHED".to_string()), "TextChoices member is a constant, got {constants:?}");
    assert!(variables.contains(&"app_name".to_string()), "lowercase binding is a variable, got {variables:?}");
    assert!(variables.contains(&"urlpatterns".to_string()), "urlpatterns is a variable, got {variables:?}");
}

#[test]
fn settings_string_list_is_captured_as_signature() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("site");

    let source = "INSTALLED_APPS = [
    'django.contrib.admin',
    'django_spire.core',
    'workspace.billing',
]
";

    let output = extractor.extract(&project, "site/settings.py", source);
    let apps = output
        .nodes
        .iter()
        .find(|node| node.name == "INSTALLED_APPS")
        .expect("INSTALLED_APPS node");

    assert_eq!(apps.kind, NodeKind::Constant, "INSTALLED_APPS is a constant");

    let signature = apps.signature.as_deref().unwrap_or("");

    assert!(signature.contains("django_spire.core"), "signature lists the apps, got {signature:?}");
    assert!(signature.contains("workspace.billing"), "signature lists the apps, got {signature:?}");
}

#[test]
fn dunder_all_marks_is_exported() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "__all__ = ['PublicThing', 'helper']


class PublicThing:
    pass


class PrivateThing:
    pass


def helper():
    pass
";

    let output = extractor.extract(&project, "blog/api.py", source);
    let exported = |name: &str| {
        output.nodes.iter().find(|node| node.name == name).is_some_and(|node| node.is_exported)
    };

    assert!(exported("PublicThing"), "PublicThing in __all__ is exported");
    assert!(exported("helper"), "helper in __all__ is exported");
    assert!(!exported("PrivateThing"), "PrivateThing absent from __all__ stays unexported");
}

#[test]
fn drf_action_and_api_view_emit_routes() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("api");

    let source = "from rest_framework.decorators import action, api_view


class ArticleViewSet(ViewSet):
    @action(detail=True)
    def publish(self, request, pk=None):
        pass


@api_view(['GET'])
def stats(request):
    pass
";

    let output = extractor.extract(&project, "api/views.py", source);
    let routes = names_of(&output, NodeKind::Route);

    assert!(routes.iter().any(|route| route.contains("publish")), "@action emits a route, got {routes:?}");
    assert!(routes.iter().any(|route| route.contains("stats")), "@api_view emits a route, got {routes:?}");

    let publish = output.nodes.iter().find(|node| node.name == "publish").expect("publish node");
    let routes_to_publish = output
        .edges
        .iter()
        .any(|edge| edge.kind == EdgeKind::RoutesTo && edge.target == publish.id);

    assert!(routes_to_publish, "a RoutesTo edge targets the action method");
}

#[test]
fn typed_self_attribute_dispatches_to_its_class() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "class ArticleService:
    repository: ArticleRepository

    def fetch(self):
        return self.repository.find_all()
";

    let output = extractor.extract(&project, "blog/services.py", source);
    let dispatched = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Calls
            && reference.reference_name == "find_all"
            && reference.candidates.iter().any(|candidate| candidate == "ArticleRepository")
    });

    assert!(dispatched, "self.repository.find_all() carries the ArticleRepository typed-receiver candidate");
}

#[test]
fn a_model_reached_through_its_module_still_names_the_queryset_dispatch() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "def listing(request):
    return models.Article.objects.published()
";

    let output = extractor.extract(&project, "blog/views.py", source);

    let dispatched = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Calls
            && reference.reference_name == "published"
            && reference.candidates.iter().any(|candidate| candidate == "Article")
    });

    assert!(dispatched, "models.Article.objects.published() names Article as the dispatch model");
}

#[test]
fn a_runtime_manager_receiver_names_no_dispatch_model() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "class Factory:
    def build(self):
        return self.obj_class.objects.published()
";

    let output = extractor.extract(&project, "blog/factory.py", source);

    let named = output.unresolved_refs.iter().any(|reference| {
        reference.reference_name == "published"
            && reference.candidates.iter().any(|candidate| candidate == "obj_class")
    });

    assert!(!named, "a lower-case runtime receiver must not be taken for a model name");
}

#[test]
fn a_super_call_carries_the_enclosing_class_and_its_sentinel() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "class ArticleTestCase(BaseTestCase):
    def setUp(self):
        super().setUp()
";

    let output = extractor.extract(&project, "blog/tests.py", source);

    let marked = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Calls
            && reference.reference_name == "setUp"
            && reference.candidates.first().is_some_and(|first| first == SUPER_DISPATCH)
            && reference.candidates.iter().any(|candidate| candidate.ends_with("ArticleTestCase"))
    });

    assert!(marked, "super().setUp() carries the super sentinel and the enclosing class");
}

#[test]
fn an_explicit_two_argument_super_is_left_to_the_generic_path() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "class Article(Base):
    def save(self):
        super(Other, self).save()
";

    let output = extractor.extract(&project, "blog/models.py", source);

    let marked = output.unresolved_refs.iter().any(|reference| {
        reference.reference_name == "save"
            && reference.candidates.iter().any(|candidate| candidate == SUPER_DISPATCH)
    });

    assert!(!marked, "super(Other, self) names a different class to skip, so it is not marked");
}

#[test]
fn malformed_source_does_not_panic() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let output = extractor.extract(&project, "blog/broken.py", "def (:::\n  class");

    assert!(
        output.nodes.iter().any(|node| node.kind == NodeKind::File),
        "even unparseable input yields a file node",
    );
}

const SIGNALS: &str = "from django.db.models.signals import post_save
from django.dispatch import receiver

from .models import Article


@receiver(post_save, sender=Article)
def on_article_saved(sender, instance, **kwargs):
    pass


def on_article_deleted(sender, instance, **kwargs):
    pass


post_save.connect(on_article_deleted, sender=Article)
";

fn run_signals() -> ExtractionOutput {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    extractor.extract(&project, "blog/signals.py", SIGNALS)
}

#[test]
fn receiver_decorator_links_handler_to_sender_model() {
    let output = run_signals();

    let handler = output
        .nodes
        .iter()
        .find(|node| node.name == "on_article_saved")
        .expect("on_article_saved handler node");

    let receives = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Receives
            && reference.reference_name == "Article"
            && reference.from_node_id == handler.id
    });

    assert!(receives, "@receiver handler must receive on its sender model Article");
}

#[test]
fn signal_connect_links_wiring_to_sender_model() {
    let output = run_signals();

    let wiring = output.unresolved_refs.iter().find(|reference| {
        reference.reference_kind == EdgeKind::Receives
            && reference.reference_name == "Article"
            && reference.candidates.iter().any(|candidate| candidate == "on_article_deleted")
    });

    assert!(wiring.is_some(), "signal.connect must record the model and handler it wires");
}

#[test]
fn template_kwarg_emits_renders_ref() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("app");

    let source = "def detail_view(request):\n    return workspace_views.list_view(request, template='hr/employee.html')\n";

    let output = extractor.extract(&project, "app/views.py", source);

    let renders = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Renders && reference.reference_name == "hr/employee.html"
    });

    assert!(renders, "a template= string kwarg yields a Renders ref to that template");
}

#[test]
fn extracts_type_annotation_edges() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "from .models import Article, Author


class ArticleService:
    repository: ArticleRepository

    def fetch(self, author: Author) -> Article:
        local: Widget = build()
        return Article()

    def names(self) -> list[str]:
        return []
";

    let output = extractor.extract(&project, "blog/services.py", source);

    let has = |kind: EdgeKind, name: &str| {
        output
            .unresolved_refs
            .iter()
            .any(|reference| reference.reference_kind == kind && reference.reference_name == name)
    };

    assert!(has(EdgeKind::Returns, "Article"), "return annotation -> Returns to Article");
    assert!(has(EdgeKind::TypeOf, "Author"), "parameter annotation -> TypeOf to Author");
    assert!(has(EdgeKind::TypeOf, "ArticleRepository"), "class attribute annotation -> TypeOf");

    assert!(!has(EdgeKind::Returns, "str"), "builtin str is filtered from type edges");
    assert!(!has(EdgeKind::TypeOf, "Widget"), "a function-local annotation is not linked");
}

#[test]
fn emits_context_type_ref_for_get_object_or_404() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");
    let source = "from django.shortcuts import get_object_or_404\n\n\
        def detail_view(request, pk):\n    \
            widget = get_object_or_404(Widget, pk=pk)\n    \
            return widget\n";

    let output = extractor.extract(&project, "blog/views.py", source);

    let context = output
        .unresolved_refs
        .iter()
        .find(|reference| reference.reference_kind == EdgeKind::ContextType)
        .expect("a ContextType ref for the get_object_or_404 local");

    assert_eq!(context.reference_name, "Widget", "the model name is the call's first argument");

    assert_eq!(
        context.candidates.first().map(String::as_str),
        Some("widget"),
        "the local's name rides in candidates for the member synthesis to key on",
    );
}

#[test]
fn emits_context_type_ref_for_this_stacks_instance_shortcuts() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    for shortcut in ["get_object_or_null_obj", "get_object_or_none"] {
        let source = format!(
            "def form_view(request, pk):\n    \
                widget = {shortcut}(models.Widget, pk=pk)\n    \
                return widget\n"
        );

        let output = extractor.extract(&project, "blog/views.py", &source);

        let context = output
            .unresolved_refs
            .iter()
            .find(|reference| reference.reference_kind == EdgeKind::ContextType)
            .unwrap_or_else(|| panic!("a ContextType ref for the {shortcut} local"));

        assert_eq!(
            context.reference_name,
            "Widget",
            "{shortcut} names the model in its first argument",
        );

        assert_eq!(
            context.candidates.first().map(String::as_str),
            Some("widget"),
            "{shortcut} types the local the same way get_object_or_404 does",
        );

        assert!(
            !context.candidates.iter().any(|candidate| candidate == "\u{1}collection-context"),
            "{shortcut} returns one instance, never a collection",
        );
    }
}

#[test]
fn emits_collection_context_type_for_queryset() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");
    let source = "def list_view(request):\n    \
            records = Widget.objects.filter(active=True)\n    \
            one = Widget.objects.get(pk=1)\n    \
            return records\n";

    let output = extractor.extract(&project, "blog/views.py", source);

    let context: Vec<(&str, Option<&str>, bool)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::ContextType)
        .map(|reference| {
            (
                reference.reference_name.as_str(),
                reference.candidates.first().map(String::as_str),
                reference.candidates.iter().any(|candidate| candidate == "\u{1}collection-context"),
            )
        })
        .collect();

    assert!(
        context.contains(&("Widget", Some("records"), true)),
        "Model.objects.filter(...) types the local as a collection of the model, got {context:?}",
    );

    assert!(
        context.contains(&("Widget", Some("one"), false)),
        "Model.objects.get(...) types the local as a single instance, got {context:?}",
    );
}

#[test]
fn emits_context_type_for_glue_registration() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");
    let source = "import django_glue as dg\n\
        def form_view(request):\n    \
            dg.glue_query_set(request, 'trailers', Asset.objects.filter(kind='t'))\n    \
            Glue.queryset(request=request, unique_name='gorillas', target=Gorilla.objects.all())\n    \
            Glue.model(request, 'one', Widget.objects.get(pk=1))\n    \
            return 1\n";

    let output = extractor.extract(&project, "blog/views.py", source);

    let context: Vec<(&str, Option<&str>, bool)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::ContextType)
        .map(|reference| {
            (
                reference.reference_name.as_str(),
                reference.candidates.first().map(String::as_str),
                reference.candidates.iter().any(|candidate| candidate == "\u{1}collection-context"),
            )
        })
        .collect();

    assert!(
        context.contains(&("Asset", Some("trailers"), true)),
        "old `dg.glue_query_set(request, 'trailers', Asset.objects...)` keys a collection by the glue name, got {context:?}",
    );

    assert!(
        context.contains(&("Gorilla", Some("gorillas"), true)),
        "new `Glue.queryset(unique_name=, target=Gorilla.objects.all())` (kwargs) keys a collection, got {context:?}",
    );

    assert!(
        context.contains(&("Widget", Some("one"), false)),
        "new `Glue.model(request, 'one', Widget.objects.get())` (positional) keys an instance, got {context:?}",
    );
}

#[test]
fn async_def_sets_the_async_flag() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "async def load():\n    return 1\n\n\ndef sync_load():\n    return 2\n";

    let output = extractor.extract(&project, "blog/io.py", source);

    let load = output.nodes.iter().find(|node| node.name == "load").expect("a load node");
    let sync = output.nodes.iter().find(|node| node.name == "sync_load").expect("a sync_load node");

    assert!(load.is_async, "an async def is marked async");
    assert!(!sync.is_async, "a plain def is not async");
}

#[test]
fn static_and_class_method_decorators_are_captured() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("blog");

    let source = "class Helper:\n    @staticmethod\n    def make():\n        return 1\n\n    @classmethod\n    def build(cls):\n        return 2\n\n    def plain(self):\n        return 3\n";

    let output = extractor.extract(&project, "blog/helper.py", source);

    let make = output.nodes.iter().find(|node| node.name == "make").expect("a make node");
    let build = output.nodes.iter().find(|node| node.name == "build").expect("a build node");
    let plain = output.nodes.iter().find(|node| node.name == "plain").expect("a plain node");

    assert!(make.is_static, "@staticmethod sets is_static");

    assert!(
        make.decorators.iter().any(|decorator| decorator.contains("staticmethod")),
        "the staticmethod decorator text is captured, got {:?}",
        make.decorators,
    );

    assert!(
        build.decorators.iter().any(|decorator| decorator.contains("classmethod")),
        "the classmethod decorator text is captured, got {:?}",
        build.decorators,
    );

    assert!(!plain.is_static, "an undecorated method is not static");
}

#[test]
fn a_field_carries_its_help_text_and_never_clips_mid_identifier() {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("workspace");

    let source = concat!(
        "from django.core.validators import MinValueValidator\n",
        "from django.db import models\n",
        "\n",
        "\n",
        "class ProductionLine(models.Model):\n",
        "    cycle_time_seconds = models.IntegerField(\n",
        "        default=900,\n",
        "        help_text='Time for material to travel from the hopper.',\n",
        "    )\n",
        "\n",
        "    concurrent_station_count = models.IntegerField(\n",
        "        default=CONCURRENT_STATION_COUNT_MIN,\n",
        "        validators=[MinValueValidator(CONCURRENT_STATION_COUNT_MIN)],\n",
        "    )\n",
    );

    let output = extractor.extract(&project, "app/production/line/models.py", source);

    let signature_of = |name: &str| {
        output
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Field && node.name == name)
            .and_then(|node| node.signature.clone())
            .unwrap_or_default()
    };

    let cycle_time = signature_of("cycle_time_seconds");

    assert!(cycle_time.contains("default=900"), "the schema argument leads, got {cycle_time:?}");

    assert!(
        cycle_time.contains("\"Time for material to travel from the hopper.\""),
        "and the help text rides along after it, got {cycle_time:?}",
    );

    // The clip has to fall on a boundary. A cut mid-name prints a fragment that
    // reads as a different symbol and that no search will ever find.
    let stations = signature_of("concurrent_station_count");

    assert!(
        !stations.contains("CONCURRENT_STATION_CO..."),
        "a value is never clipped mid-identifier, got {stations:?}",
    );

    assert!(
        stations.contains("validators=[MinValueValidator(..."),
        "it backs up to the last boundary instead, got {stations:?}",
    );
}
