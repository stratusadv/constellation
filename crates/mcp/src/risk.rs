//! Review-risk scoring for the symbols a diff touched.
//!
//! `constellation_changed` used to walk diff hunks in file order and truncate at
//! the limit, so a large branch diff returned an arbitrary first N rather than
//! the N worth reviewing. Every input needed to rank them is already indexed, so
//! this module turns them into one `0.0..=1.0` score per changed symbol.
//!
//! The weights sum to one, so a maximal symbol scores exactly one and the clamp
//! in [`score`] is a guard, not a load-bearing normalization. When a derived
//! input is absent (no ingested git history, no computed flows) its factor is
//! dropped and the remaining weights are renormalized, so an unpopulated index
//! still produces a comparable score instead of a silently deflated one.

pub use constellation_graph::{SECURITY_KEYWORDS, security_keyword};

/// The covering-reference count at which the test-coverage gap closes entirely.
pub const TEST_SATURATION: u32 = 5;

/// The distinct-caller count at which raw fan-in stops adding risk.
pub const CALLER_SATURATION: u32 = 20;

/// The cross-app caller count at which app-boundary reach saturates.
pub const CROSS_APP_SATURATION: u32 = 5;

/// The cross-project caller count at which repository-boundary reach saturates.
/// A caller in another repository is a strong signal, so it saturates fast.
pub const CROSS_PROJECT_SATURATION: u32 = 2;

/// The window, in days, over which a file's commits count as churn.
pub const CHURN_WINDOW_DAYS: i64 = 90;

/// The commit count within [`CHURN_WINDOW_DAYS`] at which churn saturates.
pub const CHURN_SATURATION: u32 = 10;

/// The changed-line count at which diff size saturates.
pub const DIFF_LINES_SATURATION: u32 = 100;

/// The summed criticality of the flows a symbol belongs to at which flow
/// participation saturates. Criticality-weighted, so membership in one critical
/// flow outweighs membership in several trivial ones.
pub const FLOW_PARTICIPATION_SATURATION: f64 = 2.0;

/// The tolerance within which a weight set counts as summing to one, absorbing
/// the binary floating-point representation error in the literal weights.
const WEIGHT_SUM_EPSILON: f64 = 1e-9;

/// A weighted component of a symbol's review risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskFactor {
    CallerCount,
    Churn,
    CrossAppCallers,
    CrossProjectCallers,
    DiffSize,
    FlowParticipation,
    Security,
    TestGap,
}

impl RiskFactor {
    /// The factors, in a fixed order, so each traversal is statically bounded.
    pub const ALL: [RiskFactor; 8] = [
        RiskFactor::CallerCount,
        RiskFactor::Churn,
        RiskFactor::CrossAppCallers,
        RiskFactor::CrossProjectCallers,
        RiskFactor::DiffSize,
        RiskFactor::FlowParticipation,
        RiskFactor::Security,
        RiskFactor::TestGap,
    ];

    /// The snake_case label for this factor.
    pub fn as_str(self) -> &'static str {
        match self {
            RiskFactor::CallerCount => "caller_count",
            RiskFactor::Churn => "churn",
            RiskFactor::CrossAppCallers => "cross_app_callers",
            RiskFactor::CrossProjectCallers => "cross_project_callers",
            RiskFactor::DiffSize => "diff_size",
            RiskFactor::FlowParticipation => "flow_participation",
            RiskFactor::Security => "security",
            RiskFactor::TestGap => "test_gap",
        }
    }
}

/// The weight each factor carries in the blended score. The weights sum to one.
#[derive(Clone, Copy, Debug)]
pub struct RiskWeights {
    pub caller_count: f64,
    pub churn: f64,
    pub cross_app_callers: f64,
    pub cross_project_callers: f64,
    pub diff_size: f64,
    pub flow_participation: f64,
    pub security: f64,
    pub test_gap: f64,
}

