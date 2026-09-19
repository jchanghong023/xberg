//! Native DOC extractor for Word 97-2003 binary format.
//!
//! Extracts text directly from OLE/CFB compound documents without LibreOffice.

use crate::Result;
use crate::core::config::ExtractionConfig;
use crate::core::mime::LEGACY_WORD_MIME_TYPE;
use crate::extraction::doc::{DocParagraph, extract_doc_text};
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::Metadata;
use crate::types::internal::{ElementKind, InternalDocument, InternalElement};
use ahash::AHashMap;
use async_trait::async_trait;
use std::borrow::Cow;
#[cfg_attr(alef, alef(skip))]
/// Native DOC extractor using OLE/CFB parsing.
///
/// This extractor handles Word 97-2003 binary (.doc) files without
/// requiring LibreOffice, providing ~50x faster extraction.
pub struct DocExtractor;

impl DocExtractor {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Default for DocExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for DocExtractor {
    fn name(&self) -> &str {
        "doc-extractor"
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

    fn description(&self) -> &str {
        "Native DOC text extraction via OLE/CFB parsing"
    }

    fn author(&self) -> &str {
        "Xberg Team"
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl InternalDocumentExtractor for DocExtractor {
    async fn extract_content(
        &self,
        content: &[u8],
        mime_type: &str,
        config: &ExtractionConfig,
    ) -> Result<InternalDocument> {
        let result = {
            #[cfg(feature = "tokio-runtime")]
            if crate::core::batch_mode::is_batch_mode() {
                if config.cancel_token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
                    return Err(crate::error::XbergError::Cancelled);
                }
                let content_owned = content.to_vec();
                let span = tracing::Span::current();
                tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
                    let _guard = span.entered();
                    extract_doc_text(&content_owned)
                })
                .await
                .map_err(|e| crate::error::XbergError::parsing(format!("DOC extraction task failed: {e}")))?
            } else {
                extract_doc_text(content)
            }

            #[cfg(not(feature = "tokio-runtime"))]
            {
                if config.cancel_token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
                    return Err(crate::error::XbergError::Cancelled);
                }
                extract_doc_text(content)
            }
        }?;

        let mut doc = InternalDocument::new("doc");
        doc.mime_type = mime_type.to_string();
        doc.processing_warnings.extend(result.processing_warnings);

        doc.metadata = build_metadata(result.metadata);

        // Elements follow Word's own paragraph structure, matching what the
        // DOCX path does with `w:p`. The blank-line fallback is only for
        // documents that carry no paragraph properties at all (Word 6/95, or
        // the contiguous fallback), where there is nothing finer to use.
        if result.paragraphs.is_empty() {
            push_blank_line_chunks(&mut doc, &result.content);
        } else {
            push_paragraph_elements(&mut doc, &result.paragraphs);
        }

        Ok(doc)
    }

    fn supported_mime_types(&self) -> &[&str] {
        &[LEGACY_WORD_MIME_TYPE]
    }

    fn priority(&self) -> i32 {
        60
    }
}

/// Map the `.doc` module's metadata onto the public [`Metadata`] shape.
fn build_metadata(source: crate::extraction::doc::DocMetadata) -> Metadata {
    let mut additional = AHashMap::new();
    if let Some(revision) = source.revision_number {
        additional.insert(Cow::Borrowed("revision"), serde_json::Value::String(revision));
    }
    additional.insert(
        Cow::Borrowed("extraction_method"),
        serde_json::Value::String("native_ole".to_string()),
    );

    let (authors, created_by) = match source.author {
        Some(author) => (Some(vec![author.clone()]), Some(author)),
        None => (None, None),
    };

    Metadata {
        title: source.title,
        subject: source.subject,
        authors,
        created_by,
        modified_by: source.last_author,
        additional,
        ..Default::default()
    }
}

