//! Rebuild an image's recognized text as a monospace grid that keeps every line's
//! relative position.
//!
//! Flat OCR text loses the one thing a picture's text is usually arranged around: where it
//! sits. A label in the top-right corner comes back as the first or last line of a stream, so
//! a diagram, a slide title block or a table of widgets reads as noise. The grid emitted here
//! puts each recognized line back at its own row and column (in display columns, so CJK stays
//! two columns wide), which is what a `text` fenced block renders faithfully.

use crate::types::internal::{ElementKind, InternalDocument};

/// Hard caps so one pathological page (a dense circuit diagram) cannot emit a megabyte of
/// padding. Beyond these the block is truncated rather than dropped.
const MAX_COLS: usize = 400;
const MAX_ROWS: usize = 400;

/// Display columns a character occupies in a monospace font.
///
/// Only the wide ranges matter for text OCR: CJK ideographs, kana, Hangul, fullwidth forms and
/// the CJK extension planes are drawn one em wide (two columns), everything else half an em.
fn char_columns(character: char) -> usize {
    let code = character as u32;
    let wide = matches!(code,
        0x1100..=0x115F          // Hangul Jamo
        | 0x2E80..=0x303E        // CJK radicals, Kangxi, CJK symbols
        | 0x3041..=0x33FF        // kana, CJK compatibility
        | 0x3400..=0x4DBF        // CJK extension A
        | 0x4E00..=0x9FFF        // CJK unified
        | 0xA000..=0xA4CF        // Yi
        | 0xAC00..=0xD7A3        // Hangul syllables
        | 0xF900..=0xFAFF        // CJK compatibility ideographs
        | 0xFE30..=0xFE6F        // CJK compatibility forms
        | 0xFF00..=0xFF60        // fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD      // CJK extensions B+
    );
    if wide { 2 } else { 1 }
}

/// Display width of `text` in columns.
fn display_width(text: &str) -> usize {
    text.chars().map(char_columns).sum()
}

/// Lay an OCR result's recognized lines out on a monospace grid.
///
/// Reads the OCR document's line elements, whose boxes carry the geometry the public
/// `ocr_elements` list only exposes when the caller asked for it.
pub(crate) fn layout_ocr_text(document: &InternalDocument) -> Option<String> {
    let items: Vec<(f64, f64, f64, String)> = document
        .elements
        .iter()
        .filter_map(|element| {
            if !matches!(element.kind, ElementKind::OcrText { .. }) {
                return None;
            }
            let text = element.text.trim();
            if text.is_empty() {
                return None;
            }
            let bbox = element.bbox?;
            let height = (bbox.y1 - bbox.y0).abs();
            Some((bbox.x0, bbox.y0, height, text.to_string()))
        })
        .collect();
    layout_boxes(items)
}

