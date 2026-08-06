//! Opaque page cursors for the listing tools.
//!
//! Truncating with "(+N more; narrow the pattern)" assumes narrowing is
//! possible. Often it is not: a route map with four hundred routes, an impact
//! set with three hundred callers, and a `files` listing for a large app are all
//! legitimately that size, and the tail is what the agent needs next.
//!
//! The cursor is stateless and carries the server's generation counter, so a
//! page taken before a mid-session re-index cannot be silently continued against
//! a shifted result set. When the generation has moved on, the cursor is
//! reported expired and the first page is returned instead, which is wrong in a
//! way the agent can see rather than wrong in a way it cannot.
//!
//! The encoding is `<offset>.<generation>` rather than base64: it needs no
//! dependency, it survives a round trip through any transport, and a human
//! debugging a paging bug can read it.

use std::fmt;

/// The separator between the two fields of an encoded cursor.
const CURSOR_SEPARATOR: char = '.';

/// The fail-fast bound on a decoded offset. Every paginated tool caps its own
/// result set far below this; the bound exists so a hand-written or corrupted
/// cursor cannot ask for an absurd skip.
pub const CURSOR_OFFSET_MAX: usize = 1_000_000;

/// A decoded page cursor: where to resume, and the index generation it was
/// issued against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub generation: u64,
    pub offset: usize,
}

/// The reasons a cursor could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// The offset or generation was not a number.
    Malformed,
    /// The offset exceeded [`CURSOR_OFFSET_MAX`].
    OffsetTooLarge,
    /// The two fields were not separated by a single separator.
    WrongShape,
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            CursorError::Malformed => "its offset or generation is not a number",
            CursorError::OffsetTooLarge => "its offset is implausibly large",
            CursorError::WrongShape => "it is not of the form <offset>.<generation>",
        };

        formatter.write_str(message)
    }
}

/// The cursor encoded for the agent to pass back.
pub fn encode(offset: usize, generation: u64) -> String {
    assert!(offset <= CURSOR_OFFSET_MAX, "an issued cursor stays within its offset bound");

    format!("{offset}{CURSOR_SEPARATOR}{generation}")
}

/// The cursor decoded, or the reason it could not be.
pub fn decode(text: &str) -> Result<Cursor, CursorError> {
    let trimmed = text.trim();

    let Some((offset, generation)) = trimmed.split_once(CURSOR_SEPARATOR) else {
        return Err(CursorError::WrongShape);
    };

    if generation.contains(CURSOR_SEPARATOR) {
        return Err(CursorError::WrongShape);
    }

    let offset: usize = offset.parse().map_err(|_| CursorError::Malformed)?;
    let generation: u64 = generation.parse().map_err(|_| CursorError::Malformed)?;

    if offset > CURSOR_OFFSET_MAX {
        return Err(CursorError::OffsetTooLarge);
    }

    Ok(Cursor { generation, offset })
}

/// The offset one tool call should start at, and what to tell the agent when the
/// requested cursor could not be honoured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// A note to prepend to the response, or `None` when the cursor was honoured
    /// (or none was passed).
    pub note: Option<String>,
    /// The zero-based offset into the full result set.
    pub offset: usize,
}

/// The page a cursor argument resolves to against the current generation. Every
/// failure path restarts from the first page *and says so*: silently returning
/// page one for an expired cursor would let an agent believe it had paged
/// through a set it had only seen the head of.
pub fn resolve(cursor: Option<&str>, generation: u64) -> Page {
    let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) else {
        return Page::default();
    };

    match decode(cursor) {
        Ok(decoded) if decoded.generation == generation => {
            Page { note: None, offset: decoded.offset }
        }
        Ok(_) => Page {
            note: Some(
                "cursor expired (the index changed since it was issued); \
                 showing the first page, re-run without cursor to page again"
                    .to_string(),
            ),
            offset: 0,
        },
        Err(error) => Page {
            note: Some(format!("cursor {cursor:?} rejected: {error}; showing the first page")),
            offset: 0,
        },
    }
}

/// The `next: cursor=<value>` line for a truncated response, or `None` when the
/// page reached the end of the result set.
pub fn next_line(offset: usize, shown: usize, total: usize, generation: u64) -> Option<String> {
    let consumed = offset.saturating_add(shown);

    if consumed >= total || consumed > CURSOR_OFFSET_MAX {
        return None;
    }

    assert!(consumed > offset || shown == 0, "a next page always advances the offset");

    Some(format!(
        "next: cursor={} ({} of {total} shown)",
        encode(consumed, generation),
        consumed,
    ))
}

