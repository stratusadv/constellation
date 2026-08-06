//! Multi-axis symbol filtering.
//!
//! Every axis constellation can filter on is already indexed, and nothing
//! composes them. "Models with a foreign key to `Order`, changed in the last
//! thirty days, with no covering tests" is four tool calls and a manual
//! intersection today. This is the one place where adding a tool *reduces* total
//! surface area, because it subsumes several call sequences.
//!
//! # Semantics and cost are separate
//!
//! Criteria are ANDed, and the order they arrive in is semantic only: the
//! evaluator reorders them by cost, cheapest first, regardless of what the
//! caller passed. A single indexed node column (`kind`, `project`, `file`,
//! `language`, `name`, `lines`) narrows the candidate set for free; the derived
//! scalars (`callers`, `churn`, `tested`, `risk`) and the edge joins (`calls`,
//! `extends`, `relates_to`, `renders`) then run against whatever survived, from
//! one bulk read each rather than one query per candidate.
//!
//! # Glob, not regex
//!
//! `matches` is a glob (`*` and `?`), not a regular expression. constellation
//! keeps a deliberately small dependency surface, and a regex engine is a large
//! thing to pull in for a filter whose realistic use is `*_view` or
//! `**/models.py`. The tool description says so plainly rather than implying a
//! capability that is not there.
//!
//! # No complexity axis
//!
//! `lines` is the honest, free proxy for how much a symbol does. constellation
//! does not compute cyclomatic complexity, and adding a complexity pass to
//! extraction is a separate decision. The tool description says `lines` is a
//! proxy rather than implying complexity.

use std::fmt;

use constellation_graph::{Language, NodeKind};

/// The most criteria one call may carry. Past a dozen the query is better
/// expressed as two calls with the intersection done by the caller.
pub const WINNOW_CRITERIA_MAX: usize = 12;

/// The most candidate symbols one call scans before reporting truncation.
pub const WINNOW_CANDIDATES_MAX: usize = 50_000;

/// The result count a call that names none comes back with.
pub const WINNOW_RESULTS_DEFAULT: u32 = 25;

/// The most results one call may ask for, whatever it passes.
pub const WINNOW_RESULTS_MAX: u32 = 200;

/// The cap on a glob pattern's length, so a pathological pattern cannot make
/// matching expensive per candidate.
pub const PATTERN_CHARS_MAX: usize = 256;

/// The longest haystack glob matching will walk. A symbol name or file path past
/// this is reported as no match rather than matched expensively; nothing
/// constellation indexes comes close, so the cap exists to make the step bound
/// below provable rather than to change any real answer.
const GLOB_TEXT_CHARS_MAX: usize = 4_096;

/// The bound on glob matcher steps for one candidate.
///
/// Derived, not chosen. The matcher's worst case is one step per (pattern
/// character, text character) pair, plus one final pass over the pattern, and
/// both lengths are capped before matching begins. So no input that reaches
/// [`glob_matches`] can exhaust this, which makes the assertion on it a real
/// invariant instead of a panic waiting for a long file path to arrive.
const GLOB_STEPS_MAX: usize = PATTERN_CHARS_MAX * GLOB_TEXT_CHARS_MAX + PATTERN_CHARS_MAX;

/// The default churn window when a `churn` criterion names none.
pub const CHURN_WINDOW_DAYS_DEFAULT: u32 = 90;

/// The axis list, declared once.
///
/// The enum, [`Axis::ALL`], and [`Axis::as_str`] all come from this one table,
/// because they must agree and nothing but discipline was making them. `ALL` is
/// what the "unknown axis" error lists and what [`Axis::from_str_label`] scans,
/// so a variant added to the enum but forgotten in `ALL` is an axis the schema
/// advertises, the agent passes, and the server rejects as unknown. Adding a
/// row here is the whole change.
macro_rules! declare_axes {
    ($($variant:ident => $label:literal),+ $(,)?) => {
        /// A filterable property of a symbol.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Axis {
            $($variant),+
        }

        impl Axis {
            /// The axes, for the error message that lists the valid values.
            pub const ALL: [Axis; [$(Axis::$variant),+].len()] = [$(Axis::$variant),+];

            /// The snake_case label a caller passes.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Axis::$variant => $label),+
                }
            }
        }
    };
}

