use constellation_graph::Span;
use tree_sitter::Node as TsNode;

/// A node's source text, empty on the UTF-8-invalid path valid source
/// never takes.
pub(crate) fn node_text<'bytes>(bytes: &'bytes [u8], node: TsNode<'_>) -> &'bytes str {
    node.utf8_text(bytes).unwrap_or("")
}

/// A 1-based [`Span`] covering a tree node.
pub(crate) fn span_of(node: TsNode<'_>) -> Span {
    let start = node.start_position();
    let end = node.end_position();

    Span::new(
        line_1based(start.row),
        line_1based(end.row),
        to_u32(start.column),
        to_u32(end.column),
    )
}

/// A saturating `usize` -> `u32` cast; source positions fit well under the cap.
pub(crate) fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The 1-based line a 0-based tree-sitter row converts to.
pub(crate) fn line_1based(row: usize) -> u32 {
    let line = to_u32(row).saturating_add(1);

    assert!(line >= 1, "a 1-based line is at least one");

    line
}

#[cfg(test)]
mod tests {
    use super::{line_1based, to_u32};

    #[test]
    fn line_1based_shifts_zero_based_rows_up_by_one() {
        assert_eq!(line_1based(0), 1, "the first row is line one");
        assert_eq!(line_1based(41), 42, "row 41 is line 42");
    }

    #[test]
    fn line_1based_saturates_at_the_u32_ceiling() {
        assert_eq!(line_1based(usize::MAX), u32::MAX, "an enormous row saturates rather than wrapping past one");
    }

    #[test]
    fn to_u32_passes_small_values_and_saturates_large_ones() {
        assert_eq!(to_u32(0), 0);
        assert_eq!(to_u32(4_096), 4_096);
        assert_eq!(to_u32(u32::MAX as usize), u32::MAX, "the largest u32 round-trips");
        assert_eq!(to_u32(usize::MAX), u32::MAX, "an out-of-range usize saturates to the ceiling");
    }
}
