//! Parse-time benchmark for the Python extractor, the hottest path in the
//! pipeline, run on every source file. Exercises per-node id/qualified-name
//! construction, reference allocation, decorator handling, and the AST walk, so
//! it is the bench to watch when changing extraction allocation patterns.

use constellation_extraction::{Extractor, PythonExtractor};
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
