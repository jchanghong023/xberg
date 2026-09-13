//! PowerPoint presentation extractor.

use crate::Result;
use crate::core::config::ExtractionConfig;
use crate::extractors::security::SecurityBudget;
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::internal::InternalDocument;
use crate::types::internal_builder::InternalDocumentBuilder;
use crate::types::metadata::Metadata;
use crate::types::uri::ExtractedUri;
use ahash::AHashMap;
use async_trait::async_trait;
use std::borrow::Cow;
use std::path::Path;
#[cfg_attr(alef, alef(skip))]
/// PowerPoint presentation extractor.
///
/// Supports: .pptx, .pptm, .ppsx
pub struct PptxExtractor;

impl Default for PptxExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl PptxExtractor {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PptxExtractor {
    /// Build an `InternalDocument` from PPTX extracted text.
    ///
    /// Parses each archive-derived slide independently so page metadata never
    /// depends on headings or marker-like user text.
    ///
    /// Try to strip an ordered-list prefix like `1. `, `2. `, `10. ` from a line.
    /// Returns the remaining text after the prefix, or `None` if the line does not
    /// start with a `<digits>. ` pattern.
    fn strip_ordered_prefix(line: &str) -> Option<&str> {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || i + 2 > bytes.len() {
            return None;
        }
        if bytes[i] == b'.' && bytes[i + 1] == b' ' {
            Some(&line[i + 2..])
        } else {
            None
        }
    }

    /// Build the delimited forms of a deck's math runs, longest first.
    ///
    /// A math run reaches markdown text as `$$latex$$` (display) or `$latex$`
    /// (inline). Matching the exact strings the OMML converter produced keeps
    /// author text that merely holds a `$` out of the formula list.
    fn math_forms(formulas: &[(String, bool)]) -> Vec<(String, String)> {
        let mut forms: Vec<(String, String)> = formulas
            .iter()
            .map(|(latex, is_display)| {
                let delimiter = if *is_display { "$$" } else { "$" };
                (format!("{delimiter}{latex}{delimiter}"), latex.clone())
            })
            .collect();
        forms.sort_by_key(|form| std::cmp::Reverse(form.0.len()));
        forms
    }

    /// Pull the math spans out of one line of extracted text.
    ///
    /// Returns the line without its math and the LaTeX of every span removed, in
    /// the order the spans appeared.
    ///
    /// Plain text carries no delimiters, so there a line that is exactly one
    /// formula's LaTeX becomes that formula. This covers the equation shape a
    /// slide deck usually holds. Math mixed into a line of plain text stays in
    /// the line, because the LaTeX and the words around it are the same
    /// characters. Markdown text keeps its delimiters, so both shapes of math
    /// come out of it.
    fn split_line_math<'a>(
        line: &'a str,
        forms: &[(String, String)],
        formulas: &[(String, bool)],
        plain_output: bool,
    ) -> (Cow<'a, str>, Vec<String>) {
        if formulas.is_empty() {
            return (Cow::Borrowed(line), Vec::new());
        }
        if plain_output {
            let trimmed = line.trim();
            return match formulas.iter().find(|(latex, _)| latex.as_str() == trimmed) {
                Some((latex, _)) => (Cow::Borrowed(""), vec![latex.clone()]),
                None => (Cow::Borrowed(line), Vec::new()),
            };
        }
        if !line.contains('$') {
            return (Cow::Borrowed(line), Vec::new());
        }

        let mut rest = line;
        let mut text = String::new();
        let mut found: Vec<String> = Vec::new();
        loop {
            let earliest = forms
                .iter()
                .filter_map(|form| rest.find(form.0.as_str()).map(|pos| (pos, form)))
                .min_by_key(|(pos, _)| *pos);
            let Some((pos, form)) = earliest else {
                break;
            };
            text.push_str(&rest[..pos]);
            found.push(form.1.clone());
            rest = &rest[pos + form.0.len()..];
        }
        if found.is_empty() {
            return (Cow::Borrowed(line), Vec::new());
        }
        text.push_str(rest);

        (Cow::Owned(text.split_whitespace().collect::<Vec<_>>().join(" ")), found)
    }

    /// Emit a formula element per LaTeX string, in order.
    fn push_line_formulas(
        builder: &mut InternalDocumentBuilder,
        formulas: &[String],
        slide_num: u32,
        budget: &mut SecurityBudget,
    ) -> Result<()> {
        for latex in formulas {
            budget.account_text(latex.len())?;
            builder.push_formula(latex, Some(slide_num), None);
        }
        Ok(())
    }

    fn build_internal_document(
        slide_contents: &[(u32, String)],
        slide_count: u32,
        formulas: &[(String, bool)],
        plain_output: bool,
        budget: &mut SecurityBudget,
    ) -> Result<InternalDocument> {
        let mut builder = InternalDocumentBuilder::new("pptx");
        let mut saw_title = false;
        let forms = Self::math_forms(formulas);

        for (slide_num, content) in slide_contents {
            let mut in_notes = false;

            for block in content.split("\n\n") {
                budget.step()?;
                let trimmed = block.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if trimmed.starts_with("### Notes:") || trimmed == "Notes:" {
                    in_notes = true;
                    continue;
                }

                if let Some(title_text) = trimmed.strip_prefix("# ") {
                    in_notes = false;
                    saw_title = true;
                    let (title_text, title_formulas) =
                        Self::split_line_math(title_text, &forms, formulas, plain_output);
                    Self::push_line_formulas(&mut builder, &title_formulas, *slide_num, budget)?;
                    let title = title_text.trim();
                    if !title.is_empty() {
                        budget.account_text(title.len())?;
                        builder.push_heading(2, title, Some(*slide_num), None);
                    }
                    continue;
                }

                if in_notes {
                    in_notes = false;
                }

                if trimmed.starts_with('|') {
                    // A block that opens with a table row can still carry picture placeholders:
                    // the content builder appends one for every picture, and a picture that sits
                    // inside the table's area lands in the same blank-line-separated block.
                    // Handing those lines to the table parser turned them into cells, where the
                    // image promotion — which walks paragraphs — never saw them, so the picture
                    // disappeared from the output. Split them back out and let each keep its
                    // place around the table.
                    let lines: Vec<&str> = trimmed.lines().collect();
                    let first_table = lines.iter().position(|line| line.trim_start().starts_with('|'));
                    let (leading, rest) = lines.split_at(first_table.unwrap_or(lines.len()));
                    let (table_lines, trailing): (Vec<&str>, Vec<&str>) =
                        rest.iter().partition(|line| line.trim_start().starts_with('|'));

                    let push_lines = |builder: &mut InternalDocumentBuilder,
                                          lines: &[&str],
                                          budget: &mut SecurityBudget|
                     -> Result<()> {
                        for line in lines {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            let (line_text, line_formulas) =
                                Self::split_line_math(line, &forms, formulas, plain_output);
                            Self::push_line_formulas(builder, &line_formulas, *slide_num, budget)?;
                            let text = line_text.trim();
                            if !text.is_empty() {
                                budget.account_text(text.len())?;
                                builder.push_paragraph(text, Vec::new(), Some(*slide_num), None);
                            }
                        }
                        Ok(())
                    };

                    push_lines(&mut builder, leading, budget)?;
                    let table_text = table_lines.join("\n");
                    let cells = Self::parse_markdown_table(&table_text);
                    if !cells.is_empty() {
                        builder.push_table_from_cells(&cells, Some(*slide_num), None);
                    }
                    push_lines(&mut builder, &trailing, budget)?;
                    continue;
                }

                let mut in_list: Option<bool> = None;

                for line in trimmed.lines() {
                    let (line_text, line_formulas) = Self::split_line_math(line, &forms, formulas, plain_output);
                    let lt = line_text.trim();
                    if lt.is_empty() && line_formulas.is_empty() {
                        if in_list.is_some() {
                            builder.end_list();
                            in_list = None;
                        }
                        continue;
                    }

                    let list_match = if let Some(item_text) = lt.strip_prefix("- ") {
                        Some((false, item_text))
                    } else {
                        Self::strip_ordered_prefix(lt).map(|item_text| (true, item_text))
                    };

                    if let Some((ordered, item_text)) = list_match {
                        match in_list {
                            Some(prev_ordered) if prev_ordered != ordered => {
                                builder.end_list();
                                builder.push_list(ordered);
                                in_list = Some(ordered);
                            }
                            None => {
                                builder.push_list(ordered);
                                in_list = Some(ordered);
                            }
                            _ => {}
                        }
                        // A list item's math becomes its own element ahead of the
                        // item, the order DOCX uses for math runs in a paragraph.
                        Self::push_line_formulas(&mut builder, &line_formulas, *slide_num, budget)?;
                        budget.account_text(item_text.len())?;
                        builder.push_list_item(item_text, ordered, vec![], Some(*slide_num), None);
                    } else {
                        if in_list.is_some() {
                            builder.end_list();
                            in_list = None;
                        }
                        Self::push_line_formulas(&mut builder, &line_formulas, *slide_num, budget)?;
                        if !lt.is_empty() {
                            budget.account_text(lt.len())?;
                            builder.push_paragraph(lt, vec![], Some(*slide_num), None);
                        }
                    }
                }

                if in_list.is_some() {
                    builder.end_list();
                }
            }
        }

        // Preserve the legacy all-untitled output shape: it contains one Slide
        // sentinel, while titled decks do not gain new thematic-break/JSON nodes.
        if !saw_title && slide_count > 0 {
            builder.push_slide(1, None, Some(1));
        }

        Ok(builder.build())
    }

