//! A single trailing `next:` line suggesting the follow-up call.
//!
//! Tool descriptions carry workflow guidance in prose, but prose is static: it
//! cannot say "this response contained an untested symbol, so run `tests`
//! next". A per-response hint can, and that content filter is the part worth
//! doing. A hint that fires unconditionally is a second copy of the tool
//! description, which the agent has already read.
//!
//! Three rules keep it from becoming noise:
//!
//! 1. **One line, never two.** A response with two suggestions has none.
//! 2. **Never suggest a tool whose precondition is unmet.** No `affected_flows`
//!    when no flows are computed, no `tests` when nothing was uncovered, no
//!    `impact` when no symbol was named.
//! 3. **Silence at the byte budget.** A response already at its cap gives up
//!    content to make room for a hint, which is a bad trade.

use std::collections::VecDeque;

/// The tool names a session's intent is inferred from.
pub const HINT_HISTORY_MAX: usize = 8;

/// The tools that make up a review sequence. A session dominated by these gets
/// review follow-ups suggested ahead of exploration ones.
const REVIEW_TOOLS: &[&str] = &[
    "constellation_affected_flows",
    "constellation_changed",
    "constellation_impact",
    "constellation_symbol_history",
    "constellation_tests",
];

/// The facts about what a response contained, which decide whether a candidate
/// follow-up's precondition holds.
#[derive(Clone, Debug, Default)]
pub struct HintFacts {
    /// Whether the response is already at its byte budget, in which case no hint
    /// is emitted at all.
    pub at_byte_budget: bool,
    /// Whether any flow has been computed, the precondition for suggesting the
    /// flow tools.
    pub flows_available: bool,
    /// Whether the response contained a symbol with no covering test.
    pub has_uncovered_symbol: bool,
    /// Whether the response contained a Django model.
    pub named_model: bool,
    /// A symbol the response named, which the follow-up would target.
    pub named_symbol: Option<String>,
}

/// The last few tool names, as the session's inferred intent.
#[derive(Debug, Default)]
pub struct SessionIntent {
    recent: VecDeque<&'static str>,
}

impl SessionIntent {
    /// An empty intent, which reads as exploration until proven otherwise.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool call recorded, evicting the oldest past [`HINT_HISTORY_MAX`].
    pub fn record(&mut self, tool: &'static str) {
        self.recent.push_back(tool);

        while self.recent.len() > HINT_HISTORY_MAX {
            self.recent.pop_front();
        }

        assert!(self.recent.len() <= HINT_HISTORY_MAX, "the intent window stays bounded");
    }

    /// Whether the session is reviewing rather than exploring: at least half the
    /// remembered calls are review tools, and there is something to judge from.
    pub fn is_reviewing(&self) -> bool {
        if self.recent.is_empty() {
            return false;
        }

        let reviewing = self.recent.iter().filter(|tool| REVIEW_TOOLS.contains(tool)).count();

        reviewing * 2 >= self.recent.len()
    }

    /// The number of calls remembered.
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    /// Whether nothing has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }
}

/// A candidate follow-up: which tool, whether it belongs to a review
/// sequence, and the phrasing shown when it wins.
struct Candidate {
    phrasing: &'static str,
    precondition: Precondition,
    review: bool,
}

/// The condition a response must satisfy for a candidate to be offered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Precondition {
    /// The precondition that always holds.
    Always,
    /// The precondition that flows have been computed.
    FlowsAvailable,
    /// The precondition that the response named a model.
    NamedModel,
    /// The precondition that the response named a symbol.
    NamedSymbol,
    /// The precondition that the response contained an uncovered symbol.
    Uncovered,
}

impl Precondition {
    /// Whether this precondition holds for a given response.
    fn holds(self, facts: &HintFacts) -> bool {
        match self {
            Precondition::Always => true,
            Precondition::FlowsAvailable => facts.flows_available,
            Precondition::NamedModel => facts.named_model,
            Precondition::NamedSymbol => facts.named_symbol.is_some(),
            Precondition::Uncovered => facts.has_uncovered_symbol,
        }
    }
}

