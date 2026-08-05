//! The scoring primitives every benchmark reports through.
//!
//! Deliberately plain: mean reciprocal rank, precision, recall, F1, and a
//! four-bytes-per-token approximation. The approximation is stated in every
//! report rather than hidden, because a token count that looks exact but is not
//! is worse than one that is openly approximate.

/// The bytes-per-token divisor used to approximate a token count.
///
/// English prose and code both land near four bytes per token for the
/// byte-pair encodings in use; nothing here calls a tokenizer, so every token
/// figure this harness reports is an approximation and the report says so.
pub const BYTES_PER_TOKEN: usize = 4;

/// The rank at which mean reciprocal rank is truncated. A result past twenty is
/// past what an agent reads.
pub const RECIPROCAL_RANK_CUTOFF: usize = 20;

/// The reciprocal of a one-based rank, zero when the expected answer did not
/// appear within [`RECIPROCAL_RANK_CUTOFF`].
pub fn reciprocal_rank(rank: Option<usize>) -> f64 {
    let Some(rank) = rank else {
        return 0.0;
    };

    if rank == 0 || rank > RECIPROCAL_RANK_CUTOFF {
        return 0.0;
    }

    let score = 1.0 / rank as f64;

    assert!(score >= 0.0, "a reciprocal rank is never negative");
    assert!(score <= 1.0, "a reciprocal rank never exceeds one");

    score
}

/// The mean of a set of reciprocal ranks, zero for an empty set.
pub fn mean_reciprocal_rank(ranks: &[Option<usize>]) -> f64 {
    if ranks.is_empty() {
        return 0.0;
    }

    let total: f64 = ranks.iter().copied().map(reciprocal_rank).sum();
    let mean = total / ranks.len() as f64;

    assert!(mean >= 0.0, "a mean reciprocal rank is never negative");
    assert!(mean <= 1.0, "a mean reciprocal rank never exceeds one");

    mean
}

/// The precision, recall, and F1 of a prediction against a ground-truth set.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Accuracy {
    pub f1: f64,
    pub precision: f64,
    pub recall: f64,
}

/// The accuracy of `predicted` against `expected`, both treated as sets.
pub fn accuracy(predicted_total: usize, expected_total: usize, overlap: usize) -> Accuracy {
    assert!(overlap <= predicted_total, "the overlap is a subset of the prediction");
    assert!(overlap <= expected_total, "the overlap is a subset of the ground truth");

    let precision = ratio(overlap, predicted_total);
    let recall = ratio(overlap, expected_total);

    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    assert!((0.0..=1.0).contains(&f1), "an F1 lands in 0..=1");

    Accuracy { f1, precision, recall }
}

/// A `numerator / denominator` ratio, zero when the denominator is zero.
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }

    let value = numerator as f64 / denominator as f64;

    assert!((0.0..=1.0).contains(&value), "a ratio of a subset lands in 0..=1");

    value
}

/// The approximate token count of a byte length, at [`BYTES_PER_TOKEN`].
pub fn approximate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

#[cfg(test)]
mod tests {
    use super::{
        RECIPROCAL_RANK_CUTOFF, accuracy, approximate_tokens, mean_reciprocal_rank, reciprocal_rank,
    };

    #[test]
    fn a_reciprocal_rank_stays_in_the_unit_range() {
        assert_eq!(reciprocal_rank(Some(1)), 1.0, "the top hit scores one");
        assert_eq!(reciprocal_rank(Some(2)), 0.5);
        assert_eq!(reciprocal_rank(None), 0.0, "a miss scores zero");
        assert_eq!(reciprocal_rank(Some(0)), 0.0, "rank is one-based; zero is not a rank");

        assert_eq!(
            reciprocal_rank(Some(RECIPROCAL_RANK_CUTOFF + 1)),
            0.0,
            "a hit past the cutoff is a miss, because an agent never reads that far",
        );
    }

    #[test]
    fn the_mean_averages_over_every_question_including_misses() {
        assert_eq!(mean_reciprocal_rank(&[]), 0.0, "an empty goldset scores zero");
        assert_eq!(mean_reciprocal_rank(&[Some(1), None]), 0.5, "a miss drags the mean down");
        assert_eq!(mean_reciprocal_rank(&[Some(1), Some(1)]), 1.0);
    }

    #[test]
    fn accuracy_reports_precision_recall_and_their_harmonic_mean() {
        let perfect = accuracy(4, 4, 4);

        assert_eq!(perfect.precision, 1.0);
        assert_eq!(perfect.recall, 1.0);
        assert_eq!(perfect.f1, 1.0);

        let half = accuracy(4, 2, 2);

        assert_eq!(half.precision, 0.5, "half the prediction was right");
        assert_eq!(half.recall, 1.0, "but it found everything");
        assert!((half.f1 - 2.0 / 3.0).abs() < 1e-9, "F1 is the harmonic mean, got {}", half.f1);
    }

    #[test]
    fn an_empty_prediction_scores_zero_rather_than_dividing_by_zero() {
        let nothing = accuracy(0, 5, 0);

        assert_eq!(nothing.precision, 0.0);
        assert_eq!(nothing.recall, 0.0);
        assert_eq!(nothing.f1, 0.0);
    }

    #[test]
    fn tokens_round_up_so_a_short_response_is_never_free() {
        assert_eq!(approximate_tokens(0), 0);
        assert_eq!(approximate_tokens(1), 1, "one byte still costs a token");
        assert_eq!(approximate_tokens(4), 1);
        assert_eq!(approximate_tokens(5), 2);
    }
}