    /// Parse a markdown table block into a 2D cell grid.
    fn parse_markdown_table(table_text: &str) -> Vec<Vec<String>> {
        let mut cells = Vec::new();
        for line in table_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.contains("---") {
                continue;
            }
            let row: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect();
            if !row.is_empty() {
                cells.push(row);
            }
        }
        cells
    }
}

impl PptxExtractor {
    /// Build an InternalDocument from a PptxExtractionResult, mapping office
    /// metadata to standard `Metadata` struct fields.
    ///
    /// `budget` is threaded into the internal document builder to enforce
    /// hostile-input limits on the extracted content.
    fn build_document_from_result(
        pptx_result: crate::types::PptxExtractionResult,
        slide_contents: &[(u32, String)],
        formulas: &[(String, bool)],
        plain_output: bool,
        mime_type: &str,
        extract_images: bool,
        budget: &mut SecurityBudget,
    ) -> Result<InternalDocument> {
        let mut additional: AHashMap<Cow<'static, str>, serde_json::Value> = AHashMap::new();

        let mut pptx_metadata = pptx_result.metadata;
        pptx_metadata.image_count = Some(pptx_result.image_count as u32);
        pptx_metadata.table_count = Some(pptx_result.table_count as u32);

        let office_meta = &pptx_result.office_metadata;
        let title = office_meta.get("title").cloned();
        let subject = office_meta.get("subject").cloned();
        let created_by = office_meta.get("created_by").cloned();
        let modified_by = office_meta.get("modified_by").cloned();
        let created_at = office_meta.get("created_at").cloned();
        let modified_at = office_meta.get("modified_at").cloned();
        let authors = office_meta.get("author").map(|a| vec![a.clone()]);
        let keywords = office_meta.get("keywords").map(|k| {
            k.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });

        for (key, value) in &pptx_result.office_metadata {
            match key.as_str() {
                "title" | "subject" | "created_by" | "modified_by" | "created_at" | "modified_at" | "author"
                | "keywords" => {}
                "slide_count" => {}
                "notes_count" | "hidden_slides" => {
                    let json_value = value
                        .parse::<u64>()
                        .map(|n| serde_json::Value::Number(n.into()))
                        .unwrap_or_else(|_| serde_json::json!(value));
                    additional.insert(Cow::Owned(key.clone()), json_value);
                }
                _ => {
                    additional.insert(Cow::Owned(key.clone()), serde_json::json!(value));
                }
            }
        }

        let mut doc = Self::build_internal_document(
            slide_contents,
            pptx_result.slide_count as u32,
            formulas,
            plain_output,
            budget,
        )?;
        doc.mime_type = mime_type.to_string();

        let mut metadata = Metadata {
            title,
            subject,
            authors,
            keywords,
            created_at,
            modified_at,
            created_by,
            modified_by,
            format: Some(crate::types::FormatMetadata::Pptx(pptx_metadata)),
            additional,
            ..Default::default()
        };

        if let Some(page_structure) = pptx_result.page_structure {
            metadata.pages = Some(page_structure);
        }

        doc.metadata = metadata;

        for hyperlink in pptx_result.hyperlinks {
            doc.push_uri(ExtractedUri::hyperlink(&hyperlink.url, hyperlink.label));
        }

        doc.prebuilt_pages = pptx_result.page_contents;

        doc.revisions = pptx_result.revisions;

        if extract_images {
            doc.images = pptx_result.images;
            promote_baked_image_references(&mut doc);
            strip_repeated_decoration_images(&mut doc);
        }

        Ok(doc)
    }
}

/// Drop watermark/logo images that repeat across many slides.
///
/// Corporate decks stamp the same logo onto a large share of slides; each copy
/// becomes its own image element and its own (often OCR-empty) fence in the
/// output, burying the slide content under repeated decoration. An image whose
/// exact bytes appear on at least a quarter of the slides — and on at least
/// three — is decoration and its element is removed; unique diagrams and
/// screenshots never meet that bar. Only the referencing elements are dropped:
/// the image data itself stays in `doc.images`, so downstream consumers that
/// read the image list directly are unaffected.
fn strip_repeated_decoration_images(doc: &mut InternalDocument) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let slide_count = doc.elements.iter().filter_map(|element| element.page).max().unwrap_or(0);
    if slide_count < 3 {
        return;
    }
    let minimum_slides = (((slide_count as f64) * 0.25).ceil() as u32).max(3);

    // Slides referencing each image index.
    let mut slides_per_image: ahash::AHashMap<u32, std::collections::BTreeSet<u32>> = ahash::AHashMap::new();
    for element in doc.elements.iter() {
        if let crate::types::internal::ElementKind::Image { image_index } = element.kind
            && let Some(page) = element.page
        {
            slides_per_image.entry(image_index).or_default().insert(page);
        }
    }

    // Slides covered per content hash (several indexes can share one logo's bytes),
    // plus each image's hash: the retain below then decides from that map instead of
    // re-hashing the image bytes once per referencing element.
    let mut slides_per_hash: ahash::AHashMap<u64, std::collections::BTreeSet<u32>> = ahash::AHashMap::new();
    let mut hash_by_index: ahash::AHashMap<u32, u64> = ahash::AHashMap::new();
    for (image_index, image) in doc.images.iter().enumerate() {
        let Some(referencing) = slides_per_image.get(&(image_index as u32)) else {
            continue;
        };
        let mut hasher = DefaultHasher::new();
        image.data.hash(&mut hasher);
        let hash = hasher.finish();
        hash_by_index.insert(image_index as u32, hash);
        slides_per_hash.entry(hash).or_default().extend(referencing.iter().copied());
    }
    let decoration_hashes: std::collections::HashSet<u64> = slides_per_hash
        .into_iter()
        .filter(|(_, slides)| slides.len() as u32 >= minimum_slides)
        .map(|(hash, _)| hash)
        .collect();
    if decoration_hashes.is_empty() {
        return;
    }

    // Every index sharing a decoration image's bytes gets the same (dropped) decision.
    let decoration_indexes: ahash::AHashSet<u32> = hash_by_index
        .into_iter()
        .filter(|(_, hash)| decoration_hashes.contains(hash))
        .map(|(index, _)| index)
        .collect();
    doc.elements.retain(|element| {
        !matches!(element.kind, crate::types::internal::ElementKind::Image { image_index } if decoration_indexes.contains(&image_index))
    });
}

