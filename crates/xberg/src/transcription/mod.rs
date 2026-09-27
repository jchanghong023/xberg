//! Audio/video transcription (speech-to-text) pipeline.
//!
//! This is the internal implementation behind the `transcription` feature.
//! The public surface is the `TranscriptionConfig` (under `transcription-types`)
//! and the automatic routing that happens when an audio/video MIME type is
//! presented to the extractor registry.

#[cfg(feature = "transcription")]
pub mod container;
pub mod decode;
#[cfg(feature = "transcription")]
pub mod engine;
#[cfg(feature = "transcription")]
pub mod model;
// SenseVoice backend (FFmpeg DLL decode → Silero VAD → SenseVoice INT8),
// opt-in via `transcription.backend = "sensevoice"`. Pure addition: the
// Whisper pipeline above is untouched. The module is an FFI boundary over the
// pinned sherpa-onnx / FFmpeg shared libraries, so it carries a scoped
// `unsafe_code` allowance following the established wmf.rs / ort_discovery.rs
// pattern (crate root denies unsafe globally).
#[cfg(feature = "transcription")]
#[allow(unsafe_code)]
pub mod sensevoice;
#[cfg(feature = "transcription")]
pub mod tags;
#[cfg(feature = "transcription")]
pub mod wmf;