declare_axes! {
    CalledBy => "called_by",
    Callers => "callers",
    Calls => "calls",
    ChangedSince => "changed_since",
    Churn => "churn",
    Decorator => "decorator",
    Extends => "extends",
    File => "file",
    InFlow => "in_flow",
    Kind => "kind",
    Language => "language",
    Lines => "lines",
    Name => "name",
    Project => "project",
    RelatesTo => "relates_to",
    Renders => "renders",
    Risk => "risk",
    Tested => "tested",
}

/// The cost of evaluating an axis, which decides the evaluation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cost {
    /// An indexed column already loaded with the node. Free.
    Column,
    /// A derived scalar needing one bulk read for the whole candidate set.
    Derived,
    /// An edge join, the most expensive, run last against the narrowed set.
    Edge,
}

impl Axis {
    /// The axis parsed from its label, or `None` when unknown.
    pub fn from_str_label(label: &str) -> Option<Axis> {
        Axis::ALL.into_iter().find(|axis| axis.as_str() == label)
    }

    /// The ops this axis accepts.
    pub fn allowed_ops(self) -> &'static [Op] {
        match self {
            Axis::Kind | Axis::Language | Axis::Project => &[Op::Equal, Op::In],
            Axis::Name => &[Op::Contains, Op::Equal, Op::Matches],
            Axis::File => &[Op::Contains, Op::Matches],
            Axis::Decorator
            | Axis::Calls
            | Axis::CalledBy
            | Axis::Extends
            | Axis::RelatesTo
            | Axis::Renders => &[Op::Contains],
            Axis::Lines | Axis::Callers | Axis::Churn => {
                &[Op::Equal, Op::GreaterOrEqual, Op::GreaterThan, Op::LessOrEqual, Op::LessThan]
            }
            Axis::ChangedSince => &[Op::GreaterOrEqual],
            Axis::Tested => &[Op::Equal],
            Axis::InFlow => &[Op::Contains, Op::Equal],
            Axis::Risk => &[Op::GreaterOrEqual, Op::GreaterThan],
        }
    }

    /// The cost of this axis, which fixes its evaluation position.
    fn cost(self) -> Cost {
        match self {
            Axis::Decorator | Axis::File | Axis::Kind | Axis::Language | Axis::Lines
            | Axis::Name | Axis::Project => Cost::Column,
            Axis::CalledBy | Axis::Callers | Axis::ChangedSince | Axis::Churn | Axis::InFlow
            | Axis::Risk | Axis::Tested => Cost::Derived,
            Axis::Calls | Axis::Extends | Axis::RelatesTo | Axis::Renders => Cost::Edge,
        }
    }
}

/// The comparison a criterion applies to its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Contains,
    Equal,
    GreaterOrEqual,
    GreaterThan,
    In,
    LessOrEqual,
    LessThan,
    Matches,
}

impl Op {
    /// The ops, for the error message that lists the valid values.
    pub const ALL: [Op; 8] = [
        Op::Contains,
        Op::Equal,
        Op::GreaterOrEqual,
        Op::GreaterThan,
        Op::In,
        Op::LessOrEqual,
        Op::LessThan,
        Op::Matches,
    ];

    /// The label a caller passes. The comparison ops accept both a symbolic and
    /// a word form, so `">="` and `"gte"` are the same op.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Contains => "contains",
            Op::Equal => "eq",
            Op::GreaterOrEqual => ">=",
            Op::GreaterThan => ">",
            Op::In => "in",
            Op::LessOrEqual => "<=",
            Op::LessThan => "<",
            Op::Matches => "matches",
        }
    }

    /// The op parsed from its label, or `None` when unknown.
    pub fn from_str_label(label: &str) -> Option<Op> {
        let op = match label {
            "==" | "eq" => Op::Equal,
            ">" | "gt" => Op::GreaterThan,
            ">=" | "gte" => Op::GreaterOrEqual,
            "<" | "lt" => Op::LessThan,
            "<=" | "lte" => Op::LessOrEqual,
            "contains" => Op::Contains,
            "in" => Op::In,
            "matches" => Op::Matches,
            _ => return None,
        };

        Some(op)
    }
}

/// The reasons a criterion could not be accepted. Every variant names the valid values,
/// because an agent cannot discover them otherwise, and silently ignoring a
/// criterion would return a superset the agent believes is filtered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WinnowError {
    /// The refusal of more criteria than [`WINNOW_CRITERIA_MAX`].
    TooManyCriteria(usize),
    /// A pattern longer than [`PATTERN_CHARS_MAX`].
    PatternTooLong(usize),
    /// An axis label that names no axis.
    UnknownAxis(String),
    /// An op label that names no op.
    UnknownOp(String),
    /// An op this axis does not accept.
    UnsupportedOp { axis: Axis, op: Op },
    /// A value that is not the shape the axis needs.
    UnusableValue { axis: Axis, value: String },
}

