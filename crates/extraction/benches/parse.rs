//! Parse-time benchmarks for every language extractor, the hottest path in the
//! pipeline, run on every source file. Exercises per-node id/qualified-name
//! construction, reference allocation, decorator handling, the AST walk, and the
//! per-call tree-sitter parser setup, so these are the benches to watch when
//! changing extraction allocation patterns or parser reuse.

use constellation_extraction::{
    CssExtractor, Extractor, JavaScriptExtractor, PythonExtractor, TemplateExtractor,
};
use constellation_graph::ProjectId;

fn main() {
    divan::main();
}

/// A representative Django module: models with foreign keys and decorators, a
/// custom queryset/manager, and class-based views with type-annotated methods:
/// enough structure to exercise the extractor's hot helpers densely.
const DJANGO_SOURCE: &str = r#"
from django.db import models
from django.views.generic import ListView, DetailView
from django.utils.functional import cached_property


class TimeStampedModel(models.Model):
    created_at = models.DateTimeField(auto_now_add=True)
    updated_at = models.DateTimeField(auto_now=True)

    class Meta:
        abstract = True


class AuthorQuerySet(models.QuerySet):
    def active(self):
        return self.filter(is_active=True)

    def by_year(self, year):
        return self.filter(year=year)


class Author(TimeStampedModel):
    name = models.CharField(max_length=200)
    email = models.EmailField(unique=True)
    is_active = models.BooleanField(default=True)
    objects = AuthorQuerySet.as_manager()

    def __str__(self) -> str:
        return self.name

    @cached_property
    def article_count(self) -> int:
        return self.articles.count()


class Category(TimeStampedModel):
    label = models.CharField(max_length=120)
    parent = models.ForeignKey("self", null=True, on_delete=models.CASCADE)


class Article(TimeStampedModel):
    title = models.CharField(max_length=300)
    body = models.TextField()
    author = models.ForeignKey(Author, related_name="articles", on_delete=models.CASCADE)
    categories = models.ManyToManyField(Category, related_name="articles")

    def publish(self, editor: Author) -> bool:
        editor.article_count
        return self.author.is_active


class ArticleListView(ListView):
    model = Article
    template_name = "blog/article_list.html"
    paginate_by = 20

    def get_queryset(self):
        return Article.objects.active().by_year(self.kwargs["year"])


class ArticleDetailView(DetailView):
    model = Article
    template_name = "blog/article_detail.html"

    def get_context_data(self, **kwargs) -> dict:
        context = super().get_context_data(**kwargs)
        return context
"#;

/// A tiny Django model file, the common case in a real project (many small
/// modules). Per-file fixed costs such as tree-sitter parser construction
/// dominate here, so this bench is the most sensitive to parser reuse.
const SMALL_SOURCE: &str = r#"
from django.db import models


class Tag(models.Model):
    name = models.CharField(max_length=50)
    slug = models.SlugField(unique=True)
"#;

/// A representative JavaScript/Alpine module: ESM imports, a class with methods,
/// a `new` expression, and an `Alpine.data` component, so the JS extractor's
/// import-binding dedup and AST walk are exercised.
const JAVASCRIPT_SOURCE: &str = r#"
import { createStore, derived } from "./store.js";
import { formatMoney, parseQuery, debounce, clamp } from "../util/format.js";
import Chart from "../vendor/chart.js";

export class CartController {
    constructor(endpoint) {
        this.endpoint = endpoint;
        this.items = [];
        this.chart = new Chart(endpoint);
    }

    addItem(product) {
        this.items.push(product);
        return derived(this.items);
    }

    total() {
        return formatMoney(this.items.reduce((sum, item) => sum + item.price, 0));
    }
}

export function build(endpoint) {
    const store = createStore(endpoint);
    const update = debounce(() => store.refresh(), 200);
    return new CartController(endpoint);
}

Alpine.data("cart", () => ({
    open: false,
    items: [],
    addItem(product) {
        this.items.push(product);
        this.recount();
    },
    recount() {
        return clamp(parseQuery(this.items), 0, 99);
    },
}));
"#;

