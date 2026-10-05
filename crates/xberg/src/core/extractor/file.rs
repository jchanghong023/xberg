//! File-based extraction operations.
//!
//! This module handles extraction from filesystem paths, including:
//! - MIME type detection and validation
//! - Legacy format conversion (DOC, PPT)
//! - File validation and reading
//! - Extraction pipeline orchestration

use crate::Result;
use crate::XbergError;
use crate::core::config::{ExtractionConfig, MimeDetectionPolicy};
use crate::core::mime::{LEGACY_POWERPOINT_MIME_TYPE, LEGACY_WORD_MIME_TYPE};
use crate::plugins::InternalDocumentExtractor;
use crate::plugins::registry::RegisteredDocumentExtractor;
use crate::types::ExtractedDocument;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
// (fork) perf-tracing 的 format_extract span 与 otel 的 stage span 共用 Instrument。
use super::helpers::get_extractor;
#[cfg(any(feature = "otel", feature = "perf-tracing"))]
use tracing::Instrument;

fn ensure_builtin_extraction_method(doc: &mut crate::types::internal::InternalDocument, is_builtin: bool) {
    if !is_builtin {
        return;
    }

    let method = doc
        .metadata
        .additional
        .get("extraction_method")
        .and_then(serde_json::Value::as_str)
        .and_then(crate::types::ExtractionMethod::from_metadata_value);
    if method.is_none() {
        doc.metadata.additional.insert(
            std::borrow::Cow::Borrowed("extraction_method"),
            serde_json::Value::String(crate::types::ExtractionMethod::Native.as_str().to_string()),
        );
    }
}

#[derive(Clone, Copy)]
struct FileDetectionChecks {
    force_ocr_conflict: bool,
    scanned_pages_ocr_conflict: bool,
}

fn open_regular_file(path: &Path) -> Result<File> {
    // Stat before opening: Windows refuses to open a directory at all (ACCESS_DENIED
    // surfaces as an Io error before the is_file check could classify it), so every
    // platform must reject non-regular inputs before the open for the documented
    // Validation error to be reported consistently.
    if !path.metadata().map_err(XbergError::from)?.is_file() {
        return Err(XbergError::validation(
            "Extraction input must be a regular file".to_string(),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        // ~keep: NONBLOCK prevents a FIFO open from pinning the detection task before handle metadata rejects it.
        options.custom_flags(libc::O_NONBLOCK);
    }

    let file = options.open(path).map_err(XbergError::from)?;
    if !file.metadata().map_err(XbergError::from)?.is_file() {
        return Err(XbergError::validation(
            "Extraction input must be a regular file".to_string(),
        ));
    }
    Ok(file)
}

fn detect_file_mime_blocking(
    path: &Path,
    mime_type: Option<&str>,
    policy: MimeDetectionPolicy,
    checks: FileDetectionChecks,
) -> Result<String> {
    let mut file = open_regular_file(path)?;
    if checks.force_ocr_conflict {
        return Err(XbergError::validation(
            "force_ocr and disable_ocr cannot both be true".to_string(),
        ));
    }
    if checks.scanned_pages_ocr_conflict {
        return Err(XbergError::validation(
            "ocr_strategy selects scanned pages for OCR, but disable_ocr is true".to_string(),
        ));
    }
    crate::core::mime::detect_or_validate_file(path, &mut file, mime_type, policy)
}

#[cfg(all(feature = "tokio-runtime", not(target_arch = "wasm32")))]
async fn detect_file_mime(
    path: &Path,
    mime_type: Option<&str>,
    policy: MimeDetectionPolicy,
    checks: FileDetectionChecks,
) -> Result<String> {
    let owned_path = path.to_path_buf();
    let owned_mime_type = mime_type.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        detect_file_mime_blocking(&owned_path, owned_mime_type.as_deref(), policy, checks)
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "file MIME detection task failed");
        XbergError::Other("File MIME detection task failed".to_string())
    })?
}

#[cfg(any(not(feature = "tokio-runtime"), target_arch = "wasm32"))]
async fn detect_file_mime(
    path: &Path,
    mime_type: Option<&str>,
    policy: MimeDetectionPolicy,
    checks: FileDetectionChecks,
) -> Result<String> {
    detect_file_mime_blocking(path, mime_type, policy, checks)
}

/// Extract content from a file.
///
/// This is the main entry point for file-based extraction. It performs the following steps:
/// 1. Check cache for existing result (if caching enabled)
/// 2. Detect or validate MIME type
/// 3. Select appropriate extractor from registry
/// 4. Extract content
/// 5. Run post-processing pipeline
/// 6. Store result in cache (if caching enabled)
///
/// # Arguments
///
/// * `path` - Path to the file to extract
/// * `mime_type` - Optional MIME type override. If None, will be auto-detected
/// * `config` - Extraction configuration
///
/// # Returns
///
/// An `ExtractedDocument` containing the extracted content and metadata.
///
/// # Errors
///
/// Returns `XbergError::Io` if the file doesn't exist (NotFound) or for other file I/O errors.
/// Returns `XbergError::UnsupportedFormat` if MIME type is not supported.
///
/// # Example
///
/// This function is crate-internal; the public entry point that reaches it is
/// [`crate::extract`] with a URI input.
///
/// ```rust,no_run
/// use xberg::{ExtractInput, ExtractionConfig, extract};
///
/// # async fn example() -> xberg::Result<()> {
/// let config = ExtractionConfig::default();
/// let output = extract(ExtractInput::from_uri("document.pdf"), &config).await?;
/// println!("Content: {}", output.results[0].content);
/// # Ok(())
/// # }
/// ```
#[cfg_attr(feature = "otel", tracing::instrument(
    skip(config, path),
    fields(
        { crate::telemetry::conventions::OPERATION } = crate::telemetry::conventions::operations::EXTRACT_FILE,
        { crate::telemetry::conventions::DOCUMENT_FILENAME } = tracing::field::Empty,
        { crate::telemetry::conventions::OTEL_STATUS_CODE } = tracing::field::Empty,
        { crate::telemetry::conventions::ERROR_TYPE } = tracing::field::Empty,
        { crate::telemetry::conventions::ERROR_MESSAGE } = tracing::field::Empty,
    )
))]
pub(crate) async fn extract_file(
    path: impl AsRef<Path>,
    mime_type: Option<&str>,
    config: &ExtractionConfig,
) -> Result<ExtractedDocument> {
    let path = path.as_ref();

    #[cfg(feature = "otel")]
    {
        let span = tracing::Span::current();
        span.record(
            crate::telemetry::conventions::DOCUMENT_FILENAME,
            crate::telemetry::spans::sanitize_path(path),
        );
    }

    // `token.cancel()` below needs a token to signal, but `config.cancel_token` is
    // `None` on every binding-driven and CLI-driven call (see
    // `ExtractionConfig::ensure_cancel_token`) — install an internal fallback so a
    // timeout actually stops the extraction instead of merely returning
    // `Err(Timeout)` while the spawned work keeps running. A caller-supplied token
    // is always left untouched. Gated identically to the timeout block below: on
    // wasm32 / without `tokio-runtime` there is no timeout to enforce, so no token
    // is ever needed.
    #[cfg(all(feature = "tokio-runtime", not(target_arch = "wasm32")))]
    let owned_config_with_cancel_token;
    #[cfg(all(feature = "tokio-runtime", not(target_arch = "wasm32")))]
    let config: &ExtractionConfig = if config.extraction_timeout_secs.is_some() && config.cancel_token.is_none() {
        let mut owned = config.clone();
        owned.ensure_cancel_token();
        owned_config_with_cancel_token = owned;
        &owned_config_with_cancel_token
    } else {
        config
    };

    let extraction_future = Box::pin(async {
        let ocr_disabled = config.effective_disable_ocr();
        let checks = FileDetectionChecks {
            force_ocr_conflict: config.force_ocr && ocr_disabled,
            scanned_pages_ocr_conflict: matches!(
                config.ocr_strategy,
                crate::core::config::OcrStrategy::ScannedPages { .. }
            ) && ocr_disabled,
        };
        let detected_mime = detect_file_mime(path, mime_type, config.mime_detection_policy, checks).await?;

        #[cfg(not(feature = "office"))]
        match detected_mime.as_str() {
            LEGACY_WORD_MIME_TYPE => {
                return Err(XbergError::UnsupportedFormat(
                    "Legacy Word extraction requires the `office` feature".to_string(),
                ));
            }
            LEGACY_POWERPOINT_MIME_TYPE => {
                return Err(XbergError::UnsupportedFormat(
                    "Legacy PowerPoint extraction requires the `office` feature".to_string(),
                ));
            }
            _ => {}
        }

        #[cfg(feature = "office")]
        {
            let _ = LEGACY_WORD_MIME_TYPE;
            let _ = LEGACY_POWERPOINT_MIME_TYPE;
        }

        Box::pin(extract_file_with_extractor(path, &detected_mime, config)).await
    });

    // without a JS/WASI shim), which aborts the whole module with an uncatchable
    // `unreachable` trap. `tokio-runtime` can be enabled transitively on that target
    // (e.g. by `layout-tract` inside `wasm-target`), so gating on the feature alone is
    // not enough — explicitly exclude wasm32 here, matching `run_timed_extraction` in
    #[cfg(all(feature = "tokio-runtime", not(target_arch = "wasm32")))]
    let result = if let Some(secs) = config.extraction_timeout_secs {
        let start = std::time::Instant::now();
        match tokio::time::timeout(std::time::Duration::from_secs(secs), extraction_future).await {
            Ok(inner) => inner,
            Err(_elapsed) => {
                if let Some(ref token) = config.cancel_token {
                    token.cancel();
                }
                Err(crate::XbergError::Timeout {
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    limit_ms: secs * 1000,
                })
            }
        }
    } else {
        extraction_future.await
    };

    #[cfg(any(not(feature = "tokio-runtime"), target_arch = "wasm32"))]
    let result = {
        // Without a usable tokio timer (no 'tokio-runtime' feature, or the WASM build,
        // where `std::time::Instant::now()` panics) there is no timer to enforce a
        // timeout, but the default ExtractionConfig sets extraction_timeout_secs, so
        // erroring here would reject every default call. Ignore the unenforceable
        // limit and run the extraction instead. ~keep
        if config.extraction_timeout_secs.is_some() {
            tracing::debug!(
                "extraction_timeout_secs is ignored on this target (no usable tokio timer); running without a timeout"
            );
        }
        extraction_future.await
    };

    #[cfg(feature = "otel")]
    if let Err(ref e) = result {
        crate::telemetry::spans::record_error_on_current_span(e);
    }

    result
}