/// Turn the `![alt](target)` placeholders the PPTX content builder bakes into slide text into
/// real image elements.
///
/// A placeholder left as paragraph text is escaped by the Markdown writer (`\![alt](target)`)
/// and points at a path inside the package (`../media/image4.png`), so it is neither a live
/// image reference nor resolvable next to the extracted output. Rendering from an image element
/// yields the same `![alt](image_N.ext)` form DOCX/PDF already produce, naming the file the
/// extractor writes.
///
/// Each placeholder carries the package target it was baked from, and every extracted image
/// records that target as its `source_path`, so the two are matched by identity. A positional
/// pairing cannot work: the builder emits placeholders in slide geometry order (`y`, then `x`)
/// while `doc.images` follows slide XML order, and an image whose bytes could not be read is
/// skipped when the images are collected — either one shifts every later placeholder onto the
/// wrong picture and alt text.
///
/// One paragraph can hold more than one placeholder — two pictures side by side on a slide are
/// written into the same paragraph — and the slide's own text can share it. Promoting one
/// paragraph as a single image dropped the further references (and the text with them), so this
/// walks every reference in the paragraph, emits one image element per reference in order, and
/// keeps the surrounding text as its own paragraph. A placeholder whose image could not be read
/// is left as text rather than dropped, as before. ~keep
fn promote_baked_image_references(doc: &mut InternalDocument) {
    use crate::types::internal::ElementKind;

    let mut used = vec![false; doc.images.len()];
    let elements = std::mem::take(&mut doc.elements);
    let mut promoted = Vec::with_capacity(elements.len());

    for elem in elements {
        if !matches!(elem.kind, ElementKind::Paragraph) {
            promoted.push(elem);
            continue;
        }
        let references = markdown_image_references(&elem.text);
        if references.is_empty() {
            promoted.push(elem);
            continue;
        }

        let mut cursor = 0usize;
        let leftover = |text: &str,
                            cursor: &mut usize,
                            end: usize,
                            elem: &crate::types::internal::InternalElement| {
            let slice = text[*cursor..end].trim();
            *cursor = end;
            if slice.is_empty() {
                None
            } else {
                let mut paragraph =
                    crate::types::internal::InternalElement::text(ElementKind::Paragraph, slice, elem.depth);
                paragraph.page = elem.page;
                paragraph.bbox = elem.bbox;
                paragraph.layer = elem.layer;
                Some(paragraph)
            }
        };

        for (range, alt, target) in references {
            let Some(image_index) = take_image_for_target(&doc.images, &used, &target) else {
                // No extracted image for this placeholder (its bytes were unreadable, or it is
                // the slide's own text that happens to look like a reference): leave it as text.
                continue;
            };
            if let Some(paragraph) = leftover(&elem.text, &mut cursor, range.start, &elem) {
                promoted.push(paragraph);
            }
            let description = &mut doc.images[image_index].description;
            if description.as_deref().map(str::trim).unwrap_or("").is_empty() && !alt.is_empty() {
                *description = Some(alt);
            }
            let mut image = elem.clone();
            image.kind = ElementKind::Image {
                image_index: image_index as u32,
            };
            image.text = doc.images[image_index].description.clone().unwrap_or_default();
            image.annotations = Vec::new();
            promoted.push(image);
            cursor = range.end;
            used[image_index] = true;
        }

        if let Some(paragraph) = leftover(&elem.text, &mut cursor, elem.text.len(), &elem) {
            promoted.push(paragraph);
        }
    }

    doc.elements = promoted;
}

/// Byte ranges and alt texts of every `![alt](target)` reference in `text`.
///
/// The pipeline's `is_markdown_image_reference` only answers whether a whole string *is* one
/// reference, which is all the callers that rewrite pre-rendered text need; this walks the
/// string, so a paragraph holding several placeholders yields all of them.
fn markdown_image_references(text: &str) -> Vec<(std::ops::Range<usize>, String, String)> {
    let mut found = Vec::new();
    let mut from = 0usize;
    while let Some(open) = text[from..].find("![") {
        let start = from + open;
        let Some(alt_close) = text[start + 2..].find("](").map(|offset| start + 2 + offset) else {
            break;
        };
        let Some(target_close) = text[alt_close + 2..].find(')').map(|offset| alt_close + 2 + offset) else {
            break;
        };
        found.push((
            start..target_close + 1,
            text[start + 2..alt_close].trim().to_string(),
            text[alt_close + 2..target_close].trim().to_string(),
        ));
        from = target_close + 1;
    }
    found
}

/// Index of the extracted image a placeholder refers to.
///
/// The placeholder was baked from the slide's rel target and the image records that same target
/// as its `source_path`, so the match is by identity: positional pairing cannot survive the two
/// orders differing (placeholders follow slide geometry, images follow slide XML) or an unreadable
/// image being skipped.
///
/// A placeholder whose baked target is empty — the slide referenced an image whose rel could not
/// be resolved — carries no identity to match on, so it keeps the previous positional behaviour.
/// A placeholder that names a target no image claims is the slide's own text (or an image whose
/// bytes could not be read) and stays text rather than taking a neighbour's picture.
fn take_image_for_target(
    images: &[crate::types::ExtractedImage],
    used: &[bool],
    target: &str,
) -> Option<usize> {
    let unused = |index: usize| !used.get(index).copied().unwrap_or(true);

    images
        .iter()
        .enumerate()
        .find(|(index, image)| {
            unused(*index) && image.source_path.as_deref().is_some_and(|path| path.trim() == target)
        })
        .map(|(index, _)| index)
        .or_else(|| {
            if !target.is_empty() {
                return None;
            }
            images.iter().enumerate().find(|(index, _)| unused(*index)).map(|(index, _)| index)
        })
}