/// Place `(left, top, height, text)` lines on a monospace grid, or `None` when there is
/// nothing to place.
///
/// Rows are driven by each line's vertical position over the median line height, columns by
/// its left edge over half the median line height (one display column), so both axes keep the
/// source picture's proportions. A line that would land on top of another is pushed down one
/// row rather than overwriting it. A `text` holding several lines — hOCR paragraphs carry
/// their lines joined by `\n` — is laid out line by line; counting the whole block as one row's
/// width exceeded the column cap and dropped the text entirely.
pub(crate) fn layout_boxes(mut items: Vec<(f64, f64, f64, String)>) -> Option<String> {
    items = items
        .drain(..)
        .flat_map(|(left, top, height, text)| {
            // The lines are owned so the returned iterator does not borrow `text`.
            let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let line_count = lines.len().max(1) as f64;
            let line_height = height / line_count;
            lines
                .into_iter()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(move |(index, line)| (left, top + index as f64 * line_height, line_height, line))
                .collect::<Vec<_>>()
        })
        .collect();

    if items.is_empty() {
        return None;
    }

    let line_height = median(items.iter().map(|item| item.2)).filter(|height| *height > 0.0).unwrap_or(12.0);
    // A display column is half an em: Latin glyphs average ~0.5 em, a CJK glyph is 1 em wide
    // and occupies two columns, so the same divisor places both scripts correctly.
    let column_px = (line_height / 2.0).max(1.0);
    let min_left = items.iter().map(|item| item.0).fold(f64::INFINITY, f64::min);
    let min_top = items.iter().map(|item| item.1).fold(f64::INFINITY, f64::min);

    // (row, column, text, width) ordered top-to-bottom then left-to-right, which is also the
    // order collisions are resolved in.
    let mut placed: Vec<(usize, usize, String, usize)> = items
        .drain(..)
        .map(|(left, top, _height, text)| {
            let row = ((top - min_top) / line_height).round().max(0.0) as usize;
            let column = ((left - min_left) / column_px).round().max(0.0) as usize;
            let width = display_width(&text);
            (row, column, text, width)
        })
        .collect();
    placed.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));

    let mut grid: Vec<Vec<char>> = Vec::new();
    let mut occupied_rows = 0usize;
    for (row, mut column, text, width) in placed {
        if occupied_rows >= MAX_ROWS {
            break;
        }
        // Keep the line inside the grid: a line that would run past the last column is
        // truncated there (and one starting past it is re-based to column 0) so its text stays
        // in the block. Dropping it — the previous behaviour — silently lost OCR text from the
        // markdown while `content` still carried it.
        column = column.min(MAX_COLS.saturating_sub(1));
        let mut clipped = truncate_to_columns(&text, width, MAX_COLS - column);
        if clipped.0.is_empty() {
            // A single wide glyph cannot fit into the one column left; start the line over at
            // the grid's first column rather than losing it.
            column = 0;
            clipped = truncate_to_columns(&text, width, MAX_COLS);
        }
        let (text, width) = clipped;
        let mut row_index = row;
        loop {
            if row_index >= MAX_ROWS {
                break;
            }
            let needed = column + width;
            if needed > MAX_COLS {
                break;
            }
            let starts_free = grid
                .get(row_index)
                .map(|cells| (column..needed).all(|index| cells.get(index).copied().unwrap_or(' ') == ' '))
                .unwrap_or(true);
            if starts_free {
                if grid.len() <= row_index {
                    grid.resize(row_index + 1, Vec::new());
                }
                let cells = &mut grid[row_index];
                if cells.len() < needed {
                    cells.resize(needed, ' ');
                }
                let mut cursor = column;
                for character in text.chars() {
                    let span = char_columns(character);
                    cells[cursor] = character;
                    for follower in cursor + 1..(cursor + span).min(cells.len()) {
                        cells[follower] = '\0';
                    }
                    cursor += span;
                }
                occupied_rows = occupied_rows.max(row_index + 1);
                break;
            }
            row_index += 1;
        }
    }

    // Drop the placeholder marks that keep the second half of a wide glyph from being
    // overwritten, then trim each row's right padding.
    let rendered: Vec<String> = grid
        .iter()
        .map(|cells| {
            let text: String = cells.iter().map(|cell| if *cell == '\0' { ' ' } else { *cell }).collect();
            text.trim_end().to_string()
        })
        .collect();

    // Trim blank leading/trailing rows and re-base the left margin so the block starts at
    // column 0 without losing any line's relative offset.
    let first = rendered.iter().position(|row| !row.is_empty())?;
    let last = rendered.iter().rposition(|row| !row.is_empty())?;
    let margin = rendered[first..=last]
        .iter()
        .filter(|row| !row.is_empty())
        .map(|row| row.len() - row.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let block = rendered[first..=last]
        .iter()
        .map(|row| if row.len() >= margin { row[margin..].to_string() } else { row.clone() })
        .collect::<Vec<_>>()
        .join("\n");
    Some(block)
}