/// Fill `ExtractionConfig::source_name` from the input path when the field is
/// unset.
///
/// (fork) B-D1 / TRANSCRIPTION.md: the file path knows the input's real file
/// name, and the transcription extractor renders its SV-06 `# <file>` header
/// from `source_name` — the same `path.file_name()` source the worker
/// `transcribe` command uses, so both paths title their output identically.
/// Only a `None` field is filled; an explicit `source_name` (engine bytes
/// path, downloaded documents, API callers) always wins. A path without a
/// UTF-8 file name leaves the field unset. Runs before
/// `hash_extraction_config` so the extraction-cache key follows the
/// output-affecting name (`source_name` is deliberately hashed — see
/// `source_name_changes_the_cache_key`).
fn fill_source_name_from_path<'a>(
    config: std::borrow::Cow<'a, ExtractionConfig>,
    path: &Path,
) -> std::borrow::Cow<'a, ExtractionConfig> {
    if config.source_name.is_some() {
        return config;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return config;
    };
    let mut owned = config.into_owned();
    owned.source_name = Some(name.to_string());
    std::borrow::Cow::Owned(owned)
}

pub(in crate::core::extractor) async fn extract_file_with_extractor(
    path: &Path,
    mime_type: &str,
    config: &ExtractionConfig,
) -> Result<ExtractedDocument> {
    let config = config.normalized();
    let config = fill_source_name_from_path(config, path);
    let config = config.as_ref();

    if !config.use_cache || config.cache_ttl_secs == Some(0) {
        return extract_file_uncached(path, mime_type, config).await;
    }

    let content_hash = crate::cache::blake3_hash_file(path)?;
    let config_hash = hash_extraction_config(config, mime_type);
    let cache_key = format!("{content_hash}_{config_hash}");

    let namespace = config.cache_namespace.as_deref();

    if let Some(cache) = get_extraction_cache()
        && let Ok(Some(data)) = cache.get(&cache_key, path.to_str(), namespace, config.cache_ttl_secs)
    {
        match deserialize_extraction_cache_entry(&data) {
            Ok(entry) => {
                let (post_processor_generation, validator_generation) = lifecycle_registry_generations();
                if entry.post_processor_generation == post_processor_generation
                    && entry.validator_generation == validator_generation
                {
                    tracing::debug!(cache_key = %cache_key, "Extraction cache hit");
                    return Ok(entry.result);
                }
                tracing::debug!(cache_key = %cache_key, "Extraction cache entry has stale plugin lifecycle state");
            }
            Err(error) => {
                tracing::warn!(
                    { crate::telemetry::conventions::OPERATION } =
                        crate::telemetry::conventions::operations::CACHE_LOOKUP,
                    { crate::telemetry::conventions::CACHE_KEY } = cache_key.as_str(),
                    error = %error,
                    "Failed to decode extraction cache entry; re-extracting and replacing it"
                );
            }
        }
    }

    let lifecycle_generations_before = lifecycle_registry_generations();
    let result = Box::pin(extract_file_uncached(path, mime_type, config)).await?;
    let lifecycle_generations_after = lifecycle_registry_generations();

    if lifecycle_generations_before == lifecycle_generations_after
        && let Some(cache) = get_extraction_cache()
    {
        match serialize_extraction_cache_entry(&result, lifecycle_generations_after) {
            Ok(data) => {
                let _ = cache.set(&cache_key, data, path.to_str(), namespace, config.cache_ttl_secs);
            }
            Err(error) => {
                tracing::warn!(
                    { crate::telemetry::conventions::OPERATION } =
                        crate::telemetry::conventions::operations::CACHE_WRITE,
                    { crate::telemetry::conventions::CACHE_KEY } = cache_key.as_str(),
                    error = %error,
                    "Failed to encode extraction cache entry; the result will not be cached"
                );
            }
        }
    }

    Ok(result)
}

#[derive(serde::Deserialize)]
struct ExtractionCacheEntry {
    result: ExtractedDocument,
    post_processor_generation: u64,
    validator_generation: u64,
}

#[derive(serde::Serialize)]
struct ExtractionCacheEntryRef<'a> {
    result: &'a ExtractedDocument,
    post_processor_generation: u64,
    validator_generation: u64,
}

fn lifecycle_registry_generations() -> (u64, u64) {
    let post_processor_generation = crate::plugins::registry::get_post_processor_registry()
        .read()
        .generation();
    let validator_generation = crate::plugins::registry::get_validator_registry().read().generation();
    (post_processor_generation, validator_generation)
}

fn serialize_extraction_cache_entry(
    result: &ExtractedDocument,
    (post_processor_generation, validator_generation): (u64, u64),
) -> std::result::Result<Vec<u8>, rmp_serde::encode::Error> {
    // Named fields preserve internally tagged document nodes and keep the lifecycle generations alongside
    // the result. Schema-version changes make earlier bare payloads unreachable. ~keep
    rmp_serde::to_vec_named(&ExtractionCacheEntryRef {
        result,
        post_processor_generation,
        validator_generation,
    })
}

fn deserialize_extraction_cache_entry(
    data: &[u8],
) -> std::result::Result<ExtractionCacheEntry, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

