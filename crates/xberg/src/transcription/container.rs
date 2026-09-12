//! Container rescue for inputs the built-in decoder cannot read.
//!
//! `transcription::decode` speaks to symphonia, which ships MP4/Matroska/RIFF
//! readers only; a Windows Media file (`.wmv`, `.asf`) therefore fails the
//! normal path. Those containers are still common in user corpora and Windows
//! ships their decoders, so the audio track is pulled through Media Foundation
//! instead (see [`super::wmf`]) and handed to the same transcription pipeline.
//! Nothing is re-encoded ahead of time and no second media stack ships with the
//! binary -- an ffmpeg able to do this is >150 MB of DLLs.

use crate::Result;
use crate::transcription::decode::{PcmAudio, decode_audio_to_pcm};

/// Containers routed through the rescue path instead of symphonia.
pub(crate) const RESCUED_MIME_TYPES: &[&str] = &["video/x-ms-wmv", "video/x-ms-asf", "application/vnd.ms-asf"];

/// True when `mime_type` names a container the built-in decoder cannot open.
pub(crate) fn needs_rescue(mime_type: &str) -> bool {
    let lowered = mime_type.to_ascii_lowercase();
    RESCUED_MIME_TYPES.iter().any(|candidate| *candidate == lowered)
}

/// Decode `bytes` to 16 kHz mono PCM, rescuing unsupported containers.
///
/// Two mechanisms, in this order:
///  1. `transcription::wmf` (Windows Media Foundation) -- part of Windows, so the
///     bundle carries nothing extra; verified on Windows 11.
///  2. an external `ffmpeg`, used when Media Foundation cannot decode the file
///     (Windows Server images and "N" editions ship without the media codecs) or
///     on non-Windows targets. Only the audio track is taken (`-vn -ac 1 -ar
///     16000`), which keeps the scratch file small and avoids needing any
///     encoder beyond PCM.
///
/// `XBERG_ASF_DECODER` forces one mechanism (`mf` or `ffmpeg`); the default
/// `auto` tries Media Foundation first. Everything except the rescued
/// containers goes straight to the built-in decoder, so the existing behaviour
/// (and its error messages) is untouched.
pub(crate) fn decode_to_pcm(bytes: &[u8], mime_type: &str, max_bytes: Option<u64>) -> Result<PcmAudio> {
    if !needs_rescue(mime_type) {
        return decode_audio_to_pcm(bytes, max_bytes);
    }

    let mode = std::env::var("XBERG_ASF_DECODER")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut attempts: Vec<String> = Vec::new();

    if mode != "ffmpeg" {
        let input = TempInput::write(bytes, "asf")?;
        match super::wmf::decode_file_to_pcm(&input.path, max_bytes) {
            Ok(pcm) => return Ok(pcm),
            Err(error) => {
                attempts.push(format!("Media Foundation: {error}"));
                if mode == "mf" {
                    return Err(crate::XbergError::transcription(attempts.join("; ")));
                }
            }
        }
    }

    match decode_via_ffmpeg(bytes, max_bytes) {
        Ok(pcm) => Ok(pcm),
        Err(error) => {
            attempts.push(format!("ffmpeg: {error}"));
            Err(crate::XbergError::transcription(format!(
                "cannot decode this Windows Media ({mime_type}) input -- {}. Install ffmpeg on PATH, point XBERG_FFMPEG at it, or use a Windows build with the media codecs.",
                attempts.join("; ")
            )))
        }
    }
}

/// Extract the audio track with an external ffmpeg and decode the WAV it writes.
///
/// `-vn -ac 1 -ar 16000` asks for exactly what the engine wants, so the result
/// goes through the built-in WAV decoder with no further conversion.
fn decode_via_ffmpeg(bytes: &[u8], max_bytes: Option<u64>) -> Result<PcmAudio> {
    let ffmpeg = resolve_ffmpeg().ok_or_else(|| {
        crate::XbergError::transcription(
            "no ffmpeg found (checked XBERG_FFMPEG, the directory of the running binary, then PATH)".to_string(),
        )
    })?;

    let input = TempInput::write(bytes, "asf")?;
    let output = TempInput::path_for("wav");
    let result = std::process::Command::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(&input.path)
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-f")
        .arg("wav")
        .arg(&output)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| crate::XbergError::transcription(format!("cannot run {}: {e}", ffmpeg.display())))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(crate::XbergError::transcription(format!(
            "ffmpeg exited with {}: {}",
            result.status.code().unwrap_or(-1),
            stderr.trim().chars().take(400).collect::<String>()
        )));
    }

    let wav = std::fs::read(&output)
        .map_err(|e| crate::XbergError::transcription(format!("ffmpeg produced no audio: {e}")))?;
    let _ = std::fs::remove_file(&output);
    decode_audio_to_pcm(&wav, max_bytes)
}

/// `XBERG_FFMPEG` wins; then an ffmpeg staged next to the running binary (the
/// Windows bundle ships none, but operators drop one there); then `PATH`.
fn resolve_ffmpeg() -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(explicit) = std::env::var("XBERG_FFMPEG") {
        let path = std::path::PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join(exe);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// A byte buffer spilled to a temporary file, removed on drop.
///
/// Media Foundation's source reader opens a URL, not a memory buffer, so the
/// rescued containers take one disk round-trip on the way to the decoder.
struct TempInput {
    path: std::path::PathBuf,
}

impl TempInput {
    /// A unique scratch path in the process temp directory. The caller owns it
    /// and is responsible for removing it.
    fn path_for(suffix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("xberg-media-{}-{sequence}.{suffix}", std::process::id()))
    }

    fn write(bytes: &[u8], suffix: &str) -> Result<Self> {
        use std::io::Write as _;

        let path = Self::path_for(suffix);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| crate::XbergError::transcription(format!("cannot stage the media file at {path:?}: {e}")))?;
        file.write_all(bytes)
            .map_err(|e| crate::XbergError::transcription(format!("cannot write the media file at {path:?}: {e}")))?;
        file.flush()
            .map_err(|e| crate::XbergError::transcription(format!("cannot flush the media file at {path:?}: {e}")))?;
        Ok(Self { path })
    }
}

impl Drop for TempInput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescues_windows_media_containers_only() {
        assert!(needs_rescue("video/x-ms-wmv"));
        assert!(needs_rescue("VIDEO/X-MS-WMV"));
        assert!(needs_rescue("video/x-ms-asf"));
        assert!(needs_rescue("application/vnd.ms-asf"));
        assert!(!needs_rescue("video/mp4"));
        assert!(!needs_rescue("audio/wav"));
    }

    #[test]
    fn unsupported_container_without_rescue_stays_with_the_builtin_decoder() {
        // A WAV payload must keep using symphonia: the rescue path is reserved
        // for the containers named above.
        let err = decode_to_pcm(b"not audio", "audio/wav", None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("symphonia") || message.contains("probe") || message.contains("decode"),
            "unexpected message: {message}"
        );
    }
}
