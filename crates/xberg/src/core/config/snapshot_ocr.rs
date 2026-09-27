//! Snapshot (screenshot) OCR channel configuration.
//!
//! This block configures the *screenshot* OCR channel — the standalone
//! second OCR capability described in `docs/requirements/OCR-SNAPSHOT.md`
//! (SNAP-01/SNAP-02). It is intentionally separate from the document OCR
//! [`super::ocr::OcrConfig`] block: the two channels never share models,
//! sessions, or backend selection (SNAP-04).
//!
//! The block only carries startup settings (model root and intra-op thread
//! count). Resolution order for the model root when it is needed:
//! explicit config (this field) → `XBERG_SNAPSHOT_MODEL_DIR` environment
//! variable → `<exe directory>/models/snapshot-ocr`. When nothing resolves,
//! the consumer reports all three search locations in its error.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default ONNX Runtime intra-op thread count for the snapshot channel
/// (the reference JchTools TextSnap engine config, OCR-SNAPSHOT SNAP-07).
pub const DEFAULT_INTRA_THREADS: usize = 10;

/// Configuration for the snapshot OCR channel (`snapshot_ocr` top-level block).
///
/// Absent block = channel unused; presence alone triggers nothing — the
/// channel only runs when an explicit `ocr_snapshot` request or the
/// `snapshot-ocr` CLI subcommand asks for it (SNAP-04: explicit selection,
/// no automatic fallback).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotOcrConfig {
    /// Model root directory holding the pinned
    /// `snapshot-pp-ocrv6-small-textsnap` set: `det.onnx`, `rec.onnx`, and
    /// `dict/dict.txt`. When `None`, `XBERG_SNAPSHOT_MODEL_DIR` and
    /// `<exe directory>/models/snapshot-ocr` are tried, in that order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<PathBuf>,

    /// ONNX Runtime intra-op thread count for det/rec sessions.
    /// Default: [`DEFAULT_INTRA_THREADS`] (10).
    #[serde(default = "default_intra_threads")]
    pub intra_threads: usize,
}

impl Default for SnapshotOcrConfig {
    fn default() -> Self {
        Self {
            models_dir: None,
            intra_threads: DEFAULT_INTRA_THREADS,
        }
    }
}

fn default_intra_threads() -> usize {
    DEFAULT_INTRA_THREADS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absent block keeps `intra_threads` at the reference value (10).
    #[test]
    fn default_config_uses_reference_intra_threads() {
        let config = SnapshotOcrConfig::default();
        assert_eq!(config.intra_threads, 10);
        assert!(config.models_dir.is_none());
    }

    /// Round-trip with both fields set, unknown keys rejected.
    #[test]
    fn serde_roundtrip_and_unknown_field_rejection() {
        let config: SnapshotOcrConfig =
            serde_json::from_str(r#"{"models_dir": "E:/models/snapshot", "intra_threads": 4}"#)
                .expect("full block must parse");
        assert_eq!(
            config.models_dir.as_deref(),
            Some(std::path::Path::new("E:/models/snapshot"))
        );
        assert_eq!(config.intra_threads, 4);

        let back = serde_json::to_string(&config).expect("serializes");
        assert!(back.contains("\"intra_threads\":4"), "unexpected: {back}");

        assert!(
            serde_json::from_str::<SnapshotOcrConfig>(r#"{"nope": 1}"#).is_err(),
            "unknown fields must be rejected"
        );
    }

    /// `intra_threads` defaults to 10 when omitted; `models_dir` may be absent.
    #[test]
    fn serde_defaults_apply_for_missing_fields() {
        let config: SnapshotOcrConfig = serde_json::from_str("{}").expect("empty block parses");
        assert_eq!(config.intra_threads, DEFAULT_INTRA_THREADS);
        assert!(config.models_dir.is_none());
    }

    /// The block is reachable as a top-level `snapshot_ocr` key on
    /// `ExtractionConfig` (worker startup config, SNAP-14).
    #[test]
    fn extraction_config_accepts_the_snapshot_ocr_block() {
        let config: crate::core::config::ExtractionConfig =
            serde_json::from_str(r#"{"snapshot_ocr": {"models_dir": "E:/assets/snapshot-models"}}"#)
                .expect("snapshot_ocr block must parse on ExtractionConfig");
        let block = config.snapshot_ocr.expect("block present");
        assert_eq!(
            block.models_dir.as_deref(),
            Some(std::path::Path::new("E:/assets/snapshot-models"))
        );
        assert_eq!(block.intra_threads, DEFAULT_INTRA_THREADS);

        // Default configs must not serialize the block (wire-shape stability).
        let json = serde_json::to_string(&crate::core::config::ExtractionConfig::default())
            .expect("default config serializes");
        assert!(!json.contains("snapshot_ocr"), "absent block must stay absent: {json}");
    }
}