/// The tuned default weights. Flow participation and the test-coverage gap
/// dominate because they answer the reviewer's two first questions: does this
/// sit on a user-facing path, and is anything guarding it.
pub const RISK_WEIGHTS: RiskWeights = RiskWeights {
    caller_count: 0.05,
    churn: 0.10,
    cross_app_callers: 0.07,
    cross_project_callers: 0.08,
    diff_size: 0.05,
    flow_participation: 0.25,
    security: 0.15,
    test_gap: 0.25,
};

impl RiskWeights {
    /// The weight this set assigns to one factor.
    pub fn weight(&self, factor: RiskFactor) -> f64 {
        match factor {
            RiskFactor::CallerCount => self.caller_count,
            RiskFactor::Churn => self.churn,
            RiskFactor::CrossAppCallers => self.cross_app_callers,
            RiskFactor::CrossProjectCallers => self.cross_project_callers,
            RiskFactor::DiffSize => self.diff_size,
            RiskFactor::FlowParticipation => self.flow_participation,
            RiskFactor::Security => self.security,
            RiskFactor::TestGap => self.test_gap,
        }
    }

    /// The sum of every weight in this set, one for a well-formed set.
    pub fn total(&self) -> f64 {
        let mut sum = 0.0;

        for factor in RiskFactor::ALL {
            sum += self.weight(factor);
        }

        assert!(sum > 0.0, "a weight set must carry positive weight");

        sum
    }

    /// The same weights with every unavailable factor zeroed and the rest scaled
    /// back up to sum to one, so a project with no ingested history scores on the
    /// same `0.0..=1.0` range as one that has it.
    pub fn renormalized(&self, availability: FactorAvailability) -> RiskWeights {
        let mut kept = *self;

        if !availability.churn {
            kept.churn = 0.0;
        }

        if !availability.flow_participation {
            kept.flow_participation = 0.0;
        }

        let remaining = kept.total();

        assert!(remaining > 0.0, "at least one factor is always available");

        let scale = 1.0 / remaining;

        let scaled = RiskWeights {
            caller_count: kept.caller_count * scale,
            churn: kept.churn * scale,
            cross_app_callers: kept.cross_app_callers * scale,
            cross_project_callers: kept.cross_project_callers * scale,
            diff_size: kept.diff_size * scale,
            flow_participation: kept.flow_participation * scale,
            security: kept.security * scale,
            test_gap: kept.test_gap * scale,
        };

        debug_assert!(
            (scaled.total() - 1.0).abs() < WEIGHT_SUM_EPSILON,
            "renormalized weights sum to one",
        );

        scaled
    }
}

/// The derived inputs this index actually carries. Churn needs
/// `constellation history`; flow participation needs `constellation flows`.
/// Both are honest booleans rather than a zero measurement, so an absent pass
/// drops its factor instead of scoring every symbol as unchurned and flowless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactorAvailability {
    pub churn: bool,
    pub flow_participation: bool,
}

/// The raw, unnormalized measurements one changed symbol contributes.
#[derive(Clone, Debug, Default)]
pub struct RiskInputs {
    pub caller_count: u32,
    pub changed_lines: u32,
    pub churn_commits: u32,
    pub covering_tests: u32,
    pub cross_app_callers: u32,
    pub cross_project_callers: u32,
    pub flow_criticality_total: f64,
    /// The name of the highest-criticality flow this symbol participates in,
    /// carried through so the rendered reason can name it.
    pub flow_name_top: Option<String>,
    /// The [`SECURITY_KEYWORDS`] entry the symbol's name or qualified name
    /// matched, or `None` when it matched none.
    pub security_keyword: Option<&'static str>,
}

/// The per-factor sub-scores, the blended total, and the measurements they came
/// from, so a caller can both rank on the total and explain it.
#[derive(Clone, Debug)]
pub struct RiskFactors {
    pub caller_count: f64,
    pub churn: f64,
    pub cross_app_callers: f64,
    pub cross_project_callers: f64,
    pub diff_size: f64,
    pub flow_participation: f64,
    pub inputs: RiskInputs,
    pub security: f64,
    pub test_gap: f64,
    pub total: f64,
}

