//! GH#1812: two extractions running at once, each its own `extract` call, must both finish.
//!
//! The reported stall is two concurrent Node `extract` calls on a scanned PDF with layout
//! detection on: sequential calls finish, concurrent ones never return.
//! `max_concurrent_extractions` does not bound this -- it is a batch-scoped task count
//! (`core::config::concurrency`), so it is invisible across two separate calls, and each call
//! resolves the whole thread budget for itself.
//!
//! No test in the suite ran two concurrent layout-enabled extractions before this one:
//! `concurrency_stress.rs` has eight concurrency tests and none of them configures layout.

#![cfg(all(feature = "pdf", feature = "ocr", feature = "layout-detection"))]
#![allow(clippy::print_stdout, clippy::print_stderr)]
// ~keep `tokio::spawn`ing an extraction future requires proving it `Send`, and under `full`
// features that proof walks a long third-party chain (see `concurrency_stress.rs`).
#![recursion_limit = "256"]

mod helpers;
use helpers::{UriBatchInput, extract_bytes_document, extract_uri_documents};

use std::time::{Duration, Instant};
use xberg::core::config::{ExtractionConfig, OcrConfig, layout::LayoutDetectionConfig};

const SCANNED_PDF: &[u8] = include_bytes!("fixtures/ocr/shaded_table_scan.pdf");

/// Well above the per-extraction timeout below, so a clean `Timeout` from both calls is
/// distinguishable from the harness giving up: the former is the product reporting a limit, the
/// latter is the stall this test exists to catch. ~keep
const HARNESS_DEADLINE: Duration = Duration::from_secs(240);
const EXTRACTION_TIMEOUT_SECS: u64 = 60;

/// Layout on, OCR off: the NATIVE route, which is where `extractors::pdf` calls the synchronous
/// `extract_all_from_native_document` straight from an `async fn` and reaches the structure
/// pipeline's inline model checkouts. The OCR route below takes a different path. ~keep
fn layout_only_config() -> ExtractionConfig {
    ExtractionConfig {
        ocr: None,
        layout: Some(LayoutDetectionConfig::default()),
        extraction_timeout_secs: Some(EXTRACTION_TIMEOUT_SECS),
        ..Default::default()
    }
}

fn layout_ocr_config() -> ExtractionConfig {
    ExtractionConfig {
        ocr: Some(OcrConfig::default()),
        layout: Some(LayoutDetectionConfig::default()),
        extraction_timeout_secs: Some(EXTRACTION_TIMEOUT_SECS),
        ..Default::default()
    }
}

/// The control: one call at a time. If this cannot finish either, the fixture or the models are
/// the problem and the concurrent case below proves nothing. ~keep
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_layout_extraction_of_a_scanned_page_finishes() {
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        HARNESS_DEADLINE,
        extract_bytes_document(SCANNED_PDF, "application/pdf", &layout_ocr_config()),
    )
    .await;

    println!(
        "single: {:?} after {:?}",
        outcome.as_ref().map(Result::is_ok),
        started.elapsed()
    );
    assert!(
        outcome.is_ok(),
        "one layout-enabled extraction must return within {HARNESS_DEADLINE:?}"
    );
}

/// GH#1812. Two calls, started together, each carrying the whole thread budget. Both must return
/// -- either with a document or with a `Timeout` error, which is the product reporting its own
/// limit. Neither may sit past the harness deadline: a parked `spawn_blocking` worker never polls
/// `extraction_timeout_secs`, so a wait cycle shows up here as the harness timing out rather than
/// as two `Timeout` results. ~keep
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_layout_extractions_both_return() {
    let started = Instant::now();
    let first =
        tokio::spawn(async { extract_bytes_document(SCANNED_PDF, "application/pdf", &layout_ocr_config()).await });
    let second =
        tokio::spawn(async { extract_bytes_document(SCANNED_PDF, "application/pdf", &layout_ocr_config()).await });

    let joined = tokio::time::timeout(HARNESS_DEADLINE, async { tokio::join!(first, second) }).await;

    let Ok((first, second)) = joined else {
        panic!(
            "GH#1812: neither concurrent extraction returned within {HARNESS_DEADLINE:?}, \
             and the per-extraction timeout of {EXTRACTION_TIMEOUT_SECS}s did not fire -- \
             a parked worker is not polling it"
        );
    };

    for (label, result) in [("first", first), ("second", second)] {
        let result = result.unwrap_or_else(|join| panic!("{label} extraction task panicked: {join}"));
        match result {
            Ok(document) => println!("{label}: ok, {} chars, {:?}", document.content.len(), started.elapsed()),
            Err(error) => println!("{label}: {error}, {:?}", started.elapsed()),
        }
    }
}

