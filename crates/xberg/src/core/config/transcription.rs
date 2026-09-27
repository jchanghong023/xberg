//! Transcription configuration for audio/video speech-to-text.
//!
//! This module is behind the `transcription-types` feature for the pure-Rust
//! config structs (safe on WASM/Android) and the `transcription` feature for
//! the full ORT + decode implementation.
//!
//! Design follows the exact established pattern of `EmbeddingConfig`,
//! `LayoutDetectionConfig`, and `PaddleOcrConfig`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for audio/video transcription (speech-to-text).
///
/// When present and `enabled`, Xberg will route audio and video files
/// (mp3, mp4, m4a, wav, webm, etc.) through the transcription pipeline.
///
/// The heavy dependencies (ORT, hf-hub, symphonia) are only pulled when the
/// `transcription` feature is enabled. The config struct itself is available
/// under `transcription-types` so that `ExtractionConfig` round-trips on all
/// targets.
///
/// All fields have sensible defaults. The recommended starting point is:
///
/// ```toml
/// [extraction.transcription]
/// enabled = true
/// model = "tiny"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionConfig {
    /// Master switch. When `false`, the transcription pipeline is not run.
    ///
    /// The extractor is registered for audio/video MIME types whenever the `transcription`
    /// feature is compiled in, independently of this flag, so an audio/video input with
    /// `enabled = false` fails with an `XbergError::Transcription` explaining how to turn
    /// transcription on — it does not fall through to another extractor.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Whisper model size to use.
    ///
    /// Smaller = faster + lower memory. `tiny` is the pragmatic default for
    /// first-time users and CI.
    #[serde(default)]
    pub model: WhisperModel,

    /// Which transcription backend to run.
    ///
    /// `whisper` (default) keeps the existing Whisper ONNX pipeline unchanged.
    /// `sensevoice` routes the input through the JchTools media chain
    /// (FFmpeg DLL decode → Silero VAD → SenseVoice INT8) and emits the
    /// SV-06 Markdown structure. Any other value is rejected by serde with an
    /// "unknown variant" error naming the two valid choices.
    #[serde(default)]
    pub backend: TranscriptionBackend,

    /// Optional model root for the `sensevoice` backend (ignored by Whisper).
    ///
    /// Expected to contain (directly or under `models/`) the SenseVoice
    /// `model.int8.onnx` + `tokens.txt` and the Silero `silero_vad.onnx`.
    /// When unset, `XBERG_SENSEVOICE_MODEL_DIR` and exe-adjacent locations are
    /// tried. Native DLL locations use `XBERG_SHERPA_DLL_DIR` /
    /// `XBERG_FFMPEG_DLL_DIR`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<PathBuf>,

    /// Optional language hint (ISO-639-1 code, e.g. "en", "de").
    ///
    /// When `None` (default), the current engine falls back to English.
    /// For deterministic production output, always set this explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Whether to request segment-level timestamps.
    ///
    /// When `true`, the decoder prompt omits `<|notimestamps|>` so the model emits
    /// `<|x.xx|>` tokens, and each transcript segment becomes its own paragraph element
    /// carrying `start_ms` / `end_ms` attributes. When `false` (default), all segment
    /// text is joined into a single flat paragraph with no timing attributes.
    #[serde(default)]
    pub timestamps: bool,

    /// Hard safety limit on input duration (milliseconds).
    ///
    /// Files longer than this are rejected after decode, before model work.
    /// Default: 30 minutes. Set to `None` to disable (not recommended for
    /// untrusted input).
    #[serde(default = "default_max_duration_ms")]
    pub max_duration_ms: Option<u64>,

    /// Hard safety limit on input size (bytes).
    ///
    /// Default: 512 MiB. Protects against pathological or malicious uploads.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: Option<u64>,

    /// Wall-clock timeout for the entire transcription operation (ms).
    ///
    /// Bounds audio decode, model resolution/download, and inference together. On expiry
    /// the extraction fails with an `XbergError::Transcription`. `None` disables the bound
    /// and lets the operation run unbounded (not recommended for untrusted input).
    ///
    /// Enforced on the async extraction path only; the size and duration caps
    /// (`max_bytes`, `max_duration_ms`) are checked on every path.
    ///
    /// Default: 10 minutes.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: Option<u64>,

    /// Optional alternate Hugging Face cache root for Whisper models.
    ///
    /// When unset, hf-hub follows `HF_HUB_CACHE`, `HUGGINGFACE_HUB_CACHE`,
    /// `HF_HOME`, XDG, and platform defaults. Files remain in the standard
    /// content-addressed snapshot layout and are not copied into an Xberg cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_cache_dir: Option<PathBuf>,

    /// Allow network access to download models from Hugging Face Hub.
    ///
    /// When `false`, only previously cached models may be used. Useful for
    /// air-gapped or fully offline deployments.
    #[serde(default = "default_true")]
    pub allow_network: bool,

    /// Request SHA256 verification of downloaded model files.
    ///
    /// Defaults to `false` because the resolver downloads from mutable Hugging
    /// Face refs unless callers pin and verify models out-of-band. Explicit
    /// `true` requests are rejected by the model resolver until pinned checksum
    /// metadata is available.
    #[serde(default)]
    pub verify_hash: bool,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: WhisperModel::default(),
            backend: TranscriptionBackend::default(),
            model_dir: None,
            language: None,
            timestamps: false,
            max_duration_ms: default_max_duration_ms(),
            max_bytes: default_max_bytes(),
            timeout_ms: default_timeout_ms(),
            model_cache_dir: None,
            allow_network: true,
            verify_hash: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_duration_ms() -> Option<u64> {
    Some(30 * 60 * 1000)
}

