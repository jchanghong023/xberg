//! Built-in audio/video transcription extractor (speech-to-text).
//!
//! Only compiled when the `transcription` feature is enabled.
//! Registers for the audio and video MIME types declared in `core::mime`.
//!
//! The actual heavy lifting (FFmpeg DLL decode → Silero VAD → SenseVoice INT8)
//! lives in `crate::transcription::sensevoice`. This module is the thin
//! "plugin" adapter that the registry expects.

use std::sync::{Arc, LazyLock};

use crate::core::config::ExtractionConfig;
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::internal::{ElementKind, InternalDocument, InternalElement};
use crate::types::metadata::{AudioMetadata, FormatMetadata};
use crate::{Result, XbergError};
use ahash::AHashMap;
use async_trait::async_trait;
use tokio::task;

/// Attribute key holding a segment's start time (milliseconds, as a decimal string).
const ATTR_START_MS: &str = "start_ms";
/// Attribute key holding a segment's end time (milliseconds, as a decimal string).
const ATTR_END_MS: &str = "end_ms";

/// Semaphore that limits the number of concurrent transcription runs.
///
/// The budget matches `resolve_thread_budget` — the same value used by the
/// embedding and reranking semaphores so all inference shares one
/// per-process concurrency bound.
static TRANSCRIPTION_SEMAPHORE: LazyLock<Arc<tokio::sync::Semaphore>> = LazyLock::new(|| {
    let budget = crate::core::config::concurrency::resolve_thread_budget(None);
    Arc::new(tokio::sync::Semaphore::new(budget))
});

/// Run `future` under a wall-clock deadline, bounding total async work.
///
/// `timeout_ms = None` disables the bound and simply awaits `future`. On
/// elapse, `future` is dropped (canceling any `.await` points inside it;
/// already-spawned `spawn_blocking` tasks keep running to completion in the
/// background but their result is discarded) and a
/// [`XbergError::Transcription`](crate::XbergError) is returned so callers get
/// a clear error instead of blocking forever.
async fn apply_timeout<T, Fut>(timeout_ms: Option<u64>, future: Fut) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>>,
{
    match timeout_ms {
        Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), future)
            .await
            .map_err(|_| {
                XbergError::transcription(format!(
                    "Transcription exceeded transcription.timeout_ms limit of {ms} ms. \
                     Increase `transcription.timeout_ms` or shorten the input."
                ))
            })?,
        None => future.await,
    }
}

/// Runs `task` on the blocking pool while holding a permit from `semaphore`.
///
/// The permit is moved *into* the blocking closure rather than held by this future. A
/// `spawn_blocking` task cannot be cancelled — dropping its `JoinHandle` detaches it and the
/// closure still runs to completion — so a permit owned by the awaiting future is released the
/// moment a caller times out or drops, while the transcription it was bounding continues.
/// The pipeline is wrapped in [`apply_timeout`], so a `transcription.timeout_ms`
/// expiry drops that future on a live, designed-in code path, not a hypothetical one. Repeated
/// abandoned calls then exceed the configured concurrency and keep the model session resident on
/// the blocking pool (same defect as GH#1641, which fixed the reranker; this is the transcription
/// half). Taking the semaphore as a parameter also gives the tests a locally-owned semaphore,
/// since the global one's permit count is 1 on a small host. ~keep
async fn transcribe_holding_permit<T, E, F>(semaphore: Arc<tokio::sync::Semaphore>, task: F) -> Result<T>
where
    F: FnOnce() -> std::result::Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|e| XbergError::transcription(format!("semaphore closed: {e}")))?;

    task::spawn_blocking(move || {
        let _permit = permit;
        task()
    })
    .await
    .map_err(|e| XbergError::transcription(format!("transcription task panicked: {e}")))?
    .map_err(|e| XbergError::transcription(format!("transcription failed: {e}")))
}

/// Document name constant used by the audio transcript extractor.
///
/// `extract_content` only receives anonymous bytes (no source filename), so
/// the SV-06 `# ` header falls back to this name on the extract path. Callers
/// with a real path (e.g. the worker `transcribe` command) pass the
/// real file name straight into `sensevoice::transcribe_bytes`.
const SENSEVOICE_DOCUMENT_NAME: &str = "audio-transcript";