impl RiskFactors {
    /// The `0.0..=1.0` sub-score for one factor, before its weight applies.
    pub fn sub_score(&self, factor: RiskFactor) -> f64 {
        match factor {
            RiskFactor::CallerCount => self.caller_count,
            RiskFactor::Churn => self.churn,
            RiskFactor::CrossAppCallers => self.cross_app_callers,
            RiskFactor::CrossProjectCallers => self.cross_project_callers,
            RiskFactor::DiffSize => self.diff_size,
            RiskFactor::FlowParticipation => self.flow_participation,
            RiskFactor::Security => self.security,
            RiskFactor::TestGap => self.test_gap,
        }
    }

    /// The factors that actually moved this score, highest contribution first,
    /// dropping any that contributed nothing. Ties break on the factor's fixed
    /// order, so two identical inputs always render identically.
    pub fn ranked_contributions(&self, weights: &RiskWeights) -> Vec<(RiskFactor, f64)> {
        let mut ranked: Vec<(RiskFactor, f64)> = Vec::with_capacity(RiskFactor::ALL.len());

        for factor in RiskFactor::ALL {
            let contribution = self.sub_score(factor) * weights.weight(factor);

            if contribution > 0.0 {
                ranked.push((factor, contribution));
            }
        }

        assert!(ranked.len() <= RiskFactor::ALL.len(), "no factor contributes twice");

        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));

        ranked
    }

    /// A human phrase for one factor, drawn from the raw measurement rather than
    /// the normalized sub-score, because "9 commits/90d" is actionable where
    /// "0.9" is not. `None` when the measurement has nothing to say.
    pub fn describe(&self, factor: RiskFactor) -> Option<String> {
        let inputs = &self.inputs;

        let phrase = match factor {
            RiskFactor::CallerCount => format!("{} callers", inputs.caller_count),
            RiskFactor::Churn => format!("{} commits/{CHURN_WINDOW_DAYS}d", inputs.churn_commits),
            RiskFactor::CrossAppCallers => format!("{} cross-app callers", inputs.cross_app_callers),
            RiskFactor::CrossProjectCallers => {
                format!("{} cross-project callers", inputs.cross_project_callers)
            }
            RiskFactor::DiffSize => format!("{} lines changed", inputs.changed_lines),
            RiskFactor::FlowParticipation => {
                format!("in {:?} flow", inputs.flow_name_top.as_deref()?)
            }
            RiskFactor::Security => format!("security-sensitive ({})", inputs.security_keyword?),
            RiskFactor::TestGap => match inputs.covering_tests {
                0 => "no tests".to_string(),
                covering => format!("only {covering} covering test(s)"),
            },
        };

        Some(phrase)
    }

    /// The two or three strongest reasons this symbol scored what it did, as one
    /// comma-joined phrase, or an empty string when nothing contributed. A bare
    /// number is not actionable, so every rendered score carries this.
    pub fn summary(&self, weights: &RiskWeights, reasons_max: usize) -> String {
        assert!(reasons_max >= 1, "a summary lists at least one reason");

        let ranked = self.ranked_contributions(weights);
        let mut reasons: Vec<String> = Vec::with_capacity(reasons_max);

        for (factor, _) in ranked {
            if reasons.len() >= reasons_max {
                break;
            }

            if let Some(phrase) = self.describe(factor) {
                reasons.push(phrase);
            }
        }

        assert!(reasons.len() <= reasons_max, "the reason list respects its cap");

        reasons.join(", ")
    }
}

/// The `0.0..=1.0` ratio of a count against a saturation bound: zero at zero,
/// one at or past the bound, linear in between.
fn saturating_ratio(value: u32, saturation: u32) -> f64 {
    assert!(saturation > 0, "a saturation bound is positive");

    let ratio = f64::from(value.min(saturation)) / f64::from(saturation);

    assert!((0.0..=1.0).contains(&ratio), "a saturating ratio lands in 0..=1");

    ratio
}

/// The test-coverage gap: one with no covering reference, decaying linearly to
/// zero at [`TEST_SATURATION`] covering references.
fn test_gap_score(covering_tests: u32) -> f64 {
    let gap = 1.0 - saturating_ratio(covering_tests, TEST_SATURATION);

    assert!((0.0..=1.0).contains(&gap), "a coverage gap lands in 0..=1");

    gap
}