fn default_max_bytes() -> Option<u64> {
    Some(512 * 1024 * 1024)
}

fn default_timeout_ms() -> Option<u64> {
    Some(10 * 60 * 1000)
}

/// Supported transcription backends.
///
/// Serialized lowercase (`"whisper"` / `"sensevoice"`); serde rejects any
/// other value with an "unknown variant" error naming both valid choices, so
/// typos fail loudly instead of silently falling back to Whisper.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionBackend {
    /// Whisper ONNX pipeline (existing default, unchanged).
    #[default]
    Whisper,
    /// SenseVoice INT8 via sherpa-onnx + FFmpeg DLL decode + Silero VAD.
    SenseVoice,
}

/// Supported Whisper model sizes.
///
/// These map to published ONNX exports on Hugging Face (onnx-community or
/// similar orgs). The actual filenames and repos are resolved inside the
/// transcription engine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WhisperModel {
    /// Smallest, fastest, lowest quality. Good default for development and CI.
    #[default]
    Tiny,
    /// Reasonable quality/speed tradeoff.
    Base,
    /// Better accuracy with higher memory and cache use.
    Small,
    /// High quality; slower and more memory-intensive.
    Medium,
    /// Best quality (large-v3). Use only when latency and memory use are acceptable.
    LargeV3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_sensible() {
        let cfg = TranscriptionConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.model, WhisperModel::Tiny);
        assert!(cfg.language.is_none());
        assert!(cfg.max_duration_ms.unwrap() > 1_000_000);
        assert!(cfg.allow_network);
    }

    #[test]
    fn test_serde_roundtrip_minimal() {
        let json = r#"{"enabled": true, "model": "base", "timestamps": true}"#;
        let cfg: TranscriptionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.model, WhisperModel::Base);
        assert!(cfg.timestamps);

        let back = serde_json::to_string(&cfg).unwrap();
        assert!(back.contains("\"model\":\"base\""));
        assert!(back.contains("\"timestamps\":true"));
    }

    #[test]
    fn test_serde_omits_none_fields() {
        let cfg = TranscriptionConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("language"));
        assert!(!json.contains("model_cache_dir"));
    }

    /// `backend` defaults to `whisper` when absent, and `model_dir` is optional
    /// (SV-11 coexistence: existing configs must parse unchanged).
    #[test]
    fn backend_defaults_to_whisper_and_model_dir_is_optional() {
        let cfg: TranscriptionConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert_eq!(cfg.backend, TranscriptionBackend::Whisper);
        assert!(cfg.model_dir.is_none());

        let cfg: TranscriptionConfig =
            serde_json::from_str(r#"{"backend": "sensevoice", "model_dir": "E:/models"}"#).unwrap();
        assert_eq!(cfg.backend, TranscriptionBackend::SenseVoice);
        assert_eq!(cfg.model_dir.as_deref(), Some(std::path::Path::new("E:/models")));
    }

    /// An invalid backend value must be a clear error, not a silent fallback.
    #[test]
    fn backend_rejects_unknown_values() {
        let err = serde_json::from_str::<TranscriptionConfig>(r#"{"backend": "wav2vec"}"#)
            .expect_err("unknown backend must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("unknown variant"), "unexpected: {msg}");
        assert!(
            msg.contains("whisper") && msg.contains("sensevoice"),
            "unexpected: {msg}"
        );
    }
}
