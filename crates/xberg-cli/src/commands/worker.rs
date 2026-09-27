//! `xberg worker` - batch-local stdio worker process for embedding callers (JchTools).
//!
//! A worker serves exactly one batch: the caller starts it with a fixed extraction
//! configuration, sends one JSON request per line on stdin, and reads exactly one JSON
//! response line per request from stdout. Closing stdin ends the batch; the worker
//! finishes the in-flight request and exits. The full contract (including who owns
//! timeouts and restarts) lives in `docs/requirements/WORKER.md`.
//!
//! Supported commands:
//! - `extract` (`path`, optional `mode`): run the startup config unchanged — the
//!   original and still primary operation, with an unchanged response shape.
//! - `ocr_snapshot` (`image_base64`, SNAP-14): recognize one in-memory screenshot
//!   through the snapshot OCR channel; engine lazily loaded on the first request and
//!   reused for the batch. Success is `{"id":..,"ok":true,"text":..,"records":N,
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
//! uses (OCR engine pool, Tesseract processor, Whisper engines, layout model caches,
//! the SenseVoice session cache, the snapshot OCR engine above) is a process-level
//! lazy cache, so simply keeping the process — and one tokio runtime — alive for the
//! whole batch means the second and later files skip model loading. `xberg serve`
//! relies on the same mechanism across HTTP requests.
//!
//! Protocol purity: stdout carries protocol messages only. Diagnostics go to stderr
//! through `tracing` (the subscriber is installed in `main()` with a stderr writer),
//! and the default panic hook also writes to stderr, so even a caught panic cannot
//! corrupt the stdout stream.
//!
//! Disconnect handling (SNAP-16): the serving loop is strictly serial, so a client
//! disconnect is observed at the first response write. A failed write sets the shared
//! cancel flag (the snapshot channel honors it at its tile/batch checkpoints) and
//! aborts the loop with an IO error — the process exits instead of idling; timeouts
//! and restarts stay the caller's job (WORKER.md semantic 6).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use xberg::{ExtractInput, ExtractedDocument, ExtractionConfig, ProcessingWarning};

use super::extract::{build_runtime, single_result_from_output};
use super::snapshot_ocr::{
    ErrorKind, KIND_INPUT_INVALID, KIND_NO_TEXT, Recognized, SnapshotOcrEngine, recognize_base64,
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

/// The only supported extraction mode today: run the startup config unchanged.
const MODE_NORMAL: &str = "normal";

/// One line-JSON request read from stdin.
#[derive(Debug, Clone, Deserialize)]
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
    /// Extraction mode; `None` means [`MODE_NORMAL`]. No other mode exists yet.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Base64 image bytes (PNG or equivalent lossless encoding) — required by
    /// `ocr_snapshot`; the bytes stay in memory (SNAP-14: no disk staging required).
    #[serde(default)]
    pub(crate) image_base64: Option<String>,
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
/// own fields in [`process_line`] so their responses can still echo the id.
fn parse_request(line: &str) -> std::result::Result<WorkerRequest, String> {
    let request: WorkerRequest =
        serde_json::from_str(line).map_err(|error| format!("invalid worker request JSON: {error}"))?;
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

/// Handle one stdin line: parse it, validate the protocol-level fields, and run the
/// handler under `catch_unwind` so a panicking request becomes a failure response
/// for this id instead of killing the worker.
///
/// `handler` is never called for lines that fail protocol validation (bad JSON,
/// unsupported command, unsupported mode, missing command-specific fields).
fn process_line<F>(line: &str, handler: &mut F) -> WorkerResponse
where
    F: FnMut(WorkerRequest) -> RequestOutcome,
{
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(error) => return WorkerResponse::failure(Value::Null, error),
    };
    let id = request.id.clone();
    if let Some(mode) = request.mode.as_deref()
        && mode != MODE_NORMAL
    {
        return WorkerResponse::failure(
            id,
            format!("unsupported mode '{mode}': only '{MODE_NORMAL}' is supported"),
        );
    }
    match request.command.as_str() {
        COMMAND_EXTRACT => {}
        COMMAND_OCR_SNAPSHOT => {
            if request.image_base64.is_none() {
                return WorkerResponse::failure(
                    id,
                    "ocr_snapshot requires image_base64 (base64 PNG or equivalent lossless bytes)".to_string(),
                );
            }
        }
        COMMAND_SNAPSHOT_STATE => {}
        COMMAND_TRANSCRIBE => {
            if request.path.is_none() {
                return WorkerResponse::failure(id, "transcribe requires path (local media file)".to_string());
            }
        }
        other => {
            return WorkerResponse::failure(
                id,
                format!(
                    "unsupported command '{other}': only '{COMMAND_EXTRACT}', '{COMMAND_OCR_SNAPSHOT}', \
                     '{COMMAND_SNAPSHOT_STATE}' and '{COMMAND_TRANSCRIBE}' are supported"
                ),
            );
        }
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request))) {
        Ok(outcome) => render_outcome(id, outcome),
        Err(panic) => WorkerResponse::failure(id, render_panic(panic)),
    }
}