/// The follow-up candidates for one tool, in preference order before the
/// session-intent reordering.
fn candidates(tool: &str) -> &'static [Candidate] {
    match tool {
        "constellation_changed" => &[
            Candidate {
                phrasing: "affected_flows to see which user-facing flows this diff touches",
                precondition: Precondition::FlowsAvailable,
                review: true,
            },
            Candidate {
                phrasing: "tests on the top-risk symbol to find what guards it",
                precondition: Precondition::Uncovered,
                review: true,
            },
            Candidate {
                phrasing: "impact on the top-risk symbol for its blast radius",
                precondition: Precondition::NamedSymbol,
                review: true,
            },
        ],
        "constellation_explore" => &[
            Candidate {
                phrasing: "tests to check what covers the uncovered symbols above",
                precondition: Precondition::Uncovered,
                review: true,
            },
            Candidate {
                phrasing: "model for that model's effective schema across its bases",
                precondition: Precondition::NamedModel,
                review: false,
            },
            Candidate {
                phrasing: "impact to see what depends on this before changing it",
                precondition: Precondition::NamedSymbol,
                review: true,
            },
        ],
        "constellation_search" => &[Candidate {
            phrasing: "explore with the same query to read the code, not just the locations",
            precondition: Precondition::Always,
            review: false,
        }],
        "constellation_node" => &[
            Candidate {
                phrasing: "callers to see how it is used, with each call site's source line",
                precondition: Precondition::NamedSymbol,
                review: false,
            },
            Candidate {
                phrasing: "impact for the transitive blast radius",
                precondition: Precondition::NamedSymbol,
                review: true,
            },
        ],
        "constellation_callers" | "constellation_callees" => &[Candidate {
            phrasing: "impact for the transitive picture, not just one hop",
            precondition: Precondition::NamedSymbol,
            review: true,
        }],
        "constellation_impact" => &[
            Candidate {
                phrasing: "tests to find what guards this before changing it",
                precondition: Precondition::NamedSymbol,
                review: true,
            },
            Candidate {
                phrasing: "affected_flows to see the user-facing paths involved",
                precondition: Precondition::FlowsAvailable,
                review: true,
            },
        ],
        "constellation_tests" => &[Candidate {
            phrasing: "impact to see everything a change here would reach",
            precondition: Precondition::NamedSymbol,
            review: true,
        }],
        "constellation_model" => &[
            Candidate {
                phrasing: "subclasses to see every type inheriting these fields",
                precondition: Precondition::NamedModel,
                review: false,
            },
            Candidate {
                phrasing: "impact before changing a field or relation",
                precondition: Precondition::NamedSymbol,
                review: true,
            },
        ],
        "constellation_overview" => &[
            Candidate {
                phrasing: "flows for the execution paths ranked by criticality",
                precondition: Precondition::FlowsAvailable,
                review: false,
            },
            Candidate {
                phrasing: "routes for the URL surface, then explore for the code",
                precondition: Precondition::Always,
                review: false,
            },
        ],
        "constellation_routes" => &[Candidate {
            phrasing: "feature on a route name for its whole vertical slice",
            precondition: Precondition::Always,
            review: false,
        }],
        "constellation_flows" => &[Candidate {
            phrasing: "feature on a flow's entry point for its vertical slice",
            precondition: Precondition::Always,
            review: false,
        }],
        "constellation_affected_flows" => &[Candidate {
            phrasing: "impact on the symbols inside the most critical flow",
            precondition: Precondition::NamedSymbol,
            review: true,
        }],
        "constellation_at" => &[Candidate {
            phrasing: "callers on that symbol to see what reached it",
            precondition: Precondition::NamedSymbol,
            review: false,
        }],
        "constellation_files" => &[Candidate {
            phrasing: "explore with an identifier from that area to read the code",
            precondition: Precondition::Always,
            review: false,
        }],
        _ => &[],
    }
}

/// The single follow-up line for one response, or `None` when nothing applies.
///
/// Deterministic for a given `(tool, facts, intent)`: the candidate order is
/// fixed and the intent reordering is a stable partition, so the same response
/// always produces the same hint.
///
/// The suggested tool is backticked, not left bare. Every client namespaces these
/// tools with a prefix of its own, and the protocol never carries that prefix
/// here: a call arrives under the name this server registered, so a hint cannot
/// spell the name its reader would have to type. A bare `next: impact` reads as
/// if it could, and inviting an agent to call a name absent from its tool list is
/// the invalid-tool failure this server's naming was fixed to stop. Backticked,
/// it reads as the tool to find in that list.
pub fn hint(tool: &str, facts: &HintFacts, intent: &SessionIntent) -> Option<String> {
    if facts.at_byte_budget {
        return None;
    }

    let candidates = candidates(tool);

    if candidates.is_empty() {
        return None;
    }

    let reviewing = intent.is_reviewing();

    // A stable partition, so within each half the fixed order still decides.
    let chosen = candidates
        .iter()
        .filter(|candidate| candidate.precondition.holds(facts))
        .find(|candidate| candidate.review == reviewing)
        .or_else(|| candidates.iter().find(|candidate| candidate.precondition.holds(facts)))?;

    let (name, rest) = chosen.phrasing.split_once(' ').unwrap_or((chosen.phrasing, ""));

    Some(format!("next: `{name}` {rest}\n"))
}

