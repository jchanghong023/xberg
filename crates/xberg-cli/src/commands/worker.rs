//! `xberg worker` - resident stdio worker process for embedding callers (JchTools).
//!
//! One process owns the document runtime and a separate resident screenshot engine.
//! Document and screenshot requests run concurrently on dedicated threads, while a
//! single protocol writer emits complete JSON lines correlated by `id`. Configuration
//! is fixed at startup; keeping stdin open reuses both models across batches.
//! Closing stdin drains document work and cancels screenshots. See
//! `docs/requirements/WORKER.md` for lifecycle and cooperative per-request timeouts.
//!
//! Supported commands:
//! - `extract` (`path`, optional `mode`): normal uses startup settings; fast
//!   disables document layout and image OCR in a request-local config copy.
//! - `cancel` (`target_id`), `timeout_ms` on work requests, and in-process
//!   `formats`, `capabilities`, `model_state` queries keep the same connection.
//! - `keepalive` (P6): cheap liveness probe — proves the event loop answers,
//!   loads no models, resets no timer; carries a small `models` summary.
//! - `shutdown` (P3, optional `grace_ms`): acknowledged first
//!   (`{"ok":true,"accepted":true}`), then the worker stops taking work, drains
//!   in-flight requests up to the grace cap and exits 0.
//! - `version` (P5): build identity plus the on-disk model inventory.
//! - `ocr_snapshot` (`image_base64`, SNAP-14): recognize one in-memory screenshot
//!   through the snapshot OCR channel; engine lazily loaded on the first request and
//!   reused across batches. Success is `{"id":..,"ok":true,"text":..,"records":N,
//!   "elapsed_ms":M}` (a text-free image is `ok:true` with `text:""` and
//!   `error_kind:"no_text"` so callers can tell the cases apart); failure carries
//!   `error_kind` (`model_not_ready|asset_invalid|input_invalid|no_text|cancelled|
//!   internal`, SNAP-15). `snapshot_state` (SNAP-17) reports `uninitialized|loading|
//!   ready|error` plus the error summary.
//! - `transcribe` (`path`, SV-12): transcribe one local media file through the
//!   startup config's `transcription` backend (sensevoice), answering with the SV-06
//!   `markdown`, segment list, duration and `has_audio`.
//! - Unknown commands fail with `error_kind:"unsupported_command"` while keeping
//!   the `unsupported command '<name>'` text prefix (P2; run49.1 passthrough).
//!
//! Lifecycle (P1/P3/P4): stdin EOF drains documents and cancels screenshots,
//! but never past a hard cap — the process is gone within 5 s of the trigger.
//! `shutdown` drains up to its grace and exits 0. A vanished host (failed
//! stdout write, stdout probe, dead parent, failed stdin read) cancels
//! everything and exits with the fixed code [`EXIT_PEER_DISCONNECTED`] (86);
//! `idle_timeout_ms` exits with [`EXIT_IDLE_TIMEOUT`] (87). Exit codes other
//! than 0 are routed through [`FORCED_EXIT_CODE`] so `main` applies them after
//! `run_cli` unwinds.
//!
//! Model-session reuse is the point of this command: every ML backend the library
//! uses (OCR engine pool, Tesseract processor, layout model caches, the SenseVoice
//! session cache, the snapshot OCR engine above) is a process-level
//! lazy cache, so simply keeping the process — and one tokio runtime — alive for the
//! process lifetime means the second and later files skip model loading. `xberg serve`
//! relies on the same mechanism across HTTP requests.
//!
//! Protocol purity: stdout carries protocol messages only. Diagnostics go to stderr
//! through `tracing` (the subscriber is installed in `main()` with a stderr writer),
//! and the default panic hook also writes to stderr, so even a caught panic cannot
//! corrupt the stdout stream.
//!
//! Stdin is read independently of inference so EOF cancels screenshots at their
//! existing checkpoints. Output failure stops the dispatcher without waiting for
//! another stdin line. Diagnostics and panic hooks write only to stderr.

mod control;
mod monitor;
mod scheduler;
use scheduler::{LoopExit, LoopOptions, run_worker_loop};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::path::{Path, PathBuf};
use std::time::Duration;
use xberg::{ExtractInput, ExtractedDocument, ExtractionConfig, ProcessingWarning};

use super::extract::{build_runtime, single_result_from_output};
use super::snapshot_ocr::{
    ErrorKind, KIND_INPUT_INVALID, KIND_NO_TEXT, Recognized, SnapshotOcrEngine, SnapshotState, recognize_base64,
};
use super::validate_file_exists;

/// The original extraction command (WORKER.md contract shape).
const COMMAND_EXTRACT: &str = "extract";
/// Snapshot OCR command (SNAP-14).
const COMMAND_OCR_SNAPSHOT: &str = "ocr_snapshot";
/// Snapshot channel state query (SNAP-17).
const COMMAND_SNAPSHOT_STATE: &str = "snapshot_state";
/// Media transcription command (SV-12).
const COMMAND_TRANSCRIBE: &str = "transcribe";
const COMMAND_CANCEL: &str = "cancel";
const COMMAND_FORMATS: &str = "formats";
const COMMAND_CAPABILITIES: &str = "capabilities";
const COMMAND_MODEL_STATE: &str = "model_state";
/// Liveness probe (P6): proves the event loop answers; loads nothing and resets
/// no per-request timer. The optional `models` summary is cheap state only.
const COMMAND_KEEPALIVE: &str = "keepalive";
/// Graceful stop (P3): acknowledged on the wire first, then the process drains
/// up to `grace_ms` and exits 0. The engine never refuses to exit for in-flight
/// work — at the cap it is cancelled.
const COMMAND_SHUTDOWN: &str = "shutdown";
/// Diagnostic manifest (P5): build identity plus the on-disk model inventory.
const COMMAND_VERSION: &str = "version";
/// Protocol-level unknown-command category (P2). The failure text keeps the
/// `unsupported command '<name>'` prefix the run49.1 passthrough matches on.
const KIND_UNSUPPORTED_COMMAND: &str = "unsupported_command";
/// P1: fixed exit code when the stdio peer vanished (broken stdout, dead
/// parent, failed stdin read).
pub const EXIT_PEER_DISCONNECTED: i32 = 86;
/// P4: exit code for the optional `idle_timeout_ms` self-exit (distinct from
/// the disconnect code so callers can tell the two apart).
pub const EXIT_IDLE_TIMEOUT: i32 = 87;
/// Process exit code `main` applies after `run_cli` returns normally. Set only
/// by the worker's disconnect/idle paths (P1/P4); 0 keeps the normal return.
/// Setting it instead of calling `std::process::exit` from inside the command
/// lets `run_cli` unwind (flushing tracing guards) before the hard exit.
pub static FORCED_EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Run the startup document processing settings unchanged.
const MODE_NORMAL: &str = "normal";
const MODE_FAST: &str = "fast";

/// One line-JSON request read from stdin.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WorkerRequest {
    /// Caller-supplied correlation token (any JSON value), echoed verbatim in the response.
    pub(crate) id: Value,
    /// Requested operation: [`COMMAND_EXTRACT`], [`COMMAND_OCR_SNAPSHOT`],
    /// [`COMMAND_SNAPSHOT_STATE`] or [`COMMAND_TRANSCRIBE`].
    pub(crate) command: String,
    /// Local filesystem path — required by `extract` and `transcribe`, ignored by the
    /// snapshot commands.
    #[serde(default)]
    pub(crate) path: Option<PathBuf>,
    /// Extraction mode; `None` means [`MODE_NORMAL`]; `fast` is extract-only.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Base64 image bytes (PNG or equivalent lossless encoding) — required by
    /// `ocr_snapshot`; the bytes stay in memory (SNAP-14: no disk staging required).
    #[serde(default)]
    pub(crate) image_base64: Option<String>,
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
    /// `shutdown` grace cap in milliseconds (P3); absent means the shipped default.
    #[serde(default)]
    pub(crate) grace_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_target_id")]
    pub(crate) target_id: Option<Value>,
    #[serde(skip)]
    pub(crate) control: control::RequestControl,
}

