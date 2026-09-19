//! Config command - Configuration loading and discovery
//!
//! This module provides utilities for loading extraction configuration from files
//! or discovering them automatically in the project directory.

use anyhow::{Context, Result};
use std::path::PathBuf;
use xberg::ExtractionConfig;

/// Loads extraction configuration from a file or discovers it automatically.
///
/// This function implements the CLI's configuration hierarchy:
/// 1. Explicit config file (if `--config` flag provided)
/// 2. Auto-discovered config, unless `discover` is `false`
/// 3. Default configuration (if no config file found)
///
/// A config file found by (1) or (2) is merged on top of `cli_default_config`
/// (`ExtractionConfig::from_file_over`/`discover_over`), so it overrides only the keys it
/// sets; fields it omits keep the CLI's own defaults.
///
/// # Configuration File Formats
///
/// Supports three formats, determined by file extension:
/// - `.toml`: TOML format (recommended for humans)
/// - `.yaml` / `.yml`: YAML format
/// - `.json`: JSON format
///
/// # Errors
///
/// Returns an error if:
/// - Explicit config file has unsupported extension (must be .toml, .yaml, .yml, or .json)
/// - Config file cannot be read or parsed
/// - Config file contains invalid extraction settings
pub fn load_config(config_path: Option<PathBuf>, discover: bool) -> Result<ExtractionConfig> {
    let base = cli_default_config();
    if let Some(path) = config_path {
        ExtractionConfig::from_file_over(&base, &path).with_context(|| format!("Failed to load configuration from '{}'. Ensure the file exists, is readable, and contains valid configuration.", path.display()))
    } else if discover {
        match ExtractionConfig::discover_over(&base) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Ok(base),
            Err(e) => Err(e).context("Failed to auto-discover configuration file. Searched for xberg.{toml,yaml,json} in current and parent directories. Use --config to specify an explicit path."),
        }
    } else {
        Ok(base)
    }
}

/// The CLI's own default configuration.
///
/// `output_format` stays `Plain`: `xberg extract`/`xberg batch` print the extracted text unless a
/// CLI flag, inline JSON or a config file asks for a rendered format, which is what the help text
/// and the existing CLI tests assert. The library-wide default is Markdown, so the CLI pins its
/// own here. Config files are merged *on top of* this base (`from_file_over`/`discover_over`), so
/// a file that omits `output_format` keeps `Plain` and only a value written in the file — or a
/// flag applied later — changes it.
fn cli_default_config() -> ExtractionConfig {
    ExtractionConfig {
        output_format: xberg::OutputFormat::Plain,
        ..ExtractionConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a config file the CLI starts from its own defaults, where content stays plain:
    /// the library default is Markdown, and a value merged from `--config-json` or a config file
    /// has to remain distinguishable from it.
    #[test]
    fn load_config_without_any_source_keeps_content_plain() {
        let config = load_config(None, false).expect("defaults must load");
        assert_eq!(config.output_format, xberg::OutputFormat::Plain);
    }

    /// A config file that does not mention `output_format` must not silently switch the CLI
    /// from plain text to the library's Markdown default.
    #[test]
    fn load_config_with_file_without_output_format_keeps_content_plain() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("xberg.toml");
        std::fs::write(&path, "use_cache = false\n").expect("write config");

        let config = load_config(Some(path), false).expect("config file must load");
        assert_eq!(config.output_format, xberg::OutputFormat::Plain);
        assert!(!config.use_cache, "the file's own keys must still apply");
    }

    /// An `output_format` written in the config file still wins over the CLI's pin.
    #[test]
    fn load_config_with_file_output_format_wins_over_pin() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("xberg.yaml");
        std::fs::write(&path, "output_format: markdown\n").expect("write config");

        let config = load_config(Some(path), false).expect("config file must load");
        assert_eq!(config.output_format, xberg::OutputFormat::Markdown);
    }
}
