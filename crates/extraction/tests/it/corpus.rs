//! Fixtures shaped like the real Django code constellation is pointed at.
//!
//! Every fixture here reproduces a structure that `cargo xtask fixtures survey`
//! found to be common across the indexed repositories, and each one's doc names
//! the count that earned it a place. None of them contains a line of real code:
//! the survey reports shapes and frequencies, never source, and these were then
//! written fresh in an invented apiary domain. Structure is what the extractor
//! sees, so structure is all a fixture has to carry, and it is also the only
//! part that means nothing on its own.
//!
//! This module exists because [`crate::snapshot`] was written from imagination
//! and the survey showed how far that lands from the real thing. Invented Django
//! reaches for a class-based view; the corpus has 1,107 function-based views in
//! `*views.py` and no class-based ones. Invented Django puts one namespace on a
//! route; the corpus puts three on 650 of them. Invented Django writes
//! `urls.py`; the corpus splits it four ways by response type, into
//! `page_urls.py`, `form_urls.py`, `template_urls.py`, and `json_urls.py`.
//! Every one of those is a path through the extractor that no invented fixture
//! was exercising.
//!
//! When the survey reports a shape these fixtures do not cover, that is the
//! signal to add one. When it reports that a shape they do cover has vanished
//! from the corpus, that is the signal to ask whether the fixture is still
//! earning its place.

use constellation_extraction::{Extractor, PythonExtractor, TemplateExtractor};
use constellation_graph::ProjectId;

use crate::dump::dump;

/// The project every corpus fixture is extracted under.
const PROJECT: &str = "apiary";

/// An application config module.
///
/// Corpus evidence: `apps.py` is the second most common module name in the
/// corpus (185 of them), and `AppConfig` is a top base class (130 subclasses).
/// Nothing else in the suite extracts one, so the `default_auto_field` and
/// `name` class attributes had no coverage at all.
const APPS: &str = "from django.apps import AppConfig


class BeehiveConfig(AppConfig):
    default_auto_field = 'django.db.models.BigAutoField'
    name = 'apiary.beehive'
    label = 'apiary_beehive'
";

/// A choices module.
///
/// Corpus evidence: 81 `choices.py` modules, and the two bases behind them
/// (`SpireTextChoices` and `StrEnum`) account for 158 subclasses between them.
/// The shape matters because the members are class attributes whose values are
/// tuples, which is a different assignment shape to a model field.
const CHOICES: &str = "from enum import StrEnum

from hivecore.core.choices import HiveTextChoices


class HiveStatus(HiveTextChoices):
    ACTIVE = 'active', 'Active'
    DORMANT = 'dormant', 'Dormant'
    COLLAPSED = 'collapsed', 'Collapsed'


class InspectionOutcome(StrEnum):
    PASSED = 'passed'
    FOLLOW_UP = 'follow_up'
";

/// A queryset module.
///
/// Corpus evidence: 132 `querysets.py` modules, with `HistoryQuerySet` alone
/// carrying 103 subclasses. Every method here returns `self.filter(...)`, which
/// is the queryset-dispatch shape that dominates the corpus's unresolved
/// references (`calls filter` and `calls get` are the top two at 2,116 and
/// 4,401), so this is the fixture that pins how those references come out.
const QUERYSETS: &str = "from hivecore.history.querysets import HistoryQuerySet

from apiary.beehive.choices import HiveStatus


class BeehiveQuerySet(HistoryQuerySet):
    def active(self):
        return self.filter(status=HiveStatus.ACTIVE)

    def for_apiary(self, apiary_id):
        return self.active().filter(apiary_id=apiary_id)

    def with_inspections(self):
        return self.prefetch_related('inspections').distinct()
";

/// A models module.
///
/// Corpus evidence: 170 `models.py` modules. The base combination is the one
/// the survey found in the corpus (`ActivityMixin + HistoryModelMixin`, 54
/// occurrences), which is the case the model heuristic has to get right without
/// seeing `models.Model` in the bases at all. `related_name` appears on 149
/// fields and `on_delete` on 145, so both ride along here.
const MODELS: &str = "from django.db import models

from hivecore.activity.mixins import ActivityMixin
from hivecore.history.mixins import HistoryModelMixin

from apiary.beehive.choices import HiveStatus
from apiary.beehive.querysets import BeehiveQuerySet


