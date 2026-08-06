//! `tools`: the advertised MCP tool surface, printed for a person.

use anyhow::{Result, bail};
use constellation_mcp::ConstellationServer;

/// The indent under each tool name.
const DESCRIPTION_INDENT: &str = "    ";

/// The column at which a description line wraps, matching the source line limit.
const LINE_WIDTH_MAX: usize = 100;

/// The `constellation tools` command lists every MCP tool the server advertises,
/// each with its description, so the tool surface can be read without speaking
/// JSON-RPC over stdio. The list comes from the same router `serve` answers
/// `tools/list` from, so it cannot drift from what an agent is offered.
pub(crate) fn tools_command(rest: &[String]) -> Result<()> {
    if !rest.is_empty() {
        bail!("`tools` takes no arguments. Run `constellation --help` for usage");
    }

    let mut tools = ConstellationServer::tool_router().list_all();

    assert!(!tools.is_empty(), "the server advertises at least one tool");

    tools.sort_by(|a, b| a.name.cmp(&b.name));

    println!("{} tools", tools.len());

    let width = LINE_WIDTH_MAX - DESCRIPTION_INDENT.len();

    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or("");

        assert!(!description.is_empty(), "every advertised tool is described");

        println!();
        println!("{}", tool.name);

        for line in wrap(description, width) {
            println!("{DESCRIPTION_INDENT}{line}");
        }
    }

    Ok(())
}

/// The text broken into lines of at most `width` bytes, split at spaces. A word
/// longer than `width` gets a line of its own rather than being cut mid-word.
fn wrap(text: &str, width: usize) -> Vec<String> {
    assert!(width > 0, "a zero wrap width would loop one word per line forever");

    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }

        if !line.is_empty() {
            line.push(' ');
        }

        line.push_str(word);
    }

    if !line.is_empty() {
        lines.push(line);
    }

    assert!(lines.iter().all(|line| !line.is_empty()));

    lines
}

#[cfg(test)]
mod tests {
    use super::wrap;

    #[test]
    fn short_text_stays_on_one_line() {
        assert_eq!(wrap("one two three", 80), vec!["one two three"]);
    }

    #[test]
    fn lines_break_at_spaces_within_the_width() {
        let lines = wrap("alpha beta gamma delta", 11);

        assert_eq!(lines, vec!["alpha beta", "gamma delta"]);

        for line in &lines {
            assert!(line.len() <= 11, "{line:?} exceeds the wrap width");
        }
    }

    #[test]
    fn a_word_longer_than_the_width_gets_its_own_line() {
        let lines = wrap("a incomprehensibilities b", 10);

        assert_eq!(lines, vec!["a", "incomprehensibilities", "b"]);
    }

    #[test]
    fn empty_text_produces_no_lines() {
        assert!(wrap("", 80).is_empty());
        assert!(wrap("   ", 80).is_empty());
    }
}