fn deserialize_target_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// What the extraction handler did with a request, before protocol wrapping.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the document arm predates the multi-command protocol; boxing it would \
              churn the untouched extract path for no observable gain"
)]
pub(crate) enum WorkerOutcome {
    /// A document was produced (possibly a degraded/partial one — check its warnings).
    Success(ExtractedDocument),
    /// No document; the string is the single-line failure description.
    Failure(String),
}

/// A successful snapshot recognition, before protocol wrapping.
#[derive(Debug)]
pub(crate) struct SnapshotSuccess {
    /// Layout-preserving text (empty when the image carries no text).
    pub(crate) text: String,
    /// Number of structured records.
    pub(crate) records: usize,
    /// Total decode + recognize wall time, milliseconds.
    pub(crate) elapsed_ms: u64,
    /// `Some("no_text")` on a text-free image (ok stays true so callers can
    /// distinguish an empty result from a failure).
    pub(crate) error_kind: Option<ErrorKind>,
}

/// One transcript segment in a `transcribe` response.
#[derive(Debug, Serialize)]
pub(crate) struct TranscriptSegment {
    pub(crate) start_ms: u32,
    pub(crate) end_ms: u32,
    pub(crate) text: String,
}

/// A successful `transcribe` response payload (SV-06/SV-12).
#[derive(Debug)]
pub(crate) struct TranscribeSuccess {
    /// The SV-06 Markdown document.
    pub(crate) markdown: String,
    /// `(start_ms, end_ms, text)` per transcript line, in time order.
    pub(crate) segments: Vec<TranscriptSegment>,
    /// Decoded audio duration, milliseconds (0 for trackless containers).
    pub(crate) duration_ms: u64,
    /// Whether the container carried an audio track.
    pub(crate) has_audio: bool,
}

/// What any request handler produced, before protocol wrapping. The extract arm
/// keeps the original [`WorkerOutcome`] so the extraction path is untouched.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "extract keeps its original unboxed outcome so the existing wire path is \
              byte-compatible; the other arms stay small"
)]
pub(crate) enum RequestOutcome {
    Query(Value),
    Extract(WorkerOutcome),
    Snapshot(std::result::Result<SnapshotSuccess, (String, ErrorKind)>),
    State {
        /// `uninitialized | loading | ready | error` (SNAP-17).
        state: &'static str,
        /// Error summary when the last load attempt failed.
        error: Option<String>,
    },
    Transcribe(std::result::Result<TranscribeSuccess, String>),
}

/// One line-JSON response written to stdout.
///
/// `ok: true` means the request succeeded (for `extract`: a document is present,
/// partial extractions included — degraded stages surface in `warnings`; for the
/// snapshot commands: the recognition completed, a text-free image is still
/// `ok:true`); `ok: false` means failure, with `error` saying why. The
/// per-command payload fields below are omitted outside their command, so the
/// `extract` wire shape is byte-compatible with the original contract.
#[derive(Debug, Serialize)]
pub(crate) struct WorkerResponse {
    pub(crate) id: Value,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) document: Option<ExtractedDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Copy of `document.processing_warnings` on extract success, empty on failure, so
    /// callers that ignore the document still see degraded-stage information.
    pub(crate) warnings: Vec<ProcessingWarning>,
    /// Snapshot error category (SNAP-15); present on snapshot failures and on the
    /// text-free success (`"no_text"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_kind: Option<ErrorKind>,
    /// Snapshot layout text (`ocr_snapshot` success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    /// Structured record count (`ocr_snapshot` success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) records: Option<usize>,
    /// Request wall time in milliseconds (`ocr_snapshot` success, load included).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) elapsed_ms: Option<u64>,
    /// Channel state (`snapshot_state`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<String>,
    /// `transcribe` success payload (SV-06/SV-12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) segments: Option<Vec<TranscriptSegment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) has_audio: Option<bool>,
    /// Extra keys merged into the response for shapes that need an always-present
    /// field `extract` must not carry (currently `snapshot_state`'s
    /// `"error": null`).
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub(crate) extra: Option<Value>,
}

impl WorkerResponse {
    fn success(id: Value, document: ExtractedDocument) -> Self {
        let warnings = document.processing_warnings.clone();
        Self {
            document: Some(document),
            warnings,
            ..Self::bare(id, true)
        }
    }

    fn failure(id: Value, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::bare(id, false)
        }
    }

    /// Response skeleton with every payload slot empty; command handlers fill in
    /// only the fields their command carries (the `extract` wire shape stays
    /// byte-compatible with the original contract).
    fn bare(id: Value, ok: bool) -> Self {
        Self {
            id,
            ok,
            document: None,
            error: None,
            warnings: Vec::new(),
            error_kind: None,
            text: None,
            records: None,
            elapsed_ms: None,
            state: None,
            markdown: None,
            segments: None,
            duration_ms: None,
            has_audio: None,
            extra: None,
        }
    }
}

/// Wrap a handler outcome into the wire response.
fn render_outcome(id: Value, outcome: RequestOutcome) -> WorkerResponse {
    match outcome {
        RequestOutcome::Query(value) => {
            let mut response = WorkerResponse::bare(id, true);
            response.extra = Some(value);
            response
        }
        RequestOutcome::Extract(WorkerOutcome::Success(document)) => WorkerResponse::success(id, document),
        RequestOutcome::Extract(WorkerOutcome::Failure(error)) => WorkerResponse::failure(id, error),
        RequestOutcome::Snapshot(Ok(success)) => {
            let mut response = WorkerResponse::bare(id, true);
            response.error_kind = success.error_kind;
            response.text = Some(success.text);
            response.records = Some(success.records);
            response.elapsed_ms = Some(success.elapsed_ms);
            response
        }
        RequestOutcome::Snapshot(Err((error, kind))) => {
            let mut response = WorkerResponse::failure(id, error);
            response.error_kind = Some(kind);
            response
        }
        RequestOutcome::State { state, error } => {
            let mut response = WorkerResponse::bare(id, true);
            response.state = Some(state.to_string());
            // The state query always answers ok:true; the error key is present
            // (null) so callers can read it unconditionally (SNAP-17).
            response.extra = Some(serde_json::json!({ "error": error }));
            response
        }
        RequestOutcome::Transcribe(Ok(success)) => {
            let mut response = WorkerResponse::bare(id, true);
            response.markdown = Some(success.markdown);
            response.segments = Some(success.segments);
            response.duration_ms = Some(success.duration_ms);
            response.has_audio = Some(success.has_audio);
            response
        }
        RequestOutcome::Transcribe(Err(error)) => WorkerResponse::failure(id, error),
    }
}

/// Parse one stdin line into a [`WorkerRequest`].
///
/// Unknown JSON fields are ignored (forward compatibility); missing required fields
/// (`id`, `command`) and invalid JSON are reported as a plain message suitable for a
/// failure response with a null id. `extract` additionally requires `path` here,
/// preserving the original parse-level rejection; the other commands validate their
/// own fields in [`validate_line`] so their responses can still echo the id.
fn parse_request(line: &str) -> std::result::Result<WorkerRequest, String> {
    let raw: Value = serde_json::from_str(line).map_err(|error| format!("invalid worker request JSON: {error}"))?;
    if raw.get("id").is_none() {
        return Err("invalid worker request JSON: missing field `id`".into());
    }
    let request: WorkerRequest =
        serde_json::from_value(raw).map_err(|error| format!("invalid worker request JSON: {error}"))?;
    if request.command == COMMAND_EXTRACT && request.path.is_none() {
        return Err("invalid worker request JSON: missing field `path` for command 'extract'".to_string());
    }
    Ok(request)
}

/// Render a caught panic payload into the failure-response string.
fn render_panic(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        format!("request handler panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("request handler panicked: {message}")
    } else {
        "request handler panicked (non-string panic payload)".to_string()
    }
}