/// Truncate `text` to at most `available` display columns, returning the kept text and its
/// width. A wide glyph that would straddle the limit is left out rather than split.
fn truncate_to_columns(text: &str, width: usize, available: usize) -> (String, usize) {
    if width <= available {
        return (text.to_string(), width);
    }
    let mut kept = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let span = char_columns(character);
        if used + span > available {
            break;
        }
        kept.push(character);
        used += span;
    }
    (kept, used)
}

/// Median of a non-empty sample; `None` when empty.
fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sample: Vec<f64> = values.collect();
    if sample.is_empty() {
        return None;
    }
    sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(sample[sample.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(text: &str, left: f64, top: f64, _width: f64, height: f64) -> (f64, f64, f64, String) {
        (left, top, height, text.to_string())
    }

    /// A label at the top right must stay at the top right: a later row and a larger column
    /// than a top-left label, on the first row.
    #[test]
    fn keeps_a_top_right_label_at_the_top_right() {
        let elements = vec![
            element("top left", 0.0, 0.0, 200.0, 20.0),
            element("top right", 900.0, 0.0, 200.0, 20.0),
            element("bottom", 0.0, 200.0, 200.0, 20.0),
        ];
        let block = layout_boxes(elements).expect("layout");

        let lines: Vec<&str> = block.lines().collect();
        let left_line = lines.iter().position(|line| line.contains("top left")).expect("top left");
        let right_line = lines.iter().position(|line| line.contains("top right")).expect("top right");
        let bottom_line = lines.iter().position(|line| line.contains("bottom")).expect("bottom");

        assert_eq!(left_line, 0, "top-left label belongs on the first row: {block:?}");
        assert_eq!(right_line, 0, "the two labels share a row");
        assert!(bottom_line > left_line, "the lower label must stay lower: {block:?}");
        let left_column = lines[left_line].find("top left").expect("column");
        let right_column = lines[right_line].find("top right").expect("column");
        assert!(
            right_column > left_column + 20,
            "the right-hand label must stay far to the right: {block:?}"
        );
    }

    /// Blank input yields no block rather than an empty fence.
    #[test]
    fn returns_none_without_usable_elements() {
        assert!(layout_boxes(Vec::new()).is_none());
        assert!(layout_boxes(vec![element("   ", 0.0, 0.0, 10.0, 10.0)]).is_none());
    }

    /// CJK glyphs are two display columns wide, so a Chinese line keeps its proportion
    /// against a Latin one on the same row.
    #[test]
    fn counts_cjk_as_two_columns() {
        assert_eq!(char_columns('中'), 2);
        assert_eq!(char_columns('a'), 1);
        assert_eq!(display_width("中abc"), 5);
    }

    /// A multi-line element (an hOCR paragraph carries its lines joined by `\n`) is placed line
    /// by line and kept whole: counted as one row, its total width exceeded the column cap and
    /// every line past it was dropped, so the block silently lost recognized text.
    #[test]
    fn keeps_every_line_of_a_multi_line_paragraph() {
        let lines: Vec<&str> = (0..8).map(|_| "recognized paragraph line with sixty characters xxxxxxxx").collect();
        let paragraph = lines.join("\n");
        assert!(display_width(&paragraph) > MAX_COLS, "the paragraph must exceed the column cap");

        let elements = vec![element(&paragraph, 0.0, 0.0, 800.0, 96.0)];
        let block = layout_boxes(elements).expect("layout");

        for (index, line) in lines.iter().enumerate() {
            assert!(
                block.contains(&line[..40]),
                "line {index} of the paragraph must survive: {block:?}"
            );
        }
        assert_eq!(block.lines().count(), lines.len(), "one row per line: {block:?}");
    }

    /// A single line wider than the cap is truncated, not dropped.
    #[test]
    fn truncates_a_line_wider_than_the_column_cap() {
        let long = "a".repeat(MAX_COLS * 2);
        let block = layout_boxes(vec![element(&long, 0.0, 0.0, 4000.0, 20.0)]).expect("layout");

        assert_eq!(display_width(block.lines().next().unwrap()), MAX_COLS);
    }
}