/// A representative Django template with Alpine attributes: extends/include,
/// blocks, for loops, many `{{ var.attr }}` accesses, and dense Alpine
/// expressions (a class map, a multi-call handler, an `x-data` object). The
/// Alpine attribute values drive the JS-expression sub-extractor, whose
/// identifier/class/member dedup is the hot inner cost here.
const TEMPLATE_SOURCE: &str = r#"
{% extends "base/layout.html" %}
{% load static i18n %}

{% block content %}
<section x-data="{ open: false, items: [], add() { this.items.push(this.next()); }, next() { return compute(this.items, this.open); }, reset() { this.items = []; track('reset'); } }">
    <header
        :class="{ 'is-open': open, 'is-empty': items.length === 0, 'has-items': items.length > 0, 'is-busy': loading, 'is-error': failed }"
        @click="open = !open; track('toggle'); refresh(); audit('header')"
    >
        {{ order.customer.display_name }} - {{ order.total|default:'0.00' }}
    </header>

    {% for line in order.lines %}
        <article>
            <span>{{ line.product.name }}</span>
            <span>{{ line.quantity }} x {{ line.unit_price }}</span>
            <span>{{ line.subtotal }}</span>
        </article>
    {% endfor %}

    {% include "orders/_summary.html" with total=order.total count=order.line_count %}
    {% include "orders/_actions.html" %}
{% endblock %}
"#;

/// A representative stylesheet: class, id, element, and descendant selectors with
/// declarations, exercising the CSS extractor's selector walk.
const CSS_SOURCE: &str = r#"
:root { --gap: 8px; --brand: #3366ff; }
.card { padding: var(--gap); border: 1px solid #ddd; border-radius: 4px; }
.card .title { font-weight: 600; color: var(--brand); }
.card .body { margin-top: var(--gap); line-height: 1.5; }
#header { position: sticky; top: 0; background: white; }
#header nav a { color: inherit; text-decoration: none; }
.btn { display: inline-block; padding: 4px 12px; }
.btn-active { background: var(--brand); color: white; }
.btn:hover { opacity: 0.9; }
ul.list > li { list-style: none; }
"#;

#[divan::bench]
fn parse_django_module(bencher: divan::Bencher) {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("bench");

    bencher.bench_local(|| {
        extractor.extract(
            divan::black_box(&project),
            divan::black_box("blog/models.py"),
            divan::black_box(DJANGO_SOURCE),
        )
    });
}

#[divan::bench]
fn parse_python_small(bencher: divan::Bencher) {
    let extractor = PythonExtractor::new();
    let project = ProjectId::new("bench");

    bencher.bench_local(|| {
        extractor.extract(
            divan::black_box(&project),
            divan::black_box("blog/tags.py"),
            divan::black_box(SMALL_SOURCE),
        )
    });
}

#[divan::bench]
fn parse_javascript_module(bencher: divan::Bencher) {
    let extractor = JavaScriptExtractor::new();
    let project = ProjectId::new("bench");

    bencher.bench_local(|| {
        extractor.extract(
            divan::black_box(&project),
            divan::black_box("shop/static/cart.js"),
            divan::black_box(JAVASCRIPT_SOURCE),
        )
    });
}

#[divan::bench]
fn parse_template_module(bencher: divan::Bencher) {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("bench");

    bencher.bench_local(|| {
        extractor.extract(
            divan::black_box(&project),
            divan::black_box("orders/templates/orders/detail.html"),
            divan::black_box(TEMPLATE_SOURCE),
        )
    });
}

#[divan::bench]
fn parse_css_module(bencher: divan::Bencher) {
    let extractor = CssExtractor::new();
    let project = ProjectId::new("bench");

    bencher.bench_local(|| {
        extractor.extract(
            divan::black_box(&project),
            divan::black_box("shop/static/styles.css"),
            divan::black_box(CSS_SOURCE),
        )
    });
}
