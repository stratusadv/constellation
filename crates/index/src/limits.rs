//! Every bound the indexer runs under, in one place.
//!
//! Batch sizes, walk caps, and debounce windows. Together they answer "how
//! much work can one index do", which is a question worth being able to read
//! in one screen.

use std::time::Duration;



/// The fail-fast bound on the number of filesystem entries one walk may visit.
pub const FILE_COUNT_MAX: u32 = 5_000_000;

/// The ceiling on files held in flight per parallel batch, so a many-core machine
/// batches store writes without hoarding memory.
const EXTRACT_BATCH_MAX: usize = 256;

/// The floor on files per parallel batch, so even a single-core machine still
/// amortizes store writes rather than committing one file at a time.
const EXTRACT_BATCH_MIN: usize = 16;

/// The files held in flight per worker thread within a batch. Peak memory (a batch's
/// source and graphs, held until persisted) scales with this times the pool size.
const EXTRACT_BATCH_PER_THREAD: usize = 8;

/// The files to extract per parallel batch on this machine, scaled to the rayon
/// pool so peak memory tracks core count: a low-core, low-memory laptop keeps
/// little in flight while a many-core workstation keeps enough to stay busy
/// between store writes. The batch is the dominant control on extraction-phase
/// peak memory, so a smaller pool both does less work at once and holds less.
pub(crate) fn extract_batch_size() -> usize {
    let threads = rayon::current_num_threads().max(1);

    threads
        .saturating_mul(EXTRACT_BATCH_PER_THREAD)
        .clamp(EXTRACT_BATCH_MIN, EXTRACT_BATCH_MAX)
}

/// The fail-fast bound on references processed in one resolution pass.
pub(crate) const REFERENCE_COUNT_MAX: u32 = 50_000_000;

/// The project node count below which a bulk in-memory load for resolution is cheap
/// enough that per-query store lookups are not worth their overhead.
pub(crate) const RESOLVE_BULK_NODES_MIN: u32 = 50_000;

/// The per-query path is chosen only when nodes outnumber pending references
/// by at least this factor: the incremental case on a large project, where a
/// full node load would dominate. Otherwise the bulk path amortizes better.
pub(crate) const RESOLVE_INCREMENTAL_RATIO: u64 = 8;

/// The most dispatcher->listener pairs one event may synthesize. The edge model is
/// all-pairs (every dispatcher of an event to every listener's handler), so the
/// edge count for an event is dispatchers x listeners. Bounding that product,
/// rather than each side independently, links a high-traffic-but-focused bus
/// (10 dispatchers x 1 listener = 10 pairs) that a per-side cap would have dropped,
/// while still skipping a generic name (`change`, `click`) whose product explodes
/// into low-signal noise.
pub(crate) const EVENT_PAIRS_MAX: usize = 64;

/// The fail-fast bound on synthesized event edges produced for one project.
pub(crate) const SYNTHESIZED_EDGES_MAX: u32 = 1_000_000;

/// The quiet period after the last filesystem event before re-indexing.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(400);

/// The coalescing window the debouncer collects events into. Short, because the
/// debouncer already merges rename pairs and directory-creation bursts that a
/// raw channel does not; the longer [`DEBOUNCE`] quiet period then applies on
/// top of it, in the watch loop.
pub(crate) const DEBOUNCE_WINDOW_MS: u64 = 50;

/// The paths accumulated across one coalescing window before the watcher stops
/// tracking them individually and schedules a whole-constellation refresh
/// instead. A `git checkout`, a rebase, or a formatter pass rewrites thousands
/// of files; past this bound the per-path bookkeeping costs more than the
/// refresh it was meant to narrow.
pub(crate) const PENDING_EVENTS_MAX: usize = 4_096;

/// The subtrees re-registered with the watcher in one burst. Recursive watch
/// registration is not uniformly reliable for directories created after
/// registration, so a directory-create event re-registers that subtree; this
/// bounds how much of that one burst may trigger.
pub(crate) const WATCH_REREGISTER_MAX: u32 = 256;

/// The idle tick the watch loop wakes on when no filesystem event arrives, so
/// the git-status snapshot still refreshes against its own time-to-live.
pub(crate) const WATCH_IDLE_TICK: Duration = Duration::from_millis(1_000);

/// The fail-fast bound on signals drained while coalescing one burst.
pub(crate) const DEBOUNCE_EVENTS_MAX: u32 = 5_000_000;

/// The fail-fast bound on how far the include tree is walked building one route's
/// namespace chain, far past any real URL nesting depth.
pub(crate) const NAMESPACE_DEPTH_MAX: u32 = 32;

/// The fail-fast bound on the ancestor classes one override search walks, far past
/// any real inheritance hierarchy, so the search is provably finite even on a
/// malformed or cyclic `extends` graph.
pub(crate) const OVERRIDE_WALK_MAX: u32 = 1_000_000;

/// The fail-fast bound on the reverse render/include walk from one accessed template
/// up to the views that render it, far past any real template nesting depth.
pub(crate) const TEMPLATE_VIEW_WALK_MAX: u32 = 1_000_000;

/// The fail-fast bound on the inheritance-chain walk one member lookup makes.
pub(crate) const MEMBER_CHAIN_WALK_MAX: u32 = 1_000_000;