class Beehive(ActivityMixin, HistoryModelMixin):
    apiary = models.ForeignKey(
        'apiary_site.Apiary',
        on_delete=models.CASCADE,
        related_name='beehives',
        related_query_name='beehive',
    )
    tag = models.CharField(max_length=32, blank=True, default='')
    status = models.CharField(
        max_length=16,
        choices=HiveStatus.choices,
        default=HiveStatus.ACTIVE,
    )
    frame_count = models.PositiveIntegerField(default=10)

    objects = BeehiveQuerySet.as_manager()

    class Meta:
        ordering = ['tag']
        verbose_name = 'Beehive'

    def __str__(self):
        return self.tag

    @property
    def is_active(self):
        return self.status == HiveStatus.ACTIVE

    @classmethod
    def active_for(cls, apiary_id):
        return cls.objects.for_apiary(apiary_id)
";

/// A service module.
///
/// Corpus evidence: 107 `service.py` modules and 196 subclasses of a base
/// model service. The corpus reaches this layer through a `self.obj` attribute
/// rather than through a parameter, which is a receiver shape the resolver has
/// to follow to attribute the calls.
const SERVICE: &str = "from hivecore.core.service import BaseHiveModelService

from apiary.beehive.choices import HiveStatus


class BeehiveService(BaseHiveModelService):
    def collapse(self):
        self.obj.status = HiveStatus.COLLAPSED
        self.obj.save()

        return self.obj

    def rename(self, tag):
        self.obj.tag = tag

        return self.obj.save()
";

/// A page views module.
///
/// Corpus evidence: this is the biggest gap the survey found. The corpus holds
/// 1,107 view symbols in `*views.py` and not one class-based view; views are
/// functions, split across `page_views.py` (97), `form_views.py` (71),
/// `template_views.py` (64), and `json_views.py` (61). `login_required()`
/// decorates 100 of them. The `template=` keyword reaching a helper, rather than
/// a direct `render`, is the corpus's way of naming a template, and it is a
/// different edge to trace than `render(request, ...)`.
const PAGE_VIEWS: &str = "from django.contrib.auth.decorators import login_required
from django.shortcuts import redirect
from django.urls import reverse

from hivecore.core.views import page_views as hive_page_views

from apiary.beehive.models import Beehive
from apiary.beehive.service import BeehiveService


@login_required()
def beehive_list_view(request):
    return hive_page_views.list_view(
        request,
        template='apiary/beehive/page/list.html',
        queryset=Beehive.objects.active(),
    )


@login_required()
def beehive_detail_view(request, pk):
    return hive_page_views.detail_view(
        request,
        template='apiary/beehive/page/detail.html',
        obj=Beehive.objects.get(pk=pk),
    )


@login_required()
def beehive_collapse_view(request, pk):
    BeehiveService(Beehive.objects.get(pk=pk)).collapse()

    return redirect(reverse('apiary:beehive:page:detail', args=[pk]))
";

/// A page URLs module.
///
/// Corpus evidence: 99 `page_urls.py` modules, and 650 routes reached through
/// three namespace segments against 245 at two and 201 at four. A fixture with
/// one segment, which is what invented Django produces, exercises none of the
/// namespace chaining that assembles `apiary:beehive:page:detail`.
const PAGE_URLS: &str = "from django.urls import path

from apiary.beehive.views import page_views

app_name = 'page'

urlpatterns = [
    path('', page_views.beehive_list_view, name='list'),
    path('<int:pk>/', page_views.beehive_detail_view, name='detail'),
    path('<int:pk>/collapse/', page_views.beehive_collapse_view, name='collapse'),
]
";

/// The app URLconf that mounts the per-response-type modules under namespaces.
///
/// Corpus evidence: this is the module that turns 650 routes into three-segment
/// reverse names. The include names its target by dotted path rather than by
/// passing a pattern list, which is the form the corpus writes and the only one
/// the resolver can follow to another module without the list in hand.
const APP_URLS: &str = "from django.urls import include, path

app_name = 'beehive'

urlpatterns = [
    path('page/', include('apiary.beehive.urls.page_urls', namespace='page')),
    path('form/', include('apiary.beehive.urls.form_urls', namespace='form')),
    path('json/', include('apiary.beehive.urls.json_urls', namespace='json')),
]
";

/// An admin module.
///
/// Corpus evidence: 149 `admin.py` modules and 66 subclasses of a project admin
/// base. The `@admin.register` decorator is the binding between an admin class
/// and the model it administers, and it is the only place that edge appears.
const ADMIN: &str = "from django.contrib import admin

from hivecore.core.admin import HiveModelAdmin

from apiary.beehive.models import Beehive


