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
mod scheduler;
use scheduler::run_worker_loop;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::path::PathBuf;
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
            id,
            ok: true,
            document: Some(document),
            error: None,
            warnings,
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

    fn failure(id: Value, error: String) -> Self {
        Self {
            id,
            ok: false,
            document: None,
            error: Some(error),
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

    /// Response skeleton with the extract payload slots empty.
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
        COMMAND_FORMATS | COMMAND_CAPABILITIES | COMMAND_MODEL_STATE => {}
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
            return Err(Box::new(WorkerResponse::failure(
                id,
                format!("unsupported command '{other}'; use capabilities to list commands"),
            )));
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
    if request.mode.as_deref() == Some(MODE_FAST) {
        config.disable_expensive_document_processing();
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
    RequestOutcome::Snapshot(match recognize_base64(engine, &encoded, cancel) {
        Ok(Recognized {
            text,
            records,
            elapsed_ms,
        }) => {
            let no_text = text.is_empty();
            Ok(SnapshotSuccess {
                records: records.len(),
                elapsed_ms: elapsed_ms as u64,
                error_kind: no_text.then_some(KIND_NO_TEXT),
                text,
            })
        }
        Err((message, kind)) => Err((message, kind)),
    })
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
pub fn worker_command(config: ExtractionConfig) -> Result<()> {
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
        COMMAND_CAPABILITIES => RequestOutcome::Query(serde_json::json!({
            "protocol_version": 2, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id(),
            "commands": ["extract", "ocr_snapshot", "transcribe", "snapshot_state", "cancel", "formats", "capabilities", "model_state"],
            "extract_modes": ["normal", "fast"], "cancellation": "cooperative", "timeout_ms": true,
            "document_snapshot_concurrent": true, "transcription": cfg!(feature = "transcription"),
            "layout": cfg!(feature = "layout-detection"), "paddle_ocr": cfg!(feature = "paddle-ocr")
        })),
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
    run_worker_loop(stdin, &mut stdout, cancel, default_timeout, document, screenshot, query)
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
    fn control_protocol_validates_target_timeout_mode_and_preserves_ids() {
        assert!(validate_line(r#"{"id":1,"command":"cancel","target_id":null}"#).is_ok());
        assert!(validate_line(r#"{"id":2,"command":"extract","path":"a.pdf","mode":"fast","timeout_ms":1}"#).is_ok());
        for command in ["formats", "capabilities", "model_state"] {
            assert!(validate_line(&json!({"id":3,"command":command}).to_string()).is_ok());
        }
        for value in [
            json!({"id":4,"command":"extract"}),
            json!({"id":4,"command":"cancel"}),
            json!({"id":4,"command":"ocr_snapshot","image_base64":"x","mode":"fast"}),
            json!({"id":4,"command":"extract","path":"x","timeout_ms":0}),
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
}
