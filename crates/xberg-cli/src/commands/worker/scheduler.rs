//! Independent document/screenshot queues in one process, with one stdout writer.

use super::control::RequestControl;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{
    COMMAND_OCR_SNAPSHOT, COMMAND_SHUTDOWN, COMMAND_SNAPSHOT_STATE, RequestOutcome, WorkerRequest, WorkerResponse,
    process_request, render_outcome, validate_line,
};
use crate::commands::extract::RUNTIME_WORKER_STACK_SIZE_BYTES;

// Bounded queues avoid retaining an unlimited number of base64 screenshots while
// still letting the other channel and status queries progress under backpressure.
const QUEUE_CAPACITY: usize = 8;

/// Hard cap from a stop trigger (stdin EOF, `shutdown`, disconnect) to process
/// exit. The P1 contract allows 5 s; 4 s leaves margin for teardown (main
/// unwinding, exit-code plumbing) so the observed exit stays inside 5 s.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(4);

/// Why the dispatcher stopped serving; drives the process exit code (P1/P3/P4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopExit {
    /// stdin closed or `shutdown` accepted; in-flight work drained within the
    /// grace cap. Exit code 0.
    SessionEnded,
    /// The stdio peer vanished: stdout write failure or probe, dead parent, or a
    /// broken stdin read. In-flight work is cancelled, not drained. Fixed
    /// non-zero exit code (86).
    PeerDisconnected,
    /// `idle_timeout_ms` elapsed with no request traffic and no in-flight work.
    /// Distinct non-zero exit code (87).
    IdleTimeout,
}

/// Exit code for each stop reason; 0 keeps `main`'s normal return.
pub(crate) fn exit_code_for(exit: LoopExit) -> i32 {
    match exit {
        LoopExit::SessionEnded => 0,
        LoopExit::PeerDisconnected => super::EXIT_PEER_DISCONNECTED,
        LoopExit::IdleTimeout => super::EXIT_IDLE_TIMEOUT,
    }
}

/// Dispatcher-level startup knobs; request handlers stay outside. Keeps
/// `run_worker_loop` at seven arguments (clippy's too-many-arguments budget).
pub(super) struct LoopOptions {
    /// Per-request timeout when the request itself carries no `timeout_ms`
    /// (the startup config's `extraction_timeout_secs`).
    pub(super) default_timeout: Option<Duration>,
    /// P4: exit after this long with no request traffic and no in-flight work.
    pub(super) idle_timeout: Option<Duration>,
    /// P1: set asynchronously by the disconnect monitor (stdout probe /
    /// parent-death watch).
    pub(super) peer_gone: Option<Arc<AtomicBool>>,
    /// P1/P3: hard cap from stop trigger to exit. Overridable so tests exercise
    /// the deadline in milliseconds instead of the shipped 4 s.
    pub(super) drain_grace: Duration,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            default_timeout: None,
            idle_timeout: None,
            peer_gone: None,
            drain_grace: EXIT_GRACE,
        }
    }
}

/// stdout wrapper for the dispatcher: the first write failure marks the peer
/// gone (polled at the top of the loop) instead of unwinding as an error, so a
/// broken pipe maps to the fixed disconnect exit code (P1), never exit 1.
struct PeerWriter<'a, W: Write> {
    inner: &'a mut W,
    gone: bool,
}

impl<'a, W: Write> PeerWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, gone: false }
    }

    /// Best-effort send; false means the peer is gone (this or an earlier call).
    fn send(&mut self, response: &WorkerResponse) -> bool {
        if self.gone {
            return false;
        }
        if write_response(self.inner, response).is_err() {
            tracing::warn!("worker stdout write failed; treating the peer as disconnected");
            self.gone = true;
        }
        !self.gone
    }
}

enum Event {
    Line(String),
    InputClosed(std::io::Result<()>),
    Response(Box<WorkerResponse>, bool),
    Finished,
}

struct CancelOnExit(Arc<AtomicBool>);

impl Drop for CancelOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn start_lane<F>(
    name: &'static str,
    screenshot: bool,
    requests: Receiver<WorkerRequest>,
    events: SyncSender<Event>,
    cancel: Arc<AtomicBool>,
    mut handler: F,
) -> Result<JoinHandle<()>>
where
    F: FnMut(WorkerRequest) -> RequestOutcome + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .stack_size(RUNTIME_WORKER_STACK_SIZE_BYTES)
        .spawn(move || {
            for request in requests {
                if screenshot && cancel.load(Ordering::Acquire) {
                    break;
                }
                // Observable overlap without logging document or image contents.
                tracing::info!(pid = std::process::id(), channel = name, id = %request.id, "worker request started");
                let control = request.control.clone();
                control.expire();
                let mut response = if control.token.is_cancelled() {
                    WorkerResponse::failure(request.id, "request stopped before execution".into())
                } else {
                    process_request(request, &mut handler)
                };
                if let Some(reason) = control.finish() {
                    response = WorkerResponse::failure(response.id, format!("request {reason}"));
                    response.error_kind = Some(reason);
                }
                tracing::info!(pid = std::process::id(), channel = name, id = %response.id, "worker request finished");
                if events.send(Event::Response(Box::new(response), screenshot)).is_err() {
                    return;
                }
            }
            let _ = events.send(Event::Finished);
        })
        .with_context(|| format!("failed to start {name} thread"))
}

