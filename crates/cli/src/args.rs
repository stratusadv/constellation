//! Argument reading for the hand-rolled command line.
//!
//! There is no argument-parsing dependency here on purpose. The surface is a
//! subcommand, one optional positional path, and a handful of flags, which is
//! small enough that a parser crate would cost more in build time and API
//! surface than it saves.

/// The flags that stand alone. Every other `--flag` is assumed to take the token
/// after it, so a switch must be listed here or the positional following it is
/// swallowed as its value: `serve --supervise db.sqlite` would lose the database.
const BOOLEAN_FLAGS: &[&str] = &["--no-hooks", "--supervise", "--symbols", "--worker"];

/// The value following `flag` in the argument list, or `None` when the flag is
/// absent or trails with no value.
pub(crate) fn flag_value(rest: &[String], flag: &str) -> Option<String> {
    let position = rest.iter().position(|argument| argument == flag)?;

    rest.get(position + 1).filter(|value| !value.starts_with("--")).cloned()
}

/// The first argument that is neither a flag nor a flag's value.
pub(crate) fn positional(rest: &[String]) -> Option<&String> {
    let mut skip_next = false;

    for (index, argument) in rest.iter().enumerate() {
        if skip_next {
            skip_next = false;

            continue;
        }

        if argument.starts_with("--") {
            let takes_value = !BOOLEAN_FLAGS.contains(&argument.as_str());

            skip_next = takes_value
                && rest.get(index + 1).is_some_and(|next| !next.starts_with("--"));

            continue;
        }

        return Some(argument);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{flag_value, positional};

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_switch_never_swallows_the_path_after_it() {
        let after = arguments(&["--supervise", "db.sqlite"]);
        let before = arguments(&["db.sqlite", "--supervise"]);

        assert_eq!(positional(&after).map(String::as_str), Some("db.sqlite"));
        assert_eq!(positional(&before).map(String::as_str), Some("db.sqlite"));
    }

    #[test]
    fn a_value_taking_flag_still_hides_its_value() {
        let rest = arguments(&["--project", "orders", "index.db"]);

        assert_eq!(positional(&rest).map(String::as_str), Some("index.db"));
        assert_eq!(flag_value(&rest, "--project"), Some("orders".to_string()));
    }

    #[test]
    fn a_trailing_flag_has_no_value_and_leaves_no_positional() {
        let rest = arguments(&["--project"]);

        assert_eq!(flag_value(&rest, "--project"), None);
        assert_eq!(positional(&rest), None);
    }
}
