//! Recency as a tie-breaker inside a relevance band.
//!
//! `explore`'s own tool description concedes the weakness it is built around:
//! "avoid generic words like `inventory`/`form_views` that match dozens of
//! files." When a query matches forty symbols, inverse document frequency has
//! nothing left to separate them with. Recency does.
//!
//! # Why the constants are not the human ones
//!
//! The mechanism is borrowed from frecency-ranked file finders, but constellation's
//! only consumer is an agent, and an agent session is short and intense where a
//! human's editing history is long and diffuse. So the decay is an order of
//! magnitude faster: a three-day half life on commit history rather than weeks,
//! and a ten-minute half life within a session. The same reasoning produces the
//! per-file cooldown: without it, an agent making twelve edits to one file in
//! two minutes would pin that file to the top of every subsequent query, which
//! is exactly the file it has already read.
//!
//! # Where it does not apply
//!
//! Recency is never a top-level sort key. A recently touched but irrelevant file
//! would then outrank the on-the-nose match, which is a strictly worse answer.
//! It enters as a bonus to the inverse-document-frequency score, capped at
//! [`RECENCY_BONUS_FRACTION`] of the median per-token weight in the candidate
//! set, so it breaks ties inside a relevance band and does nothing across bands.

use std::collections::VecDeque;

use constellation_index::WorkingTreeState;
use rustc_hash::FxHashMap;

/// The session-touch weight of a file dirty in the working tree. The strongest
/// of the three: "this file is modified right now" is usually the task.
const DIRTY_WEIGHT: f64 = 0.5;

/// The weight of a file this session has already surfaced.
const SESSION_WEIGHT: f64 = 0.3;

/// The weight of a file recently committed to.
const COMMIT_WEIGHT: f64 = 0.2;

/// The most files a session remembers. Bounded because the session map lives for
/// the whole server process; the oldest entry is evicted, so the memory is
/// constant regardless of session length.
pub const SESSION_FILES_MAX: usize = 256;

/// The per-file cooldown, in seconds. A second touch inside this window does not
/// refresh the file's recency, so a burst of edits to one file cannot inflate
/// its score above a file touched once but more recently in intent.
pub const SESSION_COOLDOWN_SECS: i64 = 300;

/// The session half life, in seconds. A file surfaced ten minutes ago carries
/// half the weight of one surfaced now.
pub const SESSION_HALF_LIFE_SECS: f64 = 600.0;

/// The commit-history decay constant, per day. `ln(2) / 0.231` is three days, so
/// a commit three days old carries half the weight of one from today.
pub const DECAY_CONSTANT: f64 = 0.231;

/// The commit-history window, in days. A file whose last commit predates this
/// contributes nothing, so an old, stable file is not perpetually boosted for
/// having once been busy.
pub const HISTORY_WINDOW_DAYS: i64 = 7;

/// The fraction of the median per-token relevance weight that a maximally recent
/// file may gain. Deliberately well under one: the bonus must not lift a file
/// out of its relevance band, only reorder within it.
pub const RECENCY_BONUS_FRACTION: f64 = 0.25;

/// The seconds in one day.
const SECONDS_PER_DAY: f64 = 86_400.0;

/// The files this session has surfaced, with the time each was last counted.
///
/// Bounded and in memory only: a session's attention is not a fact about the
/// codebase, so it is never persisted, and a restart correctly forgets it.
#[derive(Debug, Default)]
pub struct SessionFiles {
    order: VecDeque<String>,
    touched_at: FxHashMap<String, i64>,
}

impl SessionFiles {
    /// An empty session memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// A file recorded as surfaced at `now_secs`, returning whether the touch
    /// counted. A touch inside [`SESSION_COOLDOWN_SECS`] of the previous one is
    /// ignored, which is the whole point of the cooldown.
    pub fn touch(&mut self, key: &str, now_secs: i64) -> bool {
        if let Some(&previous) = self.touched_at.get(key)
            && now_secs.saturating_sub(previous) < SESSION_COOLDOWN_SECS
        {
            return false;
        }

        if self.touched_at.insert(key.to_string(), now_secs).is_none() {
            self.order.push_back(key.to_string());
        }

        while self.order.len() > SESSION_FILES_MAX {
            if let Some(evicted) = self.order.pop_front() {
                self.touched_at.remove(&evicted);
            }
        }

        assert!(self.order.len() <= SESSION_FILES_MAX, "the session memory stays bounded");
        assert!(self.touched_at.len() <= SESSION_FILES_MAX, "the touch map matches the order queue");

        true
    }

