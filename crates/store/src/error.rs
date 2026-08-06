//! What the store can fail with, and the row budget that produces the one
//! failure that is not SQLite's.

use thiserror::Error;

/// The failures the store can surface. SQLite errors convert in via `?`;
/// [`StoreError::RowLimit`] is the store's own, raised when a read runs past
/// the bound it was designed for.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("{what} exceeded its bound of {limit} rows")]
    RowLimit { what: &'static str, limit: u32 },
}

/// A row charged against a read's row budget, failing the read once the cap
/// is passed.
///
/// A cap reached is not a programmer error, so it is not an assertion. It means
/// the database is larger than this read was designed for, and the caller is
/// usually a server that must still answer the *next* request. A named error
/// becomes a tool response naming the bound that was hit; the assertion it
/// replaces became a caught panic and a generic "internal error" with the limit
/// nowhere in it, which is the same failure the tools are careful never to
/// produce for a truncated listing.
pub(crate) fn charge(seen: &mut u32, limit: u32, what: &'static str) -> Result<(), StoreError> {
    *seen = seen.saturating_add(1);

    if *seen > limit {
        return Err(StoreError::RowLimit { what, limit });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{StoreError, charge};

    #[test]
    fn a_budget_admits_rows_up_to_its_limit_then_fails() {
        let mut seen: u32 = 0;

        for row in 1..=3 {
            assert!(charge(&mut seen, 3, "test load").is_ok(), "row {row} is within the budget");
        }

        let error = charge(&mut seen, 3, "test load").expect_err("the fourth row exceeds it");

        assert!(
            matches!(error, StoreError::RowLimit { what: "test load", limit: 3 }),
            "the failure names the bound and what hit it: {error}",
        );
    }

    #[test]
    fn a_budget_reports_the_limit_in_its_message() {
        let message = StoreError::RowLimit { what: "node load", limit: 50 }.to_string();

        assert!(message.contains("node load"), "the message names the read: {message}");
        assert!(message.contains("50"), "and the bound it passed: {message}");
    }
}