@admin.register(Beehive)
class BeehiveAdmin(HiveModelAdmin):
    list_display = ['tag', 'status', 'frame_count']
    search_fields = ['tag']
    raw_id_fields = ['apiary']
";

/// A test module.
///
/// Corpus evidence: `BaseTestCase` is the single most common base class in the
/// corpus by a wide margin (1,700 subclasses), and `pytest.mark.django_db`
/// decorates 1,666 symbols. Test coverage edges are a tool surface of their own
/// (`constellation_tests`), so the shape that produces them is worth pinning.
const TESTS: &str = "import pytest

from hivecore.core.tests.test_cases import BaseTestCase

from apiary.beehive.models import Beehive
from apiary.beehive.tests.factories import BeehiveFactory


@pytest.mark.django_db
class BeehiveTestCase(BaseTestCase):
    def test_is_active(self):
        beehive = BeehiveFactory()

        return self.assertTrue(beehive.is_active)

    def test_active_for(self):
        return Beehive.active_for(1).count()
";

/// A template.
///
/// Corpus evidence: `form.html` is the most common template name in the corpus
/// (79 of them), templates are 2,780 nodes, and `includes_template` is a
/// 4,371-edge relationship. The three-segment reverse name in the `url` tag is
/// what a template looks like once routes are namespaced the way the corpus
/// namespaces them.
const TEMPLATE: &str = "{% extends 'hivecore/base/page.html' %}
{% load hive_tags %}

{% block content %}
  <div class=\"beehive-card\" x-data=\"beehiveCard\">
    <h1>{{ beehive.tag|title }}</h1>

    {% for inspection in beehive.inspections.all %}
      <p class=\"inspection-row\">{{ inspection.outcome }}</p>
    {% endfor %}

    {% if beehive.is_active %}
      <a href=\"{% url 'apiary:beehive:page:collapse' beehive.pk %}\">Collapse</a>
    {% endif %}

    {% include 'apiary/beehive/partial/form.html' with beehive=beehive %}
  </div>
{% endblock %}
";

/// A fixture extracted twice and rendered as the text its snapshot holds.
///
/// Extracting twice is how "extraction is pure and per-file" gets checked: a
/// parser that leaked state between runs would still match on the first
/// extraction in a fresh process.
fn stable(extractor: &dyn Extractor, file_path: &str, source: &str) -> String {
    let project = ProjectId::new(PROJECT);

    let first = dump(&project, file_path, &extractor.extract(&project, file_path, source));
    let second = dump(&project, file_path, &extractor.extract(&project, file_path, source));

    assert_eq!(first, second, "extracting {file_path} twice gave two different graphs");
    assert!(first.contains("nodes ("), "a dump of {file_path} carries a node section");

    first
}

/// A Python fixture extracted and rendered.
fn python(file_path: &str, source: &str) -> String {
    stable(&PythonExtractor::new(), file_path, source)
}

#[test]
fn app_config_module() {
    insta::assert_snapshot!("apps", python("apiary/beehive/apps.py", APPS));
}

#[test]
fn choices_module() {
    insta::assert_snapshot!("choices", python("apiary/beehive/choices.py", CHOICES));
}

#[test]
fn querysets_module() {
    insta::assert_snapshot!("querysets", python("apiary/beehive/querysets.py", QUERYSETS));
}

#[test]
fn models_module() {
    insta::assert_snapshot!("models", python("apiary/beehive/models.py", MODELS));
}

#[test]
fn service_module() {
    insta::assert_snapshot!("service", python("apiary/beehive/service.py", SERVICE));
}

#[test]
fn page_views_module() {
    let text = python("apiary/beehive/views/page_views.py", PAGE_VIEWS);

    insta::assert_snapshot!("page_views", text);
}

#[test]
fn page_urls_module() {
    let text = python("apiary/beehive/urls/page_urls.py", PAGE_URLS);

    insta::assert_snapshot!("page_urls", text);
}

#[test]
fn app_urls_module() {
    let text = python("apiary/beehive/urls/__init__.py", APP_URLS);

    insta::assert_snapshot!("app_urls", text);
}

#[test]
fn admin_module() {
    insta::assert_snapshot!("admin", python("apiary/beehive/admin.py", ADMIN));
}

#[test]
fn tests_module() {
    let text = python("apiary/beehive/tests/test_models.py", TESTS);

    insta::assert_snapshot!("tests", text);
}

#[test]
fn page_template() {
    let path = "apiary/templates/apiary/beehive/page/detail.html";
    let text = stable(&TemplateExtractor::new(), path, TEMPLATE);

    insta::assert_snapshot!("page_template", text);
}
