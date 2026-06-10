//! Wall-clock harness for the Tier-2 symbol-history pass (`history --symbols`),
//! the slowest git path: it reads every revision of every touched file and parses
//! it. Builds a throwaway git repo of `commits` commits each rewriting `files`
//! Python modules, ingests Tier-1 history, then times the Tier-2 symbol diff.
//!
//! Run with `cargo run --release --example history_bench -p constellation-index`.
//! Optional args: commits, files, repeat-per-file:
//! `... --example history_bench -- 200 8 4`. Use RAYON_NUM_THREADS to simulate a
//! weak laptop.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use constellation_graph::ProjectId;
use constellation_index::{ingest_history, ingest_symbol_revisions};
use constellation_store::Store;

/// One Python module's source at revision `rev`, sized by `repeat`. Each revision
/// changes a field default, a method body, and adds a revision-numbered method, so
/// consecutive revisions differ and the diff produces added/modified rows.
fn module(file_index: usize, rev: usize, repeat: usize) -> String {
    let mut body = format!(
        "from django.db import models\n\n\nclass Model{file_index}(models.Model):\n    \
         name = models.CharField(max_length={max_length})\n    \
         revision = models.IntegerField(default={rev})\n\n    \
         def compute(self, value: int) -> int:\n        return value * {rev}\n\n    \
         def method_{rev}(self) -> str:\n        return \"r{rev}\"\n",
        max_length = 100 + rev % 50,
    );

    // Pad the module so each parse does realistic work, like a real models.py.
    for block in 0..repeat {
        body.push_str(&format!(
            "\n\nclass Helper{file_index}_{block}:\n    \
             label = \"h{block}\"\n\n    \
             def run(self, items: list) -> int:\n        return len(items) + {rev}\n",
        ));
    }

    body
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("run git");

    assert!(status.success(), "git {args:?} failed");
}

fn build_repo(root: &Path, commits: usize, files: usize, repeat: usize) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).expect("create repo dir");

    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "bench@example.com"]);
    git(root, &["config", "user.name", "bench"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    for rev in 0..commits {
        for file_index in 0..files {
            let path = root.join(format!("app/models_{file_index}.py"));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create app dir");
            std::fs::write(&path, module(file_index, rev, repeat)).expect("write module");
        }

        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", &format!("rev {rev}")]);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);

    let commits: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(200);
    let files: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(8);
    let repeat: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(4);

    assert!(commits > 0 && files > 0 && repeat > 0, "counts must be positive");

    let root = std::env::temp_dir().join(format!("constellation_history_bench_{commits}_{files}"));

    eprintln!("building {commits}-commit repo over {files} files ...");
    build_repo(&root, commits, files, repeat);

    let store = Store::open(&root.join("graph.db")).expect("open store");
    let project = ProjectId::new("bench");

    store
        .upsert_project(&project, "bench", &root.to_string_lossy())
        .expect("upsert project");

    // Tier 1: populate git_commit / git_commit_file (the touch map Tier 2 reads).
    ingest_history(&store, &project, &root, 20_000).expect("ingest history");

    let started = Instant::now();
    let rows = ingest_symbol_revisions(&store, &project, &root).expect("symbol revisions");
    let elapsed = started.elapsed();

    let touches = commits * files;

    println!("commits {commits}, files {files}, touches {touches}");
    println!("symbol rows      {rows}");
    println!("tier-2 elapsed   {:.3} s", elapsed.as_secs_f64());
    println!("per touch        {:.3} ms", elapsed.as_secs_f64() * 1000.0 / touches as f64);

    let _ = std::fs::remove_dir_all(&root);
}
