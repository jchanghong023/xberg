//! Duplicate-copy resolution between an embedded picture's OCR transcription
//! and the other representations of the same content.
//!
//! The acceptance gate flags the shapes below as DUP_CONTENT ("the same
//! content appears as a fenced block and as body text"):
//!
//! 1. An embedded OLE object whose payload is itself a picture (a GIFPLAYER
//!    screenshot control, say) appends the payload's standalone extraction as
//!    a raw dump — flat OCR paragraphs plus its own preview image plus an OCR
//!    grid fence — while the host document also shows the object's preview
//!    picture at the anchor position, which the pipeline OCRs once more. One
//!    object ends up transcribed two or three times.
//! 2. A picture's OCR transcription repeats text the document already carries
//!    as its own text layer (a screenshot of a command list on one slide, the
//!    same commands as a real text box on another).
//!
//! The human golden standard resolves both by keeping the copy at the original
//! position — the anchor picture's transcription, the text layer's text — and
//! dropping the redundant copy. The passes here encode that rule by content
//! similarity over the same shingle units the gate uses ([`TextGrams`]):
//!
//! [`drop_embedded_dumps_covered_by_picture_ocr`] removes an
//! `Embedded object:` dump when some *other* picture's OCR transcription is
//! already spelled out inside the dump — the picture at its original position
//! keeps carrying the content.
//!
//! [`clear_picture_ocr_duplicating_text`] clears an image's `ocr_result` when
//! its transcription is mostly contained in the document's own text (elements
//! and table cells): the image reference stays, the lossy second spelling of
//! text the reader already has goes.

use crate::extraction::markdown_utils::TextGrams;
use crate::types::internal::{ElementKind, InternalDocument};

/// Caption prefix the embedded-object merge writes ahead of a child's raw
/// block (`ooxml_embedded::append_embedded_object_text`).
const EMBEDDED_CAPTION_PREFIX: &str = "Embedded object: ";

/// Fraction of a picture's OCR shingles that must occur inside a dump for the
/// dump to count as a second copy of that picture.
const DUMP_COVERS_PICTURE_THRESHOLD: f64 = 0.6;

/// Fraction of a picture's OCR shingles that must already exist in the
/// document's own text for the transcription to count as a duplicate copy.
/// Sits below the acceptance gate's 0.6 fence-in-body judgment on purpose: the
/// gate measures the rendered fence against rendered body text while this pass
/// measures the flat OCR content against element text, and that small basis
/// drift must not let a genuine duplicate slip through.
const OCR_IN_TEXT_THRESHOLD: f64 = 0.55;

/// Run both duplicate-copy passes. Order matters: a removed dump's text must
/// not count as "document text" when deciding whether a picture's OCR is a
/// duplicate — for the GIFPLAYER shape the dump is the *redundant* copy and
/// the anchor picture's fence is the original the golden standard keeps.
pub(super) fn resolve_duplicate_ocr_copies(doc: &mut InternalDocument) {
    drop_embedded_dumps_covered_by_picture_ocr(doc);
    clear_picture_ocr_duplicating_text(doc);
}

/// Remove `Embedded object: <name>` caption + raw-block pairs whose block
/// already transcribes some other picture's OCR result.
///
/// "Other" excludes any picture the dump itself references: a dump's own
/// embedded preview is part of the dump, not an independent original-position
/// copy, and matching against it would drop dumps that carry genuinely new
/// content alongside their preview.
fn drop_embedded_dumps_covered_by_picture_ocr(doc: &mut InternalDocument) {
    if !doc.images.iter().any(|image| image.ocr_result.is_some()) {
        return;
    }

    let mut keep = Vec::with_capacity(doc.elements.len());
    let elements = std::mem::take(&mut doc.elements);
    let mut pending: Option<(usize, usize)> = None; // (caption index, block index) to drop
    for (index, element) in elements.iter().enumerate() {
        let starts_embedded_pair = pending.is_none()
            && matches!(element.kind, ElementKind::Paragraph)
            && element.text.trim().starts_with(EMBEDDED_CAPTION_PREFIX)
            && elements
                .get(index + 1)
                .is_some_and(|next| matches!(next.kind, ElementKind::RawBlock));
        if starts_embedded_pair {
            let block = elements[index + 1].text.as_str();
            let dump_grams = TextGrams::new(block);
            let covered_elsewhere = doc.images.iter().any(|image| {
                // A picture the dump itself shows is part of the dump, not the
                // original-position copy that justifies dropping it.
                if block.contains(&format!("image_{}.", image.image_index)) {
                    return false;
                }
                let Some(ocr) = image.ocr_result.as_ref() else {
                    return false;
                };
                let grams = TextGrams::new(&ocr.content);
                grams.is_meaningful() && grams.containment_in(&dump_grams) >= DUMP_COVERS_PICTURE_THRESHOLD
            });
            if covered_elsewhere {
                pending = Some((index, index + 1));
                continue; // caption dropped; the block is skipped on the next iteration
            }
        }
        if let Some((_, block_index)) = pending
            && index == block_index
        {
            pending = None;
            continue;
        }
        keep.push(element.clone());
    }
    doc.elements = keep;
}

