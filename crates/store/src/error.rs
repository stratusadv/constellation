use thiserror::Error;

/// Failures the store can surface. SQLite and clock errors convert in via
/// `?`; the remaining variants guard impossible-but-checked conditions.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("system clock error: {0}")]
    Clock(#[from] std::time::SystemTimeError),

    #[error("system clock overflowed i64 milliseconds")]
    ClockOverflow,

    #[error("corrupt schema version on disk: {0}")]
    CorruptSchemaVersion(i64),
}