/// Whether a chunk looks like a heading, by shape rather than by style.
///
/// Used only for documents whose style sheet declares no heading style at all.
/// #1553 measured the alternative: deriving headings purely from `istd` would
/// have deleted all 13 headings from one reporter document and all 12 from
/// another, because both style their headings as bold `Normal`. Half this
/// corpus does. Shape is the only signal those documents carry. ~keep
fn looks_like_heading(text: &str, next: Option<&str>) -> bool {
    let is_single_line = !text.contains('\n');
    let is_short = text.len() <= 80;
    let no_trailing_punct = !text.ends_with('.') && !text.ends_with(':') && !text.ends_with(';');
    let next_is_longer = next.is_some_and(|next| !next.is_empty() && next.len() > text.len());
    is_single_line && is_short && no_trailing_punct && next_is_longer
}

/// Emit one element per blank-line-separated chunk.
///
/// Only reachable for documents that carry no paragraph properties -- Word
/// 6/95, and the contiguous fallback -- where Word's own paragraph boundaries
/// are not available. It merges any two paragraphs not separated by a blank
/// line, which is why it is no longer the main path.
fn push_blank_line_chunks(doc: &mut InternalDocument, content: &str) {
    let chunks: Vec<&str> = content.split("\n\n").collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        let next = chunks.get(i + 1).map(|next| next.trim());
        if looks_like_heading(trimmed, next.filter(|next| !next.is_empty())) {
            doc.push_element(InternalElement::text(ElementKind::Heading { level: 2 }, trimmed, 0));
        } else {
            doc.push_element(InternalElement::text(ElementKind::Paragraph, trimmed, 0));
        }
    }
}

/// Emit one element per Word paragraph, so a list-bound paragraph becomes a
/// `ListItem` inside a list container -- the shape the DOCX path already
/// produces for `w:numPr`.
///
/// Consecutive bound paragraphs share a container. A change of nesting depth
/// opens or closes nested containers, and a change of *kind* at the same depth
/// closes and reopens: a document can move from a numbered run straight into a
/// bulleted one at the same level, and merging those would label half the
/// items wrongly. ~keep
fn push_paragraph_elements(doc: &mut InternalDocument, paragraphs: &[DocParagraph]) {
    // The switch is "does this document EMIT styled headings", which is
    // narrower than either alternative that looks right.
    //
    // Not "does the style sheet define headings": nearly every Word style
    // sheet defines heading 1..9 whether or not the author applied one, so
    // that answers yes almost always.
    //
    // And not merely "does any paragraph carry a heading style": a list-bound
    // paragraph is emitted as a ListItem regardless of its style, matching the
    // DOCX path's handling of `w:numPr`. `simple.doc` is the case -- its one
    // Heading 1 paragraph is also list-bound, so counting it flipped the
    // document into styled mode and suppressed every heading while emitting
    // none, leaving the document with no heading structure at all. ~keep
    let styled_headings = paragraphs
        .iter()
        .any(|paragraph| paragraph.heading_level.is_some() && paragraph.list.is_none());
    // One entry per open container, holding whether it is ordered.
    let mut open: Vec<bool> = Vec::new();

    for (i, paragraph) in paragraphs.iter().enumerate() {
        let text = paragraph.content.trim();
        if text.is_empty() {
            continue;
        }

        let Some(list) = paragraph.list else {
            close_lists(doc, &mut open, 0);
            let kind = match heading_kind(paragraphs, i, text, styled_headings) {
                Some(level) => ElementKind::Heading { level },
                None => ElementKind::Paragraph,
            };
            doc.push_element(InternalElement::text(kind, text, 0));
            continue;
        };

        let depth = usize::from(list.level) + 1;
        close_lists(doc, &mut open, depth);
        if open.len() == depth && open.last() != Some(&list.ordered) {
            close_lists(doc, &mut open, depth - 1);
        }
        while open.len() < depth {
            let element_depth = u16::try_from(open.len()).unwrap_or(u16::MAX);
            doc.push_element(InternalElement::text(
                ElementKind::ListStart { ordered: list.ordered },
                "",
                element_depth,
            ));
            open.push(list.ordered);
        }

        let element_depth = u16::try_from(open.len()).unwrap_or(u16::MAX);
        doc.push_element(InternalElement::text(
            ElementKind::ListItem { ordered: list.ordered },
            text,
            element_depth,
        ));
    }

    close_lists(doc, &mut open, 0);
}

