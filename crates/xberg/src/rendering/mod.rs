//! Unified rendering of document content to output formats.
//!
//! - `render_markdown` — GFM Markdown (via comrak)
//! - `render_html` — HTML5 (via comrak)
//! - `render_djot` — Djot markup
//! - `render_doctags` — Docling DocTags (tables as OTSL)
//! - `render_dot` — Graphviz DOT (diagrams recovered from vector sources)
//! - `render_plain` — Plain text (no formatting)

pub(crate) mod common;
pub(crate) mod ocr_layout;
mod comrak_bridge;
mod djot;
mod doctags;
mod dot;
mod html;
#[cfg(feature = "html")]
pub mod html_styled;
mod json;
mod markdown;
mod plain;

pub(crate) use djot::render_djot;
pub(crate) use doctags::render_doctags;
pub(crate) use dot::render_dot;
pub(crate) use html::render_html;
#[cfg(feature = "html")]
pub use html_styled::StyledHtmlRenderer;
pub use json::render_json;

/// Indices of body paragraphs that reproduce an image's recognized text line for line.
///
/// The pipeline inlines an embedded image's OCR text into the document as body paragraphs — one
/// per line for standalone images, or a single paragraph holding every line for embedded ones —
/// while the image element keeps the same text, so a renderer that prints both shows the
/// recognized content twice. An element joins the reproduction only when *all* of its non-empty
/// lines continue it in order, so a paragraph that merely repeats one line of a picture is a
/// paragraph of the document and stays. Callers pass an empty string for anything that is not a
/// body paragraph — an empty text never matches a line, so those entries are neutral.
pub(crate) fn ocr_duplicate_indices(texts: &[&str], ocr_contents: &[&str]) -> Vec<bool> {
    let mut repeats = vec![false; texts.len()];
    for content in ocr_contents {
        let expected: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if expected.is_empty() {
            continue;
        }
        let mut next = 0usize;
        let mut run: Vec<usize> = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            // An element another OCR content already eliminated is the inlined
            // copy of that image, not document text: it must neither extend nor
            // end this content's run.
            if repeats[index] {
                continue;
            }
            let mut consumed = 0usize;
            let mut complete = true;
            for line in text.split('\n').map(str::trim).filter(|line| !line.is_empty()) {
                if next + consumed < expected.len() && line == expected[next + consumed] {
                    consumed += 1;
                } else {
                    complete = false;
                    break;
                }
            }
            if consumed == 0 {
                if text.trim().is_empty() {
                    continue;
                }
                run.clear();
                next = 0;
                continue;
            }
            if complete {
                run.push(index);
                next += consumed;
                if next == expected.len() {
                    for index in run.drain(..) {
                        repeats[index] = true;
                    }
                    // The pipeline inlines each image's recognized text once, so
                    // the first full line-for-line reproduction is that inlined
                    // copy. Stopping here keeps a body paragraph the document
                    // itself repeats later — a title above a logo image's text,
                    // a repeated warning — from being deleted with it.
                    break;
                }
                continue;
            }
            run.clear();
            next = 0;
        }
    }
    repeats
}

/// The recognized text each image will actually print, in image order.
///
/// Only an image whose own text reaches the output can make an inlined copy redundant, and only
/// a body image element that renders its OCR text does that. `ExtractedImage::image_index` is
/// the key the element stream refers to; it is not the image's position in the vector.
pub(crate) fn image_ocr_contents(doc: &crate::types::internal::InternalDocument) -> Vec<&str> {
    doc.images
        .iter()
        .filter(|image| {
            doc.elements.iter().any(|elem| {
                elem.kind == crate::types::internal::ElementKind::Image { image_index: image.image_index }
                    && elem.layer == crate::types::document_structure::ContentLayer::Body
                    && elem.should_render_image_ocr()
            })
        })
        .filter_map(|image| image.ocr_result.as_deref())
        .map(|result| result.content.as_str())
        .filter(|content| !content.is_empty())
        .collect()
}

pub(crate) use markdown::render_markdown;
pub(crate) use plain::render_plain;