/// The request-serving loop: strictly serial, one response line per request.
///
/// Reads a line, answers it completely, and only then reads the next line — the caller
/// may therefore pipeline writes only up to the response it is waiting for. Whitespace-
/// only lines are skipped (they carry no id to echo). Returns `Ok(())` on stdin EOF,
/// which is the normal end-of-batch signal.
///
/// Disconnect handling (SNAP-16): a stdout write/flush failure means the caller is
/// gone. The cancel flag is set — the snapshot channel observes it at its tile/batch
/// checkpoints — and the loop aborts with an IO error so the process exits instead of
/// idling; the caller treats the exit like a crash (WORKER.md semantic 6).
fn run_worker_loop<R, W, F>(reader: R, writer: &mut W, cancel: &AtomicBool, mut handler: F) -> Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(WorkerRequest) -> RequestOutcome,
{
    for line in reader.lines() {
        let line = line.context("failed to read a worker request line from stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if cancel.load(Ordering::Acquire) {
            anyhow::bail!("client disconnected (cancel flag set); not serving further requests");
        }
        let response = process_line(&line, &mut handler);
        let encoded = serde_json::to_string(&response).context("failed to serialize a worker response")?;
        if let Err(error) = writeln!(writer, "{encoded}") {
            cancel.store(true, Ordering::Release);
            return Err(error).context("failed to write a worker response line to stdout");
        }
        if let Err(error) = writer.flush() {
            cancel.store(true, Ordering::Release);
            return Err(error).context("failed to flush a worker response line to stdout");
        }
    }
    Ok(())
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
    let Some(path) = request.path else {
        // Unreachable through process_line (extract requires a path), kept as a
        // defensive non-panicking fallback for direct callers.
        return WorkerOutcome::Failure("extract requires path (local document file)".to_string());
    };
    let uri = path.to_string_lossy().into_owned();
    if let Err(error) = validate_file_exists(&path) {
        return WorkerOutcome::Failure(format!("{error:#}"));
    }
    let started = std::time::Instant::now();
    let input = ExtractInput::from_uri(uri.clone());
    match runtime.block_on(xberg::extract(input, config)) {
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

/// Handle one `ocr_snapshot` request (SNAP-14): decode the in-memory image and
/// recognize it through the lazily loaded, batch-reused snapshot engine.
fn ocr_snapshot_request(engine: &mut SnapshotOcrEngine, request: WorkerRequest, cancel: &AtomicBool) -> RequestOutcome {
    let Some(encoded) = request.image_base64 else {
        // Unreachable through process_line; defensive non-panicking fallback.
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
fn snapshot_state_request(engine: &SnapshotOcrEngine) -> RequestOutcome {
    let (state, error) = engine.state();
    RequestOutcome::State {
        state,
        error: error.map(str::to_string),
    }
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
    let bytes = std::fs::read(&path).map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    // SV-06 header name: the input file name without its extension (the JchTools
    // media worker convention).
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("media")
        .to_string();
    let mime_type = mime_for_media(&path);
    let result = xberg::transcription::sensevoice::transcribe_bytes(
        &bytes,
        &name,
        mime_type,
        tcfg.model_dir.as_deref(),
        tcfg.max_duration_ms,
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
/// the whole batch, then serves stdin until EOF. The snapshot OCR engine is created
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
    let handler = |request: WorkerRequest| -> RequestOutcome {
        match request.command.as_str() {
            COMMAND_EXTRACT => RequestOutcome::Extract(extract_request(&runtime, &config, request)),
            COMMAND_OCR_SNAPSHOT => ocr_snapshot_request(&mut snapshot_engine, request, &cancel),
            COMMAND_SNAPSHOT_STATE => snapshot_state_request(&snapshot_engine),
            COMMAND_TRANSCRIBE => transcribe_request(&config, request),
            // Unreachable: process_line rejects unknown commands before the handler.
            other => RequestOutcome::Extract(WorkerOutcome::Failure(format!("unsupported command '{other}'"))),
        }
    };
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    run_worker_loop(stdin, &mut stdout, &cancel, handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn request(id: Value, path: &str) -> WorkerRequest {
        WorkerRequest {
            id,
            command: COMMAND_EXTRACT.to_string(),
            path: Some(PathBuf::from(path)),
            mode: None,
            image_base64: None,
        }
    }

    fn transcribe_req(id: Value, path: &str) -> WorkerRequest {
        WorkerRequest {
            id,
            command: COMMAND_TRANSCRIBE.to_string(),
            path: Some(PathBuf::from(path)),
            mode: None,
            image_base64: None,
        }
    }

    fn extract_outcome_ok() -> RequestOutcome {
        RequestOutcome::Extract(WorkerOutcome::Success(ExtractedDocument::default()))
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
            r#"{"id":8,"command":"extract","path":"a.pdf","mode":"fast"}"#,
            &mut handler,
        );
        assert!(!mode.ok);
        assert_eq!(mode.id, json!(8));
        assert!(
            mode.error
                .as_deref()
                .unwrap_or_default()
                .contains("unsupported mode 'fast'")
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

    #[test]
    fn run_worker_loop_answers_every_request_in_order_and_stops_at_eof() {
        let input = Cursor::new(concat!(
            "{\"id\":1,\"command\":\"extract\",\"path\":\"a.txt\"}\n",
            "\n",
            "{\"id\":\"two\",\"command\":\"extract\",\"path\":\"missing.txt\"}\n",
        ));
        let mut output = Vec::new();
        let cancel = AtomicBool::new(false);
        let handler = |request: WorkerRequest| {
            if request.path.as_deref() == Some(Path::new("a.txt")) {
                extract_outcome_ok()
            } else {
                extract_outcome_failure("file not found")
            }
        };

        run_worker_loop(input, &mut output, &cancel, handler).expect("EOF is a clean exit");

        let text = String::from_utf8(output).expect("responses are UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one response per request; blank lines skipped: {text}");

        let first: Value = serde_json::from_str(lines[0]).expect("response is line JSON");
        assert_eq!(first["id"], json!(1));
        assert_eq!(first["ok"], json!(true));

        // A failed file does not end the batch: the next request is still answered.
        let second: Value = serde_json::from_str(lines[1]).expect("response is line JSON");
        assert_eq!(second["id"], json!("two"));
        assert_eq!(second["ok"], json!(false));
        assert_eq!(second["error"], json!("file not found"));
    }

    /// Mixed-command batch: a failing snapshot request and an unknown command are
    /// isolated between well-formed responses, and ids echo end to end.
    #[test]
    fn run_worker_loop_isolates_failures_across_commands() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        // A valid (blank) PNG so the request reaches model resolution and fails
        // there — an undecodable image would be `input_invalid` instead.
        let mut png = Vec::new();
        image::RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("tiny png encodes");
        let png_b64 = STANDARD.encode(&png);

        let input = Cursor::new(format!(
            concat!(
                "{{\"id\":1,\"command\":\"ocr_snapshot\",\"image_base64\":\"{png_b64}\"}}\n",
                "{{\"id\":2,\"command\":\"snapshot_state\"}}\n",
                "{{\"id\":3,\"command\":\"transcribe\",\"path\":\"E:/media/speech.mp4\"}}\n",
                "{{\"id\":4,\"command\":\"time_travel\"}}\n",
                "{{\"id\":5,\"command\":\"extract\",\"path\":\"a.txt\"}}\n",
            ),
            png_b64 = png_b64,
        ));
        let mut output = Vec::new();
        let cancel = AtomicBool::new(false);
        let mut engine = SnapshotOcrEngine::new(None);
        // Engine with no resolvable models: ocr_snapshot must fail without
        // affecting later requests.
        let handler = |request: WorkerRequest| match request.command.as_str() {
            COMMAND_OCR_SNAPSHOT => ocr_snapshot_request(&mut engine, request, &cancel),
            COMMAND_SNAPSHOT_STATE => snapshot_state_request(&engine),
            COMMAND_TRANSCRIBE => RequestOutcome::Transcribe(Err("no such media".to_string())),
            COMMAND_EXTRACT => extract_outcome_ok(),
            other => extract_outcome_failure(&format!("unsupported command '{other}'")),
        };

        run_worker_loop(input, &mut output, &cancel, handler).expect("EOF is a clean exit");

        let text = String::from_utf8(output).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "one response per request: {lines:?}");

        let ocr: Value = serde_json::from_str(lines[0]).expect("line json");
        assert_eq!(ocr["id"], json!(1));
        assert_eq!(ocr["ok"], json!(false));
        assert_eq!(
            ocr["error_kind"],
            json!("asset_invalid"),
            "unresolvable models are an asset failure"
        );

        let state: Value = serde_json::from_str(lines[1]).expect("line json");
        assert_eq!(state["id"], json!(2));
        assert_eq!(state["ok"], json!(true));
        assert_eq!(
            state["state"],
            json!("error"),
            "a failed load is reported by the state query"
        );
        assert!(state["error"].is_string());

        let transcribe: Value = serde_json::from_str(lines[2]).expect("line json");
        assert_eq!(transcribe["id"], json!(3));
        assert_eq!(transcribe["ok"], json!(false));
        assert_eq!(transcribe["error"], json!("no such media"));

        let unknown: Value = serde_json::from_str(lines[3]).expect("line json");
        assert_eq!(unknown["id"], json!(4));
        assert_eq!(unknown["ok"], json!(false));
        assert!(
            unknown["error"]
                .as_str()
                .unwrap_or_default()
                .contains("unsupported command 'time_travel'"),
            "unexpected: {unknown}"
        );

        // The failed requests did not end the batch.
        let extract: Value = serde_json::from_str(lines[4]).expect("line json");
        assert_eq!(extract["id"], json!(5));
        assert_eq!(extract["ok"], json!(true));
    }

    /// Disconnect handling (SNAP-16): a failed response write sets the cancel flag
    /// and aborts the loop instead of idling.
    #[test]
    fn run_worker_loop_sets_the_cancel_flag_when_a_write_fails() {
        struct BrokenWriter;
        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let input = Cursor::new("{\"id\":1,\"command\":\"extract\",\"path\":\"a.txt\"}\n");
        let cancel = AtomicBool::new(false);
        let handler = |_request: WorkerRequest| extract_outcome_ok();
        let result = run_worker_loop(input, &mut BrokenWriter, &cancel, handler);
        assert!(result.is_err(), "a broken pipe must abort the loop");
        assert!(
            cancel.load(Ordering::Acquire),
            "a failed response write must set the cancel flag"
        );
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

    /// `transcribe` with a Whisper (non-sensevoice) startup config is rejected —
    /// the Whisper pipeline stays untouched and never serves this command.
    #[test]
    #[cfg(feature = "transcription")]
    fn transcribe_rejects_non_sensevoice_backends() {
        use xberg::core::config::transcription::{TranscriptionBackend, TranscriptionConfig};
        let config = ExtractionConfig {
            transcription: Some(TranscriptionConfig {
                backend: TranscriptionBackend::Whisper,
                ..Default::default()
            }),
            ..Default::default()
        };
        let outcome = transcribe_request(&config, transcribe_req(json!("t"), "E:/no/speech.mp4"));
        let RequestOutcome::Transcribe(Err(error)) = outcome else {
            panic!("expected a transcribe failure");
        };
        assert!(error.contains("sensevoice"), "unexpected: {error}");
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
