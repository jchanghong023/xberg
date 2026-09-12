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
/// Everything except the rescued containers goes straight to the built-in
/// decoder, so the existing behaviour (and its error messages) is untouched.
pub(crate) fn decode_to_pcm(bytes: &[u8], mime_type: &str, max_bytes: Option<u64>) -> Result<PcmAudio> {
    if !needs_rescue(mime_type) {
        return decode_audio_to_pcm(bytes, max_bytes);
    }
    let input = TempInput::write(bytes)?;
    super::wmf::decode_file_to_pcm(&input.path, max_bytes)
}

/// A byte buffer spilled to a temporary file, removed on drop.
///
/// Media Foundation's source reader opens a URL, not a memory buffer, so the
/// rescued containers take one disk round-trip on the way to the decoder.
struct TempInput {
    path: std::path::PathBuf,
}

impl TempInput {
    fn write(bytes: &[u8]) -> Result<Self> {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicU64, Ordering};

        // No uuid dependency on this feature: process id + a monotonic counter
        // is unique enough for a scratch file that is deleted on drop.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("xberg-media-{}-{sequence}.asf", std::process::id()));
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
