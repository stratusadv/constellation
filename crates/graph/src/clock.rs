//! Reading the wall clock, in the one form every layer stores it in.
//!
//! A [`crate::Node`] carries `updated_at_ms`, history carries epoch seconds, and
//! the store stamps epoch milliseconds, so "what time is it, as this graph
//! counts time" is graph vocabulary rather than an I/O concern. It lives here
//! because three layers previously answered it three different ways: one
//! returned a `Result`, two saturated, and they disagreed about what a clock
//! set before 1970 means.
//!
//! Both readers saturate rather than fail. A clock that predates the epoch is a
//! broken machine, and the honest response is a zero timestamp, which makes
//! every window read as empty and every snapshot as expired. Failing the call
//! instead would turn a wrong clock into a failed index.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time in milliseconds since the Unix epoch, or zero for a clock
/// that predates it.
pub fn now_unix_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX));

    debug_assert!(millis >= 0, "an epoch timestamp is never negative");

    millis
}

/// The current time in seconds since the Unix epoch, or zero for a clock that
/// predates it.
pub fn now_unix_secs() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX));

    debug_assert!(seconds >= 0, "an epoch timestamp is never negative");

    seconds
}

#[cfg(test)]
mod tests {
    use super::{now_unix_millis, now_unix_secs};

    /// The seconds from the epoch to 2020-01-01, comfortably in the past of any
    /// machine that can build this.
    const YEAR_2020_SECS: i64 = 1_577_836_800;

    #[test]
    fn both_readers_agree_on_the_same_instant() {
        let seconds = now_unix_secs();
        let millis = now_unix_millis();

        assert!(seconds > YEAR_2020_SECS, "the clock reads a plausible present");

        assert!(
            (millis / 1_000 - seconds).abs() <= 1,
            "the two readers describe one clock: {millis}ms vs {seconds}s",
        );
    }

    #[test]
    fn time_does_not_run_backwards_between_reads() {
        let first = now_unix_millis();
        let second = now_unix_millis();

        assert!(second >= first, "a later read is never earlier");
    }
}