/// The slice of `items` this page covers, empty when the offset runs past the
/// end (which a stale-but-parsable cursor can produce).
pub fn slice<T>(items: &[T], offset: usize, limit: usize) -> &[T] {
    if offset >= items.len() {
        return &[];
    }

    let end = offset.saturating_add(limit).min(items.len());

    assert!(offset <= end, "a page never ends before it starts");

    &items[offset..end]
}

#[cfg(test)]
mod tests {
    use super::{CURSOR_OFFSET_MAX, Cursor, CursorError, decode, encode, next_line, resolve, slice};

    #[test]
    fn a_cursor_round_trips() {
        let encoded = encode(120, 7);

        assert_eq!(encoded, "120.7", "the encoding stays human-readable");
        assert_eq!(decode(&encoded), Ok(Cursor { generation: 7, offset: 120 }));
    }

    #[test]
    fn a_malformed_cursor_is_rejected_with_a_reason() {
        assert_eq!(decode("120"), Err(CursorError::WrongShape));
        assert_eq!(decode("120.7.3"), Err(CursorError::WrongShape));
        assert_eq!(decode("abc.7"), Err(CursorError::Malformed));
        assert_eq!(decode("120.xyz"), Err(CursorError::Malformed));
        assert_eq!(decode(&format!("{}.1", CURSOR_OFFSET_MAX + 1)), Err(CursorError::OffsetTooLarge));
    }

    #[test]
    fn a_matching_generation_resumes_where_it_left_off() {
        let page = resolve(Some("120.7"), 7);

        assert_eq!(page.offset, 120, "the page resumes at the cursor's offset");
        assert!(page.note.is_none(), "an honoured cursor needs no explanation");
    }

    #[test]
    fn a_stale_cursor_restarts_and_says_so() {
        let page = resolve(Some("120.7"), 8);

        assert_eq!(page.offset, 0, "a stale cursor restarts rather than paging a shifted set");

        assert!(
            page.note.as_deref().is_some_and(|note| note.contains("expired")),
            "and the restart is reported, never silent: {:?}",
            page.note,
        );
    }

    #[test]
    fn an_unparsable_cursor_restarts_and_names_the_problem() {
        let page = resolve(Some("garbage"), 3);

        assert_eq!(page.offset, 0);

        assert!(
            page.note.as_deref().is_some_and(|note| note.contains("rejected")),
            "the rejection is explained: {:?}",
            page.note,
        );
    }

    #[test]
    fn no_cursor_is_the_first_page_with_no_note() {
        assert_eq!(resolve(None, 4), super::Page::default());
        assert_eq!(resolve(Some("  "), 4), super::Page::default(), "blank is the same as absent");
    }

    #[test]
    fn paging_through_a_set_covers_every_item_exactly_once() {
        let items: Vec<usize> = (0..300).collect();
        let generation = 11;
        let limit = 40;

        let mut seen: Vec<usize> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages: u32 = 0;

        loop {
            pages += 1;

            assert!(pages < 32, "paging terminates well inside a sane bound");

            let page = resolve(cursor.as_deref(), generation);
            let window = slice(&items, page.offset, limit);

            seen.extend_from_slice(window);

            match next_line(page.offset, window.len(), items.len(), generation) {
                Some(line) => {
                    let value = line
                        .split("cursor=")
                        .nth(1)
                        .and_then(|rest| rest.split(' ').next())
                        .expect("the next line carries a cursor value");

                    cursor = Some(value.to_string());
                }
                None => break,
            }
        }

        assert_eq!(seen, items, "every item appears exactly once, in order");
    }

    #[test]
    fn the_last_page_offers_no_next_cursor() {
        assert!(next_line(280, 20, 300, 5).is_none(), "the set is exhausted");
        assert!(next_line(0, 300, 300, 5).is_none(), "one page covering everything is the last");
        assert!(next_line(0, 40, 300, 5).is_some(), "a partial page offers the rest");
    }

    #[test]
    fn an_offset_past_the_end_yields_an_empty_page_rather_than_panicking() {
        let items: Vec<usize> = (0..10).collect();

        assert!(slice(&items, 50, 10).is_empty(), "a runaway offset is empty, not a panic");
        assert_eq!(slice(&items, 8, 10).len(), 2, "a partial tail returns what is left");
    }
}
