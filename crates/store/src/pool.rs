//! A small fixed set of read connections to one database.
//!
//! A SQLite `Connection` is `Send` but not `Sync`, so a server holding one
//! behind a mutex serializes every request that touches the graph, however
//! read-only they all are. The database is WAL and, at serve time, never
//! written by the server, so nothing about the workload requires that: the
//! constraint is the handle, not the data.
//!
//! This is the handle made plural. Reads are spread across a fixed set of
//! connections opened once at startup, each behind its own lock, so concurrent
//! tool calls contend only when they outnumber the pool. It is a pool in the
//! sizing sense, not the checkout sense: nothing is created or destroyed per
//! request, and there is no queue to starve.
//!
//! Every connection a pool opens is `PRAGMA query_only`, which turns "the
//! server only reads" from a convention into something SQLite enforces.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError};

use crate::error::StoreError;
use crate::store::Store;

/// The most read connections one pool opens.
///
/// Each carries its own page cache, so the pool's memory scales with this. An
/// MCP client issues a handful of concurrent tool calls at most, and past that
/// the bottleneck is the page cache the connections share through the OS rather
/// than the handles themselves.
pub const READERS_MAX: usize = 4;

/// A fixed set of read connections to one constellation database.
pub struct StorePool {
    readers: Vec<Mutex<Store>>,
    /// The rotation cursor, so consecutive reads start their search for a free
    /// connection at different places instead of all queueing behind the first.
    next: AtomicUsize,
}

impl StorePool {
    /// A pool of read connections to the database at `path`, sized to `readers`
    /// clamped into `1..=READERS_MAX`.
    ///
    /// The first connection opens through [`Store::open`], so the schema check
    /// and any rebuild happen exactly once and before the rest attach.
    pub fn open(path: &Path, readers: usize) -> Result<Self, StoreError> {
        assert!(!path.as_os_str().is_empty(), "store path must not be empty");

        let count = readers.clamp(1, READERS_MAX);

        let primary = Store::open(path)?;

        primary.set_query_only()?;

        let mut connections: Vec<Mutex<Store>> = Vec::with_capacity(count);

        connections.push(Mutex::new(primary));

        for _ in 1..count {
            let reader = Store::open_reader(path)?;

            connections.push(Mutex::new(reader));
        }

        assert!(connections.len() == count, "the pool opened the connections it was sized for");

        Ok(Self { readers: connections, next: AtomicUsize::new(0) })
    }

    /// A pool of exactly one connection, wrapping a store the caller already
    /// opened.
    ///
    /// For a caller holding a `Store` it built itself (a test fixture, the eval
    /// harness, an in-memory database). Unlike [`StorePool::open`] this does not
    /// make the connection read-only, because the store is not this pool's to
    /// restrict.
    pub fn single(store: Store) -> Self {
        Self { readers: vec![Mutex::new(store)], next: AtomicUsize::new(0) }
    }

    /// The number of connections this pool holds.
    pub fn readers(&self) -> usize {
        let count = self.readers.len();

        debug_assert!(count >= 1, "a pool always holds at least one connection");

        count
    }

    /// The result of `action` run against one of the pool's connections.
    ///
    /// Takes the first free connection, starting from the rotation cursor, and
    /// blocks on that cursor's own connection only when every one is busy. A
    /// connection whose previous holder panicked is recovered rather than
    /// skipped: the server catches handler panics and keeps serving, and a
    /// poisoned connection that no one may use again would shrink the pool by
    /// one for the life of the process.
    pub fn with_read<T>(
        &self,
        action: impl FnOnce(&Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let count = self.readers.len();

        assert!(count >= 1, "a pool always holds at least one connection");

        let start = self.next.fetch_add(1, Ordering::Relaxed);

        for offset in 0..count {
            let index = start.wrapping_add(offset) % count;

            match self.readers[index].try_lock() {
                Ok(reader) => return action(&reader),
                Err(TryLockError::Poisoned(poisoned)) => return action(&poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => continue,
            }
        }

        // Every connection is busy. Wait for the one the rotation picked, so
        // waiters spread across the pool rather than all piling onto the first.
        let index = start % count;

        let reader = self.readers[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        action(&reader)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use constellation_graph::ProjectId;

    use super::{READERS_MAX, StorePool};
    use crate::store::Store;

    /// A pool over a real file, seeded with one project.
    fn seeded_pool(readers: usize) -> (tempfile::TempDir, StorePool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.db");

        {
            let store = Store::open(&path).unwrap();

            store.upsert_project(&ProjectId::new("blog"), "blog", "/tmp/blog").unwrap();
        }

        let pool = StorePool::open(&path, readers).unwrap();

        (directory, pool)
    }

    #[test]
    fn a_pool_is_sized_within_its_bounds() {
        let (_directory, pool) = seeded_pool(0);

        assert_eq!(pool.readers(), 1, "a pool always holds at least one connection");

        let (_directory, pool) = seeded_pool(usize::MAX);

        assert_eq!(pool.readers(), READERS_MAX, "and never more than the cap");
    }

    #[test]
    fn every_connection_in_a_pool_sees_the_same_database() {
        let (_directory, pool) = seeded_pool(READERS_MAX);

        for read in 0..READERS_MAX * 3 {
            let projects = pool.with_read(|store| store.all_projects()).unwrap();

            assert_eq!(projects.len(), 1, "read {read} saw the seeded project");
            assert_eq!(projects[0].name, "blog");
        }
    }

    #[test]
    fn reads_are_spread_across_the_pool_rather_than_queued_on_one() {
        let (_directory, pool) = seeded_pool(READERS_MAX);
        let pool = Arc::new(pool);
        let concurrent = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        std::thread::scope(|scope| {
            for _ in 0..READERS_MAX {
                let pool = Arc::clone(&pool);
                let concurrent = Arc::clone(&concurrent);
                let peak = Arc::clone(&peak);

                scope.spawn(move || {
                    pool.with_read(|store| {
                        let live = concurrent.fetch_add(1, Ordering::SeqCst) + 1;

                        peak.fetch_max(live, Ordering::SeqCst);

                        // Hold the connection long enough that a single-handle
                        // pool would force the others to serialize behind it.
                        std::thread::sleep(std::time::Duration::from_millis(50));

                        let projects = store.all_projects();

                        concurrent.fetch_sub(1, Ordering::SeqCst);

                        projects
                    })
                    .unwrap();
                });
            }
        });

        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "a pool of {READERS_MAX} served only one read at a time",
        );
    }

    #[test]
    fn a_pooled_connection_refuses_to_write() {
        let (_directory, pool) = seeded_pool(1);

        let refused = pool.with_read(|store| {
            store.upsert_project(&ProjectId::new("other"), "other", "/tmp/other")
        });

        assert!(refused.is_err(), "a read pool is read-only, and SQLite enforces it");
    }

    #[test]
    fn a_panicking_read_does_not_retire_a_connection() {
        let (_directory, pool) = seeded_pool(1);

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.with_read(|_| -> Result<(), crate::StoreError> { panic!("a handler panicked") })
        }));

        assert!(panicked.is_err(), "the panic propagated to the caller");

        let projects = pool
            .with_read(|store| store.all_projects())
            .expect("the poisoned connection is recovered, not retired");

        assert_eq!(projects.len(), 1, "and still answers");
    }
}