impl fmt::Display for WinnowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WinnowError::TooManyCriteria(count) => write!(
                formatter,
                "{count} criteria passed; at most {WINNOW_CRITERIA_MAX} are accepted. \
                 Split the query and intersect the results.",
            ),
            WinnowError::PatternTooLong(length) => write!(
                formatter,
                "a pattern of {length} characters was passed; at most {PATTERN_CHARS_MAX} are accepted",
            ),
            WinnowError::UnknownAxis(axis) => {
                write!(formatter, "unknown axis {axis:?}. Valid axes: {}", labels(&Axis::ALL, Axis::as_str))
            }
            WinnowError::UnknownOp(op) => write!(
                formatter,
                "unknown op {op:?}. Valid ops: {} (and the word forms gt, gte, lt, lte, ==)",
                labels(&Op::ALL, Op::as_str),
            ),
            WinnowError::UnsupportedOp { axis, op } => write!(
                formatter,
                "axis {:?} does not accept op {:?}. It accepts: {}",
                axis.as_str(),
                op.as_str(),
                labels(axis.allowed_ops(), Op::as_str),
            ),
            WinnowError::UnusableValue { axis, value } => write!(
                formatter,
                "axis {:?} cannot use value {value:?}; {}",
                axis.as_str(),
                value_hint(*axis),
            ),
        }
    }
}

/// A comma-joined list of labels, for the error messages that enumerate the
/// valid values.
fn labels<T: Copy>(values: &[T], label: fn(T) -> &'static str) -> String {
    values.iter().map(|value| label(*value)).collect::<Vec<&str>>().join(", ")
}

/// The shape of value an axis needs, for the unusable-value message.
fn value_hint(axis: Axis) -> &'static str {
    match axis {
        Axis::Callers | Axis::Churn | Axis::Lines => "it expects an integer",
        Axis::ChangedSince => "it expects a date as YYYY-MM-DD",
        Axis::Kind => "it expects a node kind, e.g. model, view, route, class, function, method",
        Axis::Language => "it expects a language: python, htmldjango, javascript, css",
        Axis::Risk => "it expects a number between 0.0 and 1.0",
        Axis::Tested => "it expects true or false",
        _ => "it expects a non-empty string",
    }
}

/// The parsed, validated value of one criterion.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A date as epoch seconds.
    Date(i64),
    /// A parsed floating-point threshold.
    Fraction(f64),
    /// A set of node kinds.
    Kinds(Vec<NodeKind>),
    /// A set of languages.
    Languages(Vec<Language>),
    /// An integer threshold.
    Number(i64),
    /// A list of one or more strings, lowercased.
    Strings(Vec<String>),
    /// A boolean.
    Truth(bool),
}

/// A parsed criterion, ready to evaluate.
#[derive(Clone, Debug)]
pub struct Criterion {
    pub axis: Axis,
    pub op: Op,
    pub value: Value,
    /// The churn window in days, meaningful only for [`Axis::Churn`].
    pub window_days: u32,
}

impl Criterion {
    /// A one-line rendering of the criterion, for the "which criterion emptied
    /// the set" message.
    pub fn describe(&self) -> String {
        format!("{} {} {}", self.axis.as_str(), self.op.as_str(), describe_value(&self.value))
    }
}

/// The value rendered back the way a caller would have written it.
fn describe_value(value: &Value) -> String {
    match value {
        Value::Date(epoch) => format!("epoch {epoch}"),
        Value::Fraction(number) => format!("{number}"),
        Value::Kinds(kinds) => kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(","),
        Value::Languages(languages) => {
            languages.iter().map(|language| language.as_str()).collect::<Vec<_>>().join(",")
        }
        Value::Number(number) => format!("{number}"),
        Value::Strings(strings) => strings.join(","),
        Value::Truth(truth) => format!("{truth}"),
    }
}

/// The raw criterion as it arrives from the tool call.
pub struct RawCriterion<'a> {
    pub axis: &'a str,
    pub op: &'a str,
    pub value: &'a str,
    pub window_days: Option<u32>,
}

