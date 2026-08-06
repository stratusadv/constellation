//! Fitting text to a byte budget.
//!
//! Every response this server produces is charged against a budget, and so is
//! the `PreToolUse` hook's context blurb in `cli`. Both need the same thing:
//! cut to a byte count without splitting a character in half. It lives here,
//! and `cli` reaches it through this crate, because two copies of a truncation
//! rule is two places for an off-by-one to hide.

use std::borrow::Cow;

/// The ellipsis appended to text that was cut, so a reader can tell a truncated
/// value from a short one.
pub const ELLIPSIS: char = '\u{2026}';

/// `text` fitted to at most `budget` bytes, cut on a UTF-8 character boundary
/// and marked with an ellipsis when anything was dropped.
///
/// The budget covers the *result*, marker included. The two copies this
/// replaced disagreed here: one appended a marker on top of the budget, so
/// every truncated explore snippet overran the budget it had just been charged
/// against, and the other dropped the marker entirely, so a cut hook blurb read
/// as a complete one.
///
/// Borrows when the text already fits, which is the common case: an explore
/// snippet is charged against a budget far larger than most symbols, and
/// copying every one of them to discover they fit is a copy per rendered
/// symbol.
pub fn truncate_at_boundary(text: &str, budget: usize) -> Cow<'_, str> {
    if text.len() <= budget {
        return Cow::Borrowed(text);
    }

    // Reserve the marker's own bytes, so the marked result still fits. A budget
    // too small to hold even the marker yields an empty string rather than one
    // that overruns it.
    let mut end = budget.saturating_sub(ELLIPSIS.len_utf8());

    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    assert!(text.is_char_boundary(end), "truncation lands on a char boundary");

    let marked = end + ELLIPSIS.len_utf8() <= budget;

    let mut cut = String::with_capacity(if marked { budget } else { end });

    cut.push_str(&text[..end]);

    if marked {
        cut.push(ELLIPSIS);
    }

    assert!(cut.len() <= budget, "a fitted value never exceeds its budget");

    Cow::Owned(cut)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{ELLIPSIS, truncate_at_boundary};

    #[test]
    fn text_within_budget_is_borrowed_whole() {
        let fitted = truncate_at_boundary("models.py", 64);

        assert!(matches!(fitted, Cow::Borrowed(_)), "a fitting value is not copied");
        assert_eq!(fitted, "models.py", "and comes back unchanged");
    }

    #[test]
    fn text_exactly_at_budget_is_left_alone() {
        let fitted = truncate_at_boundary("abcd", 4);

        assert!(matches!(fitted, Cow::Borrowed(_)), "the budget is inclusive");
        assert_eq!(fitted, "abcd");
    }

    #[test]
    fn overlong_text_is_cut_and_marked_within_the_budget() {
        let fitted = truncate_at_boundary("abcdefgh", 6);

        assert_eq!(fitted, format!("abc{ELLIPSIS}"), "the cut is marked, never silent");
        assert!(fitted.len() <= 6, "and the marker is charged against the budget");
    }

    #[test]
    fn a_cut_never_splits_a_character_and_never_overruns() {
        // Two-byte characters throughout, so most budgets land mid-character and
        // the cut must retreat rather than produce invalid UTF-8.
        let text = "ααααα";

        for budget in 0..=text.len() {
            let fitted = truncate_at_boundary(text, budget);
            let body = fitted.strip_suffix(ELLIPSIS).unwrap_or(&fitted);

            assert!(fitted.len() <= budget, "budget {budget} respected by {fitted:?}");
            assert!(text.starts_with(body), "the kept part is a prefix of the original");
        }
    }

    #[test]
    fn a_budget_too_small_for_the_marker_yields_an_empty_string() {
        assert_eq!(truncate_at_boundary("abc", 0), "", "never a value over budget");
        assert_eq!(truncate_at_boundary("abc", 1), "", "the marker alone is three bytes");
    }
}