/// The flow-participation score: the summed criticality of the flows a symbol
/// belongs to, over [`FLOW_PARTICIPATION_SATURATION`].
fn flow_participation_score(criticality_total: f64) -> f64 {
    assert!(criticality_total >= 0.0, "summed flow criticality is non-negative");
    assert!(criticality_total.is_finite(), "summed flow criticality is finite");

    (criticality_total / FLOW_PARTICIPATION_SATURATION).clamp(0.0, 1.0)
}

/// The blended review risk for one changed symbol, with the per-factor
/// sub-scores that produced it. `weights` is expected to be already
/// [`RiskWeights::renormalized`] for the inputs this index can supply.
pub fn score(inputs: RiskInputs, weights: &RiskWeights) -> RiskFactors {
    debug_assert!(
        (weights.total() - 1.0).abs() < WEIGHT_SUM_EPSILON,
        "risk weights must sum to one before scoring",
    );

    let mut factors = RiskFactors {
        caller_count: saturating_ratio(inputs.caller_count, CALLER_SATURATION),
        churn: saturating_ratio(inputs.churn_commits, CHURN_SATURATION),
        cross_app_callers: saturating_ratio(inputs.cross_app_callers, CROSS_APP_SATURATION),
        cross_project_callers: saturating_ratio(
            inputs.cross_project_callers,
            CROSS_PROJECT_SATURATION,
        ),
        diff_size: saturating_ratio(inputs.changed_lines, DIFF_LINES_SATURATION),
        flow_participation: flow_participation_score(inputs.flow_criticality_total),
        security: f64::from(u8::from(inputs.security_keyword.is_some())),
        test_gap: test_gap_score(inputs.covering_tests),
        inputs,
        total: 0.0,
    };

    let mut total = 0.0;

    for factor in RiskFactor::ALL {
        total += factors.sub_score(factor) * weights.weight(factor);
    }

    let total = total.clamp(0.0, 1.0);

    assert!(total >= 0.0, "a risk score is never negative");
    assert!(total <= 1.0, "a risk score never exceeds one");

    factors.total = total;

    factors
}

#[cfg(test)]
mod tests {
    use super::{
        CHURN_SATURATION, FactorAvailability, RISK_WEIGHTS, RiskFactor, RiskInputs, RiskWeights,
        TEST_SATURATION, score, security_keyword,
    };

    const EVERY_FACTOR: FactorAvailability =
        FactorAvailability { churn: true, flow_participation: true };

    fn inputs() -> RiskInputs {
        RiskInputs::default()
    }

    #[test]
    fn the_default_weights_sum_to_one() {
        assert!(
            (RISK_WEIGHTS.total() - 1.0).abs() < 1e-9,
            "the shipped weights sum to one, got {}",
            RISK_WEIGHTS.total(),
        );
    }

    #[test]
    fn every_renormalized_subset_sums_to_one() {
        for churn in [false, true] {
            for flow_participation in [false, true] {
                let availability = FactorAvailability { churn, flow_participation };
                let weights = RISK_WEIGHTS.renormalized(availability);

                assert!(
                    (weights.total() - 1.0).abs() < 1e-9,
                    "weights renormalize to one for {availability:?}, got {}",
                    weights.total(),
                );

                if !churn {
                    assert_eq!(weights.churn, 0.0, "an absent factor carries no weight");
                }

                if !flow_participation {
                    assert_eq!(weights.flow_participation, 0.0, "an absent factor carries no weight");
                }
            }
        }
    }

    #[test]
    fn an_untested_symbol_outranks_a_covered_one() {
        let untested = score(inputs(), &RISK_WEIGHTS);
        let covered = score(
            RiskInputs { covering_tests: TEST_SATURATION, ..inputs() },
            &RISK_WEIGHTS,
        );

        assert!(
            untested.total > covered.total,
            "zero coverage scores above full coverage: {} vs {}",
            untested.total,
            covered.total,
        );
    }

