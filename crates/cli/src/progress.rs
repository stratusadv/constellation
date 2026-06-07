use std::fmt::Write as _;
use std::io::{IsTerminal, Write};

use constellation_index::IndexPhase;

/// The width of the gradient bar, in cells.
const BAR_WIDTH: u32 = 28;

/// The gradient endpoints (sRGB) for the filled bar: violet to cyan.
const GRADIENT_START: (u8, u8, u8) = (139, 92, 246);
const GRADIENT_END: (u8, u8, u8) = (34, 211, 238);

/// The spinner frames, sparse to dense, matching the indexing motion.
const SPINNER_UNICODE: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽"];
const SPINNER_ASCII: &[&str] = &[".", "*", "+", "x", "o", "O"];

/// A single-line gradient progress bar drawn to stderr and redrawn in place. A
/// no-op when stderr is not a terminal, so piped or redirected output stays
/// clean (stdout, which carries the command's summary, is never touched).
pub struct Progress {
    enabled: bool,
    unicode: bool,
    label: String,
    spinner_frame: u32,
    last_filled: u32,
    drawn: bool,
}

impl Progress {
    /// A progress bar with the given label, auto-detecting terminal and Unicode support.
    pub fn new(label: &str) -> Self {
        assert!(!label.is_empty(), "progress label must not be empty");

        Self {
            enabled: std::io::stderr().is_terminal(),
            unicode: supports_unicode(),
            label: label.to_string(),
            spinner_frame: 0,
            last_filled: u32::MAX,
            drawn: false,
        }
    }

    /// The render of the bar (or the resolving spinner) for one indexing phase event.
    pub fn on_phase(&mut self, phase: IndexPhase) {
        match phase {
            IndexPhase::Extracting { files_done, files_total } => {
                self.draw_bar(files_done, files_total);
            }
            IndexPhase::Resolving => self.draw_spinner("resolving references"),
        }
    }

    /// The clear of the bar line so the caller's summary prints on a clean row.
    pub fn finish(&mut self) {
        if !self.enabled || !self.drawn {
            return;
        }

        let mut stderr = std::io::stderr();

        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();

        self.drawn = false;
    }

    fn draw_bar(&mut self, files_done: u32, files_total: u32) {
        assert!(files_done <= files_total, "progress cannot exceed the total");

        if !self.enabled {
            return;
        }

        let filled = if files_total == 0 {
            BAR_WIDTH
        } else {
            (u64::from(files_done) * u64::from(BAR_WIDTH) / u64::from(files_total)) as u32
        };

        let filled = filled.min(BAR_WIDTH);

        if self.drawn && filled == self.last_filled {
            return;
        }

        self.last_filled = filled;
        self.spinner_frame = self.spinner_frame.wrapping_add(1);

        let percent = if files_total == 0 {
            100
        } else {
            (u64::from(files_done) * 100 / u64::from(files_total)) as u32
        };

        let rail = if self.unicode { "│" } else { "|" };
        let bar = self.gradient_bar(filled);
        let spinner = self.spinner();
        let label = &self.label;

        let mut stderr = std::io::stderr();

        let _ = write!(
            stderr,
            "\r\x1b[K\x1b[2m{rail}\x1b[0m  {spinner} {label}  {bar}  \
             {percent:>3}%  ({files_done}/{files_total})",
        );
        let _ = stderr.flush();

        self.drawn = true;
    }

    fn draw_spinner(&mut self, message: &str) {
        assert!(!message.is_empty(), "spinner message must not be empty");

        if !self.enabled {
            return;
        }

        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        self.last_filled = u32::MAX;

        let rail = if self.unicode { "│" } else { "|" };
        let ellipsis = if self.unicode { "…" } else { "..." };
        let spinner = self.spinner();

        let mut stderr = std::io::stderr();

        let _ = write!(stderr, "\r\x1b[K\x1b[2m{rail}\x1b[0m  {spinner} {message}{ellipsis}");
        let _ = stderr.flush();

        self.drawn = true;
    }

    fn gradient_bar(&self, filled: u32) -> String {
        assert!(filled <= BAR_WIDTH, "filled cells stay within the bar width");

        let filled_glyph = if self.unicode { "█" } else { "#" };
        let empty_glyph = if self.unicode { "░" } else { "-" };

        let span = (BAR_WIDTH - 1).max(1) as f32;
        let mut bar = String::with_capacity(BAR_WIDTH as usize * 24);

        for cell in 0..filled {
            let fraction = cell as f32 / span;
            let (red, green, blue) = lerp_color(GRADIENT_START, GRADIENT_END, fraction);

            // Write the escape directly into the bar instead of allocating a
            // temporary String per cell.
            let _ = write!(bar, "\x1b[38;2;{red};{green};{blue}m{filled_glyph}");
        }

        bar.push_str("\x1b[0m\x1b[2m");

        for _ in filled..BAR_WIDTH {
            bar.push_str(empty_glyph);
        }

        bar.push_str("\x1b[0m");

        bar
    }