/// SenseVoice pipeline: FFmpeg DLL decode → Silero VAD → SenseVoice INT8,
/// producing the SV-06 Markdown structure.
///
/// Runs on the blocking thread pool under the shared transcription semaphore;
/// the enclosing [`apply_timeout`] in `extract_content` bounds the whole call.
async fn run_sensevoice_pipeline(
    content: &[u8],
    mime_type: &str,
    tcfg: &crate::core::config::transcription::TranscriptionConfig,
    cancel: crate::cancellation::CancellationToken,
) -> Result<InternalDocument> {
    let bytes = content.to_vec();
    let mime_owned = mime_type.to_string();
    let model_dir = tcfg.model_dir.clone();
    let max_duration_ms = tcfg.max_duration_ms;
    let name = SENSEVOICE_DOCUMENT_NAME.to_string();

    let result = transcribe_holding_permit(TRANSCRIPTION_SEMAPHORE.clone(), move || {
        crate::transcription::sensevoice::transcribe_bytes_cancellable(
            &bytes,
            &name,
            &mime_owned,
            model_dir.as_deref(),
            max_duration_ms,
            &cancel,
        )
    })
    .await?;

    // AudioMetadata carries the decoded duration (0 for trackless containers);
    // the sensevoice chain owns decoding, so no PCM samples are retained here.
    let mut doc = build_audio_document(result.duration_ms, mime_type);
    push_sensevoice_elements(&mut doc, &result);
    Ok(doc)
}

/// Push the SV-06 document onto `doc`: `# name`, duration line, segment
/// count, `## 转录`, then one paragraph per `[start --> end] text` segment.
/// Each segment paragraph carries `start_ms`/`end_ms` attributes (structured
/// timestamps for JSON consumers). The identical layout is also available
/// verbatim as `SenseVoiceResult::markdown`.
fn push_sensevoice_elements(doc: &mut InternalDocument, result: &crate::transcription::sensevoice::SenseVoiceResult) {
    use crate::transcription::sensevoice::{NO_AUDIO_NOTE, NO_SPEECH_NOTE, duration_line, segment_line_ms};

    doc.push_element(InternalElement::text(ElementKind::Title, &result.name, 0));
    doc.push_element(InternalElement::text(
        ElementKind::Paragraph,
        duration_line(result.has_audio, result.duration_ms as f32 / 1000.0),
        0,
    ));
    doc.push_element(InternalElement::text(
        ElementKind::Paragraph,
        format!("- 语音片段: {}", result.segments.len()),
        0,
    ));
    doc.push_element(InternalElement::text(ElementKind::Heading { level: 2 }, "转录", 0));

    if !result.has_audio {
        doc.push_element(InternalElement::text(ElementKind::Paragraph, NO_AUDIO_NOTE, 0));
        return;
    }
    if result.segments.is_empty() {
        doc.push_element(InternalElement::text(ElementKind::Paragraph, NO_SPEECH_NOTE, 0));
        return;
    }
    for (start_ms, end_ms, text) in &result.segments {
        let mut element = InternalElement::text(ElementKind::Paragraph, segment_line_ms(*start_ms, *end_ms, text), 0);
        let mut attributes = AHashMap::default();
        attributes.insert(ATTR_START_MS.to_string(), start_ms.to_string());
        attributes.insert(ATTR_END_MS.to_string(), end_ms.to_string());
        element.attributes = Some(attributes);
        doc.push_element(element);
    }
}

/// The transcription extractor.
///
/// Priority is the normal default (50). If a user registers a custom
/// higher-priority transcription backend via the plugin system, it will win.
#[cfg_attr(alef, alef(skip))]
pub struct TranscriptionExtractor;

