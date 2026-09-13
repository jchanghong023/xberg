//! Configuration file loading.
//!
//! This module provides methods for loading extraction configuration from
//! TOML, YAML, and JSON files.
//!
//! Loading here is entirely generic (`toml`/`serde_yaml_ng`/`serde_json` deserializing
//! straight into `ExtractionConfig`). New `#[serde(default)]` fields on nested config
//! types are picked up automatically with no change required in this file.
//!
//! The `*_over` variants load a file *on top of* an existing base configuration by merging
//! the file's raw values as JSON (see [`merge_config_json`]), so a partial file overrides
//! only the keys it sets instead of resetting everything to the library defaults.

use crate::core::config::merge::merge_config_json;
use crate::{Result, XbergError};
use std::path::{Path, PathBuf};

use super::core::ExtractionConfig;

impl ExtractionConfig {
    /// Load configuration from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns `XbergError::Validation` if file doesn't exist or is invalid TOML.
    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| XbergError::validation(format!("Failed to read config file {}: {}", path.display(), e)))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| XbergError::validation(format!("Invalid TOML in {}: {}", path.display(), e)))?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a YAML file.
    pub fn from_yaml_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| XbergError::validation(format!("Failed to read config file {}: {}", path.display(), e)))?;
        let config: Self = serde_yaml_ng::from_str(&content)
            .map_err(|e| XbergError::validation(format!("Invalid YAML in {}: {}", path.display(), e)))?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a JSON file.
    pub fn from_json_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| XbergError::validation(format!("Failed to read config file {}: {}", path.display(), e)))?;
        let config: Self = serde_json::from_str(&content)
            .map_err(|e| XbergError::validation(format!("Invalid JSON in {}: {}", path.display(), e)))?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a file, auto-detecting format by extension.
    ///
    /// Supported formats: `.toml`, `.yaml`, `.yml`, `.json`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let extension = path.extension().and_then(|ext| ext.to_str()).ok_or_else(|| {
            XbergError::validation(format!(
                "Cannot determine file format: no extension found in {}",
                path.display()
            ))
        })?;

        match extension.to_lowercase().as_str() {
            "toml" => Self::from_toml_file(path),
            "yaml" | "yml" => Self::from_yaml_file(path),
            "json" => Self::from_json_file(path),
            other => Err(XbergError::validation(format!(
                "Unsupported config file format: .{}. Supported formats: .toml, .yaml, .json",
                other
            ))),
        }
    }

    /// Load configuration from a file on top of an existing base configuration.
    ///
    /// Unlike [`Self::from_file`], the file is not deserialized straight into
    /// `ExtractionConfig`: it is parsed into its format's raw value, converted to JSON, and
    /// merged into `base` with [`merge_config_json`]. Only the keys the file actually sets
    /// override `base`, so a file that omits `output_format` (or any other field) keeps the
    /// base's value instead of falling back to the library default. Unknown fields are still
    /// rejected, because the merge deserializes back into the `#[serde(deny_unknown_fields)]`
    /// `ExtractionConfig`.
    ///
    /// Supported formats: `.toml`, `.yaml`, `.yml`, `.json`.
    ///
    /// # Errors
    ///
    /// Returns `XbergError::Validation` if the file cannot be read or parsed, if its document
    /// root is not a table/object of settings, or if the merged configuration is invalid.
    pub fn from_file_over(base: &Self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let extension = path.extension().and_then(|ext| ext.to_str()).ok_or_else(|| {
            XbergError::validation(format!(
                "Cannot determine file format: no extension found in {}",
                path.display()
            ))
        })?;
        let extension_lower = extension.to_lowercase();

        if !matches!(extension_lower.as_str(), "toml" | "yaml" | "yml" | "json") {
            return Err(XbergError::validation(format!(
                "Unsupported config file format: .{}. Supported formats: .toml, .yaml, .json",
                extension_lower
            )));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| XbergError::validation(format!("Failed to read config file {}: {}", path.display(), e)))?;
        let json = match extension_lower.as_str() {
            "toml" => {
                let value: toml::Value = toml::from_str(&content)
                    .map_err(|e| XbergError::validation(format!("Invalid TOML in {}: {}", path.display(), e)))?;
                raw_value_to_json(value, "TOML", path)?
            }
            "yaml" | "yml" => {
                let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content)
                    .map_err(|e| XbergError::validation(format!("Invalid YAML in {}: {}", path.display(), e)))?;
                raw_value_to_json(value, "YAML", path)?
            }
            _ => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| XbergError::validation(format!("Invalid JSON in {}: {}", path.display(), e)))?;
                raw_value_to_json(value, "JSON", path)?
            }
        };

        merge_config_json(base, &json)
            .map_err(|e| XbergError::validation(format!("Invalid configuration in {}: {}", path.display(), e)))
    }

    /// Discover configuration file.
    ///
    /// Searches for `xberg.toml` in the current directory and its parents. If no
    /// project-local config is found, falls back to a per-user global config in
    /// the platform config directory: `xberg/xberg.{toml,yaml,yml,json}` under
    /// `dirs::config_dir()` — i.e. `$XDG_CONFIG_HOME` (or `~/.config`) on Linux,
    /// `~/Library/Application Support` on macOS, `%APPDATA%` on Windows.
    pub fn discover() -> Result<Option<Self>> {
        match Self::discover_path()? {
            Some(path) => Ok(Some(Self::from_file(path)?)),
            None => Ok(None),
        }
    }

    /// Discover configuration file and load it on top of an existing base configuration.
    ///
    /// Searches the same locations as [`Self::discover`], but merges the discovered file into
    /// `base` (see [`Self::from_file_over`]) instead of replacing it.
    pub fn discover_over(base: &Self) -> Result<Option<Self>> {
        match Self::discover_path()? {
            Some(path) => Ok(Some(Self::from_file_over(base, path)?)),
            None => Ok(None),
        }
    }

    /// Path of the configuration file [`Self::discover`] would load, if any.
    fn discover_path() -> Result<Option<PathBuf>> {
        let mut current = std::env::current_dir().map_err(crate::XbergError::from)?;

        loop {
            let xberg_toml = current.join("xberg.toml");
            if xberg_toml.exists() {
                return Ok(Some(xberg_toml));
            }

            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
            } else {
                break;
            }
        }

        if let Some(config_dir) = dirs::config_dir() {
            return Ok(Self::find_config_in_dir(&config_dir.join("xberg")));
        }

        Ok(None)
    }

    /// Path of the first `xberg.{toml,yaml,yml,json}` present in `dir`, if any.
    ///
    /// Extensions are probed in a fixed order so discovery is deterministic when
    /// multiple config files coexist in the same directory.
    fn find_config_in_dir(dir: &Path) -> Option<PathBuf> {
        const CONFIG_BASENAMES: [&str; 4] = ["xberg.toml", "xberg.yaml", "xberg.yml", "xberg.json"];

        CONFIG_BASENAMES.iter().map(|basename| dir.join(basename)).find(|candidate| candidate.exists())
    }
}

