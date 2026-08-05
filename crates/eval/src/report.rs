//! The CSV and markdown a run emits.
//!
//! The markdown always carries a **Limits** section stating, verbatim, what the
//! run did not measure. That section is not decoration. A retrieval-quality
//! number without its limits reads as a fact about the world when it is a fact
//! about a goldset the same people wrote who wrote the ranking.

use std::fmt::Write as _;

/// The outcome of one benchmark row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The measurement ran and its value is meaningful.
    Ok,
    /// The measurement could not run: an absent pass, an unmeasurable case, a
    /// failed call. Excluded from every aggregate; never scored as a zero, which
    /// would silently look like a bad result rather than an absent one.
    Error,
}

impl Status {
    /// The label written to the CSV.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Error => "error",
            Status::Ok => "ok",
        }
    }
}

/// A measured row.
#[derive(Clone, Debug)]
pub struct BenchmarkRow {
    pub benchmark: &'static str,
    pub case: String,
    /// The ground truth the row was graded against, empty when the benchmark
    /// has only one. Emitted as its own column so a circular mode can never be
    /// mistaken for an independent one.
    pub ground_truth_mode: String,
    pub metric: &'static str,
    pub status: Status,
    pub detail: String,
    pub value: f64,
}

impl BenchmarkRow {
    /// A successful measurement.
    pub fn ok(benchmark: &'static str, metric: &'static str, case: impl Into<String>, value: f64) -> Self {
        Self {
            benchmark,
            case: case.into(),
            ground_truth_mode: String::new(),
            metric,
            status: Status::Ok,
            detail: String::new(),
            value,
        }
    }

    /// A measurement that could not run, with the reason.
    pub fn failed(
        benchmark: &'static str,
        metric: &'static str,
        case: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            benchmark,
            case: case.into(),
            ground_truth_mode: String::new(),
            metric,
            status: Status::Error,
            detail: detail.into(),
            value: 0.0,
        }
    }

    /// The row with its ground-truth mode set.
    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.ground_truth_mode = mode.into();

        self
    }

    /// The row with an explanatory detail attached.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();

        self
    }
}

/// The rows rendered as CSV, one header line then one line per row.
pub fn to_csv(rows: &[BenchmarkRow]) -> String {
    let mut out = String::from("benchmark,case,ground_truth_mode,metric,value,status,detail\n");

    for row in rows {
        let _ = writeln!(
            out,
            "{},{},{},{},{:.6},{},{}",
            escape(row.benchmark),
            escape(&row.case),
            escape(&row.ground_truth_mode),
            escape(row.metric),
            row.value,
            row.status.as_str(),
            escape(&row.detail),
        );
    }

    out
}

/// A CSV field quoted when it holds a comma, a quote, or a newline.
fn escape(field: &str) -> String {
    if !field.contains([',', '"', '\n']) {
        return field.to_string();
    }

    format!("\"{}\"", field.replace('"', "\"\""))
}

/// The markdown summary: a per-benchmark table of the successful rows, the
/// errors listed separately, and the Limits section.
pub fn to_markdown(name: &str, rows: &[BenchmarkRow]) -> String {
    let mut out = format!("# constellation retrieval quality: {name}\n\n");

    let mut benchmarks: Vec<&'static str> = rows.iter().map(|row| row.benchmark).collect();
    benchmarks.dedup();

    for benchmark in benchmarks {
        let matching: Vec<&BenchmarkRow> =
            rows.iter().filter(|row| row.benchmark == benchmark).collect();

        let _ = writeln!(out, "## {benchmark}\n");
        let _ = writeln!(out, "| case | ground truth | metric | value | status |");
        let _ = writeln!(out, "|---|---|---|---|---|");

        for row in &matching {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:.4} | {} |",
                row.case,
                if row.ground_truth_mode.is_empty() { "-" } else { &row.ground_truth_mode },
                row.metric,
                row.value,
                row.status.as_str(),
            );
        }

        out.push('\n');

        let failures: Vec<&&BenchmarkRow> =
            matching.iter().filter(|row| row.status != Status::Ok).collect();

        if !failures.is_empty() {
            let _ = writeln!(out, "Not measured:\n");

            for row in failures {
                let _ = writeln!(out, "- `{}`: {}", row.case, row.detail);
            }

            out.push('\n');
        }
    }

    out.push_str(LIMITS);

    out
}

