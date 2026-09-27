//! `xberg worker` - batch-local stdio worker process for embedding callers (JchTools).
//!
//! A worker serves exactly one batch: the caller starts it with a fixed extraction
//! configuration, sends one JSON request per line on stdin, and reads exactly one JSON
//! response line per request from stdout. Closing stdin ends the batch; the worker
//! finishes the in-flight request and exits. The full contract (including who owns
//! timeouts and restarts) lives in `docs/requirements/WORKER.md`.
//!
//! Model-session reuse is the point of this command: every ML backend the library
//! uses (OCR engine pool, Tesseract processor, Whisper engines, layout model caches)
//! is a process-level lazy cache, so simply keeping the process — and one tokio
//! runtime — alive for the whole batch means the second and later files skip model
//! loading. `xberg serve` relies on the same mechanism across HTTP requests.
//!
//! Protocol purity: stdout carries protocol messages only. Diagnostics go to stderr
//! through `tracing` (the subscriber is installed in `main()` with a stderr writer),
//! and the default panic hook also writes to stderr, so even a caught panic cannot
//! corrupt the stdout stream.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use xberg::{ExtractInput, ExtractedDocument, ExtractionConfig, ProcessingWarning};

use super::extract::{build_runtime, single_result_from_output};
use super::validate_file_exists;

/// The only supported request command today.
const COMMAND_EXTRACT: &str = "extract";

/// The only supported extraction mode today: run the startup config unchanged.
const MODE_NORMAL: &str = "normal";

/// One line-JSON request read from stdin.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkerRequest {
    /// Caller-supplied correlation token (any JSON value), echoed verbatim in the response.
    pub(crate) id: Value,
    /// Requested operation. Only [`COMMAND_EXTRACT`] is supported.
    pub(crate) command: String,
    /// Local filesystem path of the document to extract.
    pub(crate) path: PathBuf,
    /// Extraction mode; `None` means [`MODE_NORMAL`]. No other mode exists yet.
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

/// What the extraction handler did with a request, before protocol wrapping.
#[derive(Debug)]
pub(crate) enum WorkerOutcome {
    /// A document was produced (possibly a degraded/partial one — check its warnings).
    Success(ExtractedDocument),
    /// No document; the string is the single-line failure description.
    Failure(String),
}

/// One line-JSON response written to stdout.
///
/// `ok: true` means a document is present (partial extractions included — degraded
/// stages surface in `warnings`); `ok: false` means no document, with `error` saying why.
#[derive(Debug, Serialize)]
pub(crate) struct WorkerResponse {
    pub(crate) id: Value,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) document: Option<ExtractedDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Copy of `document.processing_warnings` on success, empty on failure, so callers
    /// that ignore the document still see degraded-stage information.
    pub(crate) warnings: Vec<ProcessingWarning>,
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
        }
    }

    fn failure(id: Value, error: String) -> Self {
        Self {
            id,
            ok: false,
            document: None,
            error: Some(error),
            warnings: Vec::new(),
        }
    }
}

/// Parse one stdin line into a [`WorkerRequest`].
///
/// Unknown JSON fields are ignored (forward compatibility); missing required fields
/// (`id`, `command`, `path`) and invalid JSON are reported as a plain message suitable
/// for a failure response with a null id.
fn parse_request(line: &str) -> std::result::Result<WorkerRequest, String> {
    serde_json::from_str(line).map_err(|error| format!("invalid worker request JSON: {error}"))
}

/// Render a caught panic payload into the failure-response string.
fn render_panic(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        format!("extraction panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("extraction panicked: {message}")
    } else {
        "extraction panicked (non-string panic payload)".to_string()
    }
}

