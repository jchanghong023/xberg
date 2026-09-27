//! Audio/video transcription (speech-to-text): FFmpeg DLL decode → Silero VAD
//! → SenseVoice INT8.
//!
//! This is the internal implementation behind the `transcription` feature.
//! The public surface is the `TranscriptionConfig` (under `transcription-types`)
//! and the automatic routing that happens when an audio/video MIME type is
//! presented to the extractor registry.
//!
//! 唯一链路（SV-01）：FFmpeg 原生共享库解码与重采样（16 kHz 单声道 s16 PCM，
//! 按原比例转 f32）→ Silero VAD 切分 → SenseVoice INT8 中文识别 → 固定术语
//! 映射 → 段级时间戳输出。模块是钉定版本的 sherpa-onnx / FFmpeg 共享库的
//! FFI 边界，沿用 wmf.rs / ort_discovery.rs 时代确立的、对整个 crate 全局
//! deny unsafe 的范围性放行模式。
#[cfg(feature = "transcription")]
#[allow(unsafe_code)]
pub mod sensevoice;