/// Reject malformed protocol messages before they enter either work queue.
fn validate_line(line: &str) -> std::result::Result<WorkerRequest, Box<WorkerResponse>> {
    let request = parse_request(line).map_err(|error| {
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|raw| raw.get("id").cloned())
            .unwrap_or(Value::Null);
        Box::new(WorkerResponse::failure(id, error))
    })?;
    let id = request.id.clone();
    if let Some(mode) = request.mode.as_deref()
        && mode != MODE_NORMAL
        && !(mode == MODE_FAST && request.command == COMMAND_EXTRACT)
    {
        return Err(Box::new(WorkerResponse::failure(
            id,
            format!("unsupported mode '{mode}': normal is supported for all tasks; fast only for extract"),
        )));
    }
    if request.timeout_ms == Some(0) {
        return Err(Box::new(WorkerResponse::failure(
            id,
            "timeout_ms must be positive".into(),
        )));
    }
    match request.command.as_str() {
        COMMAND_CANCEL => {
            if request.target_id.is_none() {
                return Err(Box::new(WorkerResponse::failure(
                    id,
                    "cancel requires target_id".into(),
                )));
            }
        }
        COMMAND_SHUTDOWN => {
            if request.grace_ms == Some(0) {
                return Err(Box::new(WorkerResponse::failure(
                    id,
                    "grace_ms must be positive".into(),
                )));
            }
        }
        COMMAND_FORMATS | COMMAND_CAPABILITIES | COMMAND_MODEL_STATE | COMMAND_KEEPALIVE | COMMAND_VERSION => {}
        COMMAND_EXTRACT => {}
        COMMAND_OCR_SNAPSHOT => {
            if request.image_base64.is_none() {
                return Err(Box::new(WorkerResponse::failure(
                    id,
                    "ocr_snapshot requires image_base64 (base64 PNG or equivalent lossless bytes)".to_string(),
                )));
            }
        }
        COMMAND_SNAPSHOT_STATE => {}
        COMMAND_TRANSCRIBE => {
            if request.path.is_none() {
                return Err(Box::new(WorkerResponse::failure(
                    id,
                    "transcribe requires path (local media file)".to_string(),
                )));
            }
        }
        other => {
            // P2: structured category alongside the legacy text prefix — the
            // run49.1 passthrough matches on the text, new callers on the kind.
            let mut response = WorkerResponse::failure(
                id,
                format!("unsupported command '{other}'; use capabilities to list commands"),
            );
            response.error_kind = Some(KIND_UNSUPPORTED_COMMAND);
            response.extra = Some(serde_json::json!({ "command": other }));
            return Err(Box::new(response));
        }
    }
    Ok(request)
}

fn process_request<F>(request: WorkerRequest, handler: &mut F) -> WorkerResponse
where
    F: FnMut(WorkerRequest) -> RequestOutcome,
{
    let id = request.id.clone();
    let screenshot = request.command == COMMAND_OCR_SNAPSHOT;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request))) {
        Ok(outcome) => render_outcome(id, outcome),
        Err(panic) => {
            let mut response = WorkerResponse::failure(id, render_panic(panic));
            if screenshot {
                response.error_kind = Some(super::snapshot_ocr::KIND_INTERNAL);
            }
            response
        }
    }
}

#[cfg(test)]
fn process_line<F>(line: &str, handler: &mut F) -> WorkerResponse
where
    F: FnMut(WorkerRequest) -> RequestOutcome,
{
    match validate_line(line) {
        Ok(request) => process_request(request, handler),
        Err(response) => *response,
    }
}

/// Run one request against the real extraction pipeline on the worker's shared runtime.
///
/// The runtime and config are fixed for the worker's lifetime, so this is the piece
/// that turns "process stays alive" into "models stay loaded": `xberg::extract` goes
/// through the process-level engine and model caches, and nothing here constructs a
/// backend or runtime per request.
fn extract_request(
    runtime: &tokio::runtime::Runtime,
    config: &ExtractionConfig,
    request: WorkerRequest,
) -> WorkerOutcome {
    let config = request_config(config, &request);
    let Some(path) = request.path else {
        // Unreachable through validate_line (extract requires a path), kept as a
        // defensive non-panicking fallback for direct callers.
        return WorkerOutcome::Failure("extract requires path (local document file)".to_string());
    };
    let uri = path.to_string_lossy().into_owned();
    if let Err(error) = validate_file_exists(&path) {
        return WorkerOutcome::Failure(format!("{error:#}"));
    }
    let started = std::time::Instant::now();
    let input = ExtractInput::from_uri(uri.clone());
    match runtime.block_on(xberg::extract(input, &config)) {
        Ok(output) => match single_result_from_output(output) {
            Ok(document) => {
                tracing::info!(
                    path = %uri,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "worker request completed"
                );
                WorkerOutcome::Success(document)
            }
            Err(error) => WorkerOutcome::Failure(format!("{error:#}")),
        },
        Err(error) => WorkerOutcome::Failure(format!("failed to extract '{uri}': {error}")),
    }
}

fn request_config(startup: &ExtractionConfig, request: &WorkerRequest) -> ExtractionConfig {
    let mut config = startup.clone();
    config.cancel_token = Some(request.control.token.clone());
    // The dispatcher owns deadlines. Do not drop an extraction future while its
    // spawn_blocking parser is still running and report a false terminal result.
    config.extraction_timeout_secs = None;
    // The `transcription` config field only exists in transcription-enabled builds
    // (the `all`-feature leg server_test rebuilds does not carry it).
    #[cfg(feature = "transcription")]
    if let Some(transcription) = &mut config.transcription {
        transcription.timeout_ms = None;
    }
    match request.mode.as_deref() {
        Some(MODE_FAST) => config.disable_expensive_document_processing(),
        // An explicit `normal` opts out of the engine's large-document
        // auto-downgrade: the caller demanded full quality for this request.
        Some(MODE_NORMAL) => config.auto_fast_pages = 0,
        _ => {}
    }
    config
}

/// Handle one `ocr_snapshot` request (SNAP-14): decode the in-memory image and
/// recognize it through the lazily loaded, resident snapshot engine.
fn ocr_snapshot_request(engine: &mut SnapshotOcrEngine, request: WorkerRequest, cancel: &AtomicBool) -> RequestOutcome {
    let Some(encoded) = request.image_base64 else {
        // Unreachable through validate_line; defensive non-panicking fallback.
        return RequestOutcome::Snapshot(Err((
            "ocr_snapshot requires image_base64 (base64 PNG or equivalent lossless bytes)".to_string(),
            KIND_INPUT_INVALID,
        )));
    };
    RequestOutcome::Snapshot(recognize_base64(engine, &encoded, cancel).map(
        |Recognized {
             text,
             records,
             elapsed_ms,
         }| {
            // `text` moves into the success below, so the emptiness check runs first.
            let error_kind = text.is_empty().then_some(KIND_NO_TEXT);
            SnapshotSuccess {
                records: records.len(),
                elapsed_ms: elapsed_ms as u64,
                error_kind,
                text,
            }
        },
    ))
}

/// Handle one `snapshot_state` request (SNAP-17). The query itself always succeeds;
/// `state` reports the channel and `error` the last load failure (null when none).
fn snapshot_state_request(status: &SnapshotState) -> RequestOutcome {
    let (state, error) = status.get();
    RequestOutcome::State { state, error }
}

/// Map a media file extension onto the MIME the sensevoice stager understands
/// (its probe does not require a hint, so unknown extensions are benign).
#[cfg(feature = "transcription")]
fn mime_for_media(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4") => "video/mp4",
        Some("m4a") => "audio/x-m4a",
        Some("mp3") | Some("mpeg") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("webm") => "audio/webm",
        Some("wmv") => "video/x-ms-wmv",
        Some("asf") => "video/x-ms-asf",
        _ => "application/octet-stream",
    }
}

/// Handle one `transcribe` request (SV-12): read the local media file and run it
/// through the startup config's fixed transcription configuration. Requests carry no
/// configuration (WORKER.md semantic) — a missing/disabled block or a backend other
/// than `sensevoice` is a failure for this request only.
#[cfg(feature = "transcription")]
fn transcribe_request(config: &ExtractionConfig, request: WorkerRequest) -> RequestOutcome {
    RequestOutcome::Transcribe(transcribe_media(config, request))
}

