//! Unified rendering of document content to output formats.
//!
//! - `render_markdown` — GFM Markdown (via comrak)
//! - `render_html` — HTML5 (via comrak)
//! - `render_djot` — Djot markup
//! - `render_doctags` — Docling DocTags (tables as OTSL)
//! - `render_dot` — Graphviz DOT (diagrams recovered from vector sources)
//! - `render_plain` — Plain text (no formatting)

pub(crate) mod common;
mod comrak_bridge;
mod djot;
mod doctags;
mod dot;
mod html;
#[cfg(feature = "html")]
pub mod html_styled;
mod json;
mod markdown;
pub(crate) mod ocr_layout;
mod plain;

pub(crate) use djot::render_djot;
pub(crate) use doctags::render_doctags;
pub(crate) use dot::render_dot;
pub(crate) use html::render_html;
#[cfg(feature = "html")]
pub use html_styled::StyledHtmlRenderer;
pub use json::render_json;

/// Attribute keys the pipeline attaches as *element metadata* rather than source content.
///
/// These are surfaced through `Element.metadata.additional` (that is their documented
/// destination — see `STYLE_NAME_ATTRIBUTE` / `TOC_ENTRY_ATTRIBUTE` in the DOCX extractor)
/// and no source document authors them, so inlining them would print bookkeeping into the
/// rendered text (`# Swimming in the lake (style_name: Title)` for a DOCX heading).
const METADATA_ONLY_ATTRIBUTE_KEYS: [&str; 2] = ["style_name", "toc_entry"];

/// ` (key: value, ...)` suffix that inlines an element's attributes into its own line.
///
/// The XML and OPML extractors keep an element's attributes on the element instead of in
/// its text, so a renderer that ignores `attributes` silently drops source data (OPML
/// `_note` from issue #131, XML `id`/`type`). Both the plain and the Markdown renderer
/// append this to headings; one helper keeps the two from drifting. Namespace
/// declarations (`xmlns*`), empty values, pipeline-metadata keys
/// ([`METADATA_ONLY_ATTRIBUTE_KEYS`]) and internal `xberg:` markers (book-keeping the
/// pipeline reads back, e.g. the image-OCR suppression flag) are not source data and are
/// never emitted.
pub(crate) fn inline_attributes_suffix(attributes: &ahash::AHashMap<String, String>) -> Option<String> {
    let mut filtered: Vec<(&str, &str)> = attributes
        .iter()
        .filter(|(key, value)| {
            !key.starts_with("xmlns")
                && !key.starts_with("xberg:")
                && !METADATA_ONLY_ATTRIBUTE_KEYS.contains(&key.as_str())
                && !value.is_empty()
        })
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if filtered.is_empty() {
        return None;
    }
    filtered.sort_by_key(|(key, _)| *key);
    let rendered: Vec<String> = filtered.iter().map(|(key, value)| format!("{key}: {value}")).collect();
    Some(format!(" ({})", rendered.join(", ")))
}

/// Indices of body paragraphs that reproduce an image's recognized text line for line.
///
/// The pipeline inlines an embedded image's OCR text into the document as body paragraphs — one
/// per line for standalone images, or a single paragraph holding every line for embedded ones —
/// while the image element keeps the same text, so a renderer that prints both shows the
/// recognized content twice. An element joins the reproduction only when *all* of its non-empty
/// lines continue it in order, so a paragraph that merely repeats one line of a picture is a
/// paragraph of the document and stays. Callers pass an empty string for anything that is not a
/// body paragraph — an empty text never matches a line, so those entries are neutral. Titles
/// and headings must be among the neutral entries: a heading that reproduces a logo's text is
/// structure standing *before* the image, while the inlined copy the dedup exists to delete is
/// a plain paragraph after it.
pub(crate) fn ocr_duplicate_indices(texts: &[&str], ocr_contents: &[&str]) -> Vec<bool> {
    let mut repeats = vec![false; texts.len()];
    for content in ocr_contents {
        let expected: Vec<&str> = content.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
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
                    // Every caller that passes `respect_ocr_flags = false` prints the
                    // recognized text from the image itself (the markdown fence, the
                    // plain/djot OCR block) regardless of the doc-level flags, so ANY
                    // full line-for-line reproduction is redundant in that output:
                    // deleting the first one loses no text — it survives via the
                    // image — whether that first hit was the pipeline's inlined copy
                    // or, with the flags off, the document's own repeat. Stopping
                    // after the first keeps a later repeat — a title-like paragraph
                    // further down, a quoted warning — from being deleted with it.
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
/// a body image element that renders its OCR text does that. `doc.images` is keyed by position:
/// `push_image` hands the position to the element it creates and `append_document` remaps
/// appended elements to their new positions, and the renderers resolve `ElementKind::Image` by
/// that same position — so this lookup is keyed by position too, not by
/// `ExtractedImage::image_index` (the export file name's number, which merging a sub-document
/// leaves stale on its appended images).
///
/// `respect_ocr_flags` mirrors the renderer's own handling of `ocr_text_only` / `append_ocr_text`:
/// the Node-style renderers (HTML) print no recognized text when neither flag is set and the
/// pipeline then also creates no inline copy, so a body paragraph that merely matches the
/// invisible OCR content is the only occurrence and must survive — those callers pass `true`.
/// The markdown fence path and the plain renderer print the recognized text unconditionally,
/// so their dedup passes `false`.
pub(crate) fn image_ocr_contents(doc: &crate::types::internal::InternalDocument, respect_ocr_flags: bool) -> Vec<&str> {
    if respect_ocr_flags && !(doc.ocr_text_only || doc.append_ocr_text) {
        return Vec::new();
    }
    doc.images
        .iter()
        .enumerate()
        .filter(|(position, _image)| {
            let position = *position as u32;
            doc.elements.iter().any(|elem| {
                elem.kind == crate::types::internal::ElementKind::Image { image_index: position }
                    && elem.layer == crate::types::document_structure::ContentLayer::Body
                    && elem.should_render_image_ocr()
            })
        })
        .filter_map(|(_position, image)| image.ocr_result.as_deref())
        .map(|result| result.content.as_str())
        .filter(|content| !content.is_empty())
        .collect()
}

pub(crate) use markdown::render_markdown;
pub(crate) use plain::render_plain;

#[cfg(test)]
mod tests {
    use super::ocr_duplicate_indices;

    /// Only the inlined copy is deleted: a body paragraph the document itself
    /// repeats after the image's text stays. The old keep-scanning behavior
    /// marked every later line-for-line reproduction too, deleting the
    /// document's own paragraph (a title above a logo image's text, a repeated
    /// warning) along with the copy.
    #[test]
    fn only_the_first_reproduction_of_ocr_text_is_deleted() {
        let texts = ["logo", "Acme Dashboard", "Acme Dashboard"];
        let repeats = ocr_duplicate_indices(&texts, &["Acme Dashboard"]);
        assert_eq!(repeats, vec![false, true, false]);
    }

    /// Two images whose recognized text is identical each eliminate their own
    /// inlined copy; an already-eliminated element can't be consumed twice and
    /// the document's original paragraph after the copies still survives.
    #[test]
    fn two_identical_ocr_contents_each_consume_their_own_copy() {
        let texts = ["copy", "copy", "original"];
        let repeats = ocr_duplicate_indices(&texts, &["copy", "copy"]);
        assert_eq!(repeats, vec![true, true, false]);
    }
}
