//! Transcription configuration for audio/video speech-to-text.
//!
//! This module is behind the `transcription-types` feature for the pure-Rust
//! config structs (safe on WASM/Android) and the `transcription` feature for
//! the full FFmpeg-DLL + sherpa-onnx implementation (SV-01).
//!
//! 唯一后端是 SenseVoice（SV-11）：Whisper 链路的专属配置键（`model`、
//! `language`、`timestamps`、`model_cache_dir`、`allow_network`、`verify_hash`）
//! 已退役，解析到它们会得到明确的迁移错误而不是静默忽略。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 退役 Whisper 配置键的哨兵类型。
///
/// 这些键曾是 Whisper 链路的配置项；SenseVoice 成为唯一后端后不再有任何取值。
/// 反序列化到任何这样的键都直接报错（错误文案指明迁移方式，SV-11：不静默映射）；
/// 序列化时永远不输出该字段。`Deserialize` 实现永远失败，因此配置解析永远不产生
/// 「键存在但值合法」的状态；`Default` 实例只存在于「键缺席」的解析路径和
/// `Default::default()` / 结构体字面量中，不承载任何信息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetiredWhisperKey;

impl Serialize for RetiredWhisperKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 不可达：该类型的实例只能由 `Default` 产生，而带 `skip_serializing`
        // 的字段永远不会被写出。提供全量实现只为满足签名。
        serializer.serialize_none()
    }
}

/// 退役键错误文案（SV-11：明确报错，不静默映射）。
const RETIRED_WHISPER_KEY_MESSAGE: &str =
    "Whisper 已移除，使用 backend:\"sensevoice\"；该配置键属于已退役的 Whisper 链路，不再有效";

impl<'de> Deserialize<'de> for RetiredWhisperKey {
    fn deserialize<D>(_: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(RETIRED_WHISPER_KEY_MESSAGE))
    }
}