/// The raw criteria parsed and validated, then reordered cheapest-first.
///
/// Validation is total: an unknown axis, an unknown op, an op the axis does not
/// accept, or an unusable value is an error naming the valid alternatives. A
/// criterion is never silently dropped, because that would return a superset the
/// caller believes is filtered.
pub fn parse(raw: &[RawCriterion<'_>]) -> Result<Vec<Criterion>, WinnowError> {
    if raw.len() > WINNOW_CRITERIA_MAX {
        return Err(WinnowError::TooManyCriteria(raw.len()));
    }

    let mut parsed: Vec<Criterion> = Vec::with_capacity(raw.len());

    for entry in raw {
        parsed.push(parse_one(entry)?);
    }

    // Cost order, not caller order: the criteria mean the same thing in any
    // order, so evaluating the cheap ones first is free narrowing. A stable sort
    // keeps two equal-cost criteria in the order the caller wrote them.
    parsed.sort_by_key(|criterion| criterion.axis.cost());

    assert!(parsed.len() == raw.len(), "parsing keeps every criterion");
    assert!(parsed.len() <= WINNOW_CRITERIA_MAX, "the criteria count stays capped");

    Ok(parsed)
}

/// A raw criterion parsed and validated.
fn parse_one(raw: &RawCriterion<'_>) -> Result<Criterion, WinnowError> {
    let axis = Axis::from_str_label(raw.axis.trim())
        .ok_or_else(|| WinnowError::UnknownAxis(raw.axis.to_string()))?;

    let op = Op::from_str_label(raw.op.trim())
        .ok_or_else(|| WinnowError::UnknownOp(raw.op.to_string()))?;

    if !axis.allowed_ops().contains(&op) {
        return Err(WinnowError::UnsupportedOp { axis, op });
    }

    if raw.value.chars().count() > PATTERN_CHARS_MAX {
        return Err(WinnowError::PatternTooLong(raw.value.chars().count()));
    }

    let value = parse_value(axis, raw.value)?;

    Ok(Criterion {
        axis,
        op,
        value,
        window_days: raw.window_days.unwrap_or(CHURN_WINDOW_DAYS_DEFAULT).max(1),
    })
}

/// The value parsed into the shape its axis needs.
fn parse_value(axis: Axis, raw: &str) -> Result<Value, WinnowError> {
    let trimmed = raw.trim();

    let unusable =
        || WinnowError::UnusableValue { axis, value: raw.to_string() };

    if trimmed.is_empty() {
        return Err(unusable());
    }

    let value = match axis {
        Axis::Kind => {
            let kinds: Option<Vec<NodeKind>> =
                split_list(trimmed).iter().map(|part| NodeKind::from_str_label(part)).collect();

            Value::Kinds(kinds.ok_or_else(unusable)?)
        }
        Axis::Language => {
            let languages: Option<Vec<Language>> =
                split_list(trimmed).iter().map(|part| Language::from_str_label(part)).collect();

            Value::Languages(languages.ok_or_else(unusable)?)
        }
        Axis::Callers | Axis::Churn | Axis::Lines => {
            Value::Number(trimmed.parse::<i64>().map_err(|_| unusable())?)
        }
        Axis::Risk => {
            let fraction = trimmed.parse::<f64>().map_err(|_| unusable())?;

            if !(0.0..=1.0).contains(&fraction) {
                return Err(unusable());
            }

            Value::Fraction(fraction)
        }
        Axis::ChangedSince => Value::Date(parse_date(trimmed).ok_or_else(unusable)?),
        Axis::Tested => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Value::Truth(true),
            "false" | "no" | "0" => Value::Truth(false),
            _ => return Err(unusable()),
        },
        Axis::InFlow => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Value::Truth(true),
            "false" | "no" | "0" => Value::Truth(false),
            _ => Value::Strings(split_list(trimmed)),
        },
        _ => Value::Strings(split_list(trimmed)),
    };

    Ok(value)
}

/// A comma-separated value split into lowercased, non-empty parts.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

/// The epoch seconds at midnight of a `YYYY-MM-DD` date, or `None`.
fn parse_date(text: &str) -> Option<i64> {
    let mut parts = text.split('-');

    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400)
}