/// Whether an extractor failure is eligible for the extractor fallback chain (#217).
///
/// Only failures indicating that *this* extractor could not handle the input in a
/// way another registered extractor for the same MIME type plausibly could are
/// eligible:
///
/// - [`XbergError::UnsupportedFormat`] — the extractor determined, past MIME-based
///   selection, that it does not actually support this content (e.g. a container
///   format that only handles some of its own sub-variants).
/// - [`XbergError::Plugin`] — a third-party extractor's own reported failure. That
///   is a property of the plugin, not necessarily of the document.
///
/// Every other variant is treated as a hard failure and is *not* retried with a
/// lower-priority extractor. In particular [`XbergError::Parsing`] — the variant
/// an encrypted file or a corrupt archive surfaces as — means the document itself
/// is the problem: every other extractor registered for the same MIME type would
/// almost certainly fail identically, so cascading through them would only add
/// latency before producing a confusing final error (e.g. a generic archive
/// extractor's error swallowing the specific "wrong password" message from the
/// primary one).
fn is_extractor_fallback_eligible(error: &XbergError) -> bool {
    matches!(error, XbergError::UnsupportedFormat(_) | XbergError::Plugin { .. })
}

/// Extract without caching logic.
///
/// Fetches extractor candidates for `mime_type` from the process-global
/// [`crate::plugins::registry::DocumentExtractorRegistry`] and delegates the
/// dispatch/fallback logic to [`extract_with_candidates`].
// (fork) perf-tracing：文件路径抽取阶段 span（MIME 探测 + 读文件 + 格式抽取 + pipeline）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(
        target = "perf",
        name = "extract_file",
        skip_all,
        fields(file = %path.display(), mime = mime_type)
    )
)]
async fn extract_file_uncached(path: &Path, mime_type: &str, config: &ExtractionConfig) -> Result<ExtractedDocument> {
    crate::core::config::concurrency::init_thread_pools(config.concurrency.as_ref());

    crate::extractors::ensure_initialized()?;

    // Probe only cache misses, inside the existing timeout/cancellation scope.
    // The caller's config remains the cache identity; the effective fast config
    // is request-local and reaches both extract_path and the processing pipeline.
    let threshold = config.auto_fast_pages;
    #[cfg(all(feature = "tokio-runtime", not(target_arch = "wasm32")))]
    let auto_downgrade = if threshold == 0 {
        None
    } else {
        let owned_path = path.to_path_buf();
        tokio::task::spawn_blocking(move || crate::core::auto_mode::evaluate_file(&owned_path, threshold))
            .await
            .ok()
            .flatten()
    };
    #[cfg(any(not(feature = "tokio-runtime"), target_arch = "wasm32"))]
    let auto_downgrade = crate::core::auto_mode::evaluate_file(path, threshold);
    let auto_config;
    let config = if auto_downgrade.is_some() {
        let mut owned = config.clone();
        owned.disable_expensive_document_processing();
        auto_config = owned;
        &auto_config
    } else {
        config
    };

    let candidates = {
        let registry = crate::plugins::registry::get_document_extractor_registry();
        let registry_read = registry.read();
        registry_read.get_candidates(path, mime_type)
    };

    let mut result = extract_with_candidates(path, mime_type, config, candidates).await?;
    if let Some(decision) = auto_downgrade {
        crate::core::diagnostics::push_warning(
            &mut result.processing_warnings,
            "auto_mode",
            decision.warning_message(),
        );
    }
    Ok(result)
}