#[cfg(feature = "transcription")]
fn transcribe_media(
    config: &ExtractionConfig,
    request: WorkerRequest,
) -> std::result::Result<TranscribeSuccess, String> {
    use xberg::core::config::transcription::TranscriptionBackend;

    let tcfg = config.transcription.as_ref().filter(|c| c.enabled).ok_or_else(|| {
        "transcribe requires a `transcription` config block with enabled = true in the worker startup config"
            .to_string()
    })?;
    if tcfg.backend != TranscriptionBackend::SenseVoice {
        return Err(
            "transcribe requires transcription.backend = \"sensevoice\" in the worker startup config".to_string(),
        );
    }
    let path = request.path.ok_or("transcribe requires path (local media file)")?;
    validate_file_exists(&path).map_err(|error| format!("{error:#}"))?;
    if request.control.token.is_cancelled() {
        return Err("transcription cancelled".into());
    }
    if let Some(max_bytes) = tcfg.max_bytes {
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if size > max_bytes {
            return Err(format!("Input size {size} exceeds transcription.max_bytes {max_bytes}"));
        }
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    // SV-06 header name: the input file's complete name including its extension,
    // exactly the JchTools media worker's `markdown()` name source (`file_name()`),
    // so callers can map each response back to its input file.
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("media")
        .to_string();
    let mime_type = mime_for_media(&path);
    let result = xberg::transcription::sensevoice::transcribe_bytes_cancellable(
        &bytes,
        &name,
        mime_type,
        tcfg.model_dir.as_deref(),
        tcfg.max_duration_ms,
        &request.control.token,
    )?;
    Ok(TranscribeSuccess {
        markdown: result.markdown,
        segments: result
            .segments
            .into_iter()
            .map(|(start_ms, end_ms, text)| TranscriptSegment { start_ms, end_ms, text })
            .collect(),
        duration_ms: result.duration_ms,
        has_audio: result.has_audio,
    })
}

/// Feature-off fallback: the command still answers (the fork feature set compiled
/// without `transcription` gets a clear per-request failure, not silence).
#[cfg(not(feature = "transcription"))]
fn transcribe_request(_config: &ExtractionConfig, _request: WorkerRequest) -> RequestOutcome {
    RequestOutcome::Transcribe(Err(
        "transcribe requires a build with the transcription feature".to_string()
    ))
}

/// Worker-only startup knobs parsed out of `--config-json` before the config
/// merge (P2/P4). `ExtractionConfig` itself never sees these keys; the merge
/// ignores them, `main` peels them off for the worker.
#[derive(Debug, Default, Clone)]
pub struct WorkerStartup {
    /// Caller identity token echoed back in `capabilities.owner` (P2).
    pub owner_token: Option<String>,
    /// Exit after this long with no request traffic and no in-flight work (P4).
    pub idle_timeout_ms: Option<u64>,
}

impl WorkerStartup {
    /// Decode the raw `--config-json` / `--config-json-base64` payload, peel off
    /// the worker-only keys (`owner_token`, P2 / `idle_timeout_ms`, P4) and
    /// return the payload with those keys removed. `ExtractionConfig` uses
    /// `deny_unknown_fields`, so the worker command must merge only the
    /// sanitized payload — every *other* unknown key still fails the merge with
    /// the usual typo-catching error. The returned `Option<String>` replaces
    /// both CLI override forms (the base64 variant is decoded here).
    pub fn strip_worker_keys(
        config_json: Option<&str>,
        config_json_base64: Option<&str>,
    ) -> Result<(Self, Option<String>)> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let (raw, original) = match (config_json, config_json_base64) {
            (Some(text), _) => (
                serde_json::from_str::<Value>(text).context("Failed to parse --config-json as JSON")?,
                text.to_string(),
            ),
            (None, Some(encoded)) => {
                let bytes = STANDARD
                    .decode(encoded)
                    .context("Failed to decode base64 in --config-json-base64")?;
                let text = String::from_utf8(bytes).context("Base64-decoded content is not valid UTF-8")?;
                (
                    serde_json::from_str::<Value>(&text)
                        .context("Failed to parse decoded --config-json-base64 as JSON")?,
                    text,
                )
            }
            (None, None) => return Ok((Self::default(), None)),
        };
        let Value::Object(mut fields) = raw else {
            // A non-object payload cannot carry worker keys; pass it through so
            // the config merge reports it exactly as `extract` would.
            return Ok((Self::default(), Some(original)));
        };
        let owner_token = match fields.remove("owner_token") {
            None => None,
            Some(Value::String(token)) => Some(token),
            Some(_) => anyhow::bail!("owner_token must be a string"),
        };
        let idle_timeout_ms = match fields.remove("idle_timeout_ms") {
            None => None,
            Some(Value::Number(number)) => {
                let millis = number.as_u64().filter(|millis| *millis > 0);
                if millis.is_none() {
                    anyhow::bail!("idle_timeout_ms must be a positive integer (milliseconds)");
                }
                millis
            }
            Some(_) => anyhow::bail!("idle_timeout_ms must be a positive integer (milliseconds)"),
        };
        let sanitized = serde_json::to_string(&Value::Object(fields)).context("failed to re-serialize config")?;
        Ok((
            Self {
                owner_token,
                idle_timeout_ms,
            },
            Some(sanitized),
        ))
    }
}

/// Per-process identity reported in `capabilities` (P2). Every field is
/// optional from the caller's perspective; we always fill the first three, and
/// `owner` appears only when the startup config carried `owner_token`.
#[derive(Debug)]
pub(crate) struct WorkerIdentity {
    instance_id: String,
    started_at: String,
    config_digest: String,
}

impl WorkerIdentity {
    pub(crate) fn new(config: &ExtractionConfig) -> Self {
        Self {
            instance_id: fresh_instance_id(),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            config_digest: sha256_hex(serde_json::to_string(config).unwrap_or_default().as_bytes()),
        }
    }
}

/// Random v4-shaped UUID from std's randomly seeded hasher (no new dependency);
/// stable for the process lifetime.
fn fresh_instance_id() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    let mut first = RandomState::new().build_hasher();
    first.write_u32(std::process::id());
    first.write_u128(nanos);
    let hi = first.finish();
    let mut second = RandomState::new().build_hasher();
    second.write_u64(hi);
    second.write_u128(!nanos);
    let lo = second.finish();
    let time_low = (hi >> 32) as u32;
    let time_mid = ((hi >> 16) & 0xffff) as u16;
    let time_hi = (0x4000 | (hi & 0x0fff)) as u16;
    let clock_seq = (0x8000 | ((lo >> 48) & 0x3fff)) as u16;
    let node = lo & 0x0000_ffff_ffff_ffff;
    format!("{time_low:08x}-{time_mid:04x}-{time_hi:04x}-{clock_seq:04x}-{node:012x}")
}

/// sha256 of an in-memory byte string, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// sha256 of an on-disk file (streamed); `None` when it cannot be read.
fn file_sha256(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// capabilities payload (P2): the pre-P2 capability keys plus the identity
/// block. `owner` is present only when the caller injected `owner_token`.
fn capabilities_payload(identity: &WorkerIdentity, owner_token: Option<&str>) -> Value {
    let mut payload = serde_json::json!({
        "protocol_version": 2,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "commands": [
            "extract", "ocr_snapshot", "transcribe", "snapshot_state", "cancel",
            "formats", "capabilities", "model_state", "keepalive", "shutdown", "version"
        ],
        "extract_modes": ["normal", "fast"],
        "cancellation": "cooperative",
        "timeout_ms": true,
        "document_snapshot_concurrent": true,
        "transcription": cfg!(feature = "transcription"),
        "layout": cfg!(feature = "layout-detection"),
        "paddle_ocr": cfg!(feature = "paddle-ocr"),
        "instance_id": identity.instance_id,
        "started_at": identity.started_at,
        "config_digest": identity.config_digest,
    });
    if let Some(owner) = owner_token {
        payload["owner"] = Value::String(owner.to_string());
    }
    payload
}

/// keepalive payload (P6): liveness plus a cheap readiness summary. It only
/// proves the event loop answers; nothing is loaded and no timer is reset.
fn keepalive_payload(status: &SnapshotState, document_backend: &str) -> Value {
    let (snapshot, _) = status.get();
    serde_json::json!({
        "models": {
            "snapshot": snapshot,
            "document": document_backend,
            "transcription": cfg!(feature = "transcription"),
        }
    })
}

/// `version` payload (P5): build identity plus the on-disk model inventory the
/// process would use. Existence/size/sha256 only — nothing is loaded.
fn version_payload(config: &ExtractionConfig) -> Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "debug": cfg!(debug_assertions),
        },
        "models": model_inventory(config),
    })
}