impl Plugin for TranscriptionExtractor {
    fn name(&self) -> &str {
        "transcription"
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    fn initialize(&self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl InternalDocumentExtractor for TranscriptionExtractor {
    async fn extract_content(
        &self,
        content: &[u8],
        mime_type: &str,
        config: &ExtractionConfig,
    ) -> Result<InternalDocument> {
        let tcfg = config.transcription.as_ref().filter(|c| c.enabled).ok_or_else(|| {
            XbergError::transcription(
                "Transcription requested for audio/video input, but no `transcription` \
                     config block was provided (or `enabled` is false). \
                     Add `transcription = { enabled = true, backend = \"sensevoice\" }` \
                     to your ExtractionConfig.",
            )
        })?;

        if let Some(max_b) = tcfg.max_bytes
            && content.len() as u64 > max_b
        {
            return Err(XbergError::transcription(format!(
                "Input size {} bytes exceeds transcription.max_bytes limit of {}",
                content.len(),
                max_b
            )));
        }

        apply_timeout(
            tcfg.timeout_ms,
            run_sensevoice_pipeline(
                content,
                mime_type,
                tcfg,
                config.cancel_token.clone().unwrap_or_default(),
            ),
        )
        .await
    }

    fn supported_mime_types(&self) -> &[&str] {
        // The `audio/mp3`, `audio/x-m4a`, `audio/x-wav` and `video/mpeg` entries are the
        // aliases core/mime.rs declares for the four canonical types beside them.
        // `validate_mime_type` accepts an alias verbatim and the registry looks extractors up
        // by exact string with no alias resolution, so an unclaimed alias is advertised as
        // supported and then fails as UnsupportedFormat (#229). The ASF/WMV entries are
        // decoded through the pinned FFmpeg shared libraries like every other container
        // (SV-02: WMV/ASF remain readable).
        &[
            "audio/mpeg",
            "audio/mp3",
            "audio/mp4",
            "audio/x-m4a",
            "audio/wav",
            "audio/x-wav",
            "audio/webm",
            "video/mp4",
            "video/mpeg",
            "video/webm",
            "video/x-ms-wmv",
            "video/x-ms-asf",
            "application/vnd.ms-asf",
        ]
    }

    fn priority(&self) -> i32 {
        50
    }
}

/// Construct an [`InternalDocument`] for one transcription result.
///
/// The document carries the decoded-audio properties reported by the
/// SenseVoice chain (16 kHz mono; `duration_ms = 0` for trackless containers)
/// in [`AudioMetadata`]. Container tag scraping (lofty) retired together with
/// the Whisper chain, so common metadata (title/artist/...) is no longer
/// populated here.
fn build_audio_document(duration_ms: u64, mime_type: &str) -> InternalDocument {
    let audio_meta = AudioMetadata {
        duration_ms: Some(duration_ms),
        codec: None,
        container: None,
        sample_rate_hz: Some(crate::transcription::sensevoice::SAMPLE_RATE_HZ),
        channels: Some(1),
        bitrate: None,
    };

    let mut doc = InternalDocument::new(SENSEVOICE_DOCUMENT_NAME);
    doc.mime_type = mime_type.to_string();
    doc.metadata.format = Some(FormatMetadata::Audio(audio_meta));
    doc
}

#[cfg(test)]
mod permit_tests {
    use super::*;
    use std::future::Future;
    use std::sync::Barrier;
    use std::task::Poll;

    #[tokio::test]
    async fn permit_stays_with_the_blocking_task_when_the_waiter_is_cancelled() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let release = Arc::new(Barrier::new(2));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let mut waiter = Box::pin(transcribe_holding_permit(Arc::clone(&semaphore), {
            let release = Arc::clone(&release);
            move || -> std::result::Result<Vec<(u32, u32, String)>, String> {
                let _ = started_tx.send(());
                release.wait();
                Ok(Vec::new())
            }
        }));

        let first = std::future::poll_fn(|cx| Poll::Ready(Future::poll(waiter.as_mut(), cx))).await;
        assert!(
            first.is_pending(),
            "the blocking task must still be running after the first poll"
        );
        started_rx.await.expect("the blocking task must have started");

        drop(waiter);

        // Observe first, release the barrier second, assert last. Asserting before the
        // `release.wait()` below parks the blocking-pool thread on the barrier forever when the
        // assertion fails, so the test binary never exits and the whole job dies on a timeout --
        // which CI reports as `cancelled`, not as this failure. ~keep
        let permit_withheld = Arc::clone(&semaphore).try_acquire_owned().is_err();

        release.wait();

        assert!(
            permit_withheld,
            "a cancelled waiter must not return the permit while its blocking task is still running"
        );
        let regained = tokio::time::timeout(std::time::Duration::from_secs(5), semaphore.acquire()).await;
        assert!(
            regained.is_ok(),
            "the permit must return once the blocking task finishes"
        );
    }

    #[tokio::test]
    async fn permit_is_returned_after_a_completed_call() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));