    /// The `0.0..=1.0` session score of one file: one when just surfaced,
    /// halving every [`SESSION_HALF_LIFE_SECS`], zero when never surfaced.
    pub fn score(&self, key: &str, now_secs: i64) -> f64 {
        let Some(&touched) = self.touched_at.get(key) else {
            return 0.0;
        };

        let age = now_secs.saturating_sub(touched).max(0) as f64;
        let score = (-std::f64::consts::LN_2 * age / SESSION_HALF_LIFE_SECS).exp();

        clamp_unit(score)
    }

    /// The number of files remembered.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether nothing has been surfaced yet.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// The `0.0..=1.0` commit-recency score of a file from the epoch seconds of the
/// most recent commit touching it. Zero outside [`HISTORY_WINDOW_DAYS`], and
/// zero when no commit is recorded.
pub fn commit_recency(last_commit_secs: Option<i64>, now_secs: i64) -> f64 {
    let Some(last_commit) = last_commit_secs else {
        return 0.0;
    };

    let age_secs = now_secs.saturating_sub(last_commit);

    if age_secs < 0 || age_secs > HISTORY_WINDOW_DAYS.saturating_mul(86_400) {
        return 0.0;
    }

    let age_days = age_secs as f64 / SECONDS_PER_DAY;

    clamp_unit((-DECAY_CONSTANT * age_days).exp())
}

/// The three recency signals blended into one `0.0..=1.0` score for a file.
pub fn file_recency(state: WorkingTreeState, session: f64, commit: f64) -> f64 {
    assert!((0.0..=1.0).contains(&session), "a session score lands in 0..=1");
    assert!((0.0..=1.0).contains(&commit), "a commit score lands in 0..=1");

    let recency = state.recency_weight() * DIRTY_WEIGHT + session * SESSION_WEIGHT + commit * COMMIT_WEIGHT;
    let recency = clamp_unit(recency);

    assert!(recency >= 0.0, "a recency score is never negative");
    assert!(recency <= 1.0, "a recency score never exceeds one");

    recency
}

/// The bonus a file's recency earns against the candidate set's median
/// per-token weight. Returns zero for an empty token set, so a query that
/// tokenizes to nothing cannot be reordered by recency alone.
pub fn recency_bonus(token_weight_median: u64, recency: f64) -> u64 {
    assert!((0.0..=1.0).contains(&recency), "a recency score lands in 0..=1");

    let cap = token_weight_median as f64 * RECENCY_BONUS_FRACTION;
    let bonus = (cap * recency).round();

    if bonus <= 0.0 {
        return 0;
    }

    let bonus = bonus as u64;

    assert!(
        bonus <= token_weight_median.max(1),
        "the bonus never reaches a whole token's weight",
    );

    bonus
}

/// The median of a set of per-token relevance weights, zero when empty. The
/// median rather than the mean, so one very rare token (whose weight can be
/// orders of magnitude above the rest) does not set the recency ceiling for the
/// whole query.
pub fn median_weight(weights: &mut [u64]) -> u64 {
    if weights.is_empty() {
        return 0;
    }

    weights.sort_unstable();

    let middle = weights.len() / 2;

    if weights.len() % 2 == 1 {
        return weights[middle];
    }

    weights[middle - 1].midpoint(weights[middle])
}

/// A score clamped into the unit range, guarding against a floating-point
/// result landing a hair outside it.
fn clamp_unit(value: f64) -> f64 {
    if value.is_nan() {
        return 0.0;
    }

    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{
        HISTORY_WINDOW_DAYS, RECENCY_BONUS_FRACTION, SESSION_COOLDOWN_SECS, SESSION_FILES_MAX,
        SessionFiles, commit_recency, file_recency, median_weight, recency_bonus,
    };

    use constellation_index::WorkingTreeState;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_dirty_file_outranks_an_equally_scored_clean_one() {
        let dirty = file_recency(WorkingTreeState::Modified, 0.0, 0.0);
        let clean = file_recency(WorkingTreeState::Clean, 0.0, 0.0);

        assert!(dirty > clean, "working-tree state is the strongest signal: {dirty} vs {clean}");
    }

    #[test]
    fn every_signal_contributes_and_the_blend_stays_in_range() {
        let none = file_recency(WorkingTreeState::Clean, 0.0, 0.0);
        let all = file_recency(WorkingTreeState::Modified, 1.0, 1.0);

        assert_eq!(none, 0.0, "a cold file scores zero");
        assert!((all - 1.0).abs() < 1e-9, "a maximally recent file scores one, got {all}");

        assert!(file_recency(WorkingTreeState::Clean, 1.0, 0.0) > none, "session alone counts");
        assert!(file_recency(WorkingTreeState::Clean, 0.0, 1.0) > none, "commits alone count");
    }

    #[test]
    fn the_session_cooldown_suppresses_a_second_touch() {
        let mut session = SessionFiles::new();

        assert!(session.touch("app/models.py", NOW), "the first touch counts");

        assert!(
            !session.touch("app/models.py", NOW + SESSION_COOLDOWN_SECS - 1),
            "a touch inside the cooldown is ignored",
        );

        assert!(
            session.touch("app/models.py", NOW + SESSION_COOLDOWN_SECS),
            "a touch at the cooldown boundary counts again",
        );
    }

    #[test]
    fn the_session_memory_evicts_oldest_first_and_stays_bounded() {
        let mut session = SessionFiles::new();

        for index in 0..(SESSION_FILES_MAX + 40) {
            session.touch(&format!("app/file{index}.py"), NOW + index as i64 * 1_000);
        }

        assert_eq!(session.len(), SESSION_FILES_MAX, "the queue is capped");
        assert_eq!(session.score("app/file0.py", NOW), 0.0, "the oldest entry was evicted");

        assert!(
            session.score(&format!("app/file{}.py", SESSION_FILES_MAX + 39), NOW + 300_000) > 0.0,
            "the newest entry survives",
        );
    }

    #[test]
    fn a_session_score_decays_with_age() {
        let mut session = SessionFiles::new();

        session.touch("app/models.py", NOW);

        let fresh = session.score("app/models.py", NOW);
        let stale = session.score("app/models.py", NOW + 1_200);

        assert!((fresh - 1.0).abs() < 1e-9, "a just-touched file scores one, got {fresh}");
        assert!(stale < fresh, "an older touch weighs less: {stale} vs {fresh}");
        assert!(stale > 0.0, "and does not vanish outright");
    }

    #[test]
    fn commit_recency_halves_every_three_days_and_ends_at_the_window() {
        let today = commit_recency(Some(NOW), NOW);
        let three_days = commit_recency(Some(NOW - 3 * 86_400), NOW);

        assert!((today - 1.0).abs() < 1e-9, "a commit today scores one, got {today}");
        assert!((three_days - 0.5).abs() < 0.01, "three days is one half life, got {three_days}");

        assert_eq!(
            commit_recency(Some(NOW - (HISTORY_WINDOW_DAYS + 1) * 86_400), NOW),
            0.0,
            "a commit outside the window contributes nothing",
        );

        assert_eq!(commit_recency(None, NOW), 0.0, "no recorded commit contributes nothing");
    }

    #[test]
    fn no_history_degrades_to_zero_without_panicking() {
        let recency = file_recency(WorkingTreeState::Clean, 0.0, commit_recency(None, NOW));

        assert_eq!(recency, 0.0, "a project with no git and no history scores flat zero");
    }

    #[test]
    fn the_bonus_never_reaches_a_whole_tokens_weight() {
        let median = 1_000_u64;

        assert_eq!(recency_bonus(median, 0.0), 0, "a cold file earns nothing");

        assert_eq!(
            recency_bonus(median, 1.0),
            (median as f64 * RECENCY_BONUS_FRACTION) as u64,
            "a maximally recent file earns exactly the capped fraction",
        );

        assert!(
            recency_bonus(median, 1.0) < median,
            "so a recency win can never outweigh matching one more query token",
        );
    }

    #[test]
    fn the_median_resists_one_very_rare_token() {
        assert_eq!(median_weight(&mut []), 0, "an empty set has no median");
        assert_eq!(median_weight(&mut [10]), 10);
        assert_eq!(median_weight(&mut [10, 20]), 15, "an even set averages the middle pair");

        assert_eq!(
            median_weight(&mut [10, 12, 14, 1_000_000]),
            13,
            "one enormous weight does not drag the median with it",
        );
    }
}
