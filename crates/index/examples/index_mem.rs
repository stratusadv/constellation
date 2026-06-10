//! Peak-heap and wall-clock harness for a full project index, the measurement
//! divan cannot give (it times, it does not weigh memory). A counting global
//! allocator tracks live bytes and their high-water mark across an index of a
//! generated corpus, so the in-flight extract-chunk buffer's contribution to
//! peak memory is visible before and after a chunking change.
//!
//! Run with `cargo run --release --example index_mem -p constellation-index`.
//! Optional args: file count, and how many times the module body is repeated per
//! file (to size each file): `... --example index_mem -- 512 6`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use std::cell::Cell;

use constellation_graph::ProjectId;
use constellation_index::{IndexPhase, index_project_reporting};
use constellation_store::Store;

/// A System-allocator wrapper that records live bytes and their peak, so the
/// example can report the index's high-water heap mark.
struct Counting;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards verbatim to the System allocator and only
// updates two atomics around it, so the allocation contract is unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };

        if !pointer.is_null() {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }

        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

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

fn main() {
    let mut args = std::env::args().skip(1);

    let files: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(512);
    let repeat: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(6);

    assert!(files > 0, "file count must be positive");
    assert!(repeat > 0, "repeat count must be positive");

    let directory = std::env::temp_dir().join(format!("constellation_index_mem_{files}_{repeat}"));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create corpus directory");

    let body = MODULE.repeat(repeat);

    for index in 0..files {
        let path = directory.join(format!("module_{index:05}.py"));
        std::fs::write(&path, &body).expect("write corpus file");
    }

    let database = directory.join("graph.db");
    let _ = std::fs::remove_file(&database);
    let store = Store::open(&database).expect("open store");
    let project = ProjectId::new("bench");

    // Reset the peak to the index's own high-water mark, discarding the corpus
    // generation above.
    PEAK_BYTES.store(LIVE_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);

    // The instant the resolution phase begins, so the wall-clock splits into the
    // extract+persist phase and the resolution phase.
    let resolving_at: Cell<Option<Instant>> = Cell::new(None);

    let started = Instant::now();
    let stats = index_project_reporting(&store, &project, "bench", &directory, |phase| {
        if matches!(phase, IndexPhase::Resolving) && resolving_at.get().is_none() {
            resolving_at.set(Some(Instant::now()));
        }
    })
    .expect("index");
    let elapsed = started.elapsed();

    let extract_secs = resolving_at.get().map(|at| at.duration_since(started).as_secs_f64());
    let resolve_secs = resolving_at.get().map(|at| at.elapsed().as_secs_f64());

    let peak_mb = PEAK_BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    let corpus_kb = (body.len() * files) as f64 / 1024.0;

    println!("files indexed    {}", stats.files_indexed);
    println!("nodes            {}", stats.nodes);
    println!("corpus on disk   {corpus_kb:.0} KiB ({files} files, repeat {repeat})");
    println!("peak heap        {peak_mb:.2} MiB");
    println!("elapsed          {:.3} s", elapsed.as_secs_f64());

    if let (Some(extract), Some(resolve)) = (extract_secs, resolve_secs) {
        println!("  extract+persist {extract:.3} s");
        println!("  resolve         {resolve:.3} s");
    }

    let _ = std::fs::remove_dir_all(&directory);
}