    fn spinner(&self) -> &'static str {
        let frames = if self.unicode { SPINNER_UNICODE } else { SPINNER_ASCII };

        frames[self.spinner_frame as usize % frames.len()]
    }
}

/// The interpolation between two sRGB colors at `fraction` in `[0, 1]`.
fn lerp_color(start: (u8, u8, u8), end: (u8, u8, u8), fraction: f32) -> (u8, u8, u8) {
    let fraction = fraction.clamp(0.0, 1.0);

    let channel = |from: u8, to: u8| -> u8 {
        let from = f32::from(from);
        let to = f32::from(to);

        (from + (to - from) * fraction).round() as u8
    };

    (
        channel(start.0, end.0),
        channel(start.1, end.1),
        channel(start.2, end.2),
    )
}

/// Whether to use Unicode bar/spinner glyphs. Truecolor gradients work in
/// modern terminals; the block glyphs are the risk, rendering as mojibake under
/// legacy Windows OEM codepages, so default to ASCII off Windows Terminal.
fn supports_unicode() -> bool {
    if std::env::var_os("CONSTELLATION_ASCII").is_some() {
        return false;
    }

    if std::env::var_os("CONSTELLATION_UNICODE").is_some() {
        return true;
    }

    if cfg!(windows) {
        return std::env::var_os("WT_SESSION").is_some();
    }

    std::env::var("TERM").map(|term| term != "linux").unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::{BAR_WIDTH, GRADIENT_END, GRADIENT_START, Progress, lerp_color};

    use constellation_index::IndexPhase;

    #[test]
    fn lerp_color_returns_the_endpoints_exactly() {
        assert_eq!(
            lerp_color(GRADIENT_START, GRADIENT_END, 0.0),
            GRADIENT_START,
            "fraction zero is the start color untouched",
        );
        assert_eq!(
            lerp_color(GRADIENT_START, GRADIENT_END, 1.0),
            GRADIENT_END,
            "fraction one is the end color untouched",
        );
    }

    #[test]
    fn lerp_color_clamps_fractions_outside_the_unit_range() {
        assert_eq!(
            lerp_color(GRADIENT_START, GRADIENT_END, -1.0),
            GRADIENT_START,
            "a negative fraction clamps back to the start",
        );
        assert_eq!(
            lerp_color(GRADIENT_START, GRADIENT_END, 2.0),
            GRADIENT_END,
            "a fraction past one clamps to the end",
        );
    }

    #[test]
    fn lerp_color_moves_each_channel_toward_its_endpoint() {
        let early = lerp_color(GRADIENT_START, GRADIENT_END, 0.25);
        let late = lerp_color(GRADIENT_START, GRADIENT_END, 0.75);

        assert!(early.0 > late.0, "red descends from 139 to 34, got {} then {}", early.0, late.0);
        assert!(early.1 < late.1, "green climbs from 92 to 211, got {} then {}", early.1, late.1);
    }

    #[test]
    fn progress_new_starts_undrawn_with_its_label() {
        let progress = Progress::new("indexing");

        assert_eq!(progress.label, "indexing", "the label is stored verbatim");
        assert_eq!(progress.spinner_frame, 0, "the spinner starts at its first frame");
        assert!(!progress.drawn, "nothing is drawn until a phase arrives");
    }

    #[test]
    #[should_panic(expected = "label must not be empty")]
    fn progress_new_rejects_an_empty_label() {
        let _ = Progress::new("");
    }

    #[test]
    #[should_panic(expected = "cannot exceed the total")]
    fn on_phase_rejects_progress_past_the_total() {
        // The bound is checked before the terminal gate, so it fires whether or
        // not stderr is a terminal, keeping this test independent of the runner.
        let mut progress = Progress::new("indexing");

        progress.on_phase(IndexPhase::Extracting { files_done: 5, files_total: 3 });
    }

    #[test]
    fn gradient_bar_colors_exactly_the_filled_cells() {
        let progress = Progress::new("indexing");
        let colored = |filled: u32| -> usize { progress.gradient_bar(filled).matches("\u{1b}[38;2;").count() };

        assert_eq!(colored(0), 0, "an empty bar colors no cells");
        assert_eq!(colored(5), 5, "each filled cell gets its own truecolor escape");
        assert_eq!(colored(BAR_WIDTH), BAR_WIDTH as usize, "a full bar colors every cell");
    }

    #[test]
    #[should_panic(expected = "stay within the bar width")]
    fn gradient_bar_rejects_overfill() {
        let progress = Progress::new("indexing");

        progress.gradient_bar(BAR_WIDTH + 1);
    }
}