/// The Limits section, stated verbatim in every report.
const LIMITS: &str = "\
## Limits

What this run did **not** measure. Read these before quoting any number above.

- **No human relevance judgements.** Every question was graded against one
  expected qualified name. A response that answered the question better, with a
  different symbol, scores zero.
- **Token counts are an approximation.** Bytes divided by four. No tokenizer was
  run. Ratios between the approaches are more trustworthy than the absolute
  figures.
- **The graph-derived impact mode is circular.** Grading a graph prediction
  against graph neighbours measures self-consistency, not accuracy. It is an
  upper bound. The co-change mode, graded against what the author actually
  touched in the same commit, is the evidence; the graph-derived mode is
  labelled as circular in its own column for exactly this reason.
- **The goldset is authored by us.** It is therefore biased toward questions we
  already know the graph answers. It measures regression well and absolute
  capability poorly.
- **One index, one point in time.** Numbers are not comparable across
  differently sized repositories, or across index versions, unless the index was
  rebuilt identically.
- **`agent_baseline` is a scripted approximation of a grep-and-read loop**, not a
  real agent. It bounds the comparison; it does not settle it.
";

#[cfg(test)]
mod tests {
    use super::{BenchmarkRow, Status, to_csv, to_markdown};

    fn rows() -> Vec<BenchmarkRow> {
        vec![
            BenchmarkRow::ok("search_quality", "mrr", "store.search_nodes", 0.62),
            BenchmarkRow::ok("impact_accuracy", "f1", "commit abc123", 0.4)
                .with_mode("co-change (same commit, seed excluded)"),
            BenchmarkRow::failed("token_efficiency", "ratio", "commit def456", "no diff"),
        ]
    }

    #[test]
    fn the_csv_carries_a_header_and_one_line_per_row() {
        let csv = to_csv(&rows());
        let lines: Vec<&str> = csv.lines().collect();

        assert_eq!(lines.len(), 4, "a header plus three rows: {csv}");
        assert!(lines[0].starts_with("benchmark,case,ground_truth_mode"), "{}", lines[0]);
        assert!(lines[3].contains("error"), "a failed row keeps its status: {}", lines[3]);
    }

    #[test]
    fn a_field_holding_a_comma_is_quoted() {
        let row = BenchmarkRow::ok("b", "m", "a,case", 1.0);
        let csv = to_csv(std::slice::from_ref(&row));

        assert!(csv.contains("\"a,case\""), "the comma is quoted: {csv}");
    }

    #[test]
    fn the_markdown_always_ends_with_the_limits_section() {
        let markdown = to_markdown("workspace", &rows());

        assert!(markdown.contains("## Limits"), "the limits section is present");
        assert!(markdown.contains("circular"), "and states the circular ground truth");
        assert!(markdown.contains("approximation"), "and the token approximation");
        assert!(markdown.contains("authored by us"), "and the goldset bias");
    }

    #[test]
    fn a_failed_row_is_listed_rather_than_scored_as_zero() {
        let markdown = to_markdown("workspace", &rows());

        assert!(markdown.contains("Not measured"), "failures get their own list: {markdown}");
        assert!(markdown.contains("no diff"), "with the reason: {markdown}");
    }

    #[test]
    fn a_ground_truth_mode_is_carried_into_both_outputs() {
        let markdown = to_markdown("workspace", &rows());

        assert!(markdown.contains("co-change"), "the mode names itself in the table");
        assert!(to_csv(&rows()).contains("co-change"), "and in its own CSV column");
    }

    #[test]
    fn status_labels_are_distinct() {
        assert_ne!(Status::Ok.as_str(), Status::Error.as_str(), "each status reads distinctly");
    }
}
