//! Regression coverage for GH#1907: when a page's OCR comes back blank or fails, the PDF route
//! retries OCR on the page's embedded image XObjects. That retry handed the backend the
//! caller's OCR config unchanged, so the embedded image of a page that is one full-page scan
//! got neither the whole-image page segmentation mode (#1786) nor the known-scan signal that
//! gives the page's render the default preprocessing (#1894). The retry now takes both on a
//! scan page.
//!
//! A stub registered as `tesseract` returns no text for every call, which sends every page to
//! the retry, and records the Tesseract config and the known-scan signal of each call. The embedded raster is not square
//! and the page is, so the retry's image is told apart from the page render by its size. OCR of
//! extracted embedded images is off, so the retry is the only call that sees the raster.
//!
//! Every case runs `force_ocr` with one Tesseract pipeline stage, which keeps PaddleOCR out
//! when it is compiled in.

#![allow(deprecated)]
#![cfg(all(feature = "pdf", feature = "ocr"))]

mod helpers;

use async_trait::async_trait;
use helpers::extract_bytes_document_blocking;
use std::sync::{Arc, Mutex};
use xberg::ExtractedDocument;
use xberg::core::config::{ExtractionConfig, OcrConfig, OcrPipelineConfig, OcrPipelineStage};
use xberg::plugins::{OcrBackend, OcrBackendType, Plugin, register_ocr_backend, unregister_ocr_backend};
use xberg::types::{ImagePreprocessingConfig, TesseractConfig};

/// The `backend_options` key that tells Tesseract the page is a known full-page scan.
const KNOWN_SCAN_OPTION: &str = "known_full_page_scan";

/// Page size in points.
const PAGE_PT: u32 = 200;
/// Embedded raster size in pixels. Not square, so no page render has this size.
const RASTER_WIDTH_PX: u32 = 300;
const RASTER_HEIGHT_PX: u32 = 420;
/// A painted image rectangle as (x, y, width, height) in points.
type ImageRect = (u32, u32, u32, u32);
/// The raster covers the whole page: the page is a scan.
const SCAN_RECT: ImageRect = (0, 0, PAGE_PT, PAGE_PT);
/// The raster covers 4% of the page: the page is not a scan.
const FIGURE_RECT: ImageRect = (10, 10, 40, 40);

/// The Tesseract config and the image size of one backend call.
#[derive(Clone)]
struct RecordedCall {
    tesseract_config: Option<TesseractConfig>,
    known_scan: bool,
    size: Option<(u32, u32)>,
}

/// The width and height from a PNG header, or `None` for bytes that are not a PNG.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

struct RecordingBackend {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl Plugin for RecordingBackend {
    fn name(&self) -> &str {
        "tesseract"
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
impl OcrBackend for RecordingBackend {
    async fn process_image(&self, image_bytes: &[u8], config: &OcrConfig) -> xberg::Result<ExtractedDocument> {
        self.calls.lock().expect("calls lock").push(RecordedCall {
            tesseract_config: config.tesseract_config.clone(),
            known_scan: config
                .backend_options
                .as_ref()
                .and_then(|options| options.get(KNOWN_SCAN_OPTION))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            size: png_size(image_bytes),
        });
        Ok(ExtractedDocument::default())
    }
    fn supports_language(&self, _language: &str) -> bool {
        true
    }
    fn backend_type(&self) -> OcrBackendType {
        OcrBackendType::Custom
    }
}

/// A one-page PDF with one grey DeviceGray raster painted into `rect`.
fn pdf_with_image(rect: ImageRect) -> Vec<u8> {
    let (x, y, width, height) = rect;
    let raster = vec![0xA0u8; (RASTER_WIDTH_PX * RASTER_HEIGHT_PX) as usize];
    let content = format!("q {width} 0 0 {height} {x} {y} cm /Im0 Do Q\n");

    let mut buf = Vec::<u8>::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::new();
    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<</Type /Catalog /Pages 2 0 R>>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<</Type /Pages /Kids [3 0 R] /Count 1>>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "3 0 obj\n<</Type /Page /MediaBox [0 0 {PAGE_PT} {PAGE_PT}] /Parent 2 0 R /Contents 4 0 R \
             /Resources <</XObject <</Im0 5 0 R>> >> >>\nendobj\n"
        )
        .as_bytes(),
    );
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "4 0 obj\n<</Length {}>>\nstream\n{content}\nendstream\nendobj\n",
            content.len() + 1
        )
        .as_bytes(),
    );
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<</Type /XObject /Subtype /Image /Width {RASTER_WIDTH_PX} /Height {RASTER_HEIGHT_PX} \
             /ColorSpace /DeviceGray /BitsPerComponent 8 /Length {}>>\nstream\n",
            raster.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&raster);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = buf.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in &offsets {
        buf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<</Size {} /Root 1 0 R>>\n", offsets.len() + 1).as_bytes());
    buf.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
    buf
}