/// Tries every extractor `candidates` in order (highest priority first,
/// see [`crate::plugins::registry::DocumentExtractorRegistry::get_candidates`]),
/// falling back to the next candidate only when the failure is
/// [fallback-eligible](is_extractor_fallback_eligible) (#217). A
/// `ProcessingWarning` records which extractor ultimately ran and why whenever a
/// higher-priority extractor was tried and failed first.
///
/// Parameterized on `candidates` rather than fetching them itself so callers
/// (in particular tests) can supply candidates from a local registry instead
/// of the process-global one, without racing concurrently running extraction
/// paths that self-heal the global registry only when it is observed
/// completely empty (see `crate::extractors::ensure_initialized`).
pub(crate) async fn extract_with_candidates(
    path: &Path,
    mime_type: &str,
    config: &ExtractionConfig,
    candidates: Vec<RegisteredDocumentExtractor>,
) -> Result<ExtractedDocument> {
    if candidates.is_empty() {
        return Err(XbergError::UnsupportedFormat(mime_type.to_string()));
    }

    let candidate_count = candidates.len();
    let mut last_error = None;

    for (index, candidate) in candidates.into_iter().enumerate() {
        // The extraction stage span wraps only the extractor invocation — post-processing
        // is covered by `run_pipeline` below and must not be nested inside it.
        // (fork) perf-tracing 优先：perf 构建只产出 perf span；otel 构建让位（两者叠用
        // 会让同一调用点出现双层 stage span，语义重复）。
        #[cfg(feature = "perf-tracing")]
        let extraction = {
            let stage_span = tracing::info_span!(
                target: "perf",
                "format_extract",
                format = candidate.plugin().name(),
                mime = mime_type
            );
            candidate
                .extract_path(path, mime_type, config)
                .instrument(stage_span)
                .await
        };
        #[cfg(all(feature = "otel", not(feature = "perf-tracing")))]
        let extraction = {
            let stage_span = crate::telemetry::spans::extraction_stage_span(
                candidate.plugin().name(),
                candidate.plugin().priority(),
            );
            candidate
                .extract_path(path, mime_type, config)
                .instrument(stage_span)
                .await
        };
        #[cfg(all(not(feature = "otel"), not(feature = "perf-tracing")))]
        let extraction = candidate.extract_path(path, mime_type, config).await;

        match extraction {
            Ok(mut doc) => {
                ensure_builtin_extraction_method(&mut doc, candidate.is_builtin());
                if index > 0 {
                    let name = candidate.plugin().name();
                    crate::core::diagnostics::push_warning(
                        &mut doc.processing_warnings,
                        "extractor-fallback",
                        format!(
                            "extractor '{name}' handled this document for MIME type '{mime_type}' after \
                             {index} higher-priority extractor(s) failed"
                        ),
                    );
                }
                let result = Box::pin(crate::core::pipeline::run_pipeline(doc, config)).await?;
                return Ok(result);
            }
            Err(e) if index + 1 < candidate_count && is_extractor_fallback_eligible(&e) => {
                tracing::debug!(
                    "Extractor '{}' failed for MIME type '{}' with a fallback-eligible error, \
                     trying the next candidate: {}",
                    candidate.plugin().name(),
                    mime_type,
                    e
                );
                last_error = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_error.unwrap_or_else(|| XbergError::UnsupportedFormat(mime_type.to_string())))
}

#[cfg(all(test, feature = "tokio-runtime", not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::cache::GenericCache;
    use crate::types::{ContentLayer, DocumentNode, DocumentStructure, GridCell, NodeContent, Table, TableGrid};
    use std::borrow::Cow;
    use tempfile::tempdir;

    fn structured_table_document() -> ExtractedDocument {
        let grid = TableGrid {
            rows: 2,
            cols: 2,
            cells: vec![GridCell {
                content: "Name".to_string(),
                row: 0,
                col: 0,
                row_span: 1,
                col_span: 1,
                is_header: true,
                bbox: None,
                heading_level: None,
                style_name: None,
            }],
        };

        ExtractedDocument {
            content: "| Name | Age |\n| --- | --- |\n| Ada | 36 |".to_string(),
            mime_type: Cow::Borrowed("application/pdf"),
            tables: vec![Table {
                cells: vec![vec!["Name".to_string(), "Age".to_string()]],
                markdown: "| Name | Age |".to_string(),
                page_number: 1,
                ..Default::default()
            }],
            document: Some(DocumentStructure {
                nodes: vec![DocumentNode {
                    id: "table-0".to_string(),
                    content: NodeContent::Table { grid },
                    parent: None,
                    children: Vec::new(),
                    content_layer: ContentLayer::Body,
                    page: Some(1),
                    page_end: None,
                    bbox: None,
                    annotations: Vec::new(),
                    attributes: None,
                }],
                source_format: Some("pdf".to_string()),
                relationships: Vec::new(),
                node_types: vec!["table".to_string()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn should_read_structured_table_document_from_written_extraction_cache_entry() {
        let directory = tempdir().unwrap();
        let cache = GenericCache::new(
            "extraction-regression".to_string(),
            Some(directory.path().to_string_lossy().into_owned()),
            1.0,
            10.0,
            0.0,
        )
        .unwrap();
        let expected = structured_table_document();
        let encoded = serialize_extraction_cache_entry(&expected, lifecycle_registry_generations()).unwrap();
        cache.set_default("structured-table", encoded, None).unwrap();

        let stored = cache
            .get_default("structured-table", None)
            .unwrap()
            .expect("the disk-backed cache entry should exist");
        let actual = deserialize_extraction_cache_entry(&stored)
            .expect("a written structured extraction result should be a cache hit");

        assert_eq!(actual.result.content, expected.content);
        assert_eq!(actual.result.document, expected.document);
        assert_eq!(actual.result.tables[0].cells, expected.tables[0].cells);
    }

    #[test]
    fn should_reject_compact_entry_and_read_named_replacement() {
        let directory = tempdir().unwrap();
        let cache = GenericCache::new(
            "extraction-migration".to_string(),
            Some(directory.path().to_string_lossy().into_owned()),
            1.0,
            10.0,
            0.0,
        )
        .unwrap();
        let expected = ExtractedDocument {
            content: "legacy compact entry".to_string(),
            mime_type: Cow::Borrowed("text/plain"),
            ..Default::default()
        };
        cache
            .set_default("legacy-compact", rmp_serde::to_vec(&expected).unwrap(), None)
            .unwrap();

        let legacy = cache.get_default("legacy-compact", None).unwrap().unwrap();
        assert!(deserialize_extraction_cache_entry(&legacy).is_err());

        cache
            .set_default(
                "legacy-compact",
                serialize_extraction_cache_entry(&expected, lifecycle_registry_generations()).unwrap(),
                None,
            )
            .unwrap();
        let replacement = cache.get_default("legacy-compact", None).unwrap().unwrap();
        let actual = deserialize_extraction_cache_entry(&replacement).unwrap();

        assert_eq!(actual.result.content, expected.content);
        assert_eq!(actual.result.mime_type, expected.mime_type);
    }

    #[tokio::test]
    async fn should_reject_non_regular_file_before_extraction() {
        let directory = tempdir().unwrap();
        let config = ExtractionConfig::default();

        let error = extract_file(directory.path(), Some("text/plain"), &config)
            .await
            .expect_err("a directory is not a document file");

        assert!(matches!(
            error,
            XbergError::Validation { message, .. } if message == "Extraction input must be a regular file"
        ));
    }

    // FORK.md：省略 images 仍须交付可预览 PNG，显式 native 保留源字节。
    #[cfg(all(feature = "ocr", feature = "image-encode"))]
    #[tokio::test]
    async fn public_image_defaults_encode_png_and_preserve_explicit_native() {
        use crate::core::config::extraction::{ImageExtractionConfig, ImageOutputFormat};
        let directory = tempdir().unwrap();
        let path = directory.path().join("image.jb2");
        let source = include_bytes!("../../extraction/image/fixtures/synthetic-valid.jb2");
        std::fs::write(&path, source).unwrap();
        let config = ExtractionConfig {
            disable_ocr: true,
            use_cache: false,
            ..Default::default()
        };
        let output = crate::extract(crate::ExtractInput::from_uri(path.to_string_lossy()), &config)
            .await
            .unwrap();
        let document = &output.results[0];
        let image = &document.images.as_ref().unwrap()[0];
        assert_eq!(image.format.as_ref(), "png");
        assert!(document.content.contains("](image_0.png)"), "{}", document.content);
        let expected = image::load_from_memory(include_bytes!("../../extraction/image/fixtures/synthetic-valid.png"))
            .unwrap()
            .to_luma8();
        assert_eq!(image::load_from_memory(&image.data).unwrap().to_luma8(), expected);
        assert!(
            document.processing_warnings.is_empty(),
            "{:?}",
            document.processing_warnings
        );

        let native = ExtractionConfig {
            images: Some(ImageExtractionConfig {
                output_format: ImageOutputFormat::Native,
                ..Default::default()
            }),
            ..config
        };
        let output = crate::extract(crate::ExtractInput::from_uri(path.to_string_lossy()), &native)
            .await
            .unwrap();
        let image = &output.results[0].images.as_ref().unwrap()[0];
        assert_eq!(image.data.as_ref(), source);
        assert!(output.results[0].content.contains("](image_0.JBIG2)"));
    }

    #[tokio::test]
    async fn should_report_missing_file_before_invalid_ocr_configuration() {
        let directory = tempdir().unwrap();
        let missing_file = directory.path().join("missing.txt");
        let config = ExtractionConfig {
            force_ocr: true,
            disable_ocr: true,
            ..Default::default()
        };

        let error = extract_file(&missing_file, None, &config)
            .await
            .expect_err("a missing file must fail before OCR configuration validation");

        assert!(matches!(
            error,
            XbergError::Io(source) if source.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(any(feature = "pdf", feature = "office"))]
    async fn public_file_extract(path: &Path, config: &ExtractionConfig) -> Result<ExtractedDocument> {
        let output = crate::extract(crate::ExtractInput::from_uri(path.to_string_lossy()), config).await?;
        assert_eq!(output.results.len(), 1);
        Ok(output.results.into_iter().next().unwrap())
    }

    #[cfg(any(feature = "pdf", feature = "office"))]
    fn assert_auto_mode_count(document: &ExtractedDocument, expected: usize) {
        assert_eq!(
            document
                .processing_warnings
                .iter()
                .filter(|warning| warning.source == "auto_mode")
                .count(),
            expected,
            "{:?}",
            document.processing_warnings
        );
    }

    #[cfg(feature = "pdf")]
    fn write_auto_mode_pdf(path: &Path, count: u32) {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut pdf = Document::with_version("1.4");
        let pages_id = pdf.new_object_id();
        let font_id = pdf.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content_id = pdf.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 12 Tf 20 100 Td (AUTO MODE native document text remains readable without any OCR model or network dependency.) Tj ET".to_vec(),
        ));
        let kids: Vec<Object> = (0..count)
            .map(|_| {
                pdf.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 600.into(), 200.into()],
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
                    "Contents" => content_id,
                })
                .into()
            })
            .collect();
        pdf.objects.insert(
            pages_id,
            dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }.into(),
        );
        let catalog_id = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        pdf.trailer.set("Root", catalog_id);
        pdf.save(path).unwrap();
    }

    #[cfg(all(feature = "pdf", feature = "ocr", not(sceptre_ocr)))]
    #[tokio::test]
    async fn public_file_auto_mode_disables_ocr_caches_one_warning_and_preserves_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.pdf");
        write_auto_mode_pdf(&path, 501);
        // The pipeline registers its post-processors lazily on the first
        // extraction, which bumps the plugin lifecycle generations; while they
        // move, the extraction-cache write is deliberately skipped so a
        // mixed-lifecycle result is never stored. Warm the registries with a
        // throwaway uncached extraction first, the way every long-lived caller
        // settles after its first document, so the cached flow below observes
        // stable generations.
        let warmup = ExtractionConfig {
            use_cache: false,
            ..Default::default()
        };
        let warmed = public_file_extract(&path, &warmup).await.unwrap();
        assert!(warmed.content.contains("AUTO MODE native document text"));
        let namespace = format!("auto-mode-{}", dir.path().file_name().unwrap().to_string_lossy());
        let config = ExtractionConfig {
            force_ocr: true,
            ocr: Some(crate::core::config::OcrConfig {
                // A valid backend name, deliberately not compiled into this
                // feature set: validation passes, real OCR resolution must fail.
                backend: "sceptre".to_string(),
                ..Default::default()
            }),
            cache_namespace: Some(namespace),
            ..Default::default()
        };
        let first = public_file_extract(&path, &config).await.unwrap();
        assert!(first.content.contains("AUTO MODE native document text"));
        assert_auto_mode_count(&first, 1);
        assert!(first.processing_warnings.iter().any(
            |warning| warning.source == "auto_mode" && warning.message.contains("501 pages > auto_fast_pages=500")
        ));
        assert_eq!(
            first
                .metadata
                .additional
                .get("extraction_method")
                .and_then(serde_json::Value::as_str),
            Some("native")
        );

        // Inspect the real disk cache before the second public call, so equal
        // results alone cannot disguise two uncached extractions.
        let normalized = fill_source_name_from_path(config.normalized(), &path);
        let cache_key = format!(
            "{}_{}",
            crate::cache::blake3_hash_file(&path).unwrap(),
            hash_extraction_config(normalized.as_ref(), "application/pdf")
        );
        // Concurrent tests register plugins while this extraction runs; the
        // cache write is deliberately skipped while lifecycle generations move.
        // Retry until a stable-lifecycle window caches the entry, which is the
        // production norm for any long-lived caller.
        let cache = get_extraction_cache().unwrap();
        let mut stored = None;
        for _ in 0..10 {
            stored = cache
                .get(
                    &cache_key,
                    path.to_str(),
                    config.cache_namespace.as_deref(),
                    config.cache_ttl_secs,
                )
                .unwrap();
            if stored.is_some() {
                break;
            }
            let _ = public_file_extract(&path, &config).await.unwrap();
        }
        let stored =
            stored.expect("the downgraded result must be cached under the original request once generations settle");
        assert_auto_mode_count(&deserialize_extraction_cache_entry(&stored).unwrap().result, 1);
        let cached = public_file_extract(&path, &config).await.unwrap();
        assert_auto_mode_count(&cached, 1);
        assert_eq!(cached.content, first.content);
        assert!(config.force_ocr);
        assert!(!config.disable_ocr);
        assert!(config.ocr.as_ref().unwrap().enabled);
        assert_eq!(config.auto_fast_pages, 500);

        let small = dir.path().join("small.pdf");
        write_auto_mode_pdf(&small, 1);
        let error = public_file_extract(&small, &config)
            .await
            .expect_err("the next request must still invoke the deliberately unavailable OCR backend");
        assert!(error.to_string().contains("sceptre"), "{error}");
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn public_file_auto_mode_keeps_threshold_and_explicit_normal_requests_normal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("threshold.pdf");
        write_auto_mode_pdf(&path, 500);
        let config = ExtractionConfig {
            use_cache: false,
            ..Default::default()
        };
        let threshold = public_file_extract(&path, &config).await.unwrap();
        assert!(threshold.content.contains("AUTO MODE native document text"));
        assert_auto_mode_count(&threshold, 0);

        write_auto_mode_pdf(&path, 501);
        let normal = ExtractionConfig {
            auto_fast_pages: 0,
            ..config
        };
        let explicit = public_file_extract(&path, &normal).await.unwrap();
        assert!(explicit.content.contains("AUTO MODE native document text"));
        assert_auto_mode_count(&explicit, 0);
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn public_file_auto_mode_leaves_broken_pdf_probe_to_native_recovery() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recoverable.pdf");
        write_auto_mode_pdf(&path, 3);
        let mut broken = std::fs::read(&path).unwrap();
        let startxref = broken.windows(9).position(|bytes| bytes == b"startxref").unwrap();
        broken.truncate(startxref);
        std::fs::write(&path, broken).unwrap();
        let config = ExtractionConfig {
            auto_fast_pages: 1,
            use_cache: false,
            ..Default::default()
        };
        let document = public_file_extract(&path, &config).await.unwrap();
        assert!(document.content.contains("AUTO MODE native document text"));
        assert_auto_mode_count(&document, 0);
    }

    #[cfg(feature = "office")]
    #[tokio::test]
    async fn public_file_auto_mode_keeps_unknown_and_bad_ooxml_page_probes_normal() {
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir.path().join("unknown.docx");
        let config = ExtractionConfig {
            use_cache: false,
            auto_fast_pages: 1,
            ..Default::default()
        };
        for properties in ["<Properties/>", "<Properties><Pages>not-a-number</Pages></Properties>"] {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                (
                    "[Content_Types].xml",
                    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
                ),
                (
                    "word/document.xml",
                    r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>OOXML native content survives an unknown page count.</w:t></w:r></w:p></w:body></w:document>"#,
                ),
                ("docProps/app.xml", properties),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
            let document = public_file_extract(&path, &config).await.unwrap();
            assert!(document.content.contains("OOXML native content survives"));
            assert_auto_mode_count(&document, 0);
        }
        let text = dir.path().join("unknown.txt");
        std::fs::write(&text, "Plain text survives without a page-count probe.").unwrap();
        let document = public_file_extract(&text, &config).await.unwrap();
        assert!(document.content.contains("Plain text survives"));
        assert_auto_mode_count(&document, 0);
    }
}

/// (fork) B-D1: `fill_source_name_from_path` only fills an *unset*
/// `source_name` from the input path; explicit values and paths without a
/// file name are left untouched.
#[cfg(test)]
mod source_name_tests {
    use super::fill_source_name_from_path;
    use crate::core::config::ExtractionConfig;
    use std::borrow::Cow;
    use std::path::Path;

    #[test]
    fn fills_unset_source_name_with_the_file_name() {
        let config = ExtractionConfig::default();
        let filled = fill_source_name_from_path(Cow::Borrowed(&config), Path::new("media/meeting-01.mp3"));
        assert_eq!(filled.source_name.as_deref(), Some("meeting-01.mp3"));
    }

    #[test]
    fn keeps_an_explicitly_configured_source_name() {
        let config = ExtractionConfig {
            source_name: Some("explicit-name.wav".to_string()),
            ..Default::default()
        };
        let filled = fill_source_name_from_path(Cow::Borrowed(&config), Path::new("other.wav"));
        assert_eq!(filled.source_name.as_deref(), Some("explicit-name.wav"));
    }

    #[test]
    fn leaves_source_name_unset_when_the_path_has_no_file_name() {
        let config = ExtractionConfig::default();
        let filled = fill_source_name_from_path(Cow::Borrowed(&config), Path::new(".."));
        assert!(filled.source_name.is_none());
    }
}

/// Clear the cache-control field on an `LlmConfig` before it is folded into the
/// extraction-cache key.
///
/// `LlmConfig::cache` configures liter-llm's own response cache — whether *it* caches,
/// not what the extraction produces — so it must be excluded from the key exactly like
/// `ocr.tesseract_config.use_cache` (see [`hash_extraction_config`]). `LlmConfig` is
/// embedded in `ExtractionConfig` at ten separate places (VLM OCR direct and pipeline-stage
/// configs, structured extraction, NER, summarization, translation, page classification,
/// chunk classification, captioning, and chunking's LLM-routed embedding model), so this is
/// centralized here rather than repeated at each call site.
fn normalize_llm_config_for_cache_key(llm: &mut crate::core::config::LlmConfig) {
    llm.cache = None;
}

/// Hash ExtractionConfig fields that affect extraction output.
///
/// Excludes cache-control fields (use_cache, cache_namespace, cache_ttl_secs,
/// the nested `ocr.tesseract_config.use_cache`, and every embedded `LlmConfig::cache` —
/// see [`normalize_llm_config_for_cache_key`]) since they don't affect the extraction
/// result. Uses a clone-and-normalize approach to ensure determinism: cache fields are
/// zeroed, then the struct is serialized to canonical JSON via serde_json's sorted-keys
/// representation.
fn hash_extraction_config(config: &ExtractionConfig, mime_type: &str) -> String {
    let mut normalized = config.clone();
    normalized.use_cache = true;
    normalized.cache_namespace = None;
    normalized.cache_ttl_secs = None;
    if let Some(ocr) = normalized.ocr.as_mut() {
        if let Some(tesseract_config) = ocr.tesseract_config.as_mut() {
            tesseract_config.use_cache = true;
        }
        if let Some(vlm_config) = ocr.vlm_config.as_mut() {
            normalize_llm_config_for_cache_key(vlm_config);
        }
        if let Some(pipeline) = ocr.pipeline.as_mut() {
            for stage in &mut pipeline.stages {
                if let Some(vlm_config) = stage.vlm_config.as_mut() {
                    normalize_llm_config_for_cache_key(vlm_config);
                }
            }
        }
    }
    if let Some(structured_extraction) = normalized.structured_extraction.as_mut() {
        normalize_llm_config_for_cache_key(&mut structured_extraction.llm);
    }
    if let Some(ner) = normalized.ner.as_mut()
        && let Some(llm) = ner.llm.as_mut()
    {
        normalize_llm_config_for_cache_key(llm);
    }
    if let Some(summarization) = normalized.summarization.as_mut()
        && let Some(llm) = summarization.llm.as_mut()
    {
        normalize_llm_config_for_cache_key(llm);
    }
    if let Some(translation) = normalized.translation.as_mut() {
        normalize_llm_config_for_cache_key(&mut translation.llm);
    }
    if let Some(page_classification) = normalized.page_classification.as_mut() {
        normalize_llm_config_for_cache_key(&mut page_classification.llm);
    }
    if let Some(chunk_classification) = normalized.chunk_classification.as_mut() {
        normalize_llm_config_for_cache_key(&mut chunk_classification.llm);
    }
    if let Some(captioning) = normalized.captioning.as_mut() {
        normalize_llm_config_for_cache_key(&mut captioning.llm);
    }
    if let Some(chunking) = normalized.chunking.as_mut()
        && let Some(embedding) = chunking.embedding.as_mut()
        && let crate::core::config::EmbeddingModelType::Llm { llm } = &mut embedding.model
    {
        normalize_llm_config_for_cache_key(llm);
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(mime_type.as_bytes());
    // The file auto-mode warning is now part of the cached result. Do not reuse
    // pre-cutover entries produced by the same original request configuration.
    hasher.update(b"\x00file-auto-mode-v1\x00");
    if let Ok(bytes) = rmp_serde::to_vec(&normalized) {
        hasher.update(&bytes);
    }

    // `#[serde(skip)]` fields are absent from the MessagePack bytes above but DO
    hasher.update(b"\x00source_name\x00");
    if let Some(name) = normalized.source_name.as_deref() {
        hasher.update(name.as_bytes());
    }
    hasher.update(b"\x00tessdata\x00");
    if let Some(ocr) = normalized.ocr.as_ref()
        && let Some(tessdata) = ocr.tessdata_bytes.as_ref()
    {
        let mut keys: Vec<&String> = tessdata.keys().collect();
        keys.sort();
        for key in keys {
            hasher.update(key.as_bytes());
            hasher.update(&(tessdata[key].len() as u64).to_le_bytes());
            hasher.update(&tessdata[key]);
        }
    }

    let hash = hasher.finalize();
    hex::encode(&hash.as_bytes()[..16])
}

/// Get or initialize the global extraction cache.
fn get_extraction_cache() -> Option<&'static crate::cache::GenericCache> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<crate::cache::GenericCache>> = OnceLock::new();

    CACHE
        .get_or_init(|| crate::cache::GenericCache::new("extraction".to_string(), None, 30.0, 2000.0, 500.0).ok())
        .as_ref()
}

pub(in crate::core::extractor) async fn extract_bytes_with_extractor(
    content: &[u8],
    mime_type: &str,
    config: &ExtractionConfig,
) -> Result<ExtractedDocument> {
    let config = config.normalized();
    let config = config.as_ref();

    crate::core::config::concurrency::init_thread_pools(config.concurrency.as_ref());

    crate::extractors::ensure_initialized()?;

    let (extractor, is_builtin) = get_extractor(mime_type)?;

    // (fork) perf-tracing 优先：perf 构建只产出 perf span，otel 构建让位（见 extract_with_candidates）。
    #[cfg(feature = "perf-tracing")]
    let mut doc = {
        let stage_span = tracing::info_span!(
            target: "perf",
            "format_extract",
            format = extractor.name(),
            mime = mime_type
        );
        Box::pin(extractor.extract_content(content, mime_type, config))
            .instrument(stage_span)
            .await?
    };
    #[cfg(all(feature = "otel", not(feature = "perf-tracing")))]
    let mut doc = {
        let stage_span = crate::telemetry::spans::extraction_stage_span(extractor.name(), extractor.priority());
        Box::pin(extractor.extract_content(content, mime_type, config))
            .instrument(stage_span)
            .await?
    };
    #[cfg(all(not(feature = "otel"), not(feature = "perf-tracing")))]
    let mut doc = Box::pin(extractor.extract_content(content, mime_type, config)).await?;

    ensure_builtin_extraction_method(&mut doc, is_builtin);

    let result = Box::pin(crate::core::pipeline::run_pipeline(doc, config)).await?;
    Ok(result)
}

#[cfg(test)]
mod cache_key_tests {
    use super::{ensure_builtin_extraction_method, hash_extraction_config};
    use crate::core::config::ExtractionConfig;

    #[test]
    fn should_default_builtin_extraction_method_to_native() {
        let mut document = crate::types::internal::InternalDocument::new("text");

        ensure_builtin_extraction_method(&mut document, true);

        assert_eq!(
            document.metadata.additional.get("extraction_method"),
            Some(&serde_json::Value::String("native".to_string()))
        );
    }

    #[test]
    fn should_leave_custom_plugin_extraction_method_unspecified() {
        let mut document = crate::types::internal::InternalDocument::new("custom");

        ensure_builtin_extraction_method(&mut document, false);

        assert!(!document.metadata.additional.contains_key("extraction_method"));
    }

    #[test]
    fn should_preserve_recognized_builtin_extraction_method() {
        let mut document = crate::types::internal::InternalDocument::new("pdf");
        document.metadata.additional.insert(
            std::borrow::Cow::Borrowed("extraction_method"),
            serde_json::Value::String("mixed".to_string()),
        );

        ensure_builtin_extraction_method(&mut document, true);

        assert_eq!(
            document.metadata.additional.get("extraction_method"),
            Some(&serde_json::Value::String("mixed".to_string()))
        );
    }

    #[test]
    fn should_replace_unrecognized_builtin_extraction_method_with_native() {
        let mut document = crate::types::internal::InternalDocument::new("doc");
        document.metadata.additional.insert(
            std::borrow::Cow::Borrowed("extraction_method"),
            serde_json::Value::String("native_ole".to_string()),
        );

        ensure_builtin_extraction_method(&mut document, true);

        assert_eq!(
            document.metadata.additional.get("extraction_method"),
            Some(&serde_json::Value::String("native".to_string()))
        );
    }

    #[test]
    fn source_name_changes_the_cache_key() {
        let a = ExtractionConfig {
            source_name: Some("snippet.py".to_string()),
            ..Default::default()
        };
        let b = ExtractionConfig {
            source_name: Some("snippet.rb".to_string()),
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&a, "text/x-source-code"),
            hash_extraction_config(&b, "text/x-source-code"),
            "source_name (serde-skipped) must be part of the cache key"
        );
    }

    #[test]
    #[cfg(feature = "ocr")]
    fn tessdata_bytes_changes_the_cache_key() {
        use crate::core::config::OcrConfig;
        use std::collections::HashMap;

        let mut eng = HashMap::new();
        eng.insert("eng".to_string(), vec![1u8, 2, 3]);
        let mut deu = HashMap::new();
        deu.insert("eng".to_string(), vec![9u8, 9, 9]);

        let a = ExtractionConfig {
            ocr: Some(OcrConfig {
                tessdata_bytes: Some(eng),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        let b = ExtractionConfig {
            ocr: Some(OcrConfig {
                tessdata_bytes: Some(deu),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&a, "image/png"),
            hash_extraction_config(&b, "image/png"),
            "tessdata_bytes (serde-skipped) must be part of the cache key"
        );
    }

    #[test]
    #[cfg(feature = "ocr")]
    fn tesseract_use_cache_does_not_change_the_cache_key() {
        use crate::core::config::OcrConfig;
        use crate::types::TesseractConfig;

        let cache_on = ExtractionConfig {
            ocr: Some(OcrConfig {
                tesseract_config: Some(TesseractConfig {
                    use_cache: true,
                    ..TesseractConfig::default()
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        let cache_off = ExtractionConfig {
            ocr: Some(OcrConfig {
                tesseract_config: Some(TesseractConfig {
                    use_cache: false,
                    ..TesseractConfig::default()
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&cache_on, "image/png"),
            hash_extraction_config(&cache_off, "image/png"),
            "ocr.tesseract_config.use_cache is a cache-control field and must not affect the \
             extraction-cache key (#693)"
        );
    }

    #[test]
    #[cfg(feature = "ocr")]
    fn tesseract_psm_changes_the_cache_key() {
        use crate::core::config::OcrConfig;
        use crate::types::TesseractConfig;

        let psm_auto = ExtractionConfig {
            ocr: Some(OcrConfig {
                tesseract_config: Some(TesseractConfig {
                    psm: Some(3),
                    ..TesseractConfig::default()
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        let psm_sparse = ExtractionConfig {
            ocr: Some(OcrConfig {
                tesseract_config: Some(TesseractConfig {
                    psm: Some(11),
                    ..TesseractConfig::default()
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&psm_auto, "image/png"),
            hash_extraction_config(&psm_sparse, "image/png"),
            "psm changes Tesseract's recognized output and must be part of the cache key"
        );
    }

    #[test]
    fn ocr_strategy_changes_the_cache_key() {
        use crate::core::config::OcrStrategy;

        let auto = ExtractionConfig::default();
        let scanned = ExtractionConfig {
            ocr_strategy: OcrStrategy::ScannedPages { min_confidence: 0.7 },
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&auto, "application/pdf"),
            hash_extraction_config(&scanned, "application/pdf"),
            "ocr_strategy selects different pages for OCR and must be part of the cache key"
        );
    }

    #[test]
    fn scanned_pages_min_confidence_changes_the_cache_key() {
        use crate::core::config::OcrStrategy;

        let lenient = ExtractionConfig {
            ocr_strategy: OcrStrategy::ScannedPages { min_confidence: 0.6 },
            ..Default::default()
        };
        let strict = ExtractionConfig {
            ocr_strategy: OcrStrategy::ScannedPages { min_confidence: 0.9 },
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&lenient, "application/pdf"),
            hash_extraction_config(&strict, "application/pdf"),
            "min_confidence selects different pages for OCR and must be part of the cache key"
        );
    }

    /// Build two `LlmConfig`s that are identical except for `cache`: one with a populated
    /// `LlmCacheConfig`, one with `None`. Shared by every `*_llm_cache_does_not_change_the_cache_key`
    /// test below, one per struct in which `LlmConfig` is embedded.
    fn llm_configs_differing_only_in_cache() -> (crate::core::config::LlmConfig, crate::core::config::LlmConfig) {
        use crate::core::config::{LlmCacheConfig, LlmConfig};

        let cache_on = LlmConfig {
            model: "openai/gpt-4o".to_string(),
            cache: Some(Box::new(LlmCacheConfig {
                backend: Some("memory".to_string()),
                max_entries: Some(512),
                ..LlmCacheConfig::default()
            })),
            ..LlmConfig::default()
        };
        let cache_off = LlmConfig {
            model: "openai/gpt-4o".to_string(),
            cache: None,
            ..LlmConfig::default()
        };
        (cache_on, cache_off)
    }

    #[test]
    #[cfg(feature = "ocr")]
    fn ocr_vlm_config_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::OcrConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let a = ExtractionConfig {
            ocr: Some(OcrConfig {
                vlm_config: Some(cache_on),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        let b = ExtractionConfig {
            ocr: Some(OcrConfig {
                vlm_config: Some(cache_off),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "application/pdf"),
            hash_extraction_config(&b, "application/pdf"),
            "ocr.vlm_config.cache is a cache-control field and must not affect the extraction-cache key"
        );
    }

    #[test]
    #[cfg(feature = "ocr")]
    fn ocr_pipeline_stage_vlm_config_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::{OcrConfig, OcrPipelineConfig, OcrPipelineStage, OcrQualityThresholds};

        fn stage(vlm_config: crate::core::config::LlmConfig) -> OcrPipelineStage {
            OcrPipelineStage {
                backend: "vlm".to_string(),
                priority: 100,
                language: None,
                tesseract_config: None,
                paddle_ocr_config: None,
                paddle_ocr_settings: None,
                vlm_config: Some(vlm_config),
                backend_options: None,
            }
        }

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let a = ExtractionConfig {
            ocr: Some(OcrConfig {
                pipeline: Some(OcrPipelineConfig {
                    stages: vec![stage(cache_on)],
                    quality_thresholds: OcrQualityThresholds::default(),
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        let b = ExtractionConfig {
            ocr: Some(OcrConfig {
                pipeline: Some(OcrPipelineConfig {
                    stages: vec![stage(cache_off)],
                    quality_thresholds: OcrQualityThresholds::default(),
                }),
                ..OcrConfig::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "application/pdf"),
            hash_extraction_config(&b, "application/pdf"),
            "ocr.pipeline.stages[].vlm_config.cache is a cache-control field and must not \
             affect the extraction-cache key"
        );
    }

    #[test]
    fn structured_extraction_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::StructuredExtractionConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| StructuredExtractionConfig {
            schema: serde_json::json!({"type": "object"}),
            schema_name: StructuredExtractionConfig::default_schema_name(),
            schema_description: None,
            strict: false,
            prompt: None,
            llm,
        };
        let a = ExtractionConfig {
            structured_extraction: Some(build(cache_on)),
            ..Default::default()
        };
        let b = ExtractionConfig {
            structured_extraction: Some(build(cache_off)),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "structured_extraction.llm.cache is a cache-control field and must not affect the \
             extraction-cache key"
        );
    }

    #[test]
    fn ner_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::NerConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let a = ExtractionConfig {
            ner: Some(NerConfig {
                llm: Some(cache_on),
                ..NerConfig::default()
            }),
            ..Default::default()
        };
        let b = ExtractionConfig {
            ner: Some(NerConfig {
                llm: Some(cache_off),
                ..NerConfig::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "ner.llm.cache is a cache-control field and must not affect the extraction-cache key"
        );
    }

    #[test]
    fn summarization_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::SummarizationConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let a = ExtractionConfig {
            summarization: Some(SummarizationConfig {
                llm: Some(cache_on),
                ..SummarizationConfig::default()
            }),
            ..Default::default()
        };
        let b = ExtractionConfig {
            summarization: Some(SummarizationConfig {
                llm: Some(cache_off),
                ..SummarizationConfig::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "summarization.llm.cache is a cache-control field and must not affect the extraction-cache key"
        );
    }

    #[test]
    fn translation_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::TranslationConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| TranslationConfig {
            target_lang: "de".to_string(),
            source_lang: None,
            preserve_markup: false,
            llm,
        };
        let a = ExtractionConfig {
            translation: Some(build(cache_on)),
            ..Default::default()
        };
        let b = ExtractionConfig {
            translation: Some(build(cache_off)),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "translation.llm.cache is a cache-control field and must not affect the extraction-cache key"
        );
    }

    #[test]
    fn page_classification_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::PageClassificationConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| PageClassificationConfig {
            prompt_template: None,
            labels: vec!["invoice".to_string(), "receipt".to_string()],
            multi_label: false,
            llm,
        };
        let a = ExtractionConfig {
            page_classification: Some(build(cache_on)),
            ..Default::default()
        };
        let b = ExtractionConfig {
            page_classification: Some(build(cache_off)),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "page_classification.llm.cache is a cache-control field and must not affect the \
             extraction-cache key"
        );
    }

    #[test]
    fn chunk_classification_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::{ChunkClassificationConfig, ChunkClassificationDefinition};

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| ChunkClassificationConfig {
            prompt_template: None,
            definitions: vec![ChunkClassificationDefinition {
                label: "topic".to_string(),
                description: "the chunk's topic".to_string(),
            }],
            llm,
            batch_size: ChunkClassificationConfig::default_batch_size(),
            max_concurrency: ChunkClassificationConfig::default_max_concurrency(),
        };
        let a = ExtractionConfig {
            chunk_classification: Some(build(cache_on)),
            ..Default::default()
        };
        let b = ExtractionConfig {
            chunk_classification: Some(build(cache_off)),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "chunk_classification.llm.cache is a cache-control field and must not affect the \
             extraction-cache key"
        );
    }

    #[test]
    fn captioning_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::CaptioningConfig;

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| CaptioningConfig {
            llm,
            prompt: None,
            min_image_area: CaptioningConfig::default_min_image_area(),
            alt_text: Default::default(),
        };
        let a = ExtractionConfig {
            captioning: Some(build(cache_on)),
            ..Default::default()
        };
        let b = ExtractionConfig {
            captioning: Some(build(cache_off)),
            ..Default::default()
        };
        assert_eq!(
            hash_extraction_config(&a, "image/png"),
            hash_extraction_config(&b, "image/png"),
            "captioning.llm.cache is a cache-control field and must not affect the extraction-cache key"
        );
    }

    #[test]
    fn captioning_alt_text_mode_changes_the_cache_key() {
        use crate::core::config::{CaptionAltTextMode, CaptioningConfig};

        let build = |alt_text| ExtractionConfig {
            captioning: Some(CaptioningConfig {
                llm: crate::core::config::LlmConfig::default(),
                prompt: None,
                min_image_area: CaptioningConfig::default_min_image_area(),
                alt_text,
            }),
            ..Default::default()
        };

        assert_ne!(
            hash_extraction_config(&build(CaptionAltTextMode::Preserve), "image/png"),
            hash_extraction_config(&build(CaptionAltTextMode::Replace), "image/png"),
            "captioning alt-text precedence changes rendered output and must affect the cache key"
        );
    }

    #[test]
    fn chunking_embedding_llm_cache_does_not_change_the_cache_key() {
        use crate::core::config::{ChunkingConfig, EmbeddingConfig, EmbeddingModelType};

        let (cache_on, cache_off) = llm_configs_differing_only_in_cache();
        let build = |llm| ExtractionConfig {
            chunking: Some(ChunkingConfig {
                embedding: Some(EmbeddingConfig {
                    model: EmbeddingModelType::Llm { llm: Box::new(llm) },
                    ..EmbeddingConfig::default()
                }),
                ..ChunkingConfig::default()
            }),
            ..Default::default()
        };
        let a = build(cache_on);
        let b = build(cache_off);
        assert_eq!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "chunking.embedding.model (EmbeddingModelType::Llm).cache is a cache-control field \
             and must not affect the extraction-cache key"
        );
    }

    /// Pinning test, not proof of the fix: an `LlmConfig` field that genuinely changes
    /// extraction output (the model routed to) must still change the cache key.
    #[test]
    fn structured_extraction_llm_model_changes_the_cache_key() {
        use crate::core::config::{LlmConfig, StructuredExtractionConfig};

        let build = |model: &str| StructuredExtractionConfig {
            schema: serde_json::json!({"type": "object"}),
            schema_name: StructuredExtractionConfig::default_schema_name(),
            schema_description: None,
            strict: false,
            prompt: None,
            llm: LlmConfig {
                model: model.to_string(),
                ..LlmConfig::default()
            },
        };
        let a = ExtractionConfig {
            structured_extraction: Some(build("openai/gpt-4o-mini")),
            ..Default::default()
        };
        let b = ExtractionConfig {
            structured_extraction: Some(build("openai/gpt-4o")),
            ..Default::default()
        };
        assert_ne!(
            hash_extraction_config(&a, "text/plain"),
            hash_extraction_config(&b, "text/plain"),
            "the routed model changes the LLM output and must be part of the extraction-cache key"
        );
    }
}

/// #217: extractor fallback chain behavior.
#[cfg(all(test, feature = "tokio-runtime", not(target_arch = "wasm32")))]
mod issue_217_fallback_tests {
    use super::*;
    use crate::core::config::{ExtractInput, ExtractionConfig};
    use crate::plugins::registry::DocumentExtractorRegistry;
    use crate::plugins::{DocumentExtractor, Plugin};
    use crate::types::ExtractedDocument;
    use std::borrow::Cow;
    use std::sync::Arc;
    use tempfile::tempdir;

    const FALLBACK_MIME: &str = "application/x-issue-217-fallback";

    /// A `DocumentExtractor` whose `extract` outcome is a plain function pointer,
    /// so each test can script a distinct sequence of successes/failures without
    /// a new type per scenario.
    struct ScriptedExtractor {
        name: &'static str,
        priority: i32,
        outcome: fn() -> Result<ExtractedDocument>,
    }

    impl Plugin for ScriptedExtractor {
        fn name(&self) -> &str {
            self.name
        }
        fn version(&self) -> String {
            "1.0.0".to_string()
        }
        fn initialize(&self) -> Result<()> {
            Ok(())
        }
        fn shutdown(&self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl DocumentExtractor for ScriptedExtractor {
        async fn extract(&self, _input: ExtractInput, _config: &ExtractionConfig) -> Result<ExtractedDocument> {
            (self.outcome)()
        }

        fn supported_mime_types(&self) -> &[&str] {
            &[FALLBACK_MIME]
        }

        fn priority(&self) -> i32 {
            self.priority
        }
    }

    fn ok_result() -> Result<ExtractedDocument> {
        Ok(ExtractedDocument {
            content: "fallback succeeded".to_string(),
            mime_type: Cow::Borrowed(FALLBACK_MIME),
            ..Default::default()
        })
    }

    fn unsupported_format_error() -> Result<ExtractedDocument> {
        Err(XbergError::UnsupportedFormat(
            "this extractor declines this specific variant".to_string(),
        ))
    }

    fn parsing_error() -> Result<ExtractedDocument> {
        Err(XbergError::Parsing {
            message: "corrupt or encrypted content".to_string(),
            source: None,
        })
    }

    fn write_temp_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("doc.bin");
        std::fs::write(&file_path, b"irrelevant bytes").unwrap();
        (dir, file_path)
    }

    /// A [`XbergError::UnsupportedFormat`] failure from the highest-priority
    /// extractor must fall through to the next-priority candidate, and the
    /// success must be recorded in `processing_warnings` naming which extractor
    /// ran and why.
    #[tokio::test]
    async fn fallback_eligible_error_tries_next_extractor_and_warns() {
        let mut registry = DocumentExtractorRegistry::new();
        registry
            .register(Arc::new(ScriptedExtractor {
                name: "picky-217",
                priority: 100,
                outcome: unsupported_format_error,
            }))
            .unwrap();
        registry
            .register(Arc::new(ScriptedExtractor {
                name: "fallback-217",
                priority: 50,
                outcome: ok_result,
            }))
            .unwrap();

        let (_dir, file_path) = write_temp_file();
        let config = ExtractionConfig::default();
        let candidates = registry.get_candidates(&file_path, FALLBACK_MIME);
        let result = extract_with_candidates(&file_path, FALLBACK_MIME, &config, candidates)
            .await
            .expect("the lower-priority extractor must still succeed");

        assert_eq!(result.content, "fallback succeeded");
        assert_eq!(result.processing_warnings.len(), 1);
        assert_eq!(result.processing_warnings[0].source, "extractor-fallback");
        assert!(
            result.processing_warnings[0].message.contains("fallback-217"),
            "warning must name the extractor that actually ran: {}",
            result.processing_warnings[0].message
        );
    }

    /// A hard failure ([`XbergError::Parsing`] — the shape an encrypted file or a
    /// corrupt archive surfaces as) must NOT cascade to a lower-priority
    /// extractor: the document itself is the problem, so retrying would only add
    /// latency before producing a confusing error.
    #[tokio::test]
    async fn non_eligible_error_does_not_cascade_to_lower_priority_extractor() {
        let mut registry = DocumentExtractorRegistry::new();
        registry
            .register(Arc::new(ScriptedExtractor {
                name: "hard-failure-217",
                priority: 100,
                outcome: parsing_error,
            }))
            .unwrap();
        registry
            .register(Arc::new(ScriptedExtractor {
                name: "never-reached-217",
                priority: 50,
                outcome: ok_result,
            }))
            .unwrap();

        let (_dir, file_path) = write_temp_file();
        let config = ExtractionConfig::default();
        let candidates = registry.get_candidates(&file_path, FALLBACK_MIME);
        let result = extract_with_candidates(&file_path, FALLBACK_MIME, &config, candidates).await;

        match result {
            Err(XbergError::Parsing { message, .. }) => {
                assert_eq!(message, "corrupt or encrypted content");
            }
            other => panic!("a hard failure must propagate directly, not cascade: {other:?}"),
        }
    }
}