/// The days since the epoch for a civil date, by Howard Hinnant's
/// days-from-civil algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_position = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_position + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// Whether `text` matches a glob `pattern`, where `*` matches any run of
/// characters and `?` matches exactly one. Both are lowercased by the caller.
///
/// Iterative with a single backtrack point, never recursive. Both lengths are
/// capped on entry, so the step count cannot reach [`GLOB_STEPS_MAX`] and an
/// adversarial pattern is answered rather than either running away or
/// panicking: this is a filter an agent supplies input to, and a filter that
/// can crash the server on a long path is worse than one that says no.
///
/// Walked over byte offsets that only ever land on a character boundary, rather
/// than over two collected `Vec<char>`s. This runs once per needle per candidate
/// over a set capped at [`WINNOW_CANDIDATES_MAX`], so collecting
/// allocated twice per call on the hottest filter the tool has, for a matcher
/// that reads each sequence strictly left to right and never needs one.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    if pattern.chars().count() > PATTERN_CHARS_MAX || text.chars().count() > GLOB_TEXT_CHARS_MAX {
        return false;
    }

    let mut pattern_at: usize = 0;
    let mut text_at: usize = 0;
    let mut star_at: Option<usize> = None;
    let mut star_text_at: usize = 0;
    let mut steps: usize = 0;

    while let Some((glyph, text_next)) = char_at(text, text_at) {
        steps += 1;

        assert!(steps <= GLOB_STEPS_MAX, "glob matching stays within {GLOB_STEPS_MAX} steps");

        match char_at(pattern, pattern_at) {
            Some((expected, pattern_next)) if expected == '?' || expected == glyph => {
                pattern_at = pattern_next;
                text_at = text_next;
            }
            Some(('*', pattern_next)) => {
                star_at = Some(pattern_at);
                star_text_at = text_at;
                pattern_at = pattern_next;
            }
            _ => {
                let Some(star) = star_at else {
                    return false;
                };

                pattern_at = char_at(pattern, star).map_or(pattern.len(), |(_, next)| next);
                star_text_at = char_at(text, star_text_at).map_or(text.len(), |(_, next)| next);
                text_at = star_text_at;
            }
        }
    }

    while let Some(('*', pattern_next)) = char_at(pattern, pattern_at) {
        pattern_at = pattern_next;
    }

    pattern_at == pattern.len()
}

/// The character at byte `offset` in `text` and the offset just past it, or
/// `None` once the offset reaches the end. `offset` is only ever produced by a
/// previous call, so it always lands on a character boundary.
fn char_at(text: &str, offset: usize) -> Option<(char, usize)> {
    if offset >= text.len() {
        return None;
    }

    let glyph = text[offset..].chars().next()?;

    Some((glyph, offset + glyph.len_utf8()))
}

/// Whether a string satisfies a string-valued criterion.
pub fn string_matches(op: Op, needles: &[String], haystack: &str) -> bool {
    let lower = haystack.to_ascii_lowercase();

    match op {
        Op::Contains => needles.iter().any(|needle| lower.contains(needle.as_str())),
        Op::Equal | Op::In => needles.contains(&lower),
        Op::Matches => needles.iter().any(|needle| glob_matches(needle, &lower)),
        _ => false,
    }
}

/// Whether a number satisfies a numeric criterion.
pub fn number_matches(op: Op, threshold: i64, actual: i64) -> bool {
    match op {
        Op::Equal => actual == threshold,
        Op::GreaterOrEqual => actual >= threshold,
        Op::GreaterThan => actual > threshold,
        Op::LessOrEqual => actual <= threshold,
        Op::LessThan => actual < threshold,
        _ => false,
    }
}

/// Whether a fraction satisfies a fractional criterion.
pub fn fraction_matches(op: Op, threshold: f64, actual: f64) -> bool {
    match op {
        Op::GreaterOrEqual => actual >= threshold,
        Op::GreaterThan => actual > threshold,
        _ => false,
    }
}

/// The order a result set comes back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rank {
    Callers,
    Churn,
    Criticality,
    Lines,
    Name,
    Risk,
}

impl Rank {
    /// The rank axes, for the error message that lists the valid values.
    pub const ALL: [Rank; 6] = [
        Rank::Callers,
        Rank::Churn,
        Rank::Criticality,
        Rank::Lines,
        Rank::Name,
        Rank::Risk,
    ];