impl Plugin for PptxExtractor {
    fn name(&self) -> &str {
        "pptx-extractor"
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
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl InternalDocumentExtractor for PptxExtractor {
    async fn extract_content(
        &self,
        content: &[u8],
        mime_type: &str,
        config: &ExtractionConfig,
    ) -> Result<InternalDocument> {
        tracing::debug!(format = "pptx", size_bytes = content.len(), "extraction starting");
        let extract_images = config.needs_image_data();
        let inject_placeholders = config
            .images
            .as_ref()
            .map(|img| img.inject_placeholders)
            .unwrap_or(true);
        let plain = matches!(config.output_format, crate::core::config::OutputFormat::Plain);
        let security_limits = config.security_limits.clone().unwrap_or_default();
        let max_pages = security_limits.max_pages;

        let mut pptx_warnings: Vec<crate::types::ProcessingWarning> = Vec::new();

        let pptx_internal = {
            #[cfg(feature = "tokio-runtime")]
            {
                if crate::core::batch_mode::is_batch_mode() {
                    if config.cancel_token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
                        return Err(crate::error::XbergError::Cancelled);
                    }
                    let content_owned = content.to_vec();
                    let options = crate::extraction::pptx::PptxExtractionOptions {
                        extract_images,
                        page_config: config.pages.clone(),
                        plain,
                        include_structure: false,
                        inject_placeholders,
                        security_limits: security_limits.clone(),
                        max_pages,
                    };
                    let span = tracing::Span::current();
                    let (result, warnings) = tokio::task::spawn_blocking(move || {
                        let _guard = span.entered();
                        let mut warnings = Vec::new();
                        let result = crate::extraction::pptx::extract_pptx_from_bytes_with_slide_contents(
                            &content_owned,
                            &options,
                            &mut warnings,
                        );
                        (result, warnings)
                    })
                    .await
                    .map_err(|e| crate::error::XbergError::parsing(format!("PPTX extraction task failed: {}", e)))?;
                    pptx_warnings = warnings;
                    result?
                } else {
                    let options = crate::extraction::pptx::PptxExtractionOptions {
                        extract_images,
                        page_config: config.pages.clone(),
                        plain,
                        include_structure: false,
                        inject_placeholders,
                        security_limits: security_limits.clone(),
                        max_pages,
                    };
                    crate::extraction::pptx::extract_pptx_from_bytes_with_slide_contents(
                        content,
                        &options,
                        &mut pptx_warnings,
                    )?
                }
            }

            #[cfg(not(feature = "tokio-runtime"))]
            {
                let options = crate::extraction::pptx::PptxExtractionOptions {
                    extract_images,
                    page_config: config.pages.clone(),
                    plain,
                    include_structure: false,
                    inject_placeholders,
                    security_limits: security_limits.clone(),
                    max_pages,
                };
                crate::extraction::pptx::extract_pptx_from_bytes_with_slide_contents(
                    content,
                    &options,
                    &mut pptx_warnings,
                )?
            }
        };

        let mut budget = SecurityBudget::from_config(config);
        let mut doc = Self::build_document_from_result(
            pptx_internal.result,
            &pptx_internal.slide_contents,
            &pptx_internal.formulas,
            pptx_internal.plain_output,
            mime_type,
            extract_images,
            &mut budget,
        )?;
        doc.processing_warnings.extend(pptx_warnings);

        if config.max_archive_depth > 0 {
            let (children, embed_warnings) = crate::extraction::ooxml_embedded::extract_ooxml_embedded_objects(
                content,
                "ppt/embeddings/",
                "pptx",
                config,
            )
            .await;
            if !children.is_empty() {
                doc.children = Some(children);
                crate::extraction::ooxml_embedded::append_embedded_object_text(&mut doc);
            }
            doc.processing_warnings.extend(embed_warnings);
        }

        tracing::debug!(
            element_count = doc.elements.len(),
            format = "pptx",
            "extraction complete"
        );
        Ok(doc)
    }

    #[cfg_attr(feature = "otel", tracing::instrument(
        skip(self, path, config),
        fields(
            extractor.name = self.name(),
        )
    ))]
    async fn extract_path(&self, path: &Path, mime_type: &str, config: &ExtractionConfig) -> Result<InternalDocument> {
        let path_str = path
            .to_str()
            .ok_or_else(|| crate::XbergError::validation("Invalid file path".to_string()))?;

        let extract_images = config.needs_image_data();
        let inject_placeholders = config
            .images
            .as_ref()
            .map(|img| img.inject_placeholders)
            .unwrap_or(true);
        let plain = matches!(config.output_format, crate::core::config::OutputFormat::Plain);
        let security_limits = config.security_limits.clone().unwrap_or_default();

        let options = crate::extraction::pptx::PptxExtractionOptions {
            extract_images,
            page_config: config.pages.clone(),
            plain,
            include_structure: false,
            inject_placeholders,
            security_limits: security_limits.clone(),
            max_pages: security_limits.max_pages,
        };
        let mut pptx_warnings: Vec<crate::types::ProcessingWarning> = Vec::new();
        let pptx_internal = crate::extraction::pptx::extract_pptx_from_path_with_slide_contents(
            path_str,
            &options,
            &mut pptx_warnings,
        )?;

        let mut budget = SecurityBudget::from_config(config);
        let mut doc = Self::build_document_from_result(
            pptx_internal.result,
            &pptx_internal.slide_contents,
            &pptx_internal.formulas,
            pptx_internal.plain_output,
            mime_type,
            extract_images,
            &mut budget,
        )?;
        doc.processing_warnings.extend(pptx_warnings);

        // `extract_content` extracts `ppt/embeddings/*` children, but the CLI (and any caller
        // that hands PPTX extractors a path) goes through `extract_path`, which reads and
        // parses the archive itself. Without this block, embedded OLE objects — the Visio/
        // Excel/`.bin` packages the slides carry — silently produce no children on that path.
        // Only the bytes are needed, so re-read the file here rather than widening the
        // extraction API; a read failure is reported as a warning, not a hard failure.
        if config.max_archive_depth > 0 {
            match std::fs::read(path) {
                Ok(content) => {
                    let (children, embed_warnings) =
                        crate::extraction::ooxml_embedded::extract_ooxml_embedded_objects(
                            &content,
                            "ppt/embeddings/",
                            "pptx",
                            config,
                        )
                        .await;
                    if !children.is_empty() {
                        doc.children = Some(children);
                        crate::extraction::ooxml_embedded::append_embedded_object_text(&mut doc);
                    }
                    doc.processing_warnings.extend(embed_warnings);
                }
                Err(error) => {
                    doc.processing_warnings.push(crate::types::ProcessingWarning {
                        source: Cow::Borrowed("pptx"),
                        message: Cow::Owned(format!(
                            "embedded objects were not extracted: could not re-read '{}': {}",
                            path.display(),
                            error
                        )),
                    });
                }
            }
        }

        Ok(doc)
    }

    fn supported_mime_types(&self) -> &[&str] {
        &[
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/vnd.ms-powerpoint.presentation.macroEnabled.12",
            "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
            "application/vnd.openxmlformats-officedocument.presentationml.template",
            "application/vnd.ms-powerpoint.template.macroEnabled.12",
        ]
    }

    fn priority(&self) -> i32 {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slide with math: the LaTeX must reach `ExtractedDocument.formulas`, not
    /// only the text. The deck holds display math in its own shape, inline math
    /// beside text, and a `$` amount that is not math at all.
    #[tokio::test]
    async fn test_slide_math_populates_formulas() {
        use crate::core::config::ExtractionConfig;
        use crate::extraction::derive::derive_extraction_result;
        use crate::plugins::InternalDocumentExtractor;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"
       xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"
       xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
    <p:cSld><p:spTree>
        <p:sp><p:txBody>
            <a:p><a:r><a:t>Budget is $5 per unit</a:t></a:r></a:p>
        </p:txBody></p:sp>
        <p:sp><p:txBody>
            <a:p>
                <mc:AlternateContent>
                    <mc:Choice Requires="a14"><a14:m>
                        <m:oMathPara><m:oMath><m:sSup>
                            <m:e><m:r><m:t>x</m:t></m:r></m:e>
                            <m:sup><m:r><m:t>2</m:t></m:r></m:sup>
                        </m:sSup></m:oMath></m:oMathPara>
                    </a14:m></mc:Choice>
                    <mc:Fallback><a:r><a:t>[equation]</a:t></a:r></mc:Fallback>
                </mc:AlternateContent>
            </a:p>
            <a:p>
                <a:r><a:t>Rate </a:t></a:r>
                <mc:AlternateContent>
                    <mc:Choice Requires="a14"><a14:m>
                        <m:oMath><m:r><m:t>a</m:t></m:r></m:oMath>
                    </a14:m></mc:Choice>
                    <mc:Fallback><a:r><a:t>[a]</a:t></a:r></mc:Fallback>
                </mc:AlternateContent>
                <a:r><a:t> per hour</a:t></a:r>
            </a:p>
        </p:txBody></p:sp>
    </p:spTree></p:cSld>
</p:sld>"#;

        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Markdown,
            ..Default::default()
        };
        let internal_doc = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("extraction failed");
        let result = derive_extraction_result(internal_doc, false, crate::core::config::OutputFormat::Markdown);

        let latex: Vec<&str> = result.formulas.iter().map(|f| f.latex.as_str()).collect();
        assert_eq!(latex, vec!["x^{2}", "a"], "both math runs reach formulas");
        assert!(
            result.content.contains("Budget is $5 per unit"),
            "a dollar amount stays text, got: {:?}",
            result.content
        );
        assert!(
            result.content.contains("Rate per hour"),
            "the text around inline math survives, got: {:?}",
            result.content
        );
    }

    /// A deck written by a tool other than PowerPoint puts the math straight
    /// into the paragraph, with no `a14:m` wrapper and no `mc:AlternateContent`.
    #[tokio::test]
    async fn test_bare_omml_in_a_paragraph_populates_formulas() {
        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
    <p:cSld><p:spTree>
        <p:sp><p:txBody>
            <a:p>
                <m:oMathPara><m:oMath><m:sSup>
                    <m:e><m:r><m:t>y</m:t></m:r></m:e>
                    <m:sup><m:r><m:t>3</m:t></m:r></m:sup>
                </m:sSup></m:oMath></m:oMathPara>
            </a:p>
        </p:txBody></p:sp>
    </p:spTree></p:cSld>
</p:sld>"#;

        assert_eq!(slide_formulas(slide_xml).await, vec!["y^{3}"]);
    }

    /// PowerPoint writes the equation as `a14:m` in its 2010 drawing namespace,
    /// with no compatibility wrapper. A real deck extracted to nothing before.
    #[tokio::test]
    async fn test_drawing_extension_math_without_a_compatibility_wrapper() {
        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"
       xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
    <p:cSld><p:spTree>
        <p:sp><p:txBody>
            <a:p><a14:m>
                <m:oMathPara><m:oMath><m:sSup>
                    <m:e><m:r><m:t>e</m:t></m:r></m:e>
                    <m:sup><m:r><m:t>x</m:t></m:r></m:sup>
                </m:sSup></m:oMath></m:oMathPara>
            </a14:m></a:p>
        </p:txBody></p:sp>
    </p:spTree></p:cSld>
</p:sld>"#;

        assert_eq!(slide_formulas(slide_xml).await, vec!["e^{x}"]);
    }

    /// Bare inline math sits beside the words of its sentence. The equation
    /// becomes a formula and the words keep their spacing.
    #[tokio::test]
    async fn test_bare_inline_omml_leaves_the_sentence_intact() {
        use crate::core::config::ExtractionConfig;
        use crate::extraction::derive::derive_extraction_result;
        use crate::plugins::InternalDocumentExtractor;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
    <p:cSld><p:spTree>
        <p:sp><p:txBody>
            <a:p>
                <a:r><a:t>Speed </a:t></a:r>
                <m:oMath><m:r><m:t>v</m:t></m:r></m:oMath>
                <a:r><a:t> in metres</a:t></a:r>
            </a:p>
        </p:txBody></p:sp>
    </p:spTree></p:cSld>
</p:sld>"#;

        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Markdown,
            ..Default::default()
        };
        let internal_doc = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("extraction failed");
        let result = derive_extraction_result(internal_doc, false, crate::core::config::OutputFormat::Markdown);

        let latex: Vec<&str> = result.formulas.iter().map(|f| f.latex.as_str()).collect();
        assert_eq!(latex, vec!["v"], "the inline equation becomes a formula");
        assert!(
            result.content.contains("Speed in metres"),
            "the sentence keeps one space where the equation left it, got: {:?}",
            result.content
        );
    }

    /// Extract one slide and return the LaTeX of every formula it yields.
    async fn slide_formulas(slide_xml: &str) -> Vec<String> {
        use crate::core::config::ExtractionConfig;
        use crate::extraction::derive::derive_extraction_result;
        use crate::plugins::InternalDocumentExtractor;

        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Markdown,
            ..Default::default()
        };
        let internal_doc = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("extraction failed");
        derive_extraction_result(internal_doc, false, crate::core::config::OutputFormat::Markdown)
            .formulas
            .iter()
            .map(|f| f.latex.clone())
            .collect()
    }

    /// Plain output carries no math delimiters, so a shape that holds nothing but
    /// math is still recognized by its exact LaTeX. Math mixed into a line of text
    /// stays in that line.
    #[tokio::test]
    async fn test_standalone_slide_math_populates_formulas_in_plain_output() {
        use crate::core::config::ExtractionConfig;
        use crate::extraction::derive::derive_extraction_result;
        use crate::plugins::InternalDocumentExtractor;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"
       xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"
       xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
    <p:cSld><p:spTree>
        <p:sp><p:txBody>
            <a:p><a:r><a:t>Energy of a body at rest</a:t></a:r></a:p>
        </p:txBody></p:sp>
        <p:sp><p:txBody>
            <a:p>
                <mc:AlternateContent>
                    <mc:Choice Requires="a14"><a14:m>
                        <m:oMathPara><m:oMath><m:sSup>
                            <m:e><m:r><m:t>x</m:t></m:r></m:e>
                            <m:sup><m:r><m:t>2</m:t></m:r></m:sup>
                        </m:sSup></m:oMath></m:oMathPara>
                    </a14:m></mc:Choice>
                    <mc:Fallback><a:r><a:t>[equation]</a:t></a:r></mc:Fallback>
                </mc:AlternateContent>
            </a:p>
        </p:txBody></p:sp>
    </p:spTree></p:cSld>
</p:sld>"#;

        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let internal_doc = extractor
            .extract_content(&pptx, mime, &ExtractionConfig::default())
            .await
            .expect("extraction failed");
        let result = derive_extraction_result(internal_doc, false, crate::core::config::OutputFormat::Plain);

        let latex: Vec<&str> = result.formulas.iter().map(|f| f.latex.as_str()).collect();
        assert_eq!(latex, vec!["x^{2}"]);
    }

    #[test]
    fn test_split_line_math_pulls_delimited_spans() {
        let formulas = vec![("x^{2}".to_string(), true), ("a".to_string(), false)];
        let forms = PptxExtractor::math_forms(&formulas);

        let (text, found) = PptxExtractor::split_line_math("Rate $a$ per hour", &forms, &formulas, false);
        assert_eq!(text, "Rate per hour");
        assert_eq!(found, vec!["a".to_string()]);

        let (text, found) = PptxExtractor::split_line_math("$$x^{2}$$", &forms, &formulas, false);
        assert_eq!(text, "");
        assert_eq!(found, vec!["x^{2}".to_string()]);
    }

    #[test]
    fn test_split_line_math_keeps_plain_dollar_text() {
        let formulas = vec![("a".to_string(), false)];
        let forms = PptxExtractor::math_forms(&formulas);

        let (text, found) = PptxExtractor::split_line_math("Budget is $5 per unit", &forms, &formulas, false);
        assert_eq!(text, "Budget is $5 per unit");
        assert!(found.is_empty(), "author text with a dollar sign is not math");
    }

    #[test]
    fn test_split_line_math_matches_undelimited_plain_output() {
        let formulas = vec![("x^{2}".to_string(), true)];
        let forms = PptxExtractor::math_forms(&formulas);

        let (text, found) = PptxExtractor::split_line_math("x^{2}", &forms, &formulas, true);
        assert_eq!(text, "");
        assert_eq!(found, vec!["x^{2}".to_string()], "plain output carries no delimiters");
    }

    /// Markdown text keeps its delimiters, so a line that merely repeats a
    /// formula's characters is author text, not math.
    #[test]
    fn test_split_line_math_leaves_undelimited_line_in_markdown_text() {
        let formulas = vec![("n".to_string(), false)];
        let forms = PptxExtractor::math_forms(&formulas);

        let (text, found) = PptxExtractor::split_line_math("n", &forms, &formulas, false);
        assert_eq!(text, "n");
        assert!(found.is_empty());
    }

    /// Math inside a bulleted line becomes its own element, and the bullet keeps
    /// its words.
    #[test]
    fn test_build_internal_document_lifts_list_item_and_title_math() {
        use crate::types::internal::ElementKind;

        let content = "# Growth is $$g^{2}$$\n\n- Rate $r$ per year\n- Plain bullet\n";
        let formulas = vec![("g^{2}".to_string(), true), ("r".to_string(), false)];
        let mut budget = SecurityBudget::with_defaults();
        let doc = PptxExtractor::build_internal_document(&[(1, content.to_string())], 1, &formulas, false, &mut budget)
            .unwrap();

        let math: Vec<&str> = doc
            .elements
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Formula))
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(math, vec!["g^{2}", "r"], "title and list-item math both emit");

        let items: Vec<&str> = doc
            .elements
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::ListItem { .. }))
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(items, vec!["Rate per year", "Plain bullet"]);

        let headings: Vec<&str> = doc
            .elements
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Heading { .. }))
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(headings, vec!["Growth is"], "the heading keeps its words");
    }

    #[test]
    fn test_pptx_extractor_plugin_interface() {
        let extractor = PptxExtractor::new();
        assert_eq!(extractor.name(), "pptx-extractor");
        assert!(extractor.initialize().is_ok());
        assert!(extractor.shutdown().is_ok());
    }

    #[test]
    fn test_pptx_extractor_supported_mime_types() {
        let extractor = PptxExtractor::new();
        let mime_types = extractor.supported_mime_types();
        assert_eq!(mime_types.len(), 5);
        assert!(mime_types.contains(&"application/vnd.openxmlformats-officedocument.presentationml.presentation"));
    }

    #[test]
    fn test_archive_slide_contents_set_table_list_heading_and_paragraph_pages() {
        use crate::types::internal::ElementKind;

        let slide_contents = vec![
            (1, "# Titled slide\n\nIntroduction".to_string()),
            (
                2,
                concat!(
                    "This untitled slide has a paragraph long enough not to be inferred as a title. ",
                    "It deliberately contains more than one hundred characters in total."
                )
                .to_string(),
            ),
            (
                3,
                concat!(
                    "| Name | Value |\n",
                    "| --- | --- |\n",
                    "| answer | 42 |\n\n",
                    "- final item"
                )
                .to_string(),
            ),
        ];
        let mut budget = SecurityBudget::with_defaults();

        let document = PptxExtractor::build_internal_document(&slide_contents, 3, &[], false, &mut budget)
            .expect("internal PPTX document should build");

        assert_eq!(document.tables.len(), 1);
        assert_eq!(document.tables[0].page_number, 3);

        let list_item = document
            .elements
            .iter()
            .find(|element| matches!(element.kind, ElementKind::ListItem { .. }))
            .expect("list item should be present");
        assert_eq!(list_item.page, Some(3));

        let heading = document
            .elements
            .iter()
            .find(|element| matches!(element.kind, ElementKind::Heading { .. }))
            .expect("heading should be present");
        assert_eq!(heading.page, Some(1));

        let second_slide_paragraph = document
            .elements
            .iter()
            .find(|element| element.text.starts_with("This untitled slide"))
            .expect("second-slide paragraph should be present");
        assert_eq!(second_slide_paragraph.page, Some(2));
    }

    #[test]
    fn test_marker_like_slide_text_cannot_change_later_page_numbers() {
        let slide_contents = vec![
            (1, "First slide".to_string()),
            (
                2,
                "<!-- Slide number: 99 -->\n\n| Name | Value |\n| --- | --- |\n| answer | 42 |".to_string(),
            ),
        ];
        let mut budget = SecurityBudget::with_defaults();

        let document = PptxExtractor::build_internal_document(&slide_contents, 2, &[], false, &mut budget)
            .expect("marker-like user text should remain ordinary slide content");

        assert_eq!(document.tables.len(), 1);
        assert_eq!(document.tables[0].page_number, 2);
        assert!(document.elements.iter().any(|element| {
            matches!(element.kind, crate::types::internal::ElementKind::Paragraph)
                && element.text == "<!-- Slide number: 99 -->"
                && element.page == Some(2)
        }));
    }

    #[tokio::test]
    async fn test_untitled_slide_with_table_gets_archive_derived_page_numbers() {
        use crate::plugins::InternalDocumentExtractor;
        use crate::types::internal::ElementKind;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree>
        <p:sp><p:txBody><a:p><a:r><a:t>This untitled slide contains a deliberately long paragraph so the extractor cannot mistake it for a title while assigning page metadata.</a:t></a:r></a:p></p:txBody></p:sp>
        <p:graphicFrame>
            <a:graphic>
                <a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
                    <a:tbl>
                        <a:tr>
                            <a:tc><a:txBody><a:p><a:r><a:t>Name</a:t></a:r></a:p></a:txBody></a:tc>
                            <a:tc><a:txBody><a:p><a:r><a:t>Value</a:t></a:r></a:p></a:txBody></a:tc>
                        </a:tr>
                        <a:tr>
                            <a:tc><a:txBody><a:p><a:r><a:t>answer</a:t></a:r></a:p></a:txBody></a:tc>
                            <a:tc><a:txBody><a:p><a:r><a:t>42</a:t></a:r></a:p></a:txBody></a:tc>
                        </a:tr>
                    </a:tbl>
                </a:graphicData>
            </a:graphic>
        </p:graphicFrame>
    </p:spTree></p:cSld>
</p:sld>"#;
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);
        let extractor = PptxExtractor::new();
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Markdown,
            ..Default::default()
        };
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

        let document = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("real PPTX extraction should succeed");

        assert_eq!(document.tables.len(), 1);
        assert_eq!(document.tables[0].page_number, 1);
        assert!(document.elements.iter().any(|element| {
            matches!(element.kind, ElementKind::Paragraph)
                && element.text.starts_with("This untitled slide")
                && element.page == Some(1)
        }));
        assert!(
            document
                .elements
                .iter()
                .any(|element| { matches!(element.kind, ElementKind::Slide { number: 1 }) && element.page == Some(1) })
        );
    }

    /// Full round-trip through PptxExtractor::extract_bytes → derive_extraction_result →
    /// ExtractedDocument.pages, asserting that speaker_notes and section_name are present.
    #[tokio::test]
    async fn test_extract_bytes_populates_speaker_notes_and_section_name() {
        use crate::core::config::{ExtractionConfig, PageConfig};
        use crate::extraction::derive::derive_extraction_result;
        use crate::plugins::InternalDocumentExtractor;

        let pptx = crate::extraction::pptx::tests::create_pptx_with_sections_and_notes(
            &[
                ("Title", Some("Intro notes.")),
                ("Body", Some("Body notes.")),
                ("End", None),
            ],
            &[("Chapter 1", &[1, 2]), ("Chapter 2", &[3])],
        );

        let extractor = PptxExtractor::new();
        let config = ExtractionConfig {
            pages: Some(PageConfig::default()),
            ..Default::default()
        };
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let internal_doc = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("extraction failed");
        let result = derive_extraction_result(internal_doc, true, crate::core::config::OutputFormat::Plain);

        assert!(
            !result.content.contains("<!-- Slide number:"),
            "internal slide markers must not leak into rendered output"
        );
        let pages = result.pages.as_ref().expect("pages should be populated");
        assert_eq!(pages.len(), 3);

        assert_eq!(pages[0].speaker_notes.as_deref(), Some("Intro notes."));
        assert_eq!(pages[0].section_name.as_deref(), Some("Chapter 1"));

        assert_eq!(pages[1].speaker_notes.as_deref(), Some("Body notes."));
        assert_eq!(pages[1].section_name.as_deref(), Some("Chapter 1"));

        assert!(pages[2].speaker_notes.is_none());
        assert_eq!(pages[2].section_name.as_deref(), Some("Chapter 2"));
    }

    /// GH#639: PPTX had no top-level archive entry-count check at all, so this fails
    /// against the unfixed code regardless of the configured limit (it never raises).
    #[tokio::test]
    async fn test_pptx_extract_content_honours_configured_archive_entry_limit() {
        use crate::core::config::ExtractionConfig;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        let extra_parts: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("ppt/extra_{}.xml", i), b"<x/>".to_vec()))
            .collect();
        let extra_refs: Vec<(&str, &[u8])> = extra_parts.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &extra_refs);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 3,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        assert!(
            result.is_err(),
            "an archive with more entries than the configured max_files_in_archive must be rejected"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains('3'),
            "error should mention the configured limit (3), got: {}",
            err_msg
        );
    }

    /// Sibling of the rejection test above: the same archive shape, but under a
    /// configured limit that comfortably fits it, must still extract successfully.
    #[tokio::test]
    async fn test_pptx_extract_content_succeeds_under_configured_archive_entry_limit() {
        use crate::core::config::ExtractionConfig;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        let extra_parts: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("ppt/extra_{}.xml", i), b"<x/>".to_vec()))
            .collect();
        let extra_refs: Vec<(&str, &[u8])> = extra_parts.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &extra_refs);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 50,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        assert!(
            result.is_ok(),
            "an archive within the configured max_files_in_archive must extract successfully: {:?}",
            result.err()
        );
    }

    /// A normal presentation with no `security_limits` override must still extract
    /// successfully under the default `SecurityLimits::max_files_in_archive`.
    #[tokio::test]
    async fn test_pptx_extract_content_succeeds_under_default_archive_entry_limit() {
        use crate::core::config::ExtractionConfig;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Default</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig::default();

        let result = extractor.extract_content(&pptx, mime, &config).await;
        assert!(
            result.is_ok(),
            "a normal presentation must extract under the default archive entry limit: {:?}",
            result.err()
        );
    }

    /// Gap: PPTX had `check_entry_count` (file count only) but no `ZipBombValidator`
    /// at all, so nothing ever checked aggregate declared uncompressed size or
    /// compression ratio -- unlike ODT/ODP (see `extractors::odt`/`extractors::odp`).
    /// Against unfixed code this test fails: `PptxContainer::open`/`from_bytes` never
    /// call `ZipBombValidator::validate`, so a highly compressible member sails
    /// through regardless of `max_compression_ratio`, and `extract_content` returns
    /// `Ok`.
    #[tokio::test]
    async fn test_pptx_extract_content_rejects_high_compression_ratio_archive() {
        use crate::core::config::ExtractionConfig;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        // 64 KiB of a single repeated byte, deflated: compresses far past any sane
        // ratio in a few hundred bytes, so the archive stays tiny while the ratio
        // comparison still fires.
        let bomb_payload = vec![0u8; 64 * 1024];
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(
            slide_xml,
            None,
            &[("ppt/media/bomb.bin", &bomb_payload)],
        );

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_compression_ratio: 5,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        let err = result.expect_err("a highly compressible member must be rejected under a low ratio limit");
        assert!(
            matches!(err, crate::error::XbergError::Security { .. }),
            "expected XbergError::Security, got: {err:?}"
        );
        assert!(
            err.to_string().to_lowercase().contains("ratio") || err.to_string().contains("ZIP bomb"),
            "error should name the ratio violation, got: {err}"
        );
    }

    /// Sibling of the ratio test above, bounding aggregate declared uncompressed
    /// size instead. Against unfixed code this also fails (no `max_archive_size`
    /// check existed for PPTX at all).
    #[tokio::test]
    async fn test_pptx_extract_content_rejects_archive_exceeding_max_size() {
        use crate::core::config::ExtractionConfig;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        // Incompressible-ish payload (pseudo-random via a simple LCG) so the entry's
        // declared uncompressed size is what trips the limit, not the ratio check.
        let mut state: u32 = 0x1234_5678;
        let payload: Vec<u8> = (0..8192)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (state >> 16) as u8
            })
            .collect();
        let extra_parts: [(&str, &[u8]); 1] = [("ppt/media/big.bin", &payload)];
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &extra_parts);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_archive_size: 1024,
                max_compression_ratio: usize::MAX,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        let err = result.expect_err("an archive declaring more bytes than max_archive_size must be rejected");
        assert!(
            matches!(err, crate::error::XbergError::Security { .. }),
            "expected XbergError::Security, got: {err:?}"
        );
    }

    /// Positive control for both tests above: a validator that rejects everything
    /// would pass the negative tests too, so this proves an ordinary presentation's
    /// exact text still extracts under the default `SecurityLimits`.
    #[tokio::test]
    async fn test_pptx_extract_content_positive_control_exact_text_under_default_limits() {
        use crate::core::config::ExtractionConfig;
        use crate::extraction::derive::derive_extraction_result;

        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>The quick brown fox jumps over the lazy dog.</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &[]);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig::default();

        let internal_doc = extractor
            .extract_content(&pptx, mime, &config)
            .await
            .expect("an ordinary presentation must extract under the default security limits");
        let result = derive_extraction_result(internal_doc, false, config.output_format);
        assert!(
            result.content.contains("The quick brown fox jumps over the lazy dog."),
            "extracted text must contain the source run's exact text, got: {:?}",
            result.content
        );
    }

    /// #1451: `max_pages` must reject a presentation once its slide count is known,
    /// before any per-slide work (text rendering, chart/diagram resolution) begins.
    /// Against unfixed code `PptxExtractionOptions` has no `max_pages` field, so this
    /// fails to compile; once the field exists but nothing reads it,
    /// `extract_content` would return `Ok` with 3 slides instead of the expected
    /// `SecurityError::TooManyPages`.
    #[tokio::test]
    async fn test_pptx_extract_content_rejects_presentation_exceeding_max_pages() {
        use crate::core::config::ExtractionConfig;

        let pptx = crate::extraction::pptx::tests::create_test_pptx_bytes(vec!["Slide 1", "Slide 2", "Slide 3"]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_pages: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        let error = result.expect_err("a presentation with more slides than max_pages must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("too many pages") || message.contains("max_pages"),
            "error must name the limit that was hit: {message}"
        );
    }

    /// A presentation exactly at the configured `max_pages` ceiling must extract in
    /// full -- a limit that rejects the boundary case too is not the fix #1451 asked
    /// for.
    #[tokio::test]
    async fn test_pptx_extract_content_succeeds_when_slide_count_is_at_max_pages() {
        use crate::core::config::ExtractionConfig;

        let pptx = crate::extraction::pptx::tests::create_test_pptx_bytes(vec!["Slide 1", "Slide 2", "Slide 3"]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_pages: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor.extract_content(&pptx, mime, &config).await;
        let doc = result.expect("a presentation exactly at max_pages must extract fully, not be rejected");
        assert!(
            !doc.elements.is_empty(),
            "extraction at the boundary must still produce content, not an empty truncated result"
        );
    }

    /// The default `SecurityLimits` (no override) must extract a multi-slide
    /// presentation exactly as before #1451: `max_pages` defaulting to anything
    /// other than `None` (unlimited) would silently start rejecting existing
    /// callers' presentations.
    #[tokio::test]
    async fn test_pptx_extract_content_succeeds_with_default_max_pages() {
        use crate::core::config::ExtractionConfig;

        let pptx = crate::extraction::pptx::tests::create_test_pptx_bytes(vec!["Slide 1", "Slide 2", "Slide 3"]);
        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig::default();

        let result = extractor.extract_content(&pptx, mime, &config).await;
        assert!(
            result.is_ok(),
            "default security limits must not reject a normal multi-slide presentation: {:?}",
            result.err()
        );
    }

    /// With no `security_limits` override the container must still enforce the default
    /// `SecurityLimits::max_files_in_archive`: "unset" means the default ceiling, not "no
    /// ceiling". One entry past that default must be rejected.
    #[tokio::test]
    async fn test_pptx_extract_content_rejects_archive_over_default_entry_limit() {
        use crate::core::config::ExtractionConfig;

        let default_limit = crate::extractors::security::SecurityLimits::default().max_files_in_archive;
        let slide_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
    <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;
        // The builder adds its own fixed parts, so this alone already exceeds the ceiling.
        let extra_parts: Vec<(String, Vec<u8>)> = (0..=default_limit)
            .map(|i| (format!("ppt/extra_{}.xml", i), b"<x/>".to_vec()))
            .collect();
        let extra_refs: Vec<(&str, &[u8])> = extra_parts.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let pptx = crate::extraction::pptx::tests::build_single_slide_pptx(slide_xml, None, &extra_refs);

        let extractor = PptxExtractor::new();
        let mime = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
        let config = ExtractionConfig::default();
        assert!(
            config.security_limits.is_none(),
            "this test must exercise the unset fallback, not an explicit limit"
        );

        let result = extractor.extract_content(&pptx, mime, &config).await;
        assert!(
            result.is_err(),
            "an archive over the default max_files_in_archive must be rejected when no limit is configured"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(&default_limit.to_string()),
            "error should mention the default limit ({default_limit}), got: {err_msg}"
        );
    }

    /// A logo stamped byte-identical onto a quarter of the slides is
    /// decoration: every element referencing it is removed, while a unique
    /// diagram and a logo that only repeats on too few slides survive.
    #[test]
    fn repeated_watermark_logos_are_dropped_across_slides() {
        use crate::types::ExtractedImage;
        use crate::types::internal::{ElementKind, InternalElement};
        use std::borrow::Cow;

        let logo = bytes::Bytes::from_static(b"LOGO-BYTES");
        let diagram = bytes::Bytes::from_static(b"DIAGRAM-BYTES");
        let rare = bytes::Bytes::from_static(b"RARE-LOGO-BYTES");
        let mut doc = InternalDocument::new("pptx");
        for slide in 1u32..=4u32 {
            let logo_index = doc.images.len() as u32;
            doc.images.push(ExtractedImage {
                data: logo.clone(),
                format: Cow::Borrowed("png"),
                image_index: logo_index,
                ..Default::default()
            });
            doc.push_element(
                InternalElement::text(
                    ElementKind::Image { image_index: logo_index },
                    "BD21298_",
                    0,
                )
                .with_page(slide),
            );
        }
        let diagram_index = doc.images.len() as u32;
        doc.images.push(ExtractedImage {
            data: diagram,
            format: Cow::Borrowed("png"),
            image_index: diagram_index,
            ..Default::default()
        });
        doc.push_element(
            InternalElement::text(ElementKind::Image { image_index: diagram_index }, "chart", 0).with_page(2),
        );
        let rare_index = doc.images.len() as u32;
        for slide in 1u32..=2u32 {
            doc.images.push(ExtractedImage {
                data: rare.clone(),
                format: Cow::Borrowed("png"),
                image_index: rare_index,
                ..Default::default()
            });
            doc.push_element(
                InternalElement::text(ElementKind::Image { image_index: rare_index }, "rare", 0)
                    .with_page(slide),
            );
        }

        strip_repeated_decoration_images(&mut doc);

        let kinds: Vec<ElementKind> = doc.elements.iter().map(|element| element.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ElementKind::Image { image_index: 4 },
                ElementKind::Image { image_index: 5 },
                ElementKind::Image { image_index: 5 },
            ],
            "the unique diagram and the rarely repeated logo must survive; got: {kinds:?}"
        );
    }

    /// Regression: two pictures written into one paragraph. Promoting the paragraph as a single
    /// image dropped the second picture and the slide text that shared the paragraph with it.
    #[test]
    fn promotes_every_baked_reference_in_a_paragraph() {
        use crate::types::ExtractedImage;
        use crate::types::internal::{ElementKind, InternalElement};
        use std::borrow::Cow;

        let mut doc = InternalDocument::new("pptx");
        doc.push_element(InternalElement::text(
            ElementKind::Paragraph,
            "![first](../media/image1.png)![second](../media/image2.wmf)尾随文字",
            0,
        ));
        doc.images = vec![
            ExtractedImage {
                format: Cow::Borrowed("png"),
                source_path: Some("../media/image1.png".to_string()),
                ..Default::default()
            },
            ExtractedImage {
                format: Cow::Borrowed("wmf"),
                source_path: Some("../media/image2.wmf".to_string()),
                ..Default::default()
            },
        ];

        promote_baked_image_references(&mut doc);

        let kinds: Vec<ElementKind> = doc.elements.iter().map(|elem| elem.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ElementKind::Image { image_index: 0 },
                ElementKind::Image { image_index: 1 },
                ElementKind::Paragraph,
            ],
            "each reference must become its own image element, in document order"
        );
        assert_eq!(doc.elements[0].text, "first", "alt text describes the first image");
        assert_eq!(doc.elements[1].text, "second", "alt text describes the second image");
        assert_eq!(
            doc.elements[2].text, "尾随文字",
            "text sharing the paragraph with the placeholders must survive"
        );
        assert_eq!(doc.images[0].description.as_deref(), Some("first"));
        assert_eq!(doc.images[1].description.as_deref(), Some("second"));
    }

    /// A paragraph holding exactly one placeholder keeps producing exactly one image element.
    #[test]
    fn promotes_a_lone_baked_reference() {
        use crate::types::ExtractedImage;
        use crate::types::internal::{ElementKind, InternalElement};
        use std::borrow::Cow;

        let mut doc = InternalDocument::new("pptx");
        doc.push_element(InternalElement::text(
            ElementKind::Paragraph,
            "![alt](../media/image7.png)",
            0,
        ));
        doc.images = vec![ExtractedImage {
            format: Cow::Borrowed("png"),
            source_path: Some("../media/image7.png".to_string()),
            ..Default::default()
        }];

        promote_baked_image_references(&mut doc);

        assert_eq!(doc.elements.len(), 1, "one placeholder must yield one element");
        assert_eq!(doc.elements[0].kind, ElementKind::Image { image_index: 0 });
    }
}