/// The native-route variant of the case above. `ModelCache` parks the OS thread with no timeout
/// and the structure pipeline checks models out inline, so this is the shape a wait cycle would
/// take. ~keep
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_native_layout_extractions_both_return() {
    let started = Instant::now();
    let first =
        tokio::spawn(async { extract_bytes_document(SCANNED_PDF, "application/pdf", &layout_only_config()).await });
    let second =
        tokio::spawn(async { extract_bytes_document(SCANNED_PDF, "application/pdf", &layout_only_config()).await });

    let joined = tokio::time::timeout(HARNESS_DEADLINE, async { tokio::join!(first, second) }).await;

    let Ok((first, second)) = joined else {
        panic!(
            "GH#1812: neither concurrent native-route extraction returned within {HARNESS_DEADLINE:?}, \
             and the per-extraction timeout of {EXTRACTION_TIMEOUT_SECS}s did not fire"
        );
    };

    for (label, result) in [("first", first), ("second", second)] {
        let result = result.unwrap_or_else(|join| panic!("{label} native extraction task panicked: {join}"));
        match result {
            Ok(document) => println!("{label}: ok, {} chars, {:?}", document.content.len(), started.elapsed()),
            Err(error) => println!("{label}: {error}, {:?}", started.elapsed()),
        }
    }
}

/// GH#1812 against the reporter's own document, which is a 1,079,202-byte 10-page scan and is not
/// vendored. Point `XBERG_1812_PDF` at it and run with `--ignored`. The fixtures above are one
/// page; the report is ten, and every page is its own set of model checkouts, so page count is the
/// axis this covers and they do not.
///
/// Sequential first, then concurrent, in one run and on one process -- that is the comparison the
/// report makes, and running both here means a slow machine cannot be mistaken for the stall. ~keep
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "GH#1812: set XBERG_1812_PDF to the reporter's 10-page scan"]
async fn the_reporters_document_finishes_sequentially_and_concurrently() {
    const REPORTER_EXTRACTION_TIMEOUT_SECS: u64 = 300;
    const REPORTER_DEADLINE: Duration = Duration::from_secs(1200);

    let path = std::env::var("XBERG_1812_PDF").expect("XBERG_1812_PDF must name the reporter's PDF");
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("XBERG_1812_PDF {path}: {error}"));
    assert!(!bytes.is_empty(), "XBERG_1812_PDF {path} is empty");
    println!("reporter document: {} bytes", bytes.len());

    let config = ExtractionConfig {
        ocr: Some(OcrConfig::default()),
        layout: Some(LayoutDetectionConfig::default()),
        extraction_timeout_secs: Some(REPORTER_EXTRACTION_TIMEOUT_SECS),
        ..Default::default()
    };

    let started = Instant::now();
    let first = extract_bytes_document(&bytes, "application/pdf", &config).await;
    let sequential_one = started.elapsed();
    let second = extract_bytes_document(&bytes, "application/pdf", &config).await;
    println!(
        "sequential: {:?} in {sequential_one:?}, then {:?} in {:?}",
        first.as_ref().map(|d| d.content.len()),
        second.as_ref().map(|d| d.content.len()),
        started.elapsed() - sequential_one
    );
    assert!(first.is_ok() && second.is_ok(), "the sequential control must succeed");

    let concurrent_started = Instant::now();
    let left = {
        let bytes = bytes.clone();
        let config = config.clone();
        tokio::spawn(async move { extract_bytes_document(&bytes, "application/pdf", &config).await })
    };
    let right = {
        let bytes = bytes.clone();
        let config = config.clone();
        tokio::spawn(async move { extract_bytes_document(&bytes, "application/pdf", &config).await })
    };

    let joined = tokio::time::timeout(REPORTER_DEADLINE, async { tokio::join!(left, right) }).await;
    let Ok((left, right)) = joined else {
        panic!(
            "GH#1812 REPRODUCED: two concurrent extractions did not return within {REPORTER_DEADLINE:?}, \
             while the same document extracted twice in sequence took {:?}",
            concurrent_started - started
        );
    };

    for (label, result) in [("left", left), ("right", right)] {
        let result = result.unwrap_or_else(|join| panic!("{label} task panicked: {join}"));
        match result {
            Ok(document) => println!(
                "{label}: ok, {} chars, {:?}",
                document.content.len(),
                concurrent_started.elapsed()
            ),
            Err(error) => println!("{label}: {error}, {:?}", concurrent_started.elapsed()),
        }
    }
}