fn model_inventory(config: &ExtractionConfig) -> Value {
    let snapshot = match super::snapshot_ocr::resolve_models_dir(
        None,
        config
            .snapshot_ocr
            .as_ref()
            .and_then(|block| block.models_dir.as_deref()),
        std::env::var_os(super::snapshot_ocr::MODELS_DIR_ENV).as_deref(),
    ) {
        Ok(dir) => serde_json::json!({
            "dir": dir.display().to_string(),
            "members": [
                model_entry("det.onnx", &dir.join("det.onnx")),
                model_entry("rec.onnx", &dir.join("rec.onnx")),
                model_entry("dict/dict.txt", &dir.join("dict").join("dict.txt")),
            ],
        }),
        Err(error) => serde_json::json!({ "error": error }),
    };
    serde_json::json!({
        "snapshot": snapshot,
        "transcription": transcription_inventory(config),
    })
}

/// Transcription inventory section; feature-off builds report their state
/// instead of guessing paths that cannot be configured in that profile.
#[cfg(feature = "transcription")]
fn transcription_inventory(config: &ExtractionConfig) -> Value {
    // Explicit config verbatim, else the environment override, else the first
    // exe-adjacent candidate (mirrors the engine's own resolution order;
    // report-only, never an error).
    let root = config
        .transcription
        .as_ref()
        .and_then(|block| block.model_dir.clone())
        .or_else(|| std::env::var_os(xberg::transcription::sensevoice::MODEL_DIR_ENV).map(PathBuf::from))
        .unwrap_or_else(transcription_exe_adjacent_root);
    serde_json::json!({
        "dir": root.display().to_string(),
        "members": [
            model_entry(
                "sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
                &root.join("sense_voice_zh_en_ja_ko_yue_2024_07_17").join("model.int8.onnx"),
            ),
            model_entry(
                "sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt",
                &root.join("sense_voice_zh_en_ja_ko_yue_2024_07_17").join("tokens.txt"),
            ),
            model_entry("vad/silero_vad.onnx", &root.join("vad").join("silero_vad.onnx")),
        ],
    })
}

#[cfg(not(feature = "transcription"))]
fn transcription_inventory(_config: &ExtractionConfig) -> Value {
    serde_json::json!({ "state": "unavailable", "error": "built without the transcription feature" })
}

/// One model-inventory row: where the engine would look, whether the file is
/// there, and its identity when it is. A configured-but-missing root reports
/// its configured path with `exists:false` — that mismatch is the diagnostic.
fn model_entry(name: &str, path: &Path) -> Value {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => serde_json::json!({
            "name": name,
            "path": path.display().to_string(),
            "exists": true,
            "size_bytes": meta.len(),
            "sha256": file_sha256(path),
        }),
        _ => serde_json::json!({
            "name": name,
            "path": path.display().to_string(),
            "exists": false,
        }),
    }
}

/// Fallback exe-adjacent transcription candidates (the engine checks
/// `media-models`, `models`, then the exe directory itself).
fn transcription_exe_adjacent_root() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_default();
    [exe_dir.join("media-models"), exe_dir.join("models"), exe_dir.clone()]
        .into_iter()
        .find(|candidate| candidate.is_dir())
        .unwrap_or(exe_dir)
}

