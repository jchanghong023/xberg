//! Regression test for https://github.com/xberg-io/xberg/issues/1888
//!
//! `DocumentCounts.pages` was `0` for a scanned PDF extracted with `disable_ocr = true`
//! and no `PageConfig`, while `metadata.format`'s PDF `page_count` -- read from the page
//! tree during metadata extraction, independent of any page tracking -- reported the
//! correct count in the same result. Neither `metadata.pages` (page boundaries) nor
//! `pages` (per-page content) gets built on that path: with OCR off a scanned page has no
//! native text to assign boundaries to, and boundary tracking itself is only switched on
//! by an explicit `PageConfig` or a handful of OCR-related settings none of which
//! `disable_ocr` alone trips. `populate_document_counts` (`core/pipeline/mod.rs`) now
//! falls back to the PDF format metadata's page count when the first two tiers are empty.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test binaries print by design; org logging policy exempts tests
#![cfg(feature = "pdf")]

use xberg::core::config::{ExtractInput, ExtractionConfig, PageConfig};
use xberg::types::{ExtractedDocument, FormatMetadata};

fn scanned_hello_pdf() -> Vec<u8> {
    std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocr/scanned_hello.pdf"))
        .expect("fixture must exist")
}

/// `metadata.format`'s PDF page count -- the value the issue's own repro compared
/// `counts.pages` against, and which the fix reads as its fallback tier.
fn pdf_format_page_count(document: &ExtractedDocument) -> Option<u32> {
    match document.metadata.format.as_ref()? {
        FormatMetadata::Pdf(pdf) => pdf.page_count,
        _ => None,
    }
}

async fn extract_scanned_hello(config: &ExtractionConfig) -> ExtractedDocument {
    let result = xberg::extract(
        ExtractInput::from_bytes(
            scanned_hello_pdf(),
            "application/pdf",
            Some("scanned_hello.pdf".to_string()),
        ),
        config,
    )
    .await
    .expect("scanned_hello.pdf must extract");

    result.results.into_iter().next().expect("one document")
}

/// The reporter's exact scenario: `disable_ocr = true`, no `PageConfig` at all.
#[tokio::test]
async fn counts_pages_matches_pdf_page_count_with_disable_ocr_and_no_page_config() {
    let config = ExtractionConfig {
        disable_ocr: true,
        ..Default::default()
    };

    let document = extract_scanned_hello(&config).await;
    let pdf_page_count = pdf_format_page_count(&document);

    assert_eq!(
        pdf_page_count,
        Some(1),
        "control: scanned_hello.pdf must report a PDF page count of 1"
    );
    assert_eq!(
        document.counts.pages, 1,
        "counts.pages ({}) must equal metadata.format.pdf.page_count ({:?}) (GH#1888)",
        document.counts.pages, pdf_page_count
    );
}

/// Same input and `disable_ocr`, but with an explicit `PageConfig { extract_pages: true }`
/// -- the config shape that already worked before the fix (it forces boundary tracking on),
/// kept here as a guard against the fix regressing that path.
#[tokio::test]
async fn counts_pages_matches_with_disable_ocr_and_extract_pages_true() {
    let config = ExtractionConfig {
        disable_ocr: true,
        pages: Some(PageConfig {
            extract_pages: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let document = extract_scanned_hello(&config).await;

    assert_eq!(document.counts.pages, 1);
}

/// The issue's second reproducer line: `PageConfig { extract_pages: false }` also already
/// worked before the fix (supplying *any* `PageConfig` switches on boundary tracking,
/// regardless of `extract_pages`'s value). Kept as a guard against the fix regressing it.
#[tokio::test]
async fn counts_pages_matches_with_disable_ocr_and_extract_pages_false() {
    let config = ExtractionConfig {
        disable_ocr: true,
        pages: Some(PageConfig {
            extract_pages: false,
            ..Default::default()
        }),
        ..Default::default()
    };

    let document = extract_scanned_hello(&config).await;

    assert_eq!(document.counts.pages, 1);
}

/// The same fallback on a MULTI-page document.
///
/// The single-page cases above would also pass if the fallback happened to yield `1` for some
/// unrelated reason, so the count is pinned once against a fixture whose answer is not 1.
/// `mixed_native_scanned.pdf` carries two pages, one native and one scanned. ~keep
#[tokio::test]
async fn counts_pages_matches_a_two_page_pdfs_page_count_with_disable_ocr_and_no_page_config() {
    let bytes = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocr/mixed_native_scanned.pdf"),
    )
    .expect("the two-page fixture must exist");

    let result = xberg::extract(
        ExtractInput::from_bytes(bytes, "application/pdf", Some("mixed_native_scanned.pdf".to_string())),
        &ExtractionConfig {
            disable_ocr: true,
            ..Default::default()
        },
    )
    .await
    .expect("extraction must succeed");
    let document = result.results.first().expect("one document");

    assert_eq!(
        pdf_format_page_count(document),
        Some(2),
        "control: the fixture must report two pages in its PDF metadata, or this test pins nothing"
    );
    assert_eq!(
        document.counts.pages, 2,
        "counts.pages ({}) must equal the document's two pages (GH#1888)",
        document.counts.pages
    );
}
