/// The milliseconds since the Unix epoch, as the i64 SQLite stores. Shared by the
/// schema initializer and the write path so every persisted timestamp agrees.
///
/// An alias for [`constellation_graph::now_unix_millis`], which owns the rule
/// that a clock predating the epoch reads as zero rather than failing the call.
/// Re-exported under the store's own name so its write sites keep reading in
/// store vocabulary.
pub(crate) use constellation_graph::now_unix_millis as now_ms;
