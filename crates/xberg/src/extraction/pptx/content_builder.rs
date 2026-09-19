//! Content builder for accumulating slide output.
//!
//! This module provides utilities for building the final markdown content
//! from slide elements and managing page boundaries.

pub(super) struct ContentBuilder {
    pub(super) content: String,
    pub(super) boundaries: Vec<crate::types::PageBoundary>,
    pub(super) page_contents: Vec<crate::types::PageContent>,
    pub(super) config: Option<crate::core::config::PageConfig>,
    pub(super) plain: bool,
}

/// Indent unit for one level of list nesting.
///
/// The markdown writer emits an item's level as this many leading spaces per
/// level, and the second-stage parser in [`crate::extractors::pptx`] reads the
/// indentation back to rebuild the nesting — the two must agree, so the width is
/// defined once, here, where it is written.
pub(crate) const LIST_INDENT: &str = "  ";

impl ContentBuilder {
    pub(super) fn new(plain: bool) -> Self {
        Self {
            content: String::with_capacity(8192),
            boundaries: Vec::new(),
            page_contents: Vec::new(),
            config: None,
            plain,
        }
    }

    pub(super) fn with_page_config(
        capacity: usize,
        config: Option<crate::core::config::PageConfig>,
        plain: bool,
    ) -> Self {
        Self {
            content: String::with_capacity(capacity),
            boundaries: if config.is_some() {
                Vec::new()
            } else {
                Vec::with_capacity(0)
            },
            page_contents: if config.is_some() {
                Vec::new()
            } else {
                Vec::with_capacity(0)
            },
            config,
            plain,
        }
    }

    pub(super) fn start_slide(&mut self, slide_number: u32) -> usize {
        let byte_start = self.content.len();

        if let Some(ref cfg) = self.config
            && cfg.insert_page_markers
        {
            let marker = cfg.marker_format.replace("{page_num}", &slide_number.to_string());
            self.content.push_str(&marker);
        }

        byte_start
    }

    pub(super) fn end_slide(
        &mut self,
        slide_number: u32,
        byte_start: usize,
        slide_content: String,
        speaker_notes: Option<String>,
        section_name: Option<String>,
    ) {
        let byte_end = self.content.len();

        if self.config.is_some() {
            self.boundaries.push(crate::types::PageBoundary {
                byte_start,
                byte_end,
                page_number: slide_number,
            });

            let is_blank = Some(crate::extraction::blank_detection::is_page_text_blank(&slide_content));
            self.page_contents.push(crate::types::PageContent {
                page_number: slide_number,
                content: slide_content,
                tables: Vec::new(),
                image_indices: vec![],
                image_preprocessing: None,
                hierarchy: None,
                is_blank,
                layout_regions: None,
                speaker_notes,
                section_name,
                sheet_name: None,
                ocr_confidence: None,
            });
        }
    }

    pub(super) fn add_slide_header(&mut self, slide_number: u32) {
        self.content.reserve(50);
        self.content.push_str("\n\n<!-- Slide number: ");
        self.content.push_str(&slide_number.to_string());
        self.content.push_str(" -->\n");
    }

    pub(super) fn add_text(&mut self, text: &str) {
        if !text.trim().is_empty() {
            if !self.content.is_empty() && !self.content.ends_with("\n\n") {
                if !self.content.ends_with('\n') {
                    self.content.push('\n');
                }
                self.content.push('\n');
            }
            self.content.push_str(text);
            if !self.content.ends_with('\n') {
                self.content.push('\n');
            }
        }
    }

    pub(super) fn add_title(&mut self, title: &str) {
        if !title.trim().is_empty() {
            if !self.plain {
                self.content.push_str("# ");
            }
            self.content.push_str(title.trim());
            self.content.push_str("\n\n");
        }
    }

    pub(super) fn add_table(&mut self, rows: &[Vec<String>]) {
        if rows.is_empty() {
            return;
        }

        let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if num_cols == 0 {
            return;
        }

        self.content.push('\n');

        if self.plain {
            let owned: Vec<Vec<String>> = rows.to_vec();
            self.content.push_str(&crate::extraction::cells_to_text(&owned));
        } else {
            // A cell's `|` would end the cell mid-text when the Markdown is read
            // back (`PptxExtractor::parse_markdown_table` splits on unescaped
            // pipes), and a raw line break would split the row. Escape both,
            // exactly what the internal renderer's `push_escaped_cell` does to
            // its own tables.
            let rows: Vec<Vec<String>> = rows
                .iter()
                .map(|row| row.iter().map(|cell| Self::escape_table_cell(cell)).collect())
                .collect();

            let mut col_widths = vec![3usize; num_cols];
            for row in &rows {
                for (i, cell) in row.iter().enumerate() {
                    col_widths[i] = col_widths[i].max(cell.len());
                }
            }

            for (row_idx, row) in rows.iter().enumerate() {
                self.content.push('|');
                for (i, cell) in row.iter().enumerate() {
                    let width = col_widths.get(i).copied().unwrap_or(3);
                    self.content.push_str(&format!(" {:width$} |", cell, width = width));
                }
                for i in row.len()..num_cols {
                    let width = col_widths.get(i).copied().unwrap_or(3);
                    self.content.push_str(&format!(" {:width$} |", "", width = width));
                }
                self.content.push('\n');

                if row_idx == 0 {
                    self.content.push('|');
                    for i in 0..num_cols {
                        let width = col_widths.get(i).copied().unwrap_or(3);
                        self.content.push_str(&format!(" {} |", "-".repeat(width)));
                    }
                    self.content.push('\n');
                }
            }
        }
    }