/// Handle one stdin line: parse it, validate the protocol-level fields, and run the
/// handler under `catch_unwind` so a panicking extraction becomes a failure response
/// for this id instead of killing the worker.
///
/// `handler` is never called for lines that fail protocol validation (bad JSON,
/// unsupported command, unsupported mode).
fn process_line<F>(line: &str, handler: &mut F) -> WorkerResponse
where
    F: FnMut(WorkerRequest) -> WorkerOutcome,
{
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(error) => return WorkerResponse::failure(Value::Null, error),
    };
    let id = request.id.clone();
    if request.command != COMMAND_EXTRACT {
        return WorkerResponse::failure(
            id,
            format!(
                "unsupported command '{}': only '{COMMAND_EXTRACT}' is supported",
                request.command
            ),
        );
    }
    if let Some(mode) = request.mode.as_deref()
        && mode != MODE_NORMAL
    {
        return WorkerResponse::failure(
            id,
            format!("unsupported mode '{mode}': only '{MODE_NORMAL}' is supported"),
        );
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request))) {
        Ok(WorkerOutcome::Success(document)) => WorkerResponse::success(id, document),
        Ok(WorkerOutcome::Failure(error)) => WorkerResponse::failure(id, error),
        Err(panic) => WorkerResponse::failure(id, render_panic(panic)),
    }
}

/// The request-serving loop: strictly serial, one response line per request.
///
/// Reads a line, answers it completely, and only then reads the next line — the caller
/// may therefore pipeline writes only up to the response it is waiting for. Whitespace-
/// only lines are skipped (they carry no id to echo). Returns `Ok(())` on stdin EOF,
/// which is the normal end-of-batch signal; any stdin/stdout IO error aborts the worker
/// with an error, which the caller treats like a crash.
fn run_worker_loop<R, W, F>(reader: R, writer: &mut W, mut handler: F) -> Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(WorkerRequest) -> WorkerOutcome,
{
    for line in reader.lines() {
        let line = line.context("failed to read a worker request line from stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line(&line, &mut handler);
        let encoded = serde_json::to_string(&response).context("failed to serialize a worker response")?;
        writeln!(writer, "{encoded}").context("failed to write a worker response line to stdout")?;
        writer
            .flush()
            .context("failed to flush a worker response line to stdout")?;
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
    let uri = request.path.to_string_lossy().into_owned();
    if let Err(error) = validate_file_exists(&request.path) {
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

/// Execute the `xberg worker` command.
///
/// Builds the one tokio runtime and resolves the one config this worker will use for
/// the whole batch, then serves stdin until EOF. Diagnostics (including the per-request
/// info log with elapsed time) go to stderr via `tracing`.
// (fork) perf-tracing：批次级性能 span；逐文件耗时另有 `worker request completed` 日志。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "worker_command", skip_all)
)]
pub fn worker_command(config: ExtractionConfig) -> Result<()> {
    let runtime = build_runtime(&config).context("failed to build the worker's tokio runtime")?;
    let handler = |request: WorkerRequest| extract_request(&runtime, &config, request);
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    run_worker_loop(stdin, &mut stdout, handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn request(id: Value, path: &str) -> WorkerRequest {
        WorkerRequest {
            id,
            command: COMMAND_EXTRACT.to_string(),
            path: PathBuf::from(path),
            mode: None,
        }
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
        assert_eq!(parsed.path, PathBuf::from("C:\\docs\\a.pdf"));
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

    #[test]
    fn process_line_reports_invalid_json_with_a_null_id() {
        let mut handler = |_request: WorkerRequest| WorkerOutcome::Failure("unreachable".to_string());
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
            WorkerOutcome::Failure("handler must not run".to_string())
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

    #[test]
    fn process_line_converts_handler_panics_into_failure_responses() {
        let mut handler = |_request: WorkerRequest| -> WorkerOutcome {
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
        let mut handler = move |_request: WorkerRequest| WorkerOutcome::Success(document.clone());
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

    #[test]
    fn run_worker_loop_answers_every_request_in_order_and_stops_at_eof() {
        let input = Cursor::new(concat!(
            "{\"id\":1,\"command\":\"extract\",\"path\":\"a.txt\"}\n",
            "\n",
            "{\"id\":\"two\",\"command\":\"extract\",\"path\":\"missing.txt\"}\n",
        ));
        let mut output = Vec::new();
        let handler = |request: WorkerRequest| {
            if request.path == PathBuf::from("a.txt") {
                WorkerOutcome::Success(ExtractedDocument::default())
            } else {
                WorkerOutcome::Failure("file not found".to_string())
            }
        };

        run_worker_loop(input, &mut output, handler).expect("EOF is a clean exit");

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
}