/// The reporter's own configuration, verbatim from their `repro.mjs`, deserialized rather than
/// hand-transcribed so a field cannot be quietly dropped in translation. Their script makes two
/// `extractBatch([input])` calls inside one `Promise.all` -- two batches of one, not one batch of
/// two -- which is the shape `max_concurrent_extractions` cannot see. ~keep
const REPORTER_CONFIG_JSON: &str = include_str!("fixtures/gh1812_reporter_config.json");

/// GH#1812 with the reporter's document AND their configuration, through the batch API they call.
/// Point `XBERG_1812_PDF` at their 10-page scan and run with `--ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "GH#1812: set XBERG_1812_PDF to the reporter's 10-page scan"]
async fn the_reporters_config_finishes_as_two_concurrent_batches() {
    const REPORTER_DEADLINE: Duration = Duration::from_secs(1200);

    let path =
        std::path::PathBuf::from(std::env::var("XBERG_1812_PDF").expect("XBERG_1812_PDF must name the reporter's PDF"));
    assert!(path.is_file(), "XBERG_1812_PDF {} is not a file", path.display());

    let config: ExtractionConfig = serde_json::from_str(REPORTER_CONFIG_JSON)
        .expect("the reporter's config must deserialize -- a rejected field is itself the finding");

    let batch_of_one = |path: std::path::PathBuf| vec![UriBatchInput { path, config: None }];

    // ~keep The sequential control runs first and in the same process: the reported contrast is
    // "sequential finishes, concurrent does not", and without measuring both here a slow machine
    // is indistinguishable from the stall.
    let sequential_started = Instant::now();
    let sequential = extract_uri_documents(batch_of_one(path.clone()), &config).await;
    report("sequential", sequential.map_err(|e| e.to_string()), sequential_started);

    let started = Instant::now();
    let left = {
        let (items, config) = (batch_of_one(path.clone()), config.clone());
        tokio::spawn(async move { extract_uri_documents(items, &config).await })
    };
    let right = {
        let (items, config) = (batch_of_one(path.clone()), config.clone());
        tokio::spawn(async move { extract_uri_documents(items, &config).await })
    };

    let joined = tokio::time::timeout(REPORTER_DEADLINE, async { tokio::join!(left, right) }).await;
    let Ok((left, right)) = joined else {
        panic!("GH#1812 REPRODUCED: two concurrent single-item batches did not return within {REPORTER_DEADLINE:?}");
    };

    for (label, result) in [("concurrent-left", left), ("concurrent-right", right)] {
        let result = result.unwrap_or_else(|join| panic!("{label} batch task panicked: {join}"));
        report(label, result.map_err(|e| e.to_string()), started);
    }
}

/// Print what one batch produced: how much content, how many of the document's ten page markers
/// survived, and every warning. A timed-out extraction here returns `Ok` with a near-empty
/// document, so the page-marker count is what separates a real result from a degraded one -- the
/// success/failure of the call does not. ~keep
fn report(label: &str, result: std::result::Result<Vec<xberg::types::ExtractedDocument>, String>, started: Instant) {
    match result {
        Ok(documents) => {
            // ~keep The batch test helper turns a per-item error into a synthetic document whose
            // content is `Error: <message>`, so a timed-out item arrives here as an `Ok` document
            // of about 56 characters. Read `metadata.error` first: mistaking that string for a
            // truncated extraction reads as silent data loss when the timeout was in fact
            // reported correctly.
            if let Some(error) = documents.first().and_then(|d| d.metadata.error.as_ref()) {
                println!(
                    "{label}: ITEM ERROR {} {}, {:?}",
                    error.error_type,
                    error.message,
                    started.elapsed()
                );
                return;
            }
            let content = documents.first().map(|d| d.content.clone()).unwrap_or_default();
            let pages_seen = (1..=10)
                .filter(|page| content.contains(&format!("A4 SCAN PAGE {page} OF 10")))
                .count();
            let warnings: Vec<String> = documents
                .first()
                .map(|d| {
                    d.processing_warnings
                        .iter()
                        .map(|w| format!("{}: {}", w.source, w.message))
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "{label}: ok, {} documents, {} chars, {pages_seen}/10 page markers, {:?}, warnings={warnings:?}",
                documents.len(),
                content.len(),
                started.elapsed()
            );
        }
        Err(error) => println!("{label}: ERR {error}, {:?}", started.elapsed()),
    }
}