    #[test]
    fn a_security_named_symbol_outranks_a_neutral_one() {
        let neutral = RiskInputs {
            security_keyword: security_keyword("format_label", "app/text.py::format_label"),
            ..inputs()
        };
        let sensitive = RiskInputs {
            security_keyword: security_keyword("verify_password", "app/text.py::verify_password"),
            ..inputs()
        };

        assert!(neutral.security_keyword.is_none(), "a neutral name matches no keyword");
        assert_eq!(sensitive.security_keyword, Some("password"), "the matched keyword is reported");

        assert!(
            score(sensitive, &RISK_WEIGHTS).total > score(neutral, &RISK_WEIGHTS).total,
            "a security-adjacent name scores strictly higher",
        );
    }

    #[test]
    fn a_cross_project_caller_outweighs_an_in_project_one() {
        let in_project = RiskInputs { caller_count: 1, ..inputs() };
        let cross_project =
            RiskInputs { caller_count: 1, cross_project_callers: 1, ..inputs() };

        assert!(
            score(cross_project, &RISK_WEIGHTS).total > score(in_project, &RISK_WEIGHTS).total,
            "a caller in another repository raises the score further",
        );
    }

    #[test]
    fn a_flow_member_outranks_a_flowless_symbol() {
        let flowless = score(inputs(), &RISK_WEIGHTS);
        let member = score(
            RiskInputs { flow_criticality_total: 0.9, ..inputs() },
            &RISK_WEIGHTS,
        );

        assert!(
            member.total > flowless.total,
            "membership in a critical flow raises the score: {} vs {}",
            member.total,
            flowless.total,
        );
    }

    #[test]
    fn a_maximal_symbol_scores_exactly_one() {
        let maximal = RiskInputs {
            caller_count: u32::MAX,
            changed_lines: u32::MAX,
            churn_commits: u32::MAX,
            covering_tests: 0,
            cross_app_callers: u32::MAX,
            cross_project_callers: u32::MAX,
            flow_criticality_total: 10.0,
            flow_name_top: Some("checkout".to_string()),
            security_keyword: Some("password"),
        };

        let scored = score(maximal, &RISK_WEIGHTS);

        assert!(
            (scored.total - 1.0).abs() < 1e-9,
            "a maximal symbol lands on exactly one, got {}",
            scored.total,
        );
    }

    #[test]
    fn the_summary_names_the_strongest_reasons() {
        let scored = score(
            RiskInputs {
                caller_count: 14,
                churn_commits: CHURN_SATURATION,
                flow_criticality_total: 1.8,
                flow_name_top: Some("checkout".to_string()),
                ..inputs()
            },
            &RISK_WEIGHTS,
        );

        let summary = scored.summary(&RISK_WEIGHTS, 3);

        assert!(summary.contains("checkout"), "the top flow is named: {summary}");
        assert!(summary.contains("no tests"), "the coverage gap is named: {summary}");
        assert_eq!(summary.matches(", ").count(), 2, "three reasons, two separators: {summary}");
    }

    #[test]
    fn the_summary_is_deterministic_for_identical_inputs() {
        let build = || score(RiskInputs { caller_count: 3, ..inputs() }, &RISK_WEIGHTS);

        assert_eq!(
            build().summary(&RISK_WEIGHTS, 3),
            build().summary(&RISK_WEIGHTS, 3),
            "identical inputs render identically",
        );
    }

    #[test]
    fn dropping_the_churn_factor_leaves_it_out_of_the_ranking() {
        let weights = RISK_WEIGHTS.renormalized(FactorAvailability {
            churn: false,
            flow_participation: false,
        });

        let scored = score(RiskInputs { churn_commits: 40, ..inputs() }, &weights);
        let ranked = scored.ranked_contributions(&weights);

        assert!(
            ranked.iter().all(|(factor, _)| *factor != RiskFactor::Churn),
            "a zero-weight factor never appears in the ranking",
        );
    }

    #[test]
    fn every_available_factor_carries_weight() {
        let weights: RiskWeights = RISK_WEIGHTS.renormalized(EVERY_FACTOR);

        for factor in RiskFactor::ALL {
            assert!(
                weights.weight(factor) > 0.0,
                "{} carries weight when available",
                factor.as_str(),
            );
        }
    }
}
