use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::StoreError;

/// Milliseconds since the Unix epoch, as the i64 SQLite stores. Shared by the
/// migration runner and the write path so every persisted timestamp agrees.
pub(crate) fn now_ms() -> Result<i64, StoreError> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();

    let millis = i64::try_from(millis).map_err(|_| StoreError::ClockOverflow)?;

    assert!(millis > 0, "current time must be after the unix epoch");

    Ok(millis)
}
