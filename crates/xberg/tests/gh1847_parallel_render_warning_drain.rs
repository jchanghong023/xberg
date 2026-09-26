//! Regression test for xberg-io/xberg#1847: the render-warning capture buffer is thread-local,
//! and the `force_ocr` route renders each batch with a rayon parallel iterator. A batch of one
//! page does not split, so rayon runs it on the calling thread and the drain in
//! `extractors::pdf::mod` sees its warnings -- which is why the single-page
//! `issue_340_pdf_render_warning_drain` passed throughout. A batch of two or more pages is sent
//! to pool workers, and nothing drained their buffers, so on a multi-core host every page of a
//! multi-page document silently lost its render warnings.
//!
//! The batch size is the resolved thread count, so this reproduces only with `max_threads` above
//! one *and* more than one page. Both are set explicitly here rather than left to the host's core
//! count, so the test means the same thing on a single-core runner.

#![cfg(all(feature = "pdf", feature = "ocr"))]

mod helpers;

use helpers::extract_bytes_document;
use xberg::core::config::{ConcurrencyConfig, ExtractionConfig, OcrConfig};
use xberg::pdf::render::install_pdf_render_diagnostics;

const PAGE_COUNT: usize = 4;

/// A `PAGE_COUNT`-page PDF whose every page points `/Resources /Font /F1` at a PDF string rather
/// than a font dictionary, so the engine logs a fallback-font warning while rendering each page.
///
/// Same malformed-resource trick as `issue_340_pdf_render_warning_drain`'s single-page builder
/// (see that file for why this failure path is deterministic and independent of installed
/// fonts), duplicated rather than shared because each `tests/*.rs` file is its own crate.
fn build_multipage_pdf_with_malformed_font_resource() -> Vec<u8> {
    let content_stream: &[u8] = b"BT /F1 24 Tf 72 700 Td (Hello) Tj 0 -40 Td (World) Tj ET";

    // Object layout: 1 catalog, 2 pages tree, 3 the shared malformed font resource,
    // then per page i: content stream at 4 + 2i and the page dict at 5 + 2i.
    let font_obj = 3usize;
    let first_page_obj = 5usize;
    let object_count = 4 + 2 * PAGE_COUNT;

    let mut pdf: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = vec![0; object_count];
    macro_rules! push_str {
        ($s:expr) => {
            pdf.extend_from_slice($s.as_bytes())
        };
    }

    push_str!("%PDF-1.5\n");

    offsets[1] = pdf.len();
    push_str!("1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let kids: Vec<String> = (0..PAGE_COUNT)
        .map(|index| format!("{} 0 R", first_page_obj + 2 * index))
        .collect();
    offsets[2] = pdf.len();
    push_str!(format!(
        "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {PAGE_COUNT} >>\nendobj\n",
        kids.join(" ")
    ));

    offsets[font_obj] = pdf.len();
    push_str!(format!("{font_obj} 0 obj\n(NotAFontDict)\nendobj\n"));

    for index in 0..PAGE_COUNT {
        let content_obj = 4 + 2 * index;
        let page_obj = first_page_obj + 2 * index;

        offsets[content_obj] = pdf.len();
        push_str!(format!(
            "{content_obj} 0 obj\n<< /Length {} >>\nstream\n",
            content_stream.len()
        ));
        pdf.extend_from_slice(content_stream);
        push_str!("\nendstream\nendobj\n");

        offsets[page_obj] = pdf.len();
        push_str!(format!(
            "{page_obj} 0 obj\n\
             << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 792]\n\
                /Resources << /Font << /F1 {font_obj} 0 R >> >>\n\
                /Contents {content_obj} 0 R >>\n\
             endobj\n"
        ));
    }

    let xref_off = pdf.len();
    push_str!(format!("xref\n0 {object_count}\n0000000000 65535 f \r\n"));
    for offset in offsets.iter().skip(1) {
        push_str!(format!("{offset:010} 00000 n \r\n"));
    }
    push_str!(format!(
        "trailer\n<< /Size {object_count} /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n"
    ));

    pdf
}

/// A multi-page `force_ocr` extraction must still surface the engine's render warnings.
///
/// Fails on the unfixed code with an empty `pdf-render` set: every page renders on a rayon
/// worker whose thread-local buffer nothing drains. Setting `max_threads` to 1 makes it pass on
/// the unfixed code too, which is the discriminator -- so the assertion below would be vacuous
/// without the explicit multi-thread budget.
#[tokio::test]
async fn parallel_force_ocr_render_keeps_every_page_warning() {
    assert!(
        install_pdf_render_diagnostics(),
        "no other component should own the tracing dispatcher in this test binary"
    );

    let pdf = build_multipage_pdf_with_malformed_font_resource();

    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            language: vec!["eng".to_string()],
            ..Default::default()
        }),
        force_ocr: true,
        concurrency: Some(ConcurrencyConfig {
            max_threads: Some(PAGE_COUNT),
            ..Default::default()
        }),
        ..Default::default()
    };

    let doc = extract_bytes_document(&pdf, "application/pdf", &config)
        .await
        .expect("a document with malformed font resources must still extract successfully");

    let render_warnings: Vec<_> = doc
        .processing_warnings
        .iter()
        .filter(|warning| warning.source == "pdf-render")
        .collect();

    assert!(
        !render_warnings.is_empty(),
        "a {PAGE_COUNT}-page force_ocr render must surface the engine's render warnings; \
         got processing_warnings: {:?}",
        doc.processing_warnings
    );
    assert!(
        render_warnings
            .iter()
            .any(|warning| warning.message.contains("rendering text with fallback font data")),
        "the warning must carry the engine's own sanitized message, got: {render_warnings:?}"
    );
}
