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

/// The mechanism `XBERG_ASF_DECODER` pinned, when it names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForcedDecoder {
    MediaFoundation,
    Ffmpeg,
}

/// Parse `XBERG_ASF_DECODER`: `""`/`auto` means try both, `mf`/`ffmpeg` pin one.
///
/// Split out of [`decode_to_pcm`] so the parsing is unit-testable without
/// touching the process environment.
fn parse_forced_decoder(value: &str) -> Result<Option<ForcedDecoder>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(None),
        "mf" => Ok(Some(ForcedDecoder::MediaFoundation)),
        "ffmpeg" => Ok(Some(ForcedDecoder::Ffmpeg)),
        other => Err(crate::XbergError::validation(format!(
            "Invalid value for XBERG_ASF_DECODER: '{other}'. Valid options are: auto, mf, ffmpeg"
        ))),
    }
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
/// `XBERG_ASF_DECODER` forces one mechanism (`mf` or `ffmpeg`); `auto` -- the
/// unset default -- tries Media Foundation first. Any other value is refused
/// rather than treated as `auto`, so a typo cannot silently unpin a mechanism.
/// Everything except the rescued containers goes straight to the built-in
/// decoder, so the existing behaviour (and its error messages) is untouched.
///
/// `max_duration_ms` and `timeout_ms` mirror the matching `transcription.*`
/// fields. The extractor already enforces both, but it cannot reclaim a decode
/// that is already running on a blocking thread, so the rescue mechanisms apply
/// them while decoding as well: Media Foundation stops at the duration, the
/// external ffmpeg gets a wall-clock deadline.
pub(crate) fn decode_to_pcm(
    bytes: &[u8],
    mime_type: &str,
    max_bytes: Option<u64>,
    max_duration_ms: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<PcmAudio> {
    if !needs_rescue(mime_type) {
        return decode_audio_to_pcm(bytes, max_bytes);
    }

    let mode = match std::env::var("XBERG_ASF_DECODER") {
        Ok(value) => parse_forced_decoder(&value)?,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::XbergError::validation(
                "XBERG_ASF_DECODER must be valid UTF-8; valid options are: auto, mf, ffmpeg",
            ));
        }
        Err(std::env::VarError::NotPresent) => None,
    };
    let mut attempts: Vec<String> = Vec::new();

    if mode != Some(ForcedDecoder::Ffmpeg) {
        let input = TempInput::write(bytes, "asf")?;
        match super::wmf::decode_file_to_pcm(&input.path, max_bytes, max_duration_ms) {
            Ok(pcm) => return Ok(pcm),
            Err(error) => {
                attempts.push(format!("Media Foundation: {error}"));
                if mode == Some(ForcedDecoder::MediaFoundation) {
                    return Err(crate::XbergError::transcription(attempts.join("; ")));
                }
            }
        }
    }

    match decode_via_ffmpeg(bytes, max_bytes, max_duration_ms, timeout_ms) {
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

/// How long the ffmpeg watchdog may wait between two checks of the child.
const FFMPEG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Extra output length handed to ffmpeg's `-t` on top of
/// `transcription.max_duration_ms`. The margin keeps an over-long input just
/// past the limit, so the extractor rejects it (`>` the configured maximum) as
/// before instead of this path silently transcribing only the first part of a
/// longer file.
const FFMPEG_DURATION_MARGIN_MS: u64 = 5_000;

/// Extract the audio track with an external ffmpeg and decode the WAV it writes.
///
/// `-vn -ac 1 -ar 16000` asks for exactly what the engine wants, so the result
/// goes through the built-in WAV decoder with no further conversion.
///
/// `max_duration_ms` bounds how much audio ffmpeg decodes and `timeout_ms`
/// bounds how long the process may run: the extractor's own timeout only drops
/// the waiting future, so without these a stuck ffmpeg would keep a blocking
/// thread, the child process and the scratch WAV alive indefinitely.
fn decode_via_ffmpeg(
    bytes: &[u8],
    max_bytes: Option<u64>,
    max_duration_ms: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<PcmAudio> {
    let ffmpeg = resolve_ffmpeg().ok_or_else(|| {
        crate::XbergError::transcription(
            "no ffmpeg found (checked XBERG_FFMPEG, the directory of the running binary, then PATH)".to_string(),
        )
    })?;

    let input = TempInput::write(bytes, "asf")?;
    // Dropped -- and therefore deleted -- however this function returns, so a
    // failing ffmpeg does not leave a scratch WAV behind.
    let output = Scratch::new("wav");
    let mut command = std::process::Command::new(&ffmpeg);
    command
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
        .arg("16000");
    if let Some(limit_ms) = max_duration_ms {
        command.arg("-t").arg(format!("{:.3}", limit_ms.saturating_add(FFMPEG_DURATION_MARGIN_MS) as f64 / 1000.0));
    }
    command
        .arg("-f")
        .arg("wav")
        .arg(&output.path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| crate::XbergError::transcription(format!("cannot run {}: {e}", ffmpeg.display())))?;

    // ffmpeg's log is drained on its own thread: a pipe nobody reads would fill
    // up and block ffmpeg while the loop below waits for it to exit.
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            // Bytes, not a `String`: ffmpeg's log is ASCII in practice, but one non-UTF-8 byte
            // makes `read_to_string` fail and yield nothing at all, losing the whole diagnostic.
            // Lossy conversion keeps every readable byte.
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });

    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(crate::XbergError::transcription(format!("cannot wait for {}: {e}", ffmpeg.display())));
            }
        }
        if let Some(deadline) = deadline
            && std::time::Instant::now() >= deadline
        {
            // Killing here is the point: the caller's `apply_timeout` drops the
            // future but leaves this blocking thread and the child process behind.
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::XbergError::transcription(format!(
                "ffmpeg did not finish within transcription.timeout_ms ({} ms) and was killed",
                timeout_ms.unwrap_or_default()
            )));
        }
        std::thread::sleep(FFMPEG_POLL_INTERVAL);
    };

    let stderr = stderr_reader.and_then(|handle| handle.join().ok()).unwrap_or_default();
    if !status.success() {
        return Err(crate::XbergError::transcription(format!(
            "ffmpeg exited with {}: {}",
            status.code().unwrap_or(-1),
            stderr.trim().chars().take(400).collect::<String>()
        )));
    }

    let wav = std::fs::read(&output.path)
        .map_err(|e| crate::XbergError::transcription(format!("ffmpeg produced no audio: {e}")))?;
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