/// Execute the `xberg worker` command.
///
/// Builds the one tokio runtime and resolves the one config this worker will use for
/// the process lifetime, then serves stdin until EOF. The snapshot OCR engine is created
/// lazily on the first `ocr_snapshot` request against `config.snapshot_ocr`; every
/// other backend rides the library's process-level caches. Diagnostics (including
/// the per-request info log with elapsed time) go to stderr via `tracing`.
// (fork) perf-tracing：批次级性能 span；逐文件耗时另有 `worker request completed` 日志。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "worker_command", skip_all)
)]
pub fn worker_command(config: ExtractionConfig, startup: WorkerStartup) -> Result<()> {
    let runtime = build_runtime(&config).context("failed to build the worker's tokio runtime")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut snapshot_engine = SnapshotOcrEngine::new(config.snapshot_ocr.clone());
    let status = snapshot_engine.state();
    let default_timeout = config.extraction_timeout_secs.map(std::time::Duration::from_secs);
    // Initialize registries once, before inference, without loading models.
    let formats = serde_json::to_value(super::formats::compiled_in_formats()?)?;
    let document_backend = config
        .ocr
        .as_ref()
        .map(|ocr| ocr.backend.clone())
        .unwrap_or_else(|| "paddle-ocr".into());
    // P2: per-process identity for capabilities; P1: host-death watchers.
    let identity = WorkerIdentity::new(&config);
    let owner_token = startup.owner_token.clone();
    let peer_gone = Arc::new(AtomicBool::new(false));
    monitor::spawn_peer_gone_monitors(Arc::clone(&peer_gone));
    let query_config = config.clone();
    let document = move |request: WorkerRequest| match request.command.as_str() {
        COMMAND_TRANSCRIBE => transcribe_request(&config, request),
        _ => RequestOutcome::Extract(extract_request(&runtime, &config, request)),
    };
    let screenshot = move |request: WorkerRequest| {
        let control = request.control.clone();
        ocr_snapshot_request(&mut snapshot_engine, request, control.token.as_atomic())
    };
    let query = move |command: &str| match command {
        COMMAND_FORMATS => RequestOutcome::Query(serde_json::json!({"formats": formats})),
        COMMAND_CAPABILITIES => RequestOutcome::Query(capabilities_payload(&identity, owner_token.as_deref())),
        COMMAND_VERSION => RequestOutcome::Query(version_payload(&query_config)),
        COMMAND_KEEPALIVE => RequestOutcome::Query(keepalive_payload(&status, &document_backend)),
        COMMAND_MODEL_STATE => {
            let (snapshot, error) = status.get();
            #[cfg(feature = "ocr")]
            let document = {
                let registry = xberg::plugins::registry::get_ocr_backend_registry();
                let registry = registry.read();
                registry.model_state(&document_backend)
            };
            #[cfg(not(feature = "ocr"))]
            let document = serde_json::json!({"state": "unavailable"});
            #[cfg(feature = "transcription")]
            let media = xberg::transcription::sensevoice::model_state();
            #[cfg(not(feature = "transcription"))]
            let media = serde_json::json!({"state": "unavailable"});
            RequestOutcome::Query(serde_json::json!({"models": {
                "snapshot": {"state": snapshot, "error": error},
                "document": {"backend": document_backend, "models": document}, "transcription": media
            }}))
        }
        _ => snapshot_state_request(&status),
    };
    // Do not move a StdinLock across threads; the reader thread owns the handle.
    let stdin = std::io::BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout().lock();
    let options = LoopOptions {
        default_timeout,
        idle_timeout: startup.idle_timeout_ms.map(Duration::from_millis),
        peer_gone: Some(peer_gone),
        ..LoopOptions::default()
    };
    let exit: LoopExit = run_worker_loop(stdin, &mut stdout, cancel, options, document, screenshot, query)?;
    let code = scheduler::exit_code_for(exit);
    if code != 0 {
        tracing::warn!(code, reason = ?exit, "worker stopping after stdio disconnect or idle timeout");
        FORCED_EXIT_CODE.store(code, std::sync::atomic::Ordering::Release);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn fast_mode_changes_only_the_request_config_and_normal_restores_ocr() {
        let startup = ExtractionConfig {
            force_ocr: true,
            ..Default::default()
        };
        let before = serde_json::to_value(&startup).expect("config serializes");
        let mut fast = request(json!(1), "large.pdf");
        fast.mode = Some(MODE_FAST.into());
        let effective = request_config(&startup, &fast);
        assert!(effective.disable_ocr);
        assert!(!effective.force_ocr);
        assert!(!effective.runs_ocr_on_embedded_images());
        assert_eq!(serde_json::to_value(&startup).unwrap(), before);
        assert_eq!(
            serde_json::to_value(&effective.snapshot_ocr).unwrap(),
            before["snapshot_ocr"]
        );
        let normal = request_config(&startup, &request(json!(2), "small.pdf"));
        assert!(!normal.disable_ocr);
        assert!(normal.force_ocr);
        #[cfg(feature = "ocr")]
        assert!(normal.runs_ocr_on_embedded_images());
    }

    #[test]
    fn explicit_normal_opts_out_of_auto_downgrade_and_omitted_mode_keeps_it() {
        let startup = ExtractionConfig::default();
        let mut normal = request(json!(1), "large.pdf");
        normal.mode = Some(MODE_NORMAL.into());
        assert_eq!(
            request_config(&startup, &normal).auto_fast_pages,
            0,
            "explicit normal must disable the engine's large-document auto-downgrade"
        );
        assert_eq!(
            request_config(&startup, &request(json!(2), "large.pdf")).auto_fast_pages,
            ExtractionConfig::default_auto_fast_pages(),
            "omitted mode keeps the engine default"
        );
    }

    #[test]
    fn control_protocol_validates_target_timeout_mode_and_preserves_ids() {
        assert!(validate_line(r#"{"id":1,"command":"cancel","target_id":null}"#).is_ok());
        assert!(validate_line(r#"{"id":2,"command":"extract","path":"a.pdf","mode":"fast","timeout_ms":1}"#).is_ok());
        for command in ["formats", "capabilities", "model_state", "keepalive", "version"] {
            assert!(validate_line(&json!({"id":3,"command":command}).to_string()).is_ok());
        }
        assert!(validate_line(r#"{"id":5,"command":"shutdown","grace_ms":250}"#).is_ok());
        for value in [
            json!({"id":4,"command":"extract"}),
            json!({"id":4,"command":"cancel"}),
            json!({"id":4,"command":"ocr_snapshot","image_base64":"x","mode":"fast"}),
            json!({"id":4,"command":"extract","path":"x","timeout_ms":0}),
            json!({"id":4,"command":"shutdown","grace_ms":0}),
        ] {
            assert_eq!(validate_line(&value.to_string()).unwrap_err().id, json!(4));
        }
    }

    fn request(id: Value, path: &str) -> WorkerRequest {
        WorkerRequest {
            id,
            command: COMMAND_EXTRACT.to_string(),
            path: Some(PathBuf::from(path)),
            mode: None,
            image_base64: None,
            ..Default::default()
        }
    }

    fn transcribe_req(id: Value, path: &str) -> WorkerRequest {
        WorkerRequest {
            id,
            command: COMMAND_TRANSCRIBE.to_string(),
            path: Some(PathBuf::from(path)),
            mode: None,
            image_base64: None,
            ..Default::default()
        }
    }

    fn extract_outcome_failure(message: &str) -> RequestOutcome {
        RequestOutcome::Extract(WorkerOutcome::Failure(message.to_string()))
    }

    fn warning(source: &'static str, message: &'static str) -> ProcessingWarning {
        ProcessingWarning {
            source: source.into(),
            message: message.into(),
        }
    }

    #[test]
    fn parse_request_accepts_the_contract_shape_and_ignores_unknown_fields() {
        let parsed =
            parse_request(r#"{"id":1,"command":"extract","path":"C:\\docs\\a.pdf","mode":"normal","future":42}"#)
                .expect("the contract's own example must parse");

        assert_eq!(parsed.id, json!(1));
        assert_eq!(parsed.command, "extract");
        assert_eq!(parsed.path.as_deref(), Some(Path::new("C:\\docs\\a.pdf")));
        assert_eq!(parsed.mode.as_deref(), Some("normal"));
    }

    #[test]
    fn parse_request_defaults_mode_and_rejects_missing_required_fields() {
        let parsed = parse_request(r#"{"id":"tag","command":"extract","path":"a.txt"}"#).expect("mode is optional");
        assert_eq!(parsed.mode, None);
        assert_eq!(parsed.id, json!("tag"));

        for broken in [
            r#"{"command":"extract","path":"a.txt"}"#,
            r#"{"id":1,"path":"a.txt"}"#,
            r#"{"id":1,"command":"extract"}"#,
            "not json at all",
        ] {
            assert!(parse_request(broken).is_err(), "must reject: {broken}");
        }
    }

    /// New commands parse with their own fields; unknown fields stay ignored and
    /// optional ones (`path` on snapshot commands) default cleanly.
    #[test]
    fn parse_request_accepts_the_new_commands() {
        let ocr = parse_request(r#"{"id":9,"command":"ocr_snapshot","image_base64":"aGk=","future":1}"#)
            .expect("ocr_snapshot with image bytes must parse");
        assert_eq!(ocr.id, json!(9));
        assert_eq!(ocr.image_base64.as_deref(), Some("aGk="));
        assert_eq!(ocr.path, None, "snapshot commands do not need a path");

        let state =
            parse_request(r#"{"id":"s","command":"snapshot_state"}"#).expect("snapshot_state needs no extra fields");
        assert_eq!(state.id, json!("s"));

        let transcribe = parse_request(r#"{"id":true,"command":"transcribe","path":"E:/m/speech.mp4"}"#)
            .expect("transcribe with a path must parse");
        assert_eq!(transcribe.path.as_deref(), Some(Path::new("E:/m/speech.mp4")));
    }

    #[test]
    fn process_line_reports_invalid_json_with_a_null_id() {
        let mut handler = |_request: WorkerRequest| extract_outcome_failure("unreachable");
        let response = process_line("}{ not json", &mut handler);

        assert_eq!(response.id, Value::Null);
        assert!(!response.ok);
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid worker request JSON")
        );
    }

    #[test]
    fn process_line_rejects_unsupported_commands_and_modes_without_calling_the_handler() {
        let mut calls = 0usize;
        let mut handler = |_request: WorkerRequest| {
            calls += 1;
            extract_outcome_failure("handler must not run")
        };

        let command = process_line(r#"{"id":7,"command":"detect","path":"a.pdf"}"#, &mut handler);
        assert!(!command.ok);
        assert_eq!(command.id, json!(7));
        assert!(
            command
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("unsupported command 'detect'")
        );
        // P2: the structured category and command name ride alongside the legacy
        // text prefix the run49.1 passthrough matches on.
        assert_eq!(command.error_kind, Some(KIND_UNSUPPORTED_COMMAND));
        let extra = command.extra.expect("unknown commands name the command");
        assert_eq!(extra["command"], json!("detect"));

        let mode = process_line(
            r#"{"id":8,"command":"extract","path":"a.pdf","mode":"invalid"}"#,
            &mut handler,
        );
        assert!(!mode.ok);
        assert_eq!(mode.id, json!(8));
        assert!(
            mode.error
                .as_deref()
                .unwrap_or_default()
                .contains("unsupported mode 'invalid'")
        );

        assert_eq!(calls, 0, "protocol rejections must not reach the handler");
    }

    /// Per-command field validation echoes the id (unlike parse-level
    /// rejections): `ocr_snapshot` needs `image_base64`, `transcribe` needs `path`.
    #[test]
    fn process_line_rejects_new_commands_with_missing_fields_and_echoes_the_id() {
        let mut calls = 0usize;
        let mut handler = |_request: WorkerRequest| {
            calls += 1;
            extract_outcome_failure("handler must not run")
        };

        let ocr = process_line(r#"{"id":11,"command":"ocr_snapshot"}"#, &mut handler);
        assert!(!ocr.ok);
        assert_eq!(ocr.id, json!(11));
        assert!(
            ocr.error.as_deref().unwrap_or_default().contains("image_base64"),
            "unexpected: {:?}",
            ocr.error
        );

        let transcribe = process_line(r#"{"id":12,"command":"transcribe"}"#, &mut handler);
        assert!(!transcribe.ok);
        assert_eq!(transcribe.id, json!(12));
        assert!(
            transcribe.error.as_deref().unwrap_or_default().contains("path"),
            "unexpected: {:?}",
            transcribe.error
        );

        assert_eq!(calls, 0, "field rejections must not reach the handler");
    }

    #[test]
    fn process_line_converts_handler_panics_into_failure_responses() {
        let mut handler = |_request: WorkerRequest| -> RequestOutcome {
            panic!("backend exploded");
        };
        let response = process_line(
            r#"{"id":3,"command":"extract","path":"a.pdf","mode":"normal"}"#,
            &mut handler,
        );

        assert!(!response.ok);
        assert_eq!(response.id, json!(3));
        assert!(response.error.as_deref().unwrap_or_default().contains("panicked"));
    }

    #[test]
    fn process_line_passes_document_and_warnings_through_on_success() {
        let mut document = ExtractedDocument::default();
        document.processing_warnings = vec![warning("ocr", "degraded to fallback")];
        let mut handler =
            move |_request: WorkerRequest| RequestOutcome::Extract(WorkerOutcome::Success(document.clone()));
        let response = process_line(r#"{"id":true,"command":"extract","path":"a.pdf"}"#, &mut handler);

        assert!(response.ok);
        assert_eq!(response.id, json!(true));
        let document = response.document.expect("success carries the document");
        assert_eq!(document.processing_warnings.len(), 1);
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(response.warnings[0].source.as_ref(), "ocr");
    }

    #[test]
    fn responses_serialize_the_contract_shape() {
        let mut document = ExtractedDocument::default();
        document.processing_warnings = vec![warning("ocr", "degraded")];
        let success = serde_json::to_value(WorkerResponse::success(json!(1), document)).expect("response serializes");
        assert_eq!(success["id"], json!(1));
        assert_eq!(success["ok"], json!(true));
        assert!(success.get("document").is_some(), "success must carry document");
        assert!(success.get("error").is_none(), "success must omit error");
        assert_eq!(success["warnings"][0]["source"], json!("ocr"));

        let failure = serde_json::to_value(WorkerResponse::failure(json!("tag"), "boom".to_string()))
            .expect("response serializes");
        assert_eq!(failure["id"], json!("tag"));
        assert_eq!(failure["ok"], json!(false));
        assert!(failure.get("document").is_none(), "failure must omit document");
        assert_eq!(failure["error"], json!("boom"));
        assert_eq!(failure["warnings"], json!([]));
    }

    /// `ocr_snapshot` wire shapes (SNAP-14/SNAP-15): success carries text, record
    /// count and elapsed time; a text-free image stays `ok:true` with
    /// `error_kind:"no_text"`; failures carry `error_kind`.
    #[test]
    fn ocr_snapshot_outcomes_serialize_the_contract_shape() {
        let outcome = RequestOutcome::Snapshot(Ok(SnapshotSuccess {
            text: "页面文字".to_string(),
            records: 3,
            elapsed_ms: 4210,
            error_kind: None,
        }));
        let value = serde_json::to_value(render_outcome(json!(5), outcome)).expect("serializes");
        assert_eq!(value["id"], json!(5));
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["text"], json!("页面文字"));
        assert_eq!(value["records"], json!(3));
        assert_eq!(value["elapsed_ms"], json!(4210));
        assert!(value.get("error").is_none(), "success must omit error");
        assert!(value.get("error_kind").is_none(), "plain success must omit error_kind");
        assert!(value.get("document").is_none(), "snapshot success must omit document");

        let no_text = RequestOutcome::Snapshot(Ok(SnapshotSuccess {
            text: String::new(),
            records: 0,
            elapsed_ms: 10,
            error_kind: Some(KIND_NO_TEXT),
        }));
        let value = serde_json::to_value(render_outcome(json!(6), no_text)).expect("serializes");
        assert_eq!(value["ok"], json!(true), "no text is still a success");
        assert_eq!(value["text"], json!(""));
        assert_eq!(value["error_kind"], json!("no_text"));

        let failure = RequestOutcome::Snapshot(Err((
            "snapshot model asset mismatch (det)".to_string(),
            crate::commands::snapshot_ocr::KIND_ASSET_INVALID,
        )));
        let value = serde_json::to_value(render_outcome(json!(7), failure)).expect("serializes");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error_kind"], json!("asset_invalid"));
        assert!(
            value["error"].as_str().unwrap_or_default().contains("asset mismatch"),
            "failure must carry the reason"
        );
    }

    /// `snapshot_state` wire shape (SNAP-17): always ok:true, `state` present,
    /// `error` key present even when null.
    #[test]
    fn snapshot_state_outcomes_serialize_the_contract_shape() {
        let value = serde_json::to_value(render_outcome(
            json!("state"),
            RequestOutcome::State {
                state: "uninitialized",
                error: None,
            },
        ))
        .expect("serializes");
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["state"], json!("uninitialized"));
        assert_eq!(
            value.get("error"),
            Some(&Value::Null),
            "error key must be present as null"
        );
        assert!(value.get("text").is_none());

        let value = serde_json::to_value(render_outcome(
            json!("state2"),
            RequestOutcome::State {
                state: "error",
                error: Some("snapshot model asset mismatch (rec)".to_string()),
            },
        ))
        .expect("serializes");
        assert_eq!(value["state"], json!("error"));
        assert_eq!(value["error"], json!("snapshot model asset mismatch (rec)"));
    }

    /// `transcribe` wire shape (SV-12): markdown + segments + duration + has_audio.
    #[test]
    fn transcribe_outcomes_serialize_the_contract_shape() {
        let outcome = RequestOutcome::Transcribe(Ok(TranscribeSuccess {
            markdown: "# speech\n## 转录\n".to_string(),
            segments: vec![TranscriptSegment {
                start_ms: 120,
                end_ms: 2400,
                text: "你好".to_string(),
            }],
            duration_ms: 5000,
            has_audio: true,
        }));
        let value = serde_json::to_value(render_outcome(json!(8), outcome)).expect("serializes");
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["markdown"], json!("# speech\n## 转录\n"));
        assert_eq!(value["segments"][0]["start_ms"], json!(120));
        assert_eq!(value["segments"][0]["end_ms"], json!(2400));
        assert_eq!(value["segments"][0]["text"], json!("你好"));
        assert_eq!(value["duration_ms"], json!(5000));
        assert_eq!(value["has_audio"], json!(true));

        let failure = RequestOutcome::Transcribe(Err("媒体模型缺失: E:/m/model.int8.onnx".to_string()));
        let value = serde_json::to_value(render_outcome(json!(9), failure)).expect("serializes");
        assert_eq!(value["ok"], json!(false));
        assert!(value["error"].as_str().unwrap_or_default().contains("媒体模型缺失"));
    }

    fn unique_temp_path(extension: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("xberg-worker-ut-{}-{sequence}.{extension}", std::process::id()))
    }

    /// Exercises the real (unmocked) request path — runtime, `xberg::extract`, and the
    /// single-result unwrapping — with a real local file, mirroring how
    /// `commands::extract`'s own tests run the real pipeline on a missing path.
    #[test]
    fn extract_request_extracts_a_local_file_and_reports_a_missing_one() {
        let config = ExtractionConfig::default();
        let runtime = build_runtime(&config).expect("test runtime builds");

        let path = unique_temp_path("txt");
        std::fs::write(&path, "hello from the worker test").expect("temp file writes");
        let outcome = extract_request(&runtime, &config, request(Value::Null, &path.to_string_lossy()));
        let _ = std::fs::remove_file(&path);
        let document = match outcome {
            WorkerOutcome::Success(document) => document,
            WorkerOutcome::Failure(error) => panic!("local text file must extract, got: {error}"),
        };
        assert!(
            document.content.contains("hello from the worker test"),
            "content must carry the file text, got: {}",
            document.content
        );

        let missing = std::env::temp_dir().join("xberg-worker-ut-definitely-missing-9f3c2a11.txt");
        match extract_request(&runtime, &config, request(Value::Null, &missing.to_string_lossy())) {
            WorkerOutcome::Failure(error) => {
                assert!(!error.is_empty(), "failure must carry a description");
            }
            WorkerOutcome::Success(_) => panic!("a missing file must fail, not succeed"),
        }
    }

    /// `transcribe` without a transcription config fails this request only and
    /// names the missing configuration (SV-12: startup config is fixed; the
    /// process keeps serving).
    #[test]
    #[cfg(feature = "transcription")]
    fn transcribe_without_config_fails_without_ending_the_batch() {
        let config = ExtractionConfig::default();
        let outcome = transcribe_request(&config, transcribe_req(json!("t"), "E:/no/speech.mp4"));
        let RequestOutcome::Transcribe(Err(error)) = outcome else {
            panic!("expected a transcribe failure");
        };
        assert!(error.contains("transcription"), "unexpected: {error}");
        assert!(
            error.contains("sensevoice") || error.contains("config"),
            "unexpected: {error}"
        );
    }

    /// The Whisper backend no longer exists, so the pre-removal
    /// "reject non-sensevoice backends" guard state is unconstructible: parsing a
    /// startup config that still says `"backend": "whisper"` (SV-11) fails before
    /// any `transcribe` request can run. This pins that contract at the CLI level.
    #[test]
    #[cfg(feature = "transcription")]
    fn startup_config_rejects_the_retired_whisper_backend() {
        let parsed = serde_json::from_value::<ExtractionConfig>(json!({
            "transcription": { "enabled": true, "backend": "whisper" }
        }));
        let error = parsed.expect_err("the retired whisper backend must be rejected");
        assert!(error.to_string().contains("unknown variant"), "unexpected: {error}");
        assert!(error.to_string().contains("sensevoice"), "unexpected: {error}");
    }

    /// Missing media files fail the request without touching any model.
    #[test]
    #[cfg(feature = "transcription")]
    fn transcribe_reports_missing_media_files() {
        use xberg::core::config::transcription::{TranscriptionBackend, TranscriptionConfig};
        let config = ExtractionConfig {
            transcription: Some(TranscriptionConfig {
                backend: TranscriptionBackend::SenseVoice,
                ..Default::default()
            }),
            ..Default::default()
        };
        let missing = unique_temp_path("definitely-missing").with_extension("mp4");
        let _ = std::fs::remove_file(&missing);
        let outcome = transcribe_request(&config, transcribe_req(json!("t"), &missing.to_string_lossy()));
        let RequestOutcome::Transcribe(Err(error)) = outcome else {
            panic!("expected a transcribe failure");
        };
        assert!(
            error.contains("File not found") || error.contains("not a file"),
            "unexpected: {error}"
        );
    }

    /// P2: capabilities carries the identity block and lists every command,
    /// including the new keepalive/shutdown/version surface.
    #[test]
    fn capabilities_report_identity_and_the_full_command_surface() {
        let identity = WorkerIdentity::new(&ExtractionConfig::default());
        let payload = capabilities_payload(&identity, None);
        assert_eq!(payload["protocol_version"], json!(2));
        for command in [
            "extract",
            "ocr_snapshot",
            "transcribe",
            "cancel",
            "keepalive",
            "shutdown",
            "version",
        ] {
            assert!(
                payload["commands"].as_array().unwrap().contains(&json!(command)),
                "capabilities.commands must list {command}"
            );
        }
        assert!(
            payload["instance_id"].as_str().unwrap().len() >= 32,
            "stable instance id"
        );
        assert!(
            payload["started_at"].as_str().unwrap().contains('T'),
            "RFC3339 timestamp"
        );
        assert_eq!(
            payload["config_digest"].as_str().unwrap().len(),
            64,
            "sha256 hex digest"
        );
        assert!(payload.get("owner").is_none(), "owner stays absent without owner_token");

        let owned = capabilities_payload(&identity, Some("jchtools-host-a"));
        assert_eq!(owned["owner"], json!("jchtools-host-a"));
        // The digest tracks the effective config.
        let mut other = ExtractionConfig::default();
        other.force_ocr = true;
        assert_ne!(WorkerIdentity::new(&other).config_digest, identity.config_digest);
    }

    /// P2/P4: the worker-only startup keys peel off both JSON forms into the
    /// startup struct, and the sanitized payload (without those keys) is what
    /// reaches the config merge. Everything else — including other unknown
    /// fields, which the merge must still reject — passes through untouched.
    #[test]
    fn worker_startup_keys_parse_from_inline_and_base64_forms() {
        let (plain, sanitized) = WorkerStartup::strip_worker_keys(
            Some(r#"{"owner_token":"owner-1","idle_timeout_ms":1500,"snapshot_ocr":{"models_dir":"E:/m"}}"#),
            None,
        )
        .expect("worker keys peel off");
        assert_eq!(plain.owner_token.as_deref(), Some("owner-1"));
        assert_eq!(plain.idle_timeout_ms, Some(1500));
        let sanitized = sanitized.expect("sanitized payload");
        let sanitized: Value = serde_json::from_str(&sanitized).unwrap();
        assert!(
            sanitized.get("owner_token").is_none(),
            "worker keys must not reach the merge"
        );
        assert!(
            sanitized.get("idle_timeout_ms").is_none(),
            "worker keys must not reach the merge"
        );
        assert_eq!(
            sanitized["snapshot_ocr"]["models_dir"],
            json!("E:/m"),
            "other fields survive"
        );

        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = STANDARD.encode(r#"{"owner_token":"owner-2"}"#);
        let (decoded, sanitized) = WorkerStartup::strip_worker_keys(None, Some(&encoded)).expect("base64 form peels");
        assert_eq!(decoded.owner_token.as_deref(), Some("owner-2"));
        assert_eq!(decoded.idle_timeout_ms, None);
        assert_eq!(sanitized.as_deref(), Some("{}"));

        let (empty, none) = WorkerStartup::strip_worker_keys(None, None).expect("no overrides is valid");
        assert_eq!(empty.owner_token, None);
        assert_eq!(empty.idle_timeout_ms, None);
        assert!(none.is_none());

        for bad in [
            r#"{"idle_timeout_ms":0}"#,
            r#"{"idle_timeout_ms":-5}"#,
            r#"{"idle_timeout_ms":"x"}"#,
            r#"{"owner_token":7}"#,
        ] {
            assert!(
                WorkerStartup::strip_worker_keys(Some(bad), None).is_err(),
                "must be rejected at startup: {bad}"
            );
        }
    }

    /// P5: the version manifest reports build identity and the on-disk model
    /// inventory — existence, size and digest for what is actually there.
    #[test]
    fn version_manifest_reports_build_and_model_inventory() {
        let dir = std::env::temp_dir().join(format!("xberg-worker-version-ut-{}", std::process::id()));
        let snapshot_dir = dir.join("snapshot");
        std::fs::create_dir_all(snapshot_dir.join("dict")).expect("dirs");
        std::fs::write(snapshot_dir.join("det.onnx"), b"det-bytes").expect("det");
        let mut config = ExtractionConfig::default();
        config.snapshot_ocr = Some(xberg::core::config::SnapshotOcrConfig {
            models_dir: Some(snapshot_dir.clone()),
            intra_threads: 1,
        });
        let payload = version_payload(&config);
        assert_eq!(payload["version"], json!(env!("CARGO_PKG_VERSION")));
        assert!(payload["build"]["os"].is_string());
        let members = payload["models"]["snapshot"]["members"].as_array().expect("members");
        let det = members
            .iter()
            .find(|m| m["name"] == json!("det.onnx"))
            .expect("det entry");
        assert_eq!(det["exists"], json!(true));
        assert_eq!(det["size_bytes"], json!(9));
        assert_eq!(det["sha256"].as_str().unwrap().len(), 64);
        let rec = members
            .iter()
            .find(|m| m["name"] == json!("rec.onnx"))
            .expect("rec entry");
        assert_eq!(rec["exists"], json!(false));
        assert!(payload["models"]["transcription"]["members"].as_array().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P6: keepalive carries a cheap readiness summary and nothing heavy.
    #[test]
    fn keepalive_payload_reports_cheap_readiness() {
        let engine = SnapshotOcrEngine::new(None);
        let status = engine.state();
        let payload = keepalive_payload(&status, "paddle-ocr");
        assert_eq!(payload["models"]["snapshot"], json!("uninitialized"));
        assert_eq!(payload["models"]["document"], json!("paddle-ocr"));
        assert!(payload["models"]["transcription"].is_boolean());
    }

    /// P1/P4: stop reasons map to the documented exit codes.
    #[test]
    fn stop_reasons_map_to_documented_exit_codes() {
        assert_eq!(scheduler::exit_code_for(LoopExit::SessionEnded), 0);
        assert_eq!(
            scheduler::exit_code_for(LoopExit::PeerDisconnected),
            EXIT_PEER_DISCONNECTED
        );
        assert_eq!(scheduler::exit_code_for(LoopExit::IdleTimeout), EXIT_IDLE_TIMEOUT);
        assert_ne!(EXIT_PEER_DISCONNECTED, 0);
        assert_ne!(EXIT_IDLE_TIMEOUT, 0);
        assert_ne!(EXIT_PEER_DISCONNECTED, EXIT_IDLE_TIMEOUT);
    }
}