/// Convert a parsed TOML/YAML/JSON document into a JSON object string for [`merge_config_json`].
///
/// A configuration file must hold a table/mapping/object at the document root; any other shape is
/// rejected here rather than silently merging nothing. `syntax` names the source syntax so the
/// message matches the parse errors raised beside it.
fn raw_value_to_json<T: serde::Serialize>(value: T, syntax: &str, path: &Path) -> Result<String> {
    let json = serde_json::to_value(value)
        .map_err(|e| XbergError::validation(format!("Invalid {syntax} in {}: {}", path.display(), e)))?;

    if !json.is_object() {
        return Err(XbergError::validation(format!(
            "Invalid {syntax} in {}: expected a table of configuration settings",
            path.display()
        )));
    }

    serde_json::to_string(&json)
        .map_err(|e| XbergError::validation(format!("Invalid {syntax} in {}: {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_config_in_dir_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let found = ExtractionConfig::find_config_in_dir(dir.path());
        assert!(found.is_none(), "empty dir must yield no config");
    }

    #[test]
    fn find_config_in_dir_probes_yaml_and_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("xberg.json"), "{}").unwrap();
        let found = ExtractionConfig::find_config_in_dir(dir.path()).expect("xberg.json must be discovered");
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "xberg.json");

        std::fs::remove_file(dir.path().join("xberg.json")).unwrap();
        std::fs::write(dir.path().join("xberg.yaml"), "use_cache: true\n").unwrap();
        let found = ExtractionConfig::find_config_in_dir(dir.path()).expect("xberg.yaml must be discovered");
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "xberg.yaml");
    }

    #[test]
    fn find_config_in_dir_prefers_toml_over_other_formats() {
        let dir = tempfile::tempdir().unwrap();
        // A valid TOML file and a deliberately invalid JSON file coexist. TOML is
        // probed first, so the path finder must return it without reading the JSON. ~keep
        std::fs::write(dir.path().join("xberg.toml"), "use_cache = true\n").unwrap();
        std::fs::write(dir.path().join("xberg.json"), "not valid json").unwrap();

        let found = ExtractionConfig::find_config_in_dir(dir.path()).expect("xberg.toml must win over xberg.json");
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "xberg.toml");
    }

    #[test]
    fn from_file_over_keeps_base_fields_the_file_omits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xberg.toml");
        std::fs::write(&path, "use_cache = false\n").unwrap();

        let base = ExtractionConfig {
            output_format: crate::OutputFormat::Plain,
            ..ExtractionConfig::default()
        };
        let loaded = ExtractionConfig::from_file_over(&base, &path).unwrap();

        assert_eq!(
            loaded.output_format,
            crate::OutputFormat::Plain,
            "a file without output_format must not reset the base's value to the library default"
        );
        assert!(!loaded.use_cache, "the file's own keys must still apply");
        assert!(base.use_cache, "the base must not be mutated");
    }

    #[test]
    fn from_file_over_lets_the_file_output_format_win() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xberg.yaml");
        std::fs::write(&path, "output_format: markdown\n").unwrap();

        let base = ExtractionConfig {
            output_format: crate::OutputFormat::Plain,
            ..ExtractionConfig::default()
        };
        let loaded = ExtractionConfig::from_file_over(&base, &path).unwrap();

        assert_eq!(loaded.output_format, crate::OutputFormat::Markdown);
    }

    #[test]
    fn from_file_over_rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xberg.json");
        std::fs::write(&path, r#"{"definitely_not_a_field": 1}"#).unwrap();

        let error = ExtractionConfig::from_file_over(&ExtractionConfig::default(), &path)
            .expect_err("an unknown field must still make config loading fail");
        assert!(
            error.to_string().contains("definitely_not_a_field"),
            "error must name the unknown field: {error}"
        );
    }

    #[test]
    fn from_toml_file_rejects_unknown_nested_fields() {
        let cases = [
            (
                "max_archive_bytes",
                "[security_limits]\nmax_archive_bytes = 1024\n",
                true,
            ),
            ("backnd", "[ocr]\nbacknd = \"tesseract\"\n", true),
            (
                "psmm",
                "[ocr]\nbackend = \"tesseract\"\n[ocr.tesseract_config]\npsmm = 6\n",
                true,
            ),
            (
                "deskww",
                "[ocr]\nbackend = \"tesseract\"\n[ocr.tesseract_config.preprocessing]\ndeskww = true\n",
                true,
            ),
            (
                "min_confidence",
                "[ocr_strategy]\nmode = \"auto\"\nmin_confidence = 0.95\n",
                true,
            ),
            (
                "quality_threshold",
                "[ocr]\n[ocr.vlm_fallback]\nmode = \"disabled\"\nquality_threshold = 0.8\n",
                true,
            ),
            (
                "pdf_backend",
                "[pdf_options]\npdf_backend = \"native\"\n",
                cfg!(feature = "pdf"),
            ),
        ];

        for (unknown_field, source, enabled) in cases {
            if !enabled {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("xberg.toml");
            std::fs::write(&path, source).unwrap();

            let error = ExtractionConfig::from_toml_file(&path)
                .expect_err("an unknown nested field must make config loading fail");
            let message = error.to_string();
            assert!(
                message.contains("Invalid TOML"),
                "wrong error for {unknown_field}: {message}"
            );
            assert!(
                message.contains(unknown_field),
                "error must name {unknown_field}: {message}"
            );
        }
    }
}