/// A scratch path that is deleted when it goes out of scope, whatever the
/// control flow did in between.
pub(crate) struct Scratch {
    pub(crate) path: std::path::PathBuf,
}

impl Scratch {
    pub(crate) fn new(suffix: &str) -> Self {
        Self {
            path: TempInput::path_for(suffix),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    ///
    /// The pid and counter alone are guessable, and these files land in a shared
    /// temp directory where a name an attacker predicts can be pre-created (as a
    /// symlink or a plain file); the random salt from the standard library's
    /// hasher seed makes the name unguessable across processes.
    fn path_for(suffix: &str) -> std::path::PathBuf {
        use std::hash::{BuildHasher as _, Hasher as _, RandomState};
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let salt = RandomState::new().build_hasher().finish();
        std::env::temp_dir().join(format!("xberg-media-{}-{sequence}-{salt:016x}.{suffix}", std::process::id()))
    }

    /// Create a fresh scratch file, refusing to reuse a name that already
    /// exists (`create_new` also refuses to follow a pre-planted symlink).
    fn create(suffix: &str) -> Result<(std::path::PathBuf, std::fs::File)> {
        for _ in 0..8 {
            let path = Self::path_for(suffix);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(crate::XbergError::transcription(format!(
                        "cannot stage the media file at {path:?}: {e}"
                    )));
                }
            }
        }
        Err(crate::XbergError::transcription(format!(
            "cannot stage the media file in {:?}: every candidate name already exists",
            std::env::temp_dir()
        )))
    }

    fn write(bytes: &[u8], suffix: &str) -> Result<Self> {
        use std::io::Write as _;

        let (path, mut file) = Self::create(suffix)?;
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
        let err = decode_to_pcm(b"not audio", "audio/wav", None, None, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("symphonia") || message.contains("probe") || message.contains("decode"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn decoder_override_accepts_the_documented_values_and_rejects_typos() {
        assert!(matches!(parse_forced_decoder(""), Ok(None)));
        assert!(matches!(parse_forced_decoder("auto"), Ok(None)));
        assert!(matches!(parse_forced_decoder(" MF "), Ok(Some(ForcedDecoder::MediaFoundation))));
        assert!(matches!(parse_forced_decoder("Ffmpeg"), Ok(Some(ForcedDecoder::Ffmpeg))));

        // A typo must not silently fall back to `auto`: an operator pinning a
        // mechanism has to learn that the pin was not understood.
        let err = parse_forced_decoder("mediafoundation").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("XBERG_ASF_DECODER"), "unexpected message: {message}");
    }

    #[test]
    fn staged_media_input_is_written_and_removed_on_drop() {
        let staged = TempInput::write(b"windows media", "asf").unwrap();
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"windows media");

        let path = staged.path.clone();
        drop(staged);
        assert!(!path.exists(), "scratch file {path:?} outlived its guard");
    }
}