/// `force_ocr` over the whole document through one Tesseract pipeline stage, with the given
/// Tesseract config and no OCR of extracted embedded images.
fn whole_document_config(tesseract_config: Option<TesseractConfig>) -> ExtractionConfig {
    let single_tesseract_stage = OcrPipelineConfig {
        stages: vec![OcrPipelineStage {
            backend: "tesseract".to_string(),
            priority: 100,
            language: None,
            tesseract_config: None,
            paddle_ocr_config: None,
            paddle_ocr_settings: None,
            vlm_config: None,
            backend_options: None,
        }],
        quality_thresholds: Default::default(),
    };
    ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            tesseract_config,
            pipeline: Some(single_tesseract_stage),
            ..Default::default()
        }),
        force_ocr: true,
        ocr_embedded_images: Some(false),
        use_cache: false,
        ..Default::default()
    }
}

/// Extract `pdf` with the stub backend and return the page-render calls and the retry calls.
fn record_calls(pdf: &[u8], config: &ExtractionConfig) -> (Vec<RecordedCall>, Vec<RecordedCall>) {
    assert!(
        std::str::from_utf8(pdf).is_err(),
        "the fixture must carry the binary raster stream"
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let _ = unregister_ocr_backend("tesseract");
    register_ocr_backend(Arc::new(RecordingBackend { calls: calls.clone() })).expect("the stub backend registers");
    let result = extract_bytes_document_blocking(pdf, "application/pdf", config);
    unregister_ocr_backend("tesseract").expect("the stub backend unregisters");
    result.expect("OCR of the fixture must not error");

    let calls = calls.lock().expect("calls lock").clone();
    let (retries, renders): (Vec<_>, Vec<_>) = calls
        .into_iter()
        .partition(|call| call.size == Some((RASTER_WIDTH_PX, RASTER_HEIGHT_PX)));
    assert!(!renders.is_empty(), "the page render must reach the backend");
    assert!(
        !retries.is_empty(),
        "a blank page must be retried on its embedded image"
    );
    (renders, retries)
}

fn psm(call: &RecordedCall) -> Option<i32> {
    call.tesseract_config.as_ref().and_then(|c| c.psm)
}

fn preprocessing(call: &RecordedCall) -> Option<&ImagePreprocessingConfig> {
    call.tesseract_config.as_ref().and_then(|c| c.preprocessing.as_ref())
}

/// A scan page's retry takes the whole-image mode and the known-scan signal that the page
/// render takes. The render's settings show the fixture is detected as a scan.
#[test]
#[serial_test::serial]
fn the_retry_of_a_scan_page_takes_the_scan_page_settings() {
    let (renders, retries) = record_calls(&pdf_with_image(SCAN_RECT), &whole_document_config(None));
    for render in &renders {
        assert!(
            psm(render).is_some() && render.known_scan,
            "the fixture's page must be detected as a scan"
        );
    }
    for retry in &retries {
        assert_eq!(
            psm(retry),
            psm(&renders[0]),
            "the embedded-image retry of a scan page must take the whole-image PSM"
        );
        assert!(
            retry.known_scan,
            "the embedded-image retry of a scan page must carry the known-scan signal"
        );
    }
}

/// A page with a small figure is not a scan, so its retry keeps the caller's config.
#[test]
#[serial_test::serial]
fn the_retry_of_a_page_that_is_not_a_scan_keeps_the_callers_config() {
    let (renders, retries) = record_calls(&pdf_with_image(FIGURE_RECT), &whole_document_config(None));
    for call in renders.iter().chain(&retries) {
        assert_eq!(psm(call), None, "a page that is not a scan keeps the engine's PSM");
        assert!(!call.known_scan, "a page that is not a scan keeps the pixel test");
    }
}

/// A caller's `psm` wins on a scan page's retry, and the retry still carries the known-scan
/// signal.
#[test]
#[serial_test::serial]
fn the_retry_of_a_scan_page_keeps_the_callers_psm() {
    const CALLER_PSM: i32 = 4;
    let caller = TesseractConfig {
        psm: Some(CALLER_PSM),
        ..Default::default()
    };
    let (_, retries) = record_calls(&pdf_with_image(SCAN_RECT), &whole_document_config(Some(caller)));
    for retry in &retries {
        assert_eq!(psm(retry), Some(CALLER_PSM), "the caller's psm must win");
        assert!(retry.known_scan, "the retry must still carry the known-scan signal");
    }
}

/// A caller's `preprocessing` reaches a scan page's retry unchanged, and the `psm` the caller
/// left unset still takes the whole-image mode.
#[test]
#[serial_test::serial]
fn the_retry_of_a_scan_page_keeps_the_callers_preprocessing() {
    const CALLER_TARGET_DPI: i32 = 240;
    let caller = TesseractConfig {
        preprocessing: Some(ImagePreprocessingConfig {
            target_dpi: CALLER_TARGET_DPI,
            ..Default::default()
        }),
        ..Default::default()
    };
    let (renders, retries) = record_calls(&pdf_with_image(SCAN_RECT), &whole_document_config(Some(caller)));
    for retry in &retries {
        assert_eq!(
            preprocessing(retry).map(|p| p.target_dpi),
            Some(CALLER_TARGET_DPI),
            "the caller's preprocessing must reach the retry"
        );
        assert!(
            psm(retry).is_some(),
            "an unset psm must still take the whole-image mode"
        );
        assert_eq!(psm(retry), psm(&renders[0]));
    }
}