        let results = transcribe_holding_permit(
            Arc::clone(&semaphore),
            || -> std::result::Result<Vec<(u32, u32, String)>, String> { Ok(Vec::new()) },
        )
        .await
        .expect("the task must succeed");

        assert_eq!(results.len(), 0, "the stub task returns no segments");
        assert_eq!(
            semaphore.available_permits(),
            1,
            "a completed call must release its permit"
        );
    }

    #[tokio::test]
    async fn semaphore_closed_error_names_the_semaphore() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        semaphore.close();

        let err = transcribe_holding_permit(semaphore, || -> std::result::Result<Vec<(u32, u32, String)>, String> {
            Ok(Vec::new())
        })
        .await
        .expect_err("a closed semaphore must be surfaced as an error");

        assert!(err.to_string().contains("semaphore closed"), "{err}");
    }

    #[tokio::test]
    async fn inference_error_is_surfaced_separately_from_a_join_error() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));

        let err = transcribe_holding_permit(semaphore, || -> std::result::Result<Vec<(u32, u32, String)>, String> {
            Err("decoder rejected the clip".to_string())
        })
        .await
        .expect_err("the inner error must propagate");

        assert!(err.to_string().contains("transcription failed"), "{err}");
        assert!(err.to_string().contains("decoder rejected the clip"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ExtractionConfig;
    use crate::core::config::transcription::TranscriptionConfig;

    #[test]
    fn test_transcription_extractor_metadata() {
        let ext = TranscriptionExtractor;
        assert_eq!(ext.name(), "transcription");
        assert!(ext.supported_mime_types().contains(&"audio/mpeg"));
        assert!(ext.supported_mime_types().contains(&"video/mp4"));
    }

    #[test]
    fn test_transcription_config_defaults_roundtrip() {
        let cfg = TranscriptionConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TranscriptionConfig = serde_json::from_str(&json).unwrap();
        assert!(back.enabled);
        assert_eq!(
            back.backend,
            crate::core::config::transcription::TranscriptionBackend::SenseVoice
        );
    }

    fn config_with_transcription(tcfg: TranscriptionConfig) -> ExtractionConfig {
        ExtractionConfig {
            transcription: Some(tcfg),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_async_no_config_returns_error() {
        let ext = TranscriptionExtractor;
        let cfg = ExtractionConfig::default();
        let result = ext.extract_content(&[], "audio/mpeg", &cfg).await;
        assert!(result.is_err(), "expected error when no transcription config (async)");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("config") || msg.contains("disabled"), "unexpected: {msg}");
        // The error names the current config shape, not the retired Whisper key.
        assert!(msg.contains("backend = \"sensevoice\""), "unexpected: {msg}");
        assert!(!msg.contains("model = \"tiny\""), "unexpected: {msg}");
    }

    #[tokio::test]
    async fn test_async_disabled_config_returns_error() {
        let ext = TranscriptionExtractor;
        let tcfg = TranscriptionConfig {
            enabled: false,
            ..Default::default()
        };
        let cfg = config_with_transcription(tcfg);
        let result = ext.extract_content(&[], "audio/mpeg", &cfg).await;
        assert!(result.is_err(), "expected error when transcription disabled");
    }

    #[tokio::test]
    async fn test_async_size_limit_enforced() {
        let ext = TranscriptionExtractor;
        let tcfg = TranscriptionConfig {
            max_bytes: Some(10),
            ..Default::default()
        };
        let cfg = config_with_transcription(tcfg);
        let oversized = vec![0u8; 11];
        let result = ext.extract_content(&oversized, "audio/mpeg", &cfg).await;
        assert!(result.is_err(), "expected error when input exceeds max_bytes (async)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceed") || msg.contains("limit") || msg.contains("size"),
            "unexpected: {msg}"
        );
    }

    /// Regression test for #278: `TranscriptionConfig::timeout_ms` had zero readers —
    /// the sibling caps (`max_bytes`, `max_duration_ms`) were enforced, but a
    /// transcription run had no wall-clock bound at all. `apply_timeout` is the
    /// mechanism `extract_content` now wraps the whole pipeline in.
    #[tokio::test]
    async fn apply_timeout_returns_error_when_future_exceeds_timeout_ms() {
        let result: Result<()> = apply_timeout(Some(10), async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok(())
        })
        .await;
        assert!(result.is_err(), "expected timeout error, got {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timeout_ms"), "unexpected message: {msg}");
    }

    #[tokio::test]
    async fn apply_timeout_passes_through_ok_when_future_finishes_in_time() {
        let result: Result<i32> = apply_timeout(Some(5_000), async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn apply_timeout_passes_through_err_when_future_finishes_in_time() {
        let result: Result<i32> =
            apply_timeout(Some(5_000), async { Err(XbergError::transcription("inner failure")) }).await;
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("inner failure"), "unexpected message: {msg}");
    }

    #[tokio::test]
    async fn apply_timeout_with_none_never_times_out() {
        let result: Result<i32> = apply_timeout(None, async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(7)
        })
        .await;
        assert_eq!(result.unwrap(), 7);
    }

    /// End-to-end wiring check: `extract_content` must actually read
    /// `transcription.timeout_ms` and apply it around the real pipeline, not just
    /// have `apply_timeout` exist unused.
    ///
    /// With a zero deadline the run must always fail. Which failure wins the race
    /// depends on the environment: with real SenseVoice assets configured
    /// (`XBERG_TEST_SENSEVOICE_ROOT`) the 239 MB model load deterministically
    /// loses to the 0 ms deadline and the timeout surfaces; without them the
    /// asset precheck completes first and the missing-asset error surfaces
    /// (the whisper-era test could assume the timeout because model loading was
    /// always slow — that assumption does not hold for a fast asset failure).
    #[tokio::test]
    async fn extract_content_enforces_timeout_ms() {
        let wav_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/audio/silence-1s.wav");
        let bytes = std::fs::read(&wav_path).unwrap_or_else(|e| panic!("missing audio fixture {wav_path:?}: {e}"));

        let ext = TranscriptionExtractor;
        let mut tcfg = TranscriptionConfig {
            timeout_ms: Some(0),
            ..Default::default()
        };
        if let Some(root) = std::env::var_os("XBERG_TEST_SENSEVOICE_ROOT") {
            tcfg.model_dir = Some(std::path::PathBuf::from(root));
        }
        let cfg = config_with_transcription(tcfg);
        let result = ext.extract_content(&bytes, "audio/wav", &cfg).await;
        assert!(result.is_err(), "expected a failed run, got {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timeout_ms") || msg.contains("媒体模型缺失"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn test_build_audio_document_populates_decoded_audio_metadata() {
        use crate::types::metadata::FormatMetadata;

        let doc = build_audio_document(61_250, "video/mp4");
        assert_eq!(doc.mime_type, "video/mp4");
        assert_eq!(doc.source_format, SENSEVOICE_DOCUMENT_NAME);

        let Some(FormatMetadata::Audio(ref audio)) = doc.metadata.format else {
            panic!("expected FormatMetadata::Audio, got {:?}", doc.metadata.format);
        };
        assert_eq!(audio.duration_ms, Some(61_250));
        assert_eq!(
            audio.sample_rate_hz,
            Some(crate::transcription::sensevoice::SAMPLE_RATE_HZ)
        );
        assert_eq!(audio.channels, Some(1));
        assert!(audio.codec.is_none() && audio.container.is_none() && audio.bitrate.is_none());
    }

    #[test]
    fn test_build_audio_document_trackless_container_reports_zero_duration() {
        use crate::types::metadata::FormatMetadata;

        let doc = build_audio_document(0, "video/mp4");
        let Some(FormatMetadata::Audio(ref audio)) = doc.metadata.format else {
            panic!("expected FormatMetadata::Audio");
        };
        assert_eq!(audio.duration_ms, Some(0));
    }

    /// SenseVoice end-to-end wiring check: `extract_content` must run the
    /// SV-06 chain and produce its element structure. Runtime-gated on the
    /// pinned assets + real media (skipped with a printed reason when absent):
    /// - `XBERG_TEST_SENSEVOICE_ROOT` — model root directory
    /// - `XBERG_TEST_SHERPA_DLL_DIR`, `XBERG_TEST_FFMPEG_DLL_DIR` — native libs
    /// - `XBERG_TEST_MEDIA_MP4` — real speech MP4
    #[cfg(feature = "transcription")]
    #[tokio::test]
    async fn extract_content_produces_sv06_structure_for_real_media() {
        let model_root = match std::env::var_os("XBERG_TEST_SENSEVOICE_ROOT")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
        {
            Some(root) => root,
            None => {
                println!("skip：XBERG_TEST_SENSEVOICE_ROOT 不在场");
                return;
            }
        };
        let media = match std::env::var_os("XBERG_TEST_MEDIA_MP4")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
        {
            Some(m) => m,
            None => {
                println!("skip：XBERG_TEST_MEDIA_MP4 不在场");
                return;
            }
        };
        if std::env::var_os("XBERG_TEST_SHERPA_DLL_DIR").is_none()
            || std::env::var_os("XBERG_TEST_FFMPEG_DLL_DIR").is_none()
        {
            println!("skip：XBERG_TEST_SHERPA_DLL_DIR / XBERG_TEST_FFMPEG_DLL_DIR 不在场");
            return;
        }

        // Map test-gate variables onto the runtime variables the backend reads.
        // SAFETY：测试进程内、其它测试写入的是相同值（Windows std 内部有锁）。
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var(
                crate::transcription::sensevoice::SHERPA_DLL_DIR_ENV,
                std::env::var_os("XBERG_TEST_SHERPA_DLL_DIR").expect("checked above"),
            );
            std::env::set_var(
                crate::transcription::sensevoice::FFMPEG_DLL_DIR_ENV,
                std::env::var_os("XBERG_TEST_FFMPEG_DLL_DIR").expect("checked above"),
            );
        }

        let bytes = std::fs::read(&media).unwrap_or_else(|e| panic!("missing test media {media:?}: {e}"));
        let tcfg = TranscriptionConfig {
            model_dir: Some(model_root),
            // Real model load + decode needs more than the 10 s default? No —
            // the default is 10 minutes; keep it explicit for clarity.
            timeout_ms: Some(600_000),
            ..Default::default()
        };
        let cfg = config_with_transcription(tcfg);
        let ext = TranscriptionExtractor;
        let doc = ext
            .extract_content(&bytes, "video/mp4", &cfg)
            .await
            .expect("sensevoice extraction must succeed");

        // SV-06 element structure: Title, duration line, segment count,
        // `## 转录` heading, segment paragraphs with start_ms/end_ms attrs.
        let kinds: Vec<String> = doc.elements.iter().map(|e| format!("{:?}", e.kind)).collect();
        assert!(
            kinds.iter().any(|k| k == "Title"),
            "expected a Title element, got {kinds:?}"
        );
        let texts: Vec<&str> = doc.elements.iter().map(|e| e.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.starts_with("- 音频时长: ")),
            "expected the duration line, got {texts:?}"
        );
        assert!(texts.contains(&"转录"), "expected the 转录 heading, got {texts:?}");
        let segments: Vec<_> = doc
            .elements
            .iter()
            .filter(|e| e.text.starts_with('[') && e.text.contains(" --> "))
            .collect();
        assert!(!segments.is_empty(), "expected transcript segment paragraphs");
        for element in &segments {
            let attributes = element.attributes.as_ref().expect("segment attrs");
            assert!(attributes.contains_key("start_ms"));
            assert!(attributes.contains_key("end_ms"));
        }
    }
}