    /// The label a caller passes.
    pub fn as_str(self) -> &'static str {
        match self {
            Rank::Callers => "callers",
            Rank::Churn => "churn",
            Rank::Criticality => "criticality",
            Rank::Lines => "lines",
            Rank::Name => "name",
            Rank::Risk => "risk",
        }
    }

    /// The rank parsed from its label, or `None` when unknown.
    pub fn from_str_label(label: &str) -> Option<Rank> {
        Rank::ALL.into_iter().find(|rank| rank.as_str() == label)
    }

    /// The comma-joined list of valid rank labels.
    pub fn valid_labels() -> String {
        labels(&Rank::ALL, Rank::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Axis, GLOB_TEXT_CHARS_MAX, Op, PATTERN_CHARS_MAX, RawCriterion, Value, WINNOW_CRITERIA_MAX,
        WinnowError, fraction_matches, glob_matches, number_matches, parse, parse_date,
        string_matches,
    };

    use constellation_graph::NodeKind;

    fn raw<'a>(axis: &'a str, op: &'a str, value: &'a str) -> RawCriterion<'a> {
        RawCriterion { axis, op, value, window_days: None }
    }

    #[test]
    fn an_unknown_axis_names_every_valid_axis() {
        let error = parse(&[raw("complexity", "gt", "10")]).unwrap_err();

        assert_eq!(error, WinnowError::UnknownAxis("complexity".to_string()));

        let message = error.to_string();

        assert!(message.contains("Valid axes"), "the message enumerates them: {message}");
        assert!(message.contains("lines"), "including the honest proxy: {message}");
    }

    #[test]
    fn an_unknown_op_names_every_valid_op() {
        let error = parse(&[raw("kind", "startswith", "model")]).unwrap_err();

        assert_eq!(error, WinnowError::UnknownOp("startswith".to_string()));
        assert!(error.to_string().contains("contains"), "{error}");
    }

    #[test]
    fn an_op_an_axis_rejects_names_the_ops_it_accepts() {
        let error = parse(&[raw("kind", "contains", "model")]).unwrap_err();

        assert_eq!(error, WinnowError::UnsupportedOp { axis: Axis::Kind, op: Op::Contains });

        let message = error.to_string();

        assert!(message.contains("It accepts"), "{message}");
        assert!(message.contains("eq") && message.contains("in"), "{message}");
    }

    #[test]
    fn an_unusable_value_explains_the_shape_expected() {
        let error = parse(&[raw("callers", "gt", "many")]).unwrap_err();

        assert!(error.to_string().contains("integer"), "{error}");

        let error = parse(&[raw("risk", "gte", "7")]).unwrap_err();

        assert!(error.to_string().contains("0.0 and 1.0"), "a risk is a fraction: {error}");

        let error = parse(&[raw("kind", "eq", "widget")]).unwrap_err();

        assert!(error.to_string().contains("node kind"), "{error}");
    }

    #[test]
    fn too_many_criteria_is_rejected_rather_than_truncated() {
        let criteria: Vec<RawCriterion<'_>> =
            (0..=WINNOW_CRITERIA_MAX).map(|_| raw("kind", "eq", "model")).collect();

        let error = parse(&criteria).unwrap_err();

        assert!(matches!(error, WinnowError::TooManyCriteria(_)), "{error}");
        assert!(error.to_string().contains("Split the query"), "{error}");
    }

    #[test]
    fn criteria_are_reordered_cheapest_first_regardless_of_input_order() {
        let parsed = parse(&[
            raw("calls", "contains", "save"),
            raw("tested", "eq", "false"),
            raw("kind", "eq", "model"),
        ])
        .unwrap();

        let order: Vec<&str> = parsed.iter().map(|criterion| criterion.axis.as_str()).collect();

        assert_eq!(
            order,
            vec!["kind", "tested", "calls"],
            "an indexed column narrows before a derived scalar, which narrows before an edge join",
        );
    }

    #[test]
    fn a_list_value_parses_into_its_typed_set() {
        let parsed = parse(&[raw("kind", "in", "model, view")]).unwrap();

        assert_eq!(parsed[0].value, Value::Kinds(vec![NodeKind::Model, NodeKind::View]));
    }

    #[test]
    fn glob_matching_handles_stars_questions_and_anchors() {
        assert!(glob_matches("*_view", "detail_view"));
        assert!(glob_matches("detail_*", "detail_view"));
        assert!(glob_matches("*models.py", "app/orders/models.py"));
        assert!(glob_matches("app/*/models.py", "app/orders/models.py"));
        assert!(glob_matches("?rder", "order"));
        assert!(glob_matches("*", "anything"));

        assert!(!glob_matches("*_view", "view_detail"));
        assert!(!glob_matches("?rder", "reorder"));
        assert!(!glob_matches("detail", "detail_view"), "a glob is anchored at both ends");
    }

    #[test]
    fn glob_backtracking_terminates_on_an_adversarial_pattern() {
        let pattern = "*a*a*a*a*a*a*b";
        let text = "a".repeat(64);

        assert!(!glob_matches(pattern, &text), "it terminates rather than running away");
    }

    #[test]
    fn a_maximal_pattern_against_a_long_path_answers_rather_than_panicking() {
        // The worst legal input: a pattern at the validation cap, every character
        // of it a star, against a path far longer than anything real. This used
        // to exhaust a fixed step bound and panic inside a tool call.
        let pattern = "*a".repeat(PATTERN_CHARS_MAX / 2);
        let text = format!("{}b", "a".repeat(GLOB_TEXT_CHARS_MAX - 1));

        assert_eq!(pattern.chars().count(), PATTERN_CHARS_MAX, "the pattern sits on the cap");
        assert!(!glob_matches(&pattern, &text), "an unmatchable worst case answers no");
    }

    #[test]
    fn a_haystack_past_the_cap_is_no_match_rather_than_a_walk() {
        let text = "a".repeat(GLOB_TEXT_CHARS_MAX + 1);

        assert!(!glob_matches("*", &text), "past the cap nothing matches, not even a bare star");
        assert!(glob_matches("*", &"a".repeat(GLOB_TEXT_CHARS_MAX)), "at the cap it still matches");
    }

    #[test]
    fn string_ops_do_what_their_names_say() {
        let needles = vec!["order".to_string()];

        assert!(string_matches(Op::Contains, &needles, "CustomerOrder"));
        assert!(!string_matches(Op::Equal, &needles, "CustomerOrder"));
        assert!(string_matches(Op::Equal, &needles, "Order"), "equality is case-insensitive");
        assert!(string_matches(Op::Matches, &["*order".to_string()], "CustomerOrder"));
    }

    #[test]
    fn numeric_and_fractional_ops_compare_the_right_way_round() {
        assert!(number_matches(Op::GreaterThan, 5, 6), "actual > threshold");
        assert!(!number_matches(Op::GreaterThan, 5, 5));
        assert!(number_matches(Op::LessOrEqual, 5, 5));
        assert!(number_matches(Op::Equal, 5, 5));

        assert!(fraction_matches(Op::GreaterOrEqual, 0.7, 0.7));
        assert!(!fraction_matches(Op::GreaterThan, 0.7, 0.7));
    }

    #[test]
    fn a_date_parses_to_midnight_utc() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("not-a-date"), None);
        assert_eq!(parse_date("2024-13-01"), None);
    }

    #[test]
    fn every_listed_axis_parses_back_from_its_own_label() {
        for axis in Axis::ALL {
            assert_eq!(
                Axis::from_str_label(axis.as_str()),
                Some(axis),
                "{axis:?} round trips through the label the schema advertises",
            );
        }

        assert_eq!(Axis::from_str_label("complexity"), None, "an unknown axis is rejected");
    }

    #[test]
    fn every_listed_op_parses_back_from_its_own_label() {
        for op in Op::ALL {
            assert_eq!(
                Op::from_str_label(op.as_str()),
                Some(op),
                "{op:?} round trips through the label it renders as",
            );
        }

        assert_eq!(Op::from_str_label("~="), None, "an unknown op is rejected");
    }

    #[test]
    fn no_axis_or_op_label_is_declared_twice() {
        for (position, axis) in Axis::ALL.iter().enumerate() {
            let earlier = &Axis::ALL[..position];

            assert!(
                !earlier.iter().any(|other| other.as_str() == axis.as_str()),
                "{axis:?} shares a label with an axis declared before it",
            );
        }

        for (position, op) in Op::ALL.iter().enumerate() {
            let earlier = &Op::ALL[..position];

            assert!(
                !earlier.iter().any(|other| other.as_str() == op.as_str()),
                "{op:?} shares a label with an op declared before it",
            );
        }
    }

    #[test]
    fn every_axis_accepts_at_least_one_op() {
        for axis in Axis::ALL {
            assert!(
                !axis.allowed_ops().is_empty(),
                "{axis:?} would be unusable: the schema lists it and every op is rejected",
            );
        }
    }
}
