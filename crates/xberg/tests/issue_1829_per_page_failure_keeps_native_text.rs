//! Regression tests for https://github.com/xberg-io/xberg/issues/1829
//!
//! A page the OCR step rejects is a per-page failure, not a configuration error: the automatic
//! route must return the native text with a warning naming the cause, rather than failing the whole
//! extraction. Complements `issue_1829_backend_options_validation.rs`, which covers the up-front
//! configuration check for the candle and paddle backends.
//!
//! Split out of that file under https://github.com/xberg-io/xberg/issues/1893: these two tests need
//! only `ocr` + `pdf` (one uses the tesseract backend, the other a local stub registered through the
//! plugin registry), and sharing its `candle-trocr` + `candle-paddleocr-vl` + `paddle_ocr`
//! conjunction meant no build ever compiled them. ~keep
#![cfg(all(feature = "ocr", feature = "pdf"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use xberg::XbergError;
use xberg::core::config::{ExtractInput, ExtractionConfig, OcrConfig};
use xberg::plugins::{OcrBackend, OcrBackendType, Plugin, register_ocr_backend, unregister_ocr_backend};

/// A page the OCR decode rejects under a security limit is a per-page failure, not a
/// configuration error: the automatic route must still return the native text with a warning.
/// The limit is set on `OcrConfig` only, so the render runs under the default limit and only
/// the OCR decode of the scanned page can reject it. ~keep
#[tokio::test]
async fn should_keep_native_text_when_a_security_limit_rejects_a_scanned_page() {
    const REJECTING_MAX_CONTENT_SIZE: usize = 5_000;
    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            security_limits: Some(xberg::extractors::security::SecurityLimits {
                max_content_size: REJECTING_MAX_CONTENT_SIZE,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let result = extract_mixed_native_scanned_pdf(&config)
        .await
        .expect("a per-page security-limit rejection must not fail the extraction");

    expect_native_text_with_ocr_warning(&result, &REJECTING_MAX_CONTENT_SIZE.to_string());
}

const REJECTING_BACKEND: &str = "gh1829-rejecting-backend";
const REJECTING_BACKEND_MESSAGE: &str = "gh1829 stub rejects this page";

/// A backend whose every page call fails with a validation error, as a custom plugin backend
/// does when it checks its own options on each page.
struct RejectingOcrBackend {
    called: Arc<AtomicBool>,
}

impl Plugin for RejectingOcrBackend {
    fn name(&self) -> &str {
        REJECTING_BACKEND
    }

    fn version(&self) -> String {
        "1.0.0".to_string()
    }

    fn initialize(&self) -> xberg::Result<()> {
        Ok(())
    }

    fn shutdown(&self) -> xberg::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl OcrBackend for RejectingOcrBackend {
    async fn process_image(&self, _image_bytes: &[u8], _config: &OcrConfig) -> xberg::Result<xberg::ExtractedDocument> {
        self.called.store(true, Ordering::SeqCst);
        Err(XbergError::Validation {
            message: REJECTING_BACKEND_MESSAGE.to_string(),
            source: None,
        })
    }

    fn supports_language(&self, _language: &str) -> bool {
        true
    }

    fn backend_type(&self) -> OcrBackendType {
        OcrBackendType::Custom
    }
}

/// A page error of the validation kind is still a per-page failure on the automatic route:
/// the native text comes back with a warning, whatever kind of error the backend returns. ~keep
#[tokio::test]
async fn should_keep_native_text_when_a_backend_rejects_a_page_with_a_validation_error() {
    let called = Arc::new(AtomicBool::new(false));
    let _ = unregister_ocr_backend(REJECTING_BACKEND);
    register_ocr_backend(Arc::new(RejectingOcrBackend {
        called: Arc::clone(&called),
    }))
    .expect("the stub backend must register");
    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: REJECTING_BACKEND.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let result = extract_mixed_native_scanned_pdf(&config).await;
    let _ = unregister_ocr_backend(REJECTING_BACKEND);

    assert!(called.load(Ordering::SeqCst), "the scanned page must reach the backend");
    let result = result.expect("a validation error on one page must not fail the extraction");
    expect_native_text_with_ocr_warning(&result, REJECTING_BACKEND_MESSAGE);
}

async fn extract_mixed_native_scanned_pdf(config: &ExtractionConfig) -> xberg::Result<xberg::ExtractionResult> {
    let name = "mixed_native_scanned.pdf";
    let bytes = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ocr")
            .join(name),
    )
    .expect("fixture must exist");
    xberg::extract(
        ExtractInput::from_bytes(bytes, "application/pdf", Some(name.to_string())),
        config,
    )
    .await
}

fn expect_native_text_with_ocr_warning(result: &xberg::ExtractionResult, needle: &str) {
    let doc = result.results.first().expect("one document");
    assert!(
        !doc.content.trim().is_empty(),
        "the native text must be returned when OCR of a page fails"
    );
    assert!(
        doc.processing_warnings
            .iter()
            .any(|warning| warning.message.contains(needle)),
        "the page failure must surface as a warning citing {needle}: {:?}",
        doc.processing_warnings
    );
}
