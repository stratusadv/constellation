//! The TOML configuration and goldsets a run reads.
//!
//! TOML rather than YAML, matching the `toml` dependency the index crate
//! already carries. Goldsets are version controlled: they are the durable part
//! of a quality harness, and one that lives outside the repository cannot be
//! reviewed alongside the ranking changes it is meant to judge.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The fail-fast bound on goldset entries one run reads.
pub const GOLDSET_ENTRIES_MAX: usize = 2_000;

/// A target repository's configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// The constellation database to evaluate against.
    pub database: PathBuf,
    /// The goldset file, relative to the config or absolute.
    pub goldset: PathBuf,
    /// A human name for the report header.
    pub name: String,
    /// The project id to scope project-scoped benchmarks to. Omit to use the
    /// only indexed project, which most workspaces have.
    #[serde(default)]
    pub project: Option<String>,
    /// The commits `token_efficiency` and `impact_accuracy` sample.
    #[serde(default = "default_commits")]
    pub commits_max: u32,
}

/// The default number of commits the history-driven benchmarks sample.
fn default_commits() -> u32 {
    50
}

/// A curated retrieval question: a query and the qualified name a correct
/// answer must surface.
#[derive(Debug, Deserialize)]
pub struct Question {
    /// The query, phrased the way an agent would phrase it.
    pub query: String,
    /// The qualified name (or a distinctive suffix of one) a correct answer
    /// must return.
    pub expected: String,
    /// The graph hops between the answer and the query's obvious anchor. One
    /// for a direct lookup; two or three for a multi-hop question.
    #[serde(default = "default_hops")]
    pub hops: u32,
}

/// The default hop count for a question that does not state one.
fn default_hops() -> u32 {
    1
}

/// A parsed goldset.
#[derive(Debug, Default, Deserialize)]
pub struct Goldset {
    #[serde(default)]
    pub question: Vec<Question>,
}

/// The reasons a configuration or goldset could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Io(std::io::Error, PathBuf),
    /// The file was not valid TOML, or did not match the expected shape.
    Parse(String, PathBuf),
    /// The goldset held more entries than [`GOLDSET_ENTRIES_MAX`].
    TooLarge(usize, PathBuf),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(error, path) => write!(formatter, "reading {}: {error}", path.display()),
            ConfigError::Parse(message, path) => {
                write!(formatter, "parsing {}: {message}", path.display())
            }
            ConfigError::TooLarge(count, path) => write!(
                formatter,
                "{} holds {count} goldset entries; at most {GOLDSET_ENTRIES_MAX} are read",
                path.display(),
            ),
        }
    }
}

/// The configuration at `path`.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text =
        std::fs::read_to_string(path).map_err(|error| ConfigError::Io(error, path.to_path_buf()))?;

    toml::from_str(&text).map_err(|error| ConfigError::Parse(error.to_string(), path.to_path_buf()))
}

/// The goldset a config names, resolved relative to the config's own directory
/// when the path is relative.
pub fn load_goldset(config: &Config, config_path: &Path) -> Result<Goldset, ConfigError> {
    let path = resolve_relative(config_path, &config.goldset);

    let text = std::fs::read_to_string(&path)
        .map_err(|error| ConfigError::Io(error, path.clone()))?;

    let goldset: Goldset = toml::from_str(&text)
        .map_err(|error| ConfigError::Parse(error.to_string(), path.clone()))?;

    if goldset.question.len() > GOLDSET_ENTRIES_MAX {
        return Err(ConfigError::TooLarge(goldset.question.len(), path));
    }

    Ok(goldset)
}

/// A path resolved against the directory holding `anchor`, or returned as-is
/// when already absolute.
pub fn resolve_relative(anchor: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    match anchor.parent() {
        Some(directory) => directory.join(path),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Goldset, resolve_relative};

    use std::path::Path;

    #[test]
    fn a_relative_goldset_resolves_against_its_config() {
        assert_eq!(
            resolve_relative(Path::new("eval/configs/workspace.toml"), Path::new("../goldsets/w.toml")),
            Path::new("eval/configs/../goldsets/w.toml"),
        );
    }

    #[test]
    fn an_absolute_goldset_is_taken_as_written() {
        let absolute = Path::new("/srv/goldsets/w.toml");

        assert_eq!(resolve_relative(Path::new("eval/configs/w.toml"), absolute), absolute);
    }

    #[test]
    fn a_question_without_a_hop_count_defaults_to_one() {
        let goldset: Goldset = toml::from_str(
            "[[question]]\nquery = \"order number\"\nexpected = \"generate_order_number\"\n",
        )
        .unwrap();

        assert_eq!(goldset.question.len(), 1);
        assert_eq!(goldset.question[0].hops, 1, "an unstated hop count is a direct lookup");
    }

    #[test]
    fn an_empty_goldset_parses_rather_than_erroring() {
        let goldset: Goldset = toml::from_str("").unwrap();

        assert!(goldset.question.is_empty(), "an empty goldset is a valid, if useless, one");
    }
}