#[cfg(test)]
mod tests {
    use super::{HINT_HISTORY_MAX, HintFacts, SessionIntent, hint};

    fn facts() -> HintFacts {
        HintFacts { named_symbol: Some("Order".to_string()), ..HintFacts::default() }
    }

    #[test]
    fn a_hint_never_names_a_tool_whose_precondition_is_unmet() {
        let no_flows = HintFacts { flows_available: false, ..facts() };
        let line = hint("constellation_changed", &no_flows, &SessionIntent::new()).unwrap();

        assert!(
            !line.contains("affected_flows"),
            "flows are not suggested when none are computed: {line}",
        );

        let with_flows = HintFacts { flows_available: true, ..facts() };
        let line = hint("constellation_changed", &with_flows, &SessionIntent::new()).unwrap();

        assert!(line.contains("affected_flows"), "and are suggested when they exist: {line}");
    }

    #[test]
    fn tests_are_suggested_only_for_an_uncovered_symbol() {
        let covered = HintFacts { named_symbol: None, ..HintFacts::default() };

        assert_eq!(
            hint("constellation_explore", &covered, &SessionIntent::new()),
            None,
            "nothing applies when the response named nothing and covered everything",
        );

        let uncovered = HintFacts { has_uncovered_symbol: true, ..HintFacts::default() };
        let line = hint("constellation_explore", &uncovered, &SessionIntent::new()).unwrap();

        assert!(line.contains("next: `tests` "), "the uncovered symbol drives it: {line}");
    }

    #[test]
    fn no_response_gains_more_than_one_hint_line() {
        let line = hint("constellation_changed", &facts(), &SessionIntent::new()).unwrap();

        assert_eq!(line.matches("next: ").count(), 1, "exactly one suggestion: {line}");
        assert_eq!(line.lines().count(), 1, "on exactly one line: {line}");
    }

    #[test]
    fn a_response_at_its_byte_budget_gets_no_hint() {
        let full = HintFacts { at_byte_budget: true, ..facts() };

        assert_eq!(
            hint("constellation_explore", &full, &SessionIntent::new()),
            None,
            "content beats a suggestion when space is tight",
        );
    }

    #[test]
    fn the_hint_is_deterministic_for_a_given_response() {
        let intent = SessionIntent::new();

        assert_eq!(
            hint("constellation_node", &facts(), &intent),
            hint("constellation_node", &facts(), &intent),
            "the same inputs always produce the same hint",
        );
    }

    #[test]
    fn a_review_sequence_prefers_a_review_follow_up() {
        let mut exploring = SessionIntent::new();
        exploring.record("constellation_overview");
        exploring.record("constellation_explore");

        let mut reviewing = SessionIntent::new();
        reviewing.record("constellation_changed");
        reviewing.record("constellation_impact");

        assert!(!exploring.is_reviewing(), "two exploration calls read as exploration");
        assert!(reviewing.is_reviewing(), "two review calls read as review");

        let facts = HintFacts { named_model: true, ..facts() };

        let exploring_line = hint("constellation_explore", &facts, &exploring).unwrap();
        let reviewing_line = hint("constellation_explore", &facts, &reviewing).unwrap();

        assert!(exploring_line.contains("next: `model` "), "exploration: {exploring_line}");
        assert!(reviewing_line.contains("next: `impact` "), "review: {reviewing_line}");
    }

    #[test]
    fn the_intent_window_stays_bounded() {
        let mut intent = SessionIntent::new();

        for _ in 0..(HINT_HISTORY_MAX * 3) {
            intent.record("constellation_explore");
        }

        assert_eq!(intent.len(), HINT_HISTORY_MAX, "the window evicts rather than growing");
    }

    #[test]
    fn an_unknown_tool_gets_no_hint() {
        assert_eq!(hint("constellation_status", &facts(), &SessionIntent::new()), None);
    }
}
