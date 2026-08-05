//! Calendar arithmetic for the history tools.
//!
//! Hand-rolled rather than pulled from a date crate: the whole requirement is
//! converting a `YYYY-MM-DD` an agent typed to a Unix timestamp and back, and
//! a dependency for that would cost more than it saves.

/// The UTC epoch seconds at the start of a "YYYY-MM-DD" date, or `None` when the
/// string is not exactly such a date.
pub(crate) fn parse_ymd_to_epoch(text: &str) -> Option<i64> {
    let mut parts = text.split('-');

    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    Some(epoch_secs_from_ymd(year, month, day))
}

/// The UTC epoch seconds at midnight of a civil date, by Howard Hinnant's
/// days-from-civil algorithm (the inverse of [`ymd_from_epoch_secs`]).
fn epoch_secs_from_ymd(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_position = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_position + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    (era * 146_097 + day_of_era - 719_468) * 86_400
}

/// The civil date (year, month, day) for `epoch_secs` UTC, by Howard Hinnant's
/// days-to-civil algorithm. Stamps history timelines with absolute dates without
/// a date-library dependency.
pub(crate) fn ymd_from_epoch_secs(epoch_secs: i64) -> (i64, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 { month_position + 3 } else { month_position - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    assert!((1..=12).contains(&month), "month falls in 1..=12");
    assert!((1..=31).contains(&day), "day falls in 1..=31");

    (year, month as u32, day as u32)
}

#[cfg(test)]
mod history_date_tests {
    use super::{epoch_secs_from_ymd, parse_ymd_to_epoch, ymd_from_epoch_secs};

    #[test]
    fn ymd_matches_known_utc_dates() {
        assert_eq!(ymd_from_epoch_secs(0), (1970, 1, 1));
        assert_eq!(ymd_from_epoch_secs(86_400), (1970, 1, 2));
        assert_eq!(ymd_from_epoch_secs(1_700_000_000), (2023, 11, 14));
    }

    #[test]
    fn epoch_from_ymd_is_midnight_and_inverts_ymd() {
        assert_eq!(epoch_secs_from_ymd(1970, 1, 1), 0);
        assert_eq!(epoch_secs_from_ymd(1970, 1, 2), 86_400);

        for &(year, month, day) in &[(1970, 1, 1), (1999, 12, 31), (2023, 11, 14), (2024, 2, 29)] {
            let midnight = epoch_secs_from_ymd(year, month, day);

            assert_eq!(ymd_from_epoch_secs(midnight), (year, month as u32, day as u32));
        }
    }

    #[test]
    fn parse_ymd_accepts_dates_and_rejects_hashes() {
        assert_eq!(parse_ymd_to_epoch("1970-01-01"), Some(0));
        assert_eq!(parse_ymd_to_epoch("2023-06-15"), Some(epoch_secs_from_ymd(2023, 6, 15)));
        assert_eq!(parse_ymd_to_epoch("deadbeef"), None);
        assert_eq!(parse_ymd_to_epoch("2023-13-01"), None);
        assert_eq!(parse_ymd_to_epoch("2023-06-15-7"), None);
    }
}