/// Clear `ocr_result` from pictures whose transcription is mostly contained in
/// the document's own text (non-OCR element text plus table cells). The image
/// reference survives; only the redundant spelling is dropped.
///
/// OCR-kind elements are excluded from the comparison basis: a standalone
/// image document carries its transcription both as `OcrText` elements and as
/// the image's `ocr_result`, and that self-agreement is the normal
/// single-copy shape, not a duplicate. Comparing against it would clear the
/// only transcription the document has.
fn clear_picture_ocr_duplicating_text(doc: &mut InternalDocument) {
    // A standalone image document *is* its picture: the transcription appears
    // both as elements and as the image's `ocr_result`, and that is the
    // single-copy shape the checker requires (fence CJK/token checks read the
    // fence). Nothing in it can be a duplicate of anything else.
    if doc.source_format == "image" {
        return;
    }
    let mut body = String::new();
    for element in &doc.elements {
        if matches!(element.kind, ElementKind::OcrText { .. }) {
            continue;
        }
        body.push_str(&element.text);
        body.push('\n');
    }
    for table in &doc.tables {
        for row in &table.cells {
            for cell in row {
                body.push_str(cell);
                body.push('\n');
            }
        }
    }
    let body_grams = TextGrams::new(&body);

    // A transcription whose shingle set already exists wholesale among
    // previously kept transcriptions is the same text again — the same
    // screenshot pasted on several slides — so later copies go. The body does
    // not seed this set: body text that spells out a transcription is the
    // fence-vs-body case handled above, and seeding would make a document
    // whose only picture transcription echoes its own text clear itself.
    let mut kept: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for image in &mut doc.images {
        let Some(ocr) = image.ocr_result.as_ref() else {
            continue;
        };
        let grams = TextGrams::new(&ocr.content);
        if !grams.is_meaningful() {
            continue;
        }
        // The gate's fence-vs-body judgment needs a body past the same floor;
        // with less text than that no fence can be a duplicate of the body,
        // but the exact-copy rule between pictures must still run.
        let in_body = body_grams.is_meaningful() && grams.containment_in(&body_grams) >= OCR_IN_TEXT_THRESHOLD;
        let duplicate = in_body || grams.keys().all(|key| kept.contains(&key));
        if duplicate {
            image.ocr_result = None;
        } else {
            kept.extend(grams.keys());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::internal::InternalElement;
    use crate::types::{ExtractedDocument, ExtractedImage};

    fn paragraph(text: &str) -> InternalElement {
        InternalElement::text(ElementKind::Paragraph, text, 0)
    }

    fn raw_block(text: &str) -> InternalElement {
        InternalElement::text(ElementKind::RawBlock, text, 0)
    }

    fn image(index: u32, ocr_content: Option<&str>) -> ExtractedImage {
        let mut image = ExtractedImage::default();
        image.image_index = index;
        image.ocr_result = ocr_content.map(|content| {
            Box::new(ExtractedDocument {
                content: content.to_string(),
                ..ExtractedDocument::default()
            })
        });
        image
    }

    fn log_like(repeat: usize) -> String {
        "ncverilog recompiling because param.v is newer than expected\n".repeat(repeat)
    }

    /// The GIFPLAYER shape: a tail dump (caption + block) that transcribes the
    /// same log a *different* picture's OCR already carries at the anchor —
    /// the dump disappears, the anchor picture's OCR survives.
    #[test]
    fn drops_a_dump_covered_by_another_pictures_ocr() {
        let mut doc = InternalDocument::new("docx");
        doc.push_element(paragraph("body text before the object"));
        doc.push_element(paragraph("Embedded object: oleObject1.bin"));
        doc.push_element(raw_block(&format!(
            "{}\n![](image_1.png)\n```text\n{}\n```",
            log_like(6),
            log_like(6)
        )));
        doc.push_element(paragraph("body text after"));
        doc.images = vec![
            image(0, Some(&log_like(6))), // anchor preview picture (image_0): the original copy
            image(1, None),               // the dump's own preview: not an independent witness
        ];

        resolve_duplicate_ocr_copies(&mut doc);

        let texts: Vec<&str> = doc.elements.iter().map(|e| e.text.as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.contains("Embedded object")),
            "caption gone: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("ncverilog")),
            "dump body gone: {texts:?}"
        );
        assert!(texts.contains(&"body text before the object") && texts.contains(&"body text after"));
        assert!(
            doc.images[0].ocr_result.is_some(),
            "anchor picture keeps its transcription"
        );
    }

    /// A dump is kept when the only picture it transcribes is its own embedded
    /// preview — that dump is the sole carrier of the content, so the caption
    /// and block survive. Its preview's transcription still goes: the kept dump
    /// already spells the text out, and a second fence would render the same
    /// block twice.
    #[test]
    fn keeps_a_dump_whose_only_witness_is_its_own_picture() {
        let mut doc = InternalDocument::new("docx");
        doc.push_element(paragraph("Embedded object: oleObject1.bin"));
        doc.push_element(raw_block(&format!("{}\n![](image_0.png)", log_like(6))));
        doc.images = vec![image(0, Some(&log_like(6)))];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.elements.iter().any(|e| e.text.contains("Embedded object")));
        assert!(
            doc.elements.iter().any(|e| e.text.contains("ncverilog")),
            "the dump body survives"
        );
        assert!(
            doc.images[0].ocr_result.is_none(),
            "the dump already carries the text; a second fence would duplicate it"
        );
    }

    /// The screenshot-of-real-text shape: a picture's OCR is mostly spelled
    /// out in the document's own text — the reference stays, the transcription
    /// is cleared.
    #[test]
    fn clears_picture_ocr_duplicating_document_text() {
        let mut doc = InternalDocument::new("pptx");
        let commands = "set drc handling K24 warning\nadd black box -auto x\nset Z handling external x\n".repeat(4);
        doc.push_element(paragraph(&commands));
        doc.push_element(paragraph("![](image_0.png)"));
        doc.images = vec![image(0, Some(&commands)), image(1, Some(&log_like(5)))];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.images[0].ocr_result.is_none(), "duplicate transcription cleared");
        assert!(doc.images[1].ocr_result.is_some(), "unrelated transcription kept");
        assert!(
            doc.elements.iter().any(|e| e.text.contains("![](image_0.png)")),
            "reference kept"
        );
    }

    /// Table cells count as document text: an OCR copy of a table's contents
    /// is a duplicate even though no element spells it out.
    #[test]
    fn table_cells_count_as_text_for_duplicate_ocr() {
        let mut doc = InternalDocument::new("pptx");
        doc.push_element(paragraph("![](image_0.png)"));
        let cell_text = "checkerboard 01010101 10101010 inverted pattern row stripe column\n".repeat(4);
        let mut table = crate::types::Table::default();
        table.cells = vec![vec![cell_text.clone()]];
        doc.tables = vec![table];
        doc.images = vec![image(0, Some(&cell_text))];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.images[0].ocr_result.is_none(), "table-spelled OCR cleared");
    }

    /// Short transcriptions and short bodies stay below the floor the
    /// acceptance gate also uses; nothing is cleared for them.
    #[test]
    fn short_texts_are_left_alone() {
        let mut doc = InternalDocument::new("pptx");
        doc.push_element(paragraph("short body"));
        doc.push_element(paragraph("![](image_0.png)"));
        doc.images = vec![image(0, Some("short body"))];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.images[0].ocr_result.is_some());
    }
    /// A standalone image document carries its transcription as `OcrText` elements
    /// alongside the image's `ocr_result`: that self-agreement is the single-copy
    /// shape and must not clear the fence.
    #[test]
    fn standalone_image_ocr_is_not_a_duplicate_of_itself() {
        let mut doc = InternalDocument::new("image");
        let transcription = log_like(6);
        let ocr_element = InternalElement::text(
            ElementKind::OcrText {
                level: crate::types::OcrElementLevel::Line,
            },
            &transcription,
            0,
        );
        doc.push_element(ocr_element);
        doc.push_element(paragraph("![](image_0.png)"));
        doc.images = vec![image(0, Some(&transcription))];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.images[0].ocr_result.is_some(), "the only transcription stays");
    }

    /// The same screenshot pasted on several slides transcribes identically: the
    /// first fence stays, later exact copies go, image references all stay.
    #[test]
    fn identical_picture_transcriptions_keep_only_the_first() {
        let mut doc = InternalDocument::new("pptx");
        let identical = log_like(6);
        doc.push_element(paragraph("![](image_0.png)"));
        doc.push_element(paragraph("![](image_1.png)"));
        doc.push_element(paragraph("![](image_2.png)"));
        let distinct = "an unrelated screenshot of some other script entirely\n".repeat(8);
        doc.images = vec![
            image(0, Some(&identical)),
            image(1, Some(&distinct)),
            image(2, Some(&identical)),
        ];

        resolve_duplicate_ocr_copies(&mut doc);

        assert!(doc.images[0].ocr_result.is_some(), "first copy stays");
        assert!(doc.images[1].ocr_result.is_some(), "distinct transcription stays");
        assert!(doc.images[2].ocr_result.is_none(), "repeated copy cleared");
    }
}