/// Decide whether a paragraph is a heading, and at what level.
///
/// #1553: the two signals are not interchangeable and neither is usable alone.
/// A document that declares heading styles is taken at its word -- `istd` is
/// what Word itself renders from, and the shape heuristic invents headings
/// there (1 detected against 7 declared in one corpus document). A document
/// that declares none has nothing to be taken at its word about, and falls
/// back to shape.
///
/// The switch is per *document*, not per paragraph. Per-paragraph fallback
/// would re-add the invented headings alongside the declared ones, which is
/// the worst of both. ~keep
fn heading_kind(paragraphs: &[DocParagraph], index: usize, text: &str, styled_headings: bool) -> Option<u8> {
    if styled_headings {
        return paragraphs.get(index).and_then(|paragraph| paragraph.heading_level);
    }
    let next = paragraphs
        .get(index + 1)
        .map(|next| next.content.trim())
        .filter(|next| !next.is_empty());
    // The shape heuristic has no notion of depth; it only ever claimed h2.
    looks_like_heading(text, next).then_some(2)
}

/// Close open list containers until only `target` remain.
fn close_lists(doc: &mut InternalDocument, open: &mut Vec<bool>, target: usize) {
    while open.len() > target {
        open.pop();
        let element_depth = u16::try_from(open.len()).unwrap_or(u16::MAX);
        doc.push_element(InternalElement::text(ElementKind::ListEnd, "", element_depth));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(relative: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        assert!(
            path.exists(),
            "corpus fixture missing at {}; fetch test_documents rather than skipping",
            path.display()
        );
        std::fs::read(&path).expect("read fixture")
    }

    async fn internal_document(relative: &str) -> InternalDocument {
        DocExtractor::new()
            .extract_content(&corpus(relative), LEGACY_WORD_MIME_TYPE, &ExtractionConfig::default())
            .await
            .expect("DOC extraction should succeed")
    }

    /// #1550: `ordered` distinguishes a numbered list from a bulleted one, and
    /// it is resolved from the list tables' `nfc` rather than assumed.
    ///
    /// A reader that never resolved `nfc` and fell back to a constant would
    /// emit one kind for everything and still look entirely plausible --
    /// twelve ordered lists and zero bulleted reads as a clean result, not as
    /// a lookup that never ran. That is precisely what happened while the
    /// `LVL` blocks were being sliced off at `lcbPlfLst`, because they live
    /// *past* that length.
    ///
    /// FIXTURE REQUIREMENT: this assertion can only fail if the document
    /// contains **both** kinds. `unit_test_lists.doc` carries `nfc` 0 (arabic)
    /// and `nfc` 23 (bullet). Swapping in a bullets-only or numbers-only
    /// document -- or "simplifying" this one -- disarms the guard silently: it
    /// keeps passing while `nfc` resolution is dead. The property that makes
    /// this test able to fail belongs to the corpus, not to the code, so it is
    /// recorded here next to the assertion that depends on it. ~keep
    #[tokio::test]
    async fn list_containers_carry_the_kind_resolved_from_nfc() {
        let doc = internal_document("../../test_documents/doc/unit_test_lists.doc").await;

        let mut ordered = 0;
        let mut bulleted = 0;
        for element in &doc.elements {
            if let ElementKind::ListStart { ordered: is_ordered } = element.kind {
                if is_ordered { ordered += 1 } else { bulleted += 1 }
            }
        }

        assert!(
            ordered > 0 && bulleted > 0,
            "the document mixes nfc 0 and nfc 23, so both container kinds must appear; \
             got {ordered} ordered and {bulleted} bulleted -- a single kind means nfc was \
             never read and a default was used"
        );
    }

    /// Every opened list container is closed, at the right nesting depth.
    #[tokio::test]
    async fn list_containers_are_balanced() {
        let doc = internal_document("../../test_documents/doc/unit_test_lists.doc").await;

        let mut depth: i32 = 0;
        for element in &doc.elements {
            match element.kind {
                ElementKind::ListStart { .. } => depth += 1,
                ElementKind::ListEnd => {
                    depth -= 1;
                    assert!(depth >= 0, "a list was closed that had not been opened");
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "every opened list container must be closed");
    }

    /// #1553: a document that applies heading styles is taken at its word,
    /// and the level comes from the style rather than being a fixed h2.
    ///
    /// FIXTURE REQUIREMENT: `unit_test_lists.doc` applies `heading 1` and
    /// `heading 3` to seven paragraphs that are NOT list-bound. A fixture
    /// whose styled headings were all list-bound would emit them as ListItems
    /// and exercise the fallback instead, passing this file's other test while
    /// leaving this one asserting nothing. ~keep
    #[tokio::test]
    async fn a_document_that_applies_heading_styles_uses_them_and_their_levels() {
        let doc = internal_document("../../test_documents/doc/unit_test_lists.doc").await;

        let levels: Vec<u8> = doc
            .elements
            .iter()
            .filter_map(|element| match element.kind {
                ElementKind::Heading { level } => Some(level),
                _ => None,
            })
            .collect();

        assert_eq!(
            levels.len(),
            7,
            "expected the 7 style-declared headings; got {levels:?}"
        );
        assert!(
            levels.contains(&1) && levels.contains(&3),
            "levels must come from the styles actually applied (heading 1 and heading 3); got {levels:?}"
        );
        assert!(
            !levels.contains(&2),
            "h2 is what the shape heuristic emits for everything; seeing it here means the \
             heuristic ran instead of the styles: {levels:?}"
        );
    }

    /// #1553: a document whose only heading-styled paragraph is list-bound
    /// emits no styled heading at all, so it must fall back to shape rather
    /// than end up with no heading structure.
    ///
    /// `simple.doc` is exactly that: its `Heading 1` paragraph also carries a
    /// list binding, and a list binding wins (matching the DOCX path's
    /// handling of `w:numPr`). Counting it as "this document uses heading
    /// styles" suppressed the heuristic while emitting nothing in its place.
    #[tokio::test]
    async fn a_document_whose_only_styled_heading_is_list_bound_falls_back_to_shape() {
        let doc = internal_document("../../test_documents/vendored/unstructured/doc/simple.doc").await;

        let headings = doc
            .elements
            .iter()
            .filter(|element| matches!(element.kind, ElementKind::Heading { .. }))
            .count();

        assert_eq!(
            headings, 3,
            "expected the shape heuristic's 3 headings; 0 means the document was treated as \
             style-declaring on the strength of a paragraph emitted as a ListItem"
        );
    }

    /// A document with no list bindings must keep the blank-line chunking it
    /// had before #1550 -- Word paragraphs are finer-grained than those chunks.
    #[tokio::test]
    async fn a_list_free_document_emits_no_list_elements() {
        let doc = internal_document("../../test_documents/vendored/unstructured/doc/fake.doc").await;

        assert!(
            !doc.elements.iter().any(|element| matches!(
                element.kind,
                ElementKind::ListStart { .. } | ElementKind::ListEnd | ElementKind::ListItem { .. }
            )),
            "a document with no list bindings must not gain list structure"
        );
    }

    #[tokio::test]
    async fn test_doc_extractor_plugin_interface() {
        let extractor = DocExtractor::new();
        assert_eq!(extractor.name(), "doc-extractor");
        assert_eq!(extractor.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(extractor.priority(), 60);
        assert_eq!(extractor.supported_mime_types(), &["application/msword"]);
    }

    #[tokio::test]
    async fn test_doc_extractor_initialize_shutdown() {
        let extractor = DocExtractor::new();
        assert!(extractor.initialize().is_ok());
        assert!(extractor.shutdown().is_ok());
    }

    #[tokio::test]
    async fn test_doc_extractor_real_file() {
        let test_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test_documents/vendored/unstructured/doc/simple.doc");
        if !test_file.exists() {
            return;
        }
        let content = std::fs::read(&test_file).expect("Failed to read test DOC");
        let extractor = DocExtractor::new();
        let config = ExtractionConfig::default();
        let result = extractor
            .extract_content(&content, "application/msword", &config)
            .await
            .expect("DOC extraction failed");
        let result =
            crate::extraction::derive::derive_extraction_result(result, true, crate::core::config::OutputFormat::Plain);
        assert!(!result.content.is_empty(), "Should extract text from DOC");
        assert_eq!(&*result.mime_type, "application/msword");
    }

    #[tokio::test]
    async fn test_doc_document_structure_with_heuristic_headings() {
        let test_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test_documents/vendored/unstructured/doc/simple.doc");
        if !test_file.exists() {
            return;
        }
        let content = std::fs::read(&test_file).expect("Failed to read test DOC");
        let extractor = DocExtractor::new();
        let config = ExtractionConfig {
            include_document_structure: true,
            ..Default::default()
        };
        let result = extractor
            .extract_content(&content, "application/msword", &config)
            .await
            .expect("DOC extraction failed");
        let result =
            crate::extraction::derive::derive_extraction_result(result, true, crate::core::config::OutputFormat::Plain);
        assert!(result.document.is_some(), "Should produce document structure for DOC");
        let doc = result.document.unwrap();
        assert!(!doc.nodes.is_empty(), "Document structure should have nodes");
    }

    /// GH#1460: the field result text must survive extraction while the field
    /// instruction is dropped, when run through the real `.doc` binary parser end
    /// to end.
    ///
    /// The nine unit tests for `normalize_doc_text` in
    /// `crate::extraction::doc::mod` only ever feed it a synthetic `&str` built
    /// from escape literals (`"\x13 HYPERLINK ... \x14 ... \x15"`). They prove the
    /// normalization function is correct in isolation, but they cannot catch a
    /// regression in the OLE/CFB parsing or piece-table walk that sits upstream of
    /// it — e.g. if the parser stopped delivering `0x13`/`0x14`/`0x15` bytes at
    /// all, or if extraction routed around `normalize_doc_text` entirely, every
    /// one of those tests would still pass. This test exercises a real vendored
    /// `.doc` file whose stream is known (via `grep -a`) to contain a literal
    /// `HYPERLINK "http://github.com/"` field instruction followed by the result
    /// text `A Link example`.
    ///
    /// The DOC extractor has no URI-extraction feature (no metadata field or
    /// extracted-links list surfaces hyperlink targets separately), so the
    /// instruction's URL is not expected to legitimately appear anywhere else in
    /// the output; asserting its absence from the text is safe.
    #[tokio::test]
    async fn should_drop_hyperlink_instruction_but_keep_result_text_when_extracting_real_doc() {
        let test_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test_documents/vendored/unstructured/doc/fake-doc-emphasized-text.doc");
        if !test_file.exists() {
            return;
        }
        let content = std::fs::read(&test_file).expect("Failed to read test DOC");
        let extractor = DocExtractor::new();
        let config = ExtractionConfig::default();
        let result = extractor
            .extract_content(&content, "application/msword", &config)
            .await
            .expect("DOC extraction failed");
        let result =
            crate::extraction::derive::derive_extraction_result(result, true, crate::core::config::OutputFormat::Plain);

        assert!(
            result.content.contains("A Link example"),
            "field result text must be kept: {:?}",
            result.content
        );
        assert!(
            !result.content.contains("HYPERLINK"),
            "field instruction keyword must be dropped: {:?}",
            result.content
        );
        assert!(
            !result.content.contains("http://github.com/"),
            "field instruction URL must be dropped: {:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn test_doc_paragraph_mapping() {
        let test_file =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/doc/unit_test_lists.doc");
        if !test_file.exists() {
            return;
        }
        let content = std::fs::read(&test_file).expect("Failed to read test DOC");
        let extractor = DocExtractor::new();
        let config = ExtractionConfig {
            include_document_structure: true,
            ..Default::default()
        };
        let result = extractor
            .extract_content(&content, "application/msword", &config)
            .await
            .expect("DOC extraction failed");
        let result =
            crate::extraction::derive::derive_extraction_result(result, true, crate::core::config::OutputFormat::Plain);
        assert!(result.document.is_some(), "Should produce document structure");
        let doc = result.document.unwrap();
        let has_paragraph = doc.nodes.iter().any(|n| {
            matches!(
                n.content,
                crate::types::document_structure::NodeContent::Paragraph { .. }
            )
        });
        assert!(has_paragraph, "DOC should produce Paragraph nodes");
    }
}