/// Configuration for audio/video transcription (speech-to-text).
///
/// When present and `enabled`, Xberg will route audio and video files
/// (mp3, mp4, m4a, wav, webm, etc.) through the transcription pipeline:
/// FFmpeg DLL decode → Silero VAD → SenseVoice INT8 (SV-01), emitting the
/// fixed SV-06 Markdown structure.
///
/// All fields have sensible defaults. The recommended starting point is:
///
/// ```toml
/// [extraction.transcription]
/// enabled = true
/// backend = "sensevoice"
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

    /// Which transcription backend to run.
    ///
    /// 只有 `"sensevoice"` 一个合法值（也是默认值，SV-01/SV-11）。其他值（包括
    /// 旧默认的 `"whisper"`）被 serde 以 unknown variant 明确拒绝。
    #[serde(default)]
    pub backend: TranscriptionBackend,

    /// Optional model root for the SenseVoice backend.
    ///
    /// Expected to contain (directly or under `models/`) the SenseVoice
    /// `model.int8.onnx` + `tokens.txt` and the Silero `silero_vad.onnx`.
    /// When unset, `XBERG_SENSEVOICE_MODEL_DIR` and exe-adjacent locations are
    /// tried. Native DLL locations use `XBERG_SHERPA_DLL_DIR` /
    /// `XBERG_FFMPEG_DLL_DIR`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<PathBuf>,

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
    /// Bounds the SenseVoice pipeline (staging, FFmpeg decode, VAD, inference)
    /// together. On expiry the extraction fails with an `XbergError::Transcription`
    /// while the detached blocking task finishes in the background. `None`
    /// disables the bound and lets the operation run unbounded (not recommended
    /// for untrusted input).
    ///
    /// Enforced on the async extraction path only; the size and duration caps
    /// (`max_bytes`, `max_duration_ms`) are checked on every path.
    ///
    /// Default: 10 minutes.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: Option<u64>,

    // ---- 退役的 Whisper 专属配置键（SV-11）：出现即报错，字段值不可构造。 ----
    /// 退役键：原 Whisper 模型枚举。出现即报错（见 [`RetiredWhisperKey`]）。
    #[serde(default, skip_serializing)]
    pub model: RetiredWhisperKey,
    /// 退役键：原 Whisper 语言提示。出现即报错（SenseVoice 固定 zh，SV-04）。
    #[serde(default, skip_serializing)]
    pub language: RetiredWhisperKey,
    /// 退役键：原 Whisper 段级时间戳开关。出现即报错（新语义固定输出段时间戳）。
    #[serde(default, skip_serializing)]
    pub timestamps: RetiredWhisperKey,
    /// 退役键：原 Whisper HF 缓存根。出现即报错。
    #[serde(default, skip_serializing)]
    pub model_cache_dir: RetiredWhisperKey,
    /// 退役键：原 Whisper 模型联网下载开关。出现即报错（SenseVoice 不联网）。
    #[serde(default, skip_serializing)]
    pub allow_network: RetiredWhisperKey,
    /// 退役键：原 Whisper 模型摘要校验开关。出现即报错（SenseVoice 资产固定摘要校验）。
    #[serde(default, skip_serializing)]
    pub verify_hash: RetiredWhisperKey,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: TranscriptionBackend::default(),
            model_dir: None,
            max_duration_ms: default_max_duration_ms(),
            max_bytes: default_max_bytes(),
            timeout_ms: default_timeout_ms(),
            model: RetiredWhisperKey,
            language: RetiredWhisperKey,
            timestamps: RetiredWhisperKey,
            model_cache_dir: RetiredWhisperKey,
            allow_network: RetiredWhisperKey,
            verify_hash: RetiredWhisperKey,
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
/// SenseVoice 是唯一后端（SV-01）：序列化为 `"sensevoice"`，serde 以
/// unknown variant 明确拒绝其他任何值（含退役的 `"whisper"`）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionBackend {
    /// SenseVoice INT8 via sherpa-onnx + FFmpeg DLL decode + Silero VAD.
    #[default]
    SenseVoice,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_sensible() {
        let cfg = TranscriptionConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, TranscriptionBackend::SenseVoice);
        assert!(cfg.model_dir.is_none());
        assert!(cfg.max_duration_ms.unwrap() > 1_000_000);
    }

    #[test]
    fn test_serde_roundtrip_minimal() {
        let json = r#"{"enabled": true, "backend": "sensevoice"}"#;
        let cfg: TranscriptionConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, TranscriptionBackend::SenseVoice);

        let back = serde_json::to_string(&cfg).unwrap();
        assert!(back.contains("\"backend\":\"sensevoice\""));
    }

    /// `backend` 缺省时默认 `sensevoice`，`model_dir` 可选（SV-11）。
    #[test]
    fn backend_defaults_to_sensevoice_and_model_dir_is_optional() {
        let cfg: TranscriptionConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert_eq!(cfg.backend, TranscriptionBackend::SenseVoice);
        assert!(cfg.model_dir.is_none());

        let cfg: TranscriptionConfig =
            serde_json::from_str(r#"{"backend": "sensevoice", "model_dir": "E:/models"}"#).unwrap();
        assert_eq!(cfg.backend, TranscriptionBackend::SenseVoice);
        assert_eq!(cfg.model_dir.as_deref(), Some(std::path::Path::new("E:/models")));
    }

    /// 无效的 backend 值必须是明确错误，不允许静默回退（SV-11）。
    #[test]
    fn backend_rejects_unknown_values() {
        let err = serde_json::from_str::<TranscriptionConfig>(r#"{"backend": "wav2vec"}"#)
            .expect_err("unknown backend must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("unknown variant"), "unexpected: {msg}");
        assert!(msg.contains("sensevoice"), "unexpected: {msg}");
    }

    /// 退役的 Whisper 专属键必须报出带迁移指引的明确错误（SV-11），
    /// 不允许静默映射或忽略。
    #[test]
    fn retired_whisper_keys_fail_with_migration_hint() {
        for key in [
            "model",
            "language",
            "timestamps",
            "model_cache_dir",
            "allow_network",
            "verify_hash",
        ] {
            let json = format!(r#"{{"enabled": true, "{key}": null}}"#);
            let err =
                serde_json::from_str::<TranscriptionConfig>(&json).expect_err("retired Whisper key must be rejected");
            let msg = err.to_string();
            assert!(msg.contains("Whisper 已移除"), "key {key}: {msg}");
            assert!(msg.contains("backend:\"sensevoice\""), "key {key}: {msg}");
        }
        // 旧 fulltest/文档里最常见的写法：字符串取值同样被拒。
        let err = serde_json::from_str::<TranscriptionConfig>(r#"{"model": "tiny"}"#)
            .expect_err("retired model key must be rejected");
        assert!(err.to_string().contains("Whisper 已移除"), "{err}");
    }

    /// 退役键不得出现在序列化输出中（配置往返不引入噪音）。
    #[test]
    fn retired_keys_are_never_serialized() {
        let json = serde_json::to_string(&TranscriptionConfig::default()).unwrap();
        for key in [
            "model",
            "language",
            "timestamps",
            "model_cache_dir",
            "allow_network",
            "verify_hash",
        ] {
            assert!(!json.contains(&format!("\"{key}\"")), "key {key} leaked: {json}");
        }
    }
}