    /// Escape one table cell for Markdown: a `\` is doubled and a `|` becomes `\|`
    /// so the reader keeps it inside the cell, and a line break becomes the same `<br>`
    /// stand-in the internal renderer uses
    /// ([`crate::rendering::common::CELL_LINE_BREAK`]).
    fn escape_table_cell(cell: &str) -> String {
        if !cell.contains(['|', '\n', '\r', '\\']) {
            return cell.to_string();
        }
        let mut out = String::with_capacity(cell.len());
        let mut chars = cell.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                // Doubled first: the reader restores `\|` to `|`, so a literal `\|` in
                // the cell must leave as `\\` + `\|`; a bare `\|` would come back as a
                // bare `|` with the backslash gone.
                '\\' => out.push_str("\\\\"),
                '|' => out.push_str("\\|"),
                '\r' => {
                    // Consume the LF of a CRLF pair so it yields one break, not two.
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push_str(crate::rendering::common::CELL_LINE_BREAK);
                }
                '\n' => out.push_str(crate::rendering::common::CELL_LINE_BREAK),
                _ => out.push(ch),
            }
        }
        out
    }

    pub(super) fn add_list_item(&mut self, level: u32, is_ordered: bool, text: &str) {
        if !self.plain {
            let indent_count = level.saturating_sub(1) as usize;
            for _ in 0..indent_count {
                self.content.push_str(LIST_INDENT);
            }

            let marker = if is_ordered { "1." } else { "-" };
            self.content.push_str(marker);
            self.content.push(' ');
        }
        self.content.push_str(text.trim());
        self.content.push('\n');
    }

    pub(super) fn add_image_with_desc(&mut self, _image_id: &str, description: Option<&str>, target: &str) {
        if !self.plain {
            let alt = description
                .map(|d| Self::escape_alt_text(&d.replace('\n', " ").replace('\r', "").trim()))
                .unwrap_or_default();
            let src = if target.is_empty() {
                String::new()
            } else {
                Self::escape_target_text(target)
            };
            if !self.content.is_empty() && !self.content.ends_with('\n') {
                self.content.push('\n');
            }
            self.content.push_str(&format!("![{}]({})\n", alt, src));
        }
    }

    /// Backslash-escape the alt-text characters that would end the `![` run early.
    ///
    /// A `]` closes the alt in CommonMark — and a `](` pair is also what the
    /// placeholder reader in `extractors::pptx::markdown_image_references`
    /// splits on — while a bare `\` would itself become an escape. Escaping
    /// `[`, `]` and `\` keeps an alt carrying them whole through the bake →
    /// promote round trip; the reader undoes exactly these escapes when it
    /// hands the alt back.
    fn escape_alt_text(alt: &str) -> String {
        let mut escaped = String::with_capacity(alt.len());
        for character in alt.chars() {
            if matches!(character, '[' | ']' | '\\') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        escaped
    }

    /// Backslash-escape the target characters that would end the destination
    /// scan early: the placeholder reader stops at the first unescaped `)`, so
    /// a rel target carrying one (`media/image (1).png`, as third-party
    /// producers write) would cut the destination short, never match its
    /// image's `source_path`, and leave the picture orphaned. A bare `\` is
    /// escaped for the same round trip; the reader undoes exactly these
    /// escapes before matching. A space stays as-is — it cannot break the
    /// reader, and promotion replaces the marker wholesale anyway.
    fn escape_target_text(target: &str) -> String {
        let mut escaped = String::with_capacity(target.len());
        for character in target.chars() {
            if matches!(character, '(' | ')' | '\\') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        escaped
    }

    pub(super) fn add_notes(&mut self, notes: &str) {
        if !notes.trim().is_empty() {
            if self.plain {
                self.content.push_str("\n\nNotes:\n");
            } else {
                self.content.push_str("\n\n### Notes:\n");
            }
            self.content.push_str(notes);
            self.content.push('\n');
        }
    }

    pub(super) fn build(
        self,
    ) -> (
        String,
        Option<Vec<crate::types::PageBoundary>>,
        Option<Vec<crate::types::PageContent>>,
    ) {
        let content = self.content.trim().to_string();
        let boundaries = if self.config.is_some() && !self.boundaries.is_empty() {
            Some(self.boundaries)
        } else {
            None
        };
        let pages = if self.config.is_some() && !self.page_contents.is_empty() {
            Some(self.page_contents)
        } else {
            None
        };
        (content, boundaries, pages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell escaper and the reader (`PptxExtractor::split_table_row`) share a
    /// contract: `\` is doubled so a literal `\|` survives the round trip instead of
    /// losing its backslash to the reader's `\|` unescape.
    #[test]
    fn escape_table_cell_doubles_backslashes_before_pipes() {
        assert_eq!(ContentBuilder::escape_table_cell("a|b"), "a\\|b");
        assert_eq!(ContentBuilder::escape_table_cell("a\\|b"), "a\\\\\\|b");
        assert_eq!(ContentBuilder::escape_table_cell("plain"), "plain");
    }
}
