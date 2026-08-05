//! Reads against the store while the watcher re-indexes underneath them.
//!
//! This is the shape of a real MCP session: the agent queries continuously while
//! its own edits trigger re-indexing. The requirement is not that a query sees
//! any particular version of the graph, but that it never panics, never
//! deadlocks, and never escapes with a poisoned lock.
//!
//! As with the operations fuzzer, commit
//! `tests/fuzz_watcher_stress.proptest-regressions` whenever proptest writes it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::common::{Workspace, module_source};
use constellation_store::Store;

use proptest::prelude::*;

/// The reader threads run against the store.
const READERS: usize = 4;

/// The time the readers hammer the store while writes land.
///
/// Short on purpose. The bug class this hunts (a read racing a re-index) is
/// found by many distinct interleavings rather than by one long soak, so the
/// same wall-clock budget buys more when it is spent on more, shorter cases.
const STRESS_DURATION: Duration = Duration::from_millis(800);

/// The bound on reads one thread performs, so a pathologically fast machine
/// still terminates.
const READS_MAX: u32 = 1_000_000;

proptest! {
    // See STRESS_DURATION: more cases, each shorter, for the same wall clock.
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn concurrent_reads_survive_re_indexing(
        writes in 4_usize..24,
        files in 2_usize..10,
    ) {
        let workspace = Workspace::new("fuzz-stress");
        let _handle = workspace.watch();

        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU32::new(0));
        let mut readers = Vec::with_capacity(READERS);

        for _ in 0..READERS {
            let database = workspace.database.clone();
            let project = workspace.project.clone();
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);

            readers.push(std::thread::spawn(move || {
                // A fresh connection per reader, as the MCP server's handlers
                // effectively have: WAL lets them read while the watcher writes.
                let Ok(store) = Store::open(&database) else {
                    return;
                };

                while !stop.load(Ordering::SeqCst) && reads.load(Ordering::SeqCst) < READS_MAX {
                    // Every read is allowed to fail (a re-index may be mid
                    // transaction); none is allowed to panic or hang.
                    let _ = store.count_nodes(&project);
                    let _ = store.count_edges();
                    let _ = store.search_nodes("Model", 20);
                    let _ = store.files_for(&project);
                    let _ = store.all_projects();

                    reads.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        let started = Instant::now();
        let mut round: usize = 0;

        while started.elapsed() < STRESS_DURATION && round < writes {
            round += 1;

            for index in 0..files {
                workspace.write(
                    &format!("app/module{index}.py"),
                    &module_source(&format!("Model{round}x{index}")),
                );
            }

            std::thread::sleep(Duration::from_millis(60));
        }

        stop.store(true, Ordering::SeqCst);

        for reader in readers {
            prop_assert!(reader.join().is_ok(), "no reader panicked or poisoned a lock");
        }

        prop_assert!(reads.load(Ordering::SeqCst) > 0, "the readers actually ran");

        let converged = workspace.wait_for_convergence();

        prop_assert!(
            converged.is_converged(),
            "the store converged despite concurrent reads.\n{}",
            converged.describe(),
        );
    }
}