fn write_response<W: Write>(writer: &mut W, response: &WorkerResponse) -> Result<()> {
    let encoded = serde_json::to_string(response).context("failed to serialize a worker response")?;
    writeln!(writer, "{encoded}").context("failed to write a worker response line to stdout")?;
    writer
        .flush()
        .context("failed to flush a worker response line to stdout")
}

fn enqueue<W: Write>(
    queue: &SyncSender<WorkerRequest>,
    request: WorkerRequest,
    writer: &mut PeerWriter<W>,
) -> Result<bool> {
    match queue.try_send(request) {
        Ok(()) => Ok(true),
        Err(TrySendError::Full(request)) => {
            let mut response = WorkerResponse::failure(request.id, "worker request queue is full; retry later".into());
            if request.command == COMMAND_OCR_SNAPSHOT {
                response.error_kind = Some(super::super::snapshot_ocr::KIND_INTERNAL);
            }
            writer.send(&response);
            Ok(false)
        }
        Err(TrySendError::Disconnected(_)) => anyhow::bail!("worker request thread stopped unexpectedly"),
    }
}

/// The input thread owns its reader; on a broken output pipe it may still be
/// blocked in an OS read. Do not join it on that error path: returning from the
/// CLI terminates the process, including that reader and any blocked native work.
pub(super) fn run_worker_loop<R, W, D, S, Q>(
    reader: R,
    writer: &mut W,
    cancel: Arc<AtomicBool>,
    options: LoopOptions,
    document: D,
    screenshot: S,
    mut query: Q,
) -> Result<LoopExit>
where
    R: BufRead + Send + 'static,
    W: Write,
    D: FnMut(WorkerRequest) -> RequestOutcome + Send + 'static,
    S: FnMut(WorkerRequest) -> RequestOutcome + Send + 'static,
    Q: FnMut(&str) -> RequestOutcome,
{
    let _cancel_on_exit = CancelOnExit(Arc::clone(&cancel));
    let (events, incoming) = mpsc::sync_channel(QUEUE_CAPACITY);
    let (documents, document_rx) = mpsc::sync_channel(QUEUE_CAPACITY);
    let (screenshots, screenshot_rx) = mpsc::sync_channel(QUEUE_CAPACITY);
    let document_thread = start_lane(
        "document",
        false,
        document_rx,
        events.clone(),
        Arc::clone(&cancel),
        document,
    )?;
    let screenshot_thread = start_lane(
        "snapshot",
        true,
        screenshot_rx,
        events.clone(),
        Arc::clone(&cancel),
        screenshot,
    )?;
    let input_cancel = Arc::clone(&cancel);
    let input_thread = thread::Builder::new()
        .name("worker-input".into())
        .spawn(move || {
            let result = (|| {
                for line in reader.lines() {
                    let line = line?;
                    if !line.trim().is_empty() && events.send(Event::Line(line)).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            })();
            // Set this in the reader, not behind an inference request in a queue.
            input_cancel.store(true, Ordering::Release);
            let _ = events.send(Event::InputClosed(result));
        })
        .context("failed to start worker input thread")?;

    let mut queues = Some((documents, screenshots));
    struct Active(Vec<(serde_json::Value, RequestControl, bool)>);
    impl Drop for Active {
        fn drop(&mut self) {
            for (_, control, _) in &self.0 {
                control.stop("cancelled");
            }
        }
    }
    let mut active = Active(Vec::new());
    let mut finished = 0;
    let mut writer = PeerWriter::new(writer);
    let mut stdin_broken = false;
    // Stop-trigger state (P1/P3): hard deadline plus the exit reason it serves.
    let mut draining: Option<(Instant, LoopExit)> = None;
    // `shutdown` was accepted: trailing requests get an explicit refusal.
    let mut shutting_down = false;
    // Whether stdin reached EOF (the input thread has nothing left to read).
    let mut stdin_eof = false;
    let mut last_activity = Instant::now();

    let exit = 'dispatch: loop {
        for (_, control, screenshot) in &active.0 {
            control.expire();
            if *screenshot && cancel.load(Ordering::Acquire) {
                control.stop("cancelled");
            }
        }
        // P1: the monitor flag, a failed stdout write, or a broken stdin read all
        // mean the host is gone — cancel everything and stop serving now.
        if stdin_broken
            || writer.gone
            || options
                .peer_gone
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            cancel.store(true, Ordering::Release);
            for (_, control, _) in &active.0 {
                control.stop("cancelled");
            }
            break 'dispatch LoopExit::PeerDisconnected;
        }
        if finished == 2 && queues.is_none() {
            break 'dispatch LoopExit::SessionEnded;
        }
        if let Some((deadline, reason)) = draining {
            // Drained, or the hard cap hit: cancel whatever is left and stop. A
            // stuck native call must not hold the process past the cap.
            if active.0.is_empty() || Instant::now() >= deadline {
                cancel.store(true, Ordering::Release);
                for (_, control, _) in &active.0 {
                    control.stop("cancelled");
                }
                break 'dispatch reason;
            }
        } else if let Some(idle) = options.idle_timeout
            && active.0.is_empty()
            && last_activity.elapsed() >= idle
        {
            cancel.store(true, Ordering::Release);
            break 'dispatch LoopExit::IdleTimeout;
        }
        let next = active.0.iter().filter_map(|(_, c, _)| c.deadline()).min();
        // Poll disconnection flags too, without spawning a timer per request.
        let wait = next
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50))
            .min(draining.map_or(Duration::from_millis(50), |(d, _)| {
                d.saturating_duration_since(Instant::now())
            }));
        let event = match incoming.recv_timeout(wait) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("worker event channel closed unexpectedly"),
        };
        match event {
            Event::Line(line) => {
                last_activity = Instant::now();
                if shutting_down {
                    let id = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|raw| raw.get("id").cloned())
                        .unwrap_or(serde_json::Value::Null);
                    writer.send(&WorkerResponse::failure(id, "worker is shutting down".into()));
                    continue;
                }
                match validate_line(&line) {
                    Err(response) => {
                        writer.send(&response);
                    }
                    Ok(request) if active.0.iter().any(|(id, _, _)| *id == request.id) => {
                        let mut response = WorkerResponse::failure(request.id, "duplicate active request id".into());
                        response.error_kind = Some("duplicate_id");
                        writer.send(&response);
                    }
                    Ok(request) if request.command == super::COMMAND_CANCEL => {
                        let target = request.target_id.unwrap_or(serde_json::Value::Null);
                        let accepted = active
                            .0
                            .iter()
                            .find(|(id, _, _)| *id == target)
                            .is_some_and(|(_, c, _)| c.stop("cancelled"));
                        let response = render_outcome(
                            request.id,
                            RequestOutcome::Query(serde_json::json!({"target_id": target, "accepted": accepted})),
                        );
                        writer.send(&response);
                    }
                    Ok(request) if request.command == COMMAND_SHUTDOWN => {
                        // P3: acknowledge first (the line is flushed), then stop
                        // taking work and drain up to the grace cap; exit 0.
                        let grace = request.grace_ms.map(Duration::from_millis);
                        writer.send(&render_outcome(
                            request.id,
                            RequestOutcome::Query(serde_json::json!({"accepted": true})),
                        ));
                        shutting_down = true;
                        queues.take();
                        let deadline = Instant::now() + grace.unwrap_or(options.drain_grace);
                        draining = Some(match draining {
                            Some((earlier, reason)) if earlier < deadline => (earlier, reason),
                            _ => (deadline, LoopExit::SessionEnded),
                        });
                    }
                    Ok(request)
                        if matches!(
                            request.command.as_str(),
                            COMMAND_SNAPSHOT_STATE
                                | super::COMMAND_FORMATS
                                | super::COMMAND_CAPABILITIES
                                | super::COMMAND_MODEL_STATE
                                | super::COMMAND_KEEPALIVE
                                | super::COMMAND_VERSION
                        ) =>
                    {
                        let outcome = query(&request.command);
                        writer.send(&render_outcome(request.id, outcome));
                    }
                    Ok(mut request) => {
                        let (documents, screenshots) = match queues.as_ref() {
                            Some(pair) => pair,
                            None => {
                                writer.send(&WorkerResponse::failure(
                                    request.id,
                                    "worker session is ending; request refused".into(),
                                ));
                                continue;
                            }
                        };
                        let screenshot = request.command == COMMAND_OCR_SNAPSHOT;
                        if screenshot && cancel.load(Ordering::Acquire) {
                            continue;
                        }
                        // The first cold request owns lazy initialization. While it is queued
                        // or loading, answer later screenshots promptly so the caller can show
                        // loading state and retry; never retain them for surprise late inference.
                        if screenshot
                            && active.0.iter().any(|(_, _, screenshot)| *screenshot)
                            && matches!(
                                query(COMMAND_SNAPSHOT_STATE),
                                RequestOutcome::State {
                                    state: "uninitialized" | "loading",
                                    ..
                                }
                            )
                        {
                            let response = render_outcome(
                                request.id,
                                RequestOutcome::Snapshot(Err((
                                    "snapshot models are loading; query snapshot_state and retry when ready".into(),
                                    super::super::snapshot_ocr::KIND_MODEL_NOT_READY,
                                ))),
                            );
                            writer.send(&response);
                            continue;
                        }
                        let timeout = request
                            .timeout_ms
                            .map(Duration::from_millis)
                            .or(options.default_timeout);
                        request.control = match RequestControl::with_timeout(timeout) {
                            Ok(control) => control,
                            Err(error) => {
                                writer.send(&WorkerResponse::failure(request.id, error.into()));
                                continue;
                            }
                        };
                        let entry = (request.id.clone(), request.control.clone(), screenshot);
                        if enqueue(if screenshot { screenshots } else { documents }, request, &mut writer)? {
                            active.0.push(entry);
                        }
                    }
                }
            }
            Event::InputClosed(result) => {
                queues.take();
                stdin_eof = true;
                if result.is_err() {
                    // Reading stdin failed — the host vanished mid-read (P1).
                    stdin_broken = true;
                    continue;
                }
                // Clean EOF (WORKER.md): drain documents, screenshots are already
                // cancelled, but never past the hard cap.
                let deadline = Instant::now() + options.drain_grace;
                draining = Some(match draining {
                    Some((earlier, reason)) if earlier < deadline => (earlier, reason),
                    _ => (deadline, LoopExit::SessionEnded),
                });
            }
            Event::Response(response, screenshot) => {
                active.0.retain(|(id, _, _)| *id != response.id);
                // A completed request is traffic: restart the idle clock when
                // work drains. Otherwise a request that simply outlasted
                // `idle_timeout_ms` (legal — the timeout only bounds *idle*
                // time) timed the worker out the instant it finished, killing
                // the resident process the caller was about to reuse.
                last_activity = Instant::now();
                if !screenshot || !cancel.load(Ordering::Acquire) {
                    writer.send(&response);
                }
            }
            Event::Finished => finished += 1,
        }
    };

    // Join only what has provably finished: a forced stop may leave lane threads
    // inside uninterruptible native calls, and after `shutdown` the input thread
    // may still be blocked in a read — dropping the handle detaches the thread
    // and the process exit reclaims it.
    if finished == 2 {
        for handle in [document_thread, screenshot_thread] {
            handle.join().map_err(|_| anyhow::anyhow!("worker thread panicked"))?;
        }
    } else {
        drop((document_thread, screenshot_thread));
    }
    if stdin_eof {
        input_thread
            .join()
            .map_err(|_| anyhow::anyhow!("worker thread panicked"))?;
    } else {
        drop(input_thread);
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worker::{SnapshotSuccess, WorkerOutcome};
    use serde_json::{Value, json};
    use std::io::{BufReader, Cursor, Read};
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(10);

    #[test]
    fn cold_snapshot_rejects_followups_until_ready_without_blocking_document_or_state() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let (started, running) = channel();
        let (release, wait) = channel();
        let status = Arc::new(std::sync::Mutex::new("uninitialized"));
        let loader_status = Arc::clone(&status);
        let query_status = Arc::clone(&status);
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                document_ok,
                move |request: WorkerRequest| {
                    assert!(
                        matches!(request.id.as_u64(), Some(1 | 6)),
                        "rejected images must never reach inference"
                    );
                    if request.id == json!(1) {
                        started.send(()).unwrap();
                        wait.recv_timeout(DEADLINE).unwrap();
                        *loader_status.lock().unwrap() = "ready";
                    }
                    snapshot_ok(request)
                },
                move |_| RequestOutcome::State {
                    state: *query_status.lock().unwrap(),
                    error: None,
                },
            )
        });
        send(&stdin, json!({"id":1,"command":"ocr_snapshot","image_base64":"cold"}));
        running.recv_timeout(DEADLINE).expect("first request owns lazy loading");
        for (id, stage) in [(2, "uninitialized"), (3, "loading")] {
            *status.lock().unwrap() = stage;
            send(
                &stdin,
                json!({"id":id,"command":"ocr_snapshot","image_base64":"must not be retained"}),
            );
            let response = output.recv_timeout(DEADLINE).expect("prompt not-ready response");
            assert_eq!(response["id"], id);
            assert_eq!(response["ok"], false);
            assert_eq!(response["error_kind"], "model_not_ready");
            assert!(response.get("text").is_none());
            assert!(!response["error"].as_str().unwrap().contains("must not be retained"));
        }
        send(&stdin, json!({"id":4,"command":"snapshot_state"}));
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["state"], "loading");
        send(&stdin, json!({"id":5,"command":"extract","path":"document.txt"}));
        let response = output
            .recv_timeout(DEADLINE)
            .expect("document independent of cold snapshot");
        assert_eq!(response["id"], 5);
        assert_eq!(response["ok"], true);
        release.send(()).unwrap();
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["id"], 1);
        send(&stdin, json!({"id":6,"command":"ocr_snapshot","image_base64":"retry"}));
        let response = output.recv_timeout(DEADLINE).unwrap();
        assert_eq!(response["id"], 6);
        assert_eq!(response["text"], "截图文字");
        drop(stdin);
        thread.join().unwrap().unwrap();
        assert!(output.try_recv().is_err(), "one final response per request");
    }

    #[test]
    fn cancel_running_and_queued_requests_preserves_other_lane_and_next_batch() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let (started, running) = channel();
        let (release, wait) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                move |request: WorkerRequest| {
                    if request.id == json!(1) {
                        started.send(()).unwrap();
                        wait.recv_timeout(DEADLINE).unwrap();
                        assert!(request.control.token.is_cancelled());
                    }
                    assert_ne!(request.id, json!(4), "cancelled queued request must never execute");
                    document_ok(request)
                },
                snapshot_ok,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"long.pdf"}));
        running.recv_timeout(DEADLINE).unwrap();
        send(&stdin, json!({"id":2,"command":"cancel","target_id":1}));
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["accepted"], true);
        send(
            &stdin,
            json!({"id":3,"command":"ocr_snapshot","image_base64":"fixture"}),
        );
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["text"], "截图文字");
        send(&stdin, json!({"id":4,"command":"extract","path":"queued.pdf"}));
        send(&stdin, json!({"id":5,"command":"cancel","target_id":4}));
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["accepted"], true);
        assert!(
            output.try_recv().is_err(),
            "no terminal response while handler still running"
        );
        release.send(()).unwrap();
        for id in [1, 4] {
            let result = output.recv_timeout(DEADLINE).unwrap();
            assert_eq!(result["id"], id);
            assert_eq!(result["error_kind"], "cancelled");
            assert_eq!(result["ok"], false);
        }
        send(&stdin, json!({"id":6,"command":"extract","path":"next.txt"}));
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["ok"], true);
        send(&stdin, json!({"id":7,"command":"cancel","target_id":1}));
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["accepted"], false);
        drop(stdin);
        thread.join().unwrap().unwrap();
    }

    #[test]
    fn timeout_stops_running_snapshot_and_skips_expired_queued_work() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let (started, running) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                document_ok,
                move |request: WorkerRequest| {
                    assert_ne!(request.id, json!(2), "expired queued request must never execute");
                    if request.id == json!(1) {
                        started.send(()).unwrap();
                        let until = Instant::now() + DEADLINE;
                        while !request.control.token.is_cancelled() {
                            assert!(Instant::now() < until);
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    snapshot_ok(request)
                },
                state,
            )
        });
        send(
            &stdin,
            json!({"id":1,"command":"ocr_snapshot","image_base64":"x","timeout_ms":200}),
        );
        running.recv_timeout(DEADLINE).unwrap();
        send(
            &stdin,
            json!({"id":2,"command":"ocr_snapshot","image_base64":"x","timeout_ms":1}),
        );
        send(&stdin, json!({"id":3,"command":"extract","path":"a.txt"}));
        let mut results = std::collections::HashMap::new();
        for _ in 0..3 {
            let response = output.recv_timeout(DEADLINE).unwrap();
            results.insert(response["id"].as_u64().unwrap(), response);
        }
        for id in [1, 2] {
            assert_eq!(results[&id]["error_kind"], "timeout");
        }
        assert_eq!(results[&3]["ok"], true);
        send(
            &stdin,
            json!({"id":4,"command":"ocr_snapshot","image_base64":"fixture"}),
        );
        assert_eq!(output.recv_timeout(DEADLINE).unwrap()["text"], "截图文字");
        drop(stdin);
        thread.join().unwrap().unwrap();
    }

    // An open, controllable stdin. Keeping it open until responses arrive also
    // exercises the distinction between an idle process and a disconnected client.
    struct Input {
        chunks: Receiver<Vec<u8>>,
        current: Cursor<Vec<u8>>,
    }

    impl Read for Input {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let count = self.current.read(buffer)?;
                if count > 0 {
                    return Ok(count);
                }
                match self.chunks.recv_timeout(DEADLINE) {
                    Ok(bytes) => self.current = Cursor::new(bytes),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                    Err(mpsc::RecvTimeoutError::Timeout) => return Err(std::io::ErrorKind::TimedOut.into()),
                }
            }
        }
    }

    struct Output {
        responses: Sender<Value>,
        pending: Vec<u8>,
    }

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let value = serde_json::from_slice(&self.pending).expect("one complete JSON response per flush");
            self.pending.clear();
            self.responses
                .send(value)
                .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
        }
    }

    fn input() -> (Sender<Vec<u8>>, BufReader<Input>) {
        let (tx, chunks) = channel();
        (
            tx,
            BufReader::new(Input {
                chunks,
                current: Cursor::new(Vec::new()),
            }),
        )
    }

    fn send(input: &Sender<Vec<u8>>, value: Value) {
        input.send(format!("{value}\n").into_bytes()).expect("stdin open");
    }

    fn document_ok(_: WorkerRequest) -> RequestOutcome {
        RequestOutcome::Extract(WorkerOutcome::Success(xberg::ExtractedDocument::default()))
    }

    fn snapshot_ok(_: WorkerRequest) -> RequestOutcome {
        RequestOutcome::Snapshot(Ok(SnapshotSuccess {
            text: "截图文字".into(),
            records: 1,
            elapsed_ms: 1,
            error_kind: None,
        }))
    }

    fn state(_: &str) -> RequestOutcome {
        RequestOutcome::State {
            state: "ready",
            error: None,
        }
    }

    #[test]
    fn screenshot_and_state_finish_while_document_is_running_and_process_reuses_handlers() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let (started, running) = channel();
        let (release, wait) = channel();
        let thread = thread::spawn(move || {
            let mut calls = 0;
            let document = move |request| {
                calls += 1;
                if calls == 1 {
                    started.send(()).expect("test listening");
                    wait.recv_timeout(DEADLINE)
                        .expect("screenshot must complete before document is released");
                }
                document_ok(request)
            };
            let mut screenshots = 0;
            let screenshot = move |request| {
                screenshots += 1;
                let mut outcome = snapshot_ok(request);
                if let RequestOutcome::Snapshot(Ok(result)) = &mut outcome {
                    result.records = screenshots;
                }
                outcome
            };
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                document,
                screenshot,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"first.txt"}));
        running.recv_timeout(DEADLINE).expect("document started");
        send(
            &stdin,
            json!({"id":2,"command":"ocr_snapshot","image_base64":"fixture"}),
        );
        let result = output
            .recv_timeout(DEADLINE)
            .expect("screenshot cannot wait for the document");
        assert_eq!(result["id"], 2);
        assert_eq!(result["text"], "截图文字");
        send(&stdin, json!({"id":3,"command":"snapshot_state"}));
        assert_eq!(
            output.recv_timeout(DEADLINE).expect("state independent")["state"],
            "ready"
        );
        release.send(()).expect("release document");
        assert_eq!(output.recv_timeout(DEADLINE).expect("document response")["id"], 1);
        // Another batch does not reconstruct the screenshot handler.
        send(
            &stdin,
            json!({"id":4,"command":"ocr_snapshot","image_base64":"fixture"}),
        );
        assert_eq!(output.recv_timeout(DEADLINE).expect("second screenshot")["records"], 2);
        send(&stdin, json!({"id":5,"command":"extract","path":"second.txt"}));
        assert_eq!(output.recv_timeout(DEADLINE).expect("second batch")["id"], 5);
        drop(stdin);
        thread.join().expect("dispatcher joined").expect("EOF clean");
    }

    #[test]
    fn eof_cancels_inflight_screenshot_without_writing_its_result() {
        let (stdin, reader) = input();
        let cancel = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancel);
        let (started, running) = channel();
        let thread = thread::spawn(move || {
            let mut output = Vec::new();
            let screenshot = move |request| {
                started.send(()).expect("test listening");
                let deadline = std::time::Instant::now() + DEADLINE;
                while !observed.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    observed.load(Ordering::Acquire),
                    "EOF must be observed during inference"
                );
                snapshot_ok(request)
            };
            run_worker_loop(
                reader,
                &mut output,
                cancel,
                LoopOptions::default(),
                document_ok,
                screenshot,
                state,
            )
            .expect("EOF clean");
            output
        });
        send(
            &stdin,
            json!({"id":1,"command":"ocr_snapshot","image_base64":"fixture"}),
        );
        running.recv_timeout(DEADLINE).expect("screenshot started");
        drop(stdin);
        assert!(
            thread.join().expect("joined").is_empty(),
            "disconnected request must not write a success"
        );
    }

    #[test]
    fn document_queue_preserves_order_and_isolates_panics_and_bad_json() {
        let reader = Cursor::new(concat!(
            "{\"id\":1,\"command\":\"extract\",\"path\":\"a.txt\"}\n",
            "{\"id\":2,\"command\":\"extract\",\"path\":\"b.txt\"}\n",
            "bad json\n\n",
            "{\"id\":3,\"command\":\"extract\",\"path\":\"c.txt\"}\n"
        ));
        let mut output = Vec::new();
        let document = |request: WorkerRequest| {
            assert_ne!(request.id, json!(2), "test handler panic");
            document_ok(request)
        };
        run_worker_loop(
            reader,
            &mut output,
            Arc::new(AtomicBool::new(false)),
            LoopOptions::default(),
            document,
            snapshot_ok,
            state,
        )
        .expect("EOF clean");
        let values: Vec<Value> = String::from_utf8(output)
            .expect("UTF8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON"))
            .collect();
        assert_eq!(values.len(), 4);
        let documents: Vec<_> = values.iter().filter(|v| !v["id"].is_null()).collect();
        assert_eq!(
            documents.iter().map(|v| v["id"].clone()).collect::<Vec<_>>(),
            vec![json!(1), json!(2), json!(3)]
        );
        assert_eq!(documents[0]["ok"], true);
        assert_eq!(documents[1]["ok"], false);
        assert!(
            documents[1]["error"]
                .as_str()
                .expect("panic diagnostic")
                .contains("test handler panic")
        );
        assert_eq!(documents[2]["ok"], true);
        assert!(values.iter().any(|v| v["id"].is_null() && v["ok"] == false));
    }

    #[test]
    fn output_failure_returns_without_waiting_for_stdin_eof() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (stdin, reader) = input();
        let cancel = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancel);
        let (done, completed) = channel();
        let handle = thread::spawn(move || {
            let result = run_worker_loop(
                reader,
                &mut BrokenWriter,
                cancel,
                LoopOptions::default(),
                document_ok,
                snapshot_ok,
                state,
            );
            done.send(matches!(result, Ok(LoopExit::PeerDisconnected)))
                .expect("test listening");
        });
        send(&stdin, json!({"id":1,"command":"snapshot_state"}));
        assert!(completed.recv_timeout(DEADLINE).expect("must not wait for EOF"));
        assert!(observed.load(Ordering::Acquire));
        drop(stdin);
        handle.join().expect("joined");
    }

    #[test]
    fn invalid_snapshot_and_transcription_do_not_poison_the_other_channel() {
        use crate::commands::snapshot_ocr::SnapshotOcrEngine;
        use crate::commands::worker::{ocr_snapshot_request, snapshot_state_request};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let mut png = Vec::new();
        image::RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("PNG encodes");
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let thread = thread::spawn(move || {
            let mut engine = SnapshotOcrEngine::new(Some(xberg::core::config::SnapshotOcrConfig {
                models_dir: Some("Z:/xberg-missing-snapshot-models".into()),
                intra_threads: 3,
            }));
            let status = engine.state();
            let cancel = Arc::new(AtomicBool::new(false));
            let snapshot_cancel = Arc::clone(&cancel);
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                cancel,
                LoopOptions::default(),
                |request| {
                    if request.command == super::super::COMMAND_TRANSCRIBE {
                        RequestOutcome::Transcribe(Err("no such media".into()))
                    } else {
                        document_ok(request)
                    }
                },
                move |request| ocr_snapshot_request(&mut engine, request, &snapshot_cancel),
                move |_| snapshot_state_request(&status),
            )
        });
        for (id, image, kind) in [
            (1, "not base64".to_string(), "input_invalid"),
            (2, STANDARD.encode(png), "asset_invalid"),
        ] {
            send(&stdin, json!({"id":id,"command":"ocr_snapshot","image_base64":image}));
            let response = output.recv_timeout(DEADLINE).expect("snapshot failure response");
            assert_eq!(response["id"], id);
            assert_eq!(response["error_kind"], kind);
            assert_eq!(response["ok"], false);
        }
        send(&stdin, json!({"id":3,"command":"snapshot_state"}));
        let response = output.recv_timeout(DEADLINE).expect("state response");
        assert_eq!(response["state"], "error");
        assert!(response["error"].is_string());
        send(&stdin, json!({"id":4,"command":"transcribe","path":"missing.mp4"}));
        assert_eq!(
            output.recv_timeout(DEADLINE).expect("transcribe failure")["error"],
            "no such media"
        );
        send(&stdin, json!({"id":5,"command":"extract","path":"next.txt"}));
        assert_eq!(output.recv_timeout(DEADLINE).expect("document unaffected")["ok"], true);
        drop(stdin);
        thread.join().expect("joined").expect("EOF clean");
    }

    #[test]
    fn keepalive_and_version_answer_inline_without_enqueuing_work() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                |request| panic!("query commands must not run document work, got {}", request.command),
                |request| panic!("query commands must not run snapshot work, got {}", request.command),
                move |command| match command {
                    "keepalive" => RequestOutcome::Query(json!({"models": {"snapshot": "ready"}})),
                    "version" => RequestOutcome::Query(json!({"version": "9.9.9-test", "models": []})),
                    _ => state(command),
                },
            )
        });
        send(&stdin, json!({"id":1,"command":"keepalive"}));
        let response = output.recv_timeout(DEADLINE).expect("keepalive answered inline");
        assert_eq!(response["id"], 1);
        assert_eq!(response["ok"], true);
        assert_eq!(response["models"]["snapshot"], "ready");
        send(&stdin, json!({"id":2,"command":"version"}));
        let response = output.recv_timeout(DEADLINE).expect("version answered inline");
        assert_eq!(response["id"], 2);
        assert_eq!(response["ok"], true);
        assert_eq!(response["version"], "9.9.9-test");
        drop(stdin);
        thread.join().unwrap().unwrap();
    }

    /// P3: `shutdown` acknowledges on the wire first, refuses later requests,
    /// lets in-flight work finish inside the grace and exits cleanly (code 0).
    #[test]
    fn shutdown_acknowledges_refuses_new_work_and_drains_inflight() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let (started, running) = channel();
        let (release, wait) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                move |request| {
                    if request.id == json!(1) {
                        started.send(()).unwrap();
                        wait.recv_timeout(DEADLINE).unwrap();
                    }
                    document_ok(request)
                },
                snapshot_ok,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"long.pdf"}));
        running.recv_timeout(DEADLINE).expect("document started");
        send(&stdin, json!({"id":2,"command":"shutdown","grace_ms":8000}));
        let accepted = output.recv_timeout(DEADLINE).expect("shutdown acknowledged");
        assert_eq!(accepted["id"], 2);
        assert_eq!(accepted["ok"], true);
        assert_eq!(accepted["accepted"], true);
        for id in [3, 4] {
            send(&stdin, json!({"id":id,"command":"extract","path":"late.txt"}));
            let refused = output.recv_timeout(DEADLINE).expect("late request refused");
            assert_eq!(refused["id"], id);
            assert_eq!(refused["ok"], false);
            assert!(refused["error"].as_str().unwrap().contains("shutting down"));
        }
        release.send(()).expect("release document");
        let drained = output
            .recv_timeout(DEADLINE)
            .expect("in-flight document still finishes");
        assert_eq!(drained["id"], 1);
        assert_eq!(drained["ok"], true);
        drop(stdin);
        let exit = thread.join().unwrap().unwrap();
        assert_eq!(exit, LoopExit::SessionEnded);
    }

    /// P3: the grace cap is a hard stop — a request stuck in native code must
    /// not hold the process open past `grace_ms`.
    #[test]
    fn shutdown_deadline_forces_exit_despite_stuck_native_work() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions::default(),
                move |request| {
                    // Simulates a native call that ignores cooperative cancellation.
                    let until = Instant::now() + Duration::from_secs(8);
                    while Instant::now() < until {
                        thread::sleep(Duration::from_millis(20));
                    }
                    document_ok(request)
                },
                snapshot_ok,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"stuck.pdf"}));
        send(&stdin, json!({"id":2,"command":"shutdown","grace_ms":150}));
        let accepted = output.recv_timeout(DEADLINE).expect("shutdown acknowledged");
        assert_eq!(accepted["accepted"], true);
        let started = Instant::now();
        let exit = thread.join().unwrap().unwrap();
        assert_eq!(exit, LoopExit::SessionEnded);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "shutdown must not wait for stuck work, took {:?}",
            started.elapsed()
        );
        drop(stdin);
    }

    /// P1: stdin EOF drains documents, but never past the hard cap — work that
    /// ignores cooperative cancellation is abandoned at the deadline (exit 0).
    #[test]
    fn eof_deadline_cancels_work_that_ignores_cancellation() {
        let (stdin, reader) = input();
        let (responses, _output) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions {
                    drain_grace: Duration::from_millis(150),
                    ..LoopOptions::default()
                },
                move |request| {
                    let until = Instant::now() + Duration::from_secs(8);
                    while Instant::now() < until {
                        thread::sleep(Duration::from_millis(20));
                    }
                    document_ok(request)
                },
                snapshot_ok,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"stuck.pdf"}));
        let started = Instant::now();
        drop(stdin);
        let exit = thread.join().unwrap().unwrap();
        assert_eq!(exit, LoopExit::SessionEnded);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "EOF must cap the drain, took {:?}",
            started.elapsed()
        );
    }

    /// P1: the monitor flag (stdout probe / parent death) cancels in-flight work
    /// and ends the loop immediately without writing anything.
    #[test]
    fn monitor_peer_gone_flag_cancels_and_exits_without_writing() {
        let (stdin, reader) = input();
        let flag = Arc::new(AtomicBool::new(false));
        let loop_flag = Arc::clone(&flag);
        let (started, running) = channel();
        let thread = thread::spawn(move || {
            let mut output = Vec::new();
            let document = move |request| {
                started.send(()).expect("test listening");
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    thread::sleep(Duration::from_millis(1));
                }
                document_ok(request)
            };
            let exit = run_worker_loop(
                reader,
                &mut output,
                Arc::new(AtomicBool::new(false)),
                LoopOptions {
                    peer_gone: Some(loop_flag),
                    ..LoopOptions::default()
                },
                document,
                snapshot_ok,
                state,
            )
            .unwrap();
            (exit, output)
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"busy.pdf"}));
        running.recv_timeout(DEADLINE).expect("document started");
        flag.store(true, Ordering::Release);
        let (exit, output) = thread.join().expect("joined");
        assert_eq!(exit, LoopExit::PeerDisconnected);
        assert!(output.is_empty(), "no responses may be written to a gone peer");
        drop(stdin);
    }

    /// P4: a quiet worker with `idle_timeout_ms` set exits on its own once no
    /// request traffic and no in-flight work remain.
    #[test]
    fn idle_timeout_exits_a_quiet_worker() {
        let (stdin, reader) = input();
        let thread = thread::spawn(move || {
            let mut output = Vec::new();
            let exit = run_worker_loop(
                reader,
                &mut output,
                Arc::new(AtomicBool::new(false)),
                LoopOptions {
                    idle_timeout: Some(Duration::from_millis(80)),
                    ..LoopOptions::default()
                },
                document_ok,
                snapshot_ok,
                state,
            )
            .unwrap();
            (exit, output)
        });
        let started = Instant::now();
        let (exit, output) = thread.join().expect("joined");
        assert_eq!(exit, LoopExit::IdleTimeout);
        assert!(output.is_empty());
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(stdin);
    }

    /// P4: the idle clock restarts when a request completes. A request that
    /// runs longer than `idle_timeout_ms` is legal (idle time bounds quiet
    /// periods, not work); timing out right after its response would kill the
    /// resident worker the caller is about to reuse (WORKER.md: exit 87 needs
    /// no in-flight requests AND no traffic for the whole window).
    #[test]
    fn idle_timeout_clock_restarts_when_a_long_request_completes() {
        let (stdin, reader) = input();
        let (responses, output) = channel();
        let thread = thread::spawn(move || {
            run_worker_loop(
                reader,
                &mut Output {
                    responses,
                    pending: Vec::new(),
                },
                Arc::new(AtomicBool::new(false)),
                LoopOptions {
                    idle_timeout: Some(Duration::from_millis(100)),
                    ..LoopOptions::default()
                },
                move |request| {
                    thread::sleep(Duration::from_millis(400));
                    document_ok(request)
                },
                snapshot_ok,
                state,
            )
        });
        send(&stdin, json!({"id":1,"command":"extract","path":"slow.pdf"}));
        let done = output.recv_timeout(DEADLINE).expect("long request finishes");
        assert_eq!(done["id"], 1);
        assert_eq!(done["ok"], true);
        // With the stale pre-response clock this keepalive lands after the
        // worker already exited with IdleTimeout and goes unanswered.
        send(&stdin, json!({"id":2,"command":"keepalive"}));
        let alive = output
            .recv_timeout(DEADLINE)
            .expect("worker still serving after a long request");
        assert_eq!(alive["id"], 2);
        assert_eq!(alive["ok"], true);
        drop(stdin);
        let exit = thread.join().unwrap().unwrap();
        assert_eq!(exit, LoopExit::SessionEnded);
    }

    /// P1: a failed stdin read (host vanished mid-read) is a disconnect, not an
    /// anyhow error — the exit code must stay the fixed non-zero one.
    #[test]
    fn broken_stdin_read_maps_to_peer_disconnected() {
        struct BrokenStdin;
        impl Read for BrokenStdin {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let reader = BufReader::new(BrokenStdin);
        let mut output = Vec::new();
        let exit = run_worker_loop(
            reader,
            &mut output,
            Arc::new(AtomicBool::new(false)),
            LoopOptions::default(),
            document_ok,
            snapshot_ok,
            state,
        )
        .unwrap();
        assert_eq!(exit, LoopExit::PeerDisconnected);
    }

    #[test]
    fn queue_backpressure_returns_a_correlated_failure_without_blocking() {
        let (queue, _receiver) = mpsc::sync_channel(1);
        let mut output = Vec::new();
        {
            let mut writer = PeerWriter::new(&mut output);
            for id in [1, 2] {
                let request =
                    super::super::parse_request(&json!({"id":id,"command":"extract","path":"a.txt"}).to_string())
                        .expect("request");
                enqueue(&queue, request, &mut writer).expect("write succeeds");
            }
        }
        let response: Value = serde_json::from_slice(&output).expect("one response for rejected request");
        assert_eq!(response["id"], 2);
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .expect("diagnostic")
                .contains("queue is full")
        );
    }
}
