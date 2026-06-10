//! Wall-clock harness for a full project index under the real System allocator,
//! the companion to `index_mem` (which perturbs timing with its counting
//! allocator). It reports the extract+persist and resolve phase split so the true
//! cost breakdown is visible, free of allocation-tracking overhead.
//!
//! Run with `cargo run --release --example index_time -p constellation-index`.
//! Optional args: file count, repeat-per-file, and run count:
//! `... --example index_time -- 512 6 3`.

use std::cell::Cell;
use std::time::Instant;

use constellation_graph::ProjectId;
use constellation_index::{IndexPhase, index_project_reporting};
use constellation_store::Store;

/// A representative Django module repeated to size each generated file.
const MODULE: &str = r#"
from django.db import models
from django.views.generic import ListView, DetailView
from django.utils.functional import cached_property


class TimeStampedModel(models.Model):
    created_at = models.DateTimeField(auto_now_add=True)
    updated_at = models.DateTimeField(auto_now=True)

    class Meta:
        abstract = True


class Author(TimeStampedModel):
    name = models.CharField(max_length=200)
    email = models.EmailField(unique=True)
    objects = models.Manager()

    def __str__(self) -> str:
        return self.name

    @cached_property
    def article_count(self) -> int:
        return self.articles.count()


class Article(TimeStampedModel):
    title = models.CharField(max_length=300)
    author = models.ForeignKey(Author, related_name="articles", on_delete=models.CASCADE)

    def publish(self, editor: Author) -> bool:
        return self.author.is_active


class ArticleListView(ListView):
    model = Article
    template_name = "blog/article_list.html"

    def get_queryset(self):
        return Article.objects.all()
"#;

fn run_once(directory: &std::path::Path, files: usize, body: &str) -> (f64, f64, f64) {
    let _ = std::fs::remove_dir_all(directory);
    std::fs::create_dir_all(directory).expect("create corpus directory");

    for index in 0..files {
        let path = directory.join(format!("module_{index:05}.py"));
        std::fs::write(&path, body).expect("write corpus file");
    }

    let store = Store::open(&directory.join("graph.db")).expect("open store");
    let project = ProjectId::new("bench");

    let resolving_at: Cell<Option<Instant>> = Cell::new(None);

    let started = Instant::now();
    index_project_reporting(&store, &project, "bench", directory, |phase| {
        if matches!(phase, IndexPhase::Resolving) && resolving_at.get().is_none() {
            resolving_at.set(Some(Instant::now()));
        }
    })
    .expect("index");
    let total = started.elapsed().as_secs_f64();

    let extract = resolving_at.get().map_or(total, |at| at.duration_since(started).as_secs_f64());
    let resolve = resolving_at.get().map_or(0.0, |at| at.elapsed().as_secs_f64());

    (total, extract, resolve)
}

fn main() {
    let mut args = std::env::args().skip(1);

    let files: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(512);
    let repeat: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(6);
    let runs: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(3);

    assert!(files > 0, "file count must be positive");
    assert!(repeat > 0, "repeat count must be positive");
    assert!(runs > 0, "run count must be positive");

    let directory = std::env::temp_dir().join(format!("constellation_index_time_{files}_{repeat}"));
    let body = MODULE.repeat(repeat);

    let mut best = (f64::MAX, 0.0, 0.0);

    for _ in 0..runs {
        let sample = run_once(&directory, files, &body);

        if sample.0 < best.0 {
            best = sample;
        }
    }

    println!("{files} files, repeat {repeat}, best of {runs} (System allocator)");
    println!("total            {:.3} s", best.0);
    println!("  extract+persist {:.3} s", best.1);
    println!("  resolve         {:.3} s", best.2);

    let _ = std::fs::remove_dir_all(&directory);
}
