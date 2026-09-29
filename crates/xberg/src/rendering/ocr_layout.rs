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
/// padding. A line wider than the column cap, or a page that would need more than the row
/// cap, refuses the layout outright (`None`): the caller then falls back to the flat OCR
/// text, because a truncated grid drops glyphs without the caller being able to tell.
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
    let mut items: Vec<(f64, f64, f64, String)> = Vec::new();
    let mut ocr_text_elements = 0usize;
    for element in &document.elements {
        if !matches!(element.kind, ElementKind::OcrText { .. }) {
            continue;
        }
        ocr_text_elements += 1;
        let text = element.text.trim();
        if text.is_empty() {
            continue;
        }
        // A line with text but no measured box cannot be placed. Emitting a grid without it
        // would silently drop the line from the fenced block while the flat `content` still
        // carries it, so the whole layout is refused and the caller keeps the flat OCR text
        // — the same "refuse rather than lose lines" contract as the row/column caps below.
        let Some(bbox) = element.bbox else {
            tracing::debug!(
                ocr_text_elements,
                document_elements = document.elements.len(),
                "OCR grid refused: a recognized line carries no bounding box"
            );
            return None;
        };
        let height = (bbox.y1 - bbox.y0).abs();
        items.push((bbox.x0, bbox.y0, height, text.to_string()));
    }
    if items.is_empty() {
        tracing::debug!(
            ocr_text_elements,
            document_elements = document.elements.len(),
            "OCR grid refused: no OCR text element with a bounding box to place"
        );
        return None;
    }
    if let Some(grid) = layout_boxes(&items) {
        return Some(grid);
    }
    tracing::debug!(
        lines = items.len(),
        "OCR grid refused by the placement pass; keeping each line at its measured column"
    );
    Some(indented_ocr_text(&items))
}

/// Place `(left, top, height, text)` lines on a monospace grid, or `None` when there is
/// nothing to place. `None` is also returned when the grid would not hold every line — a row
/// past `MAX_ROWS` or a line past `MAX_COLS` — so the caller keeps the complete OCR text.
///
/// Rows use measured vertical overlap or repeated label/value columns. Columns use the left
/// edge over half the median line height (one display column). Rounded columns that collide
/// move right on the same row without overwriting text. A `text` holding several lines —
/// hOCR paragraphs carry their lines joined by `\n` — is laid out line by line.
pub(crate) fn layout_boxes(items: &[(f64, f64, f64, String)]) -> Option<String> {
    let normalized: Vec<(f64, f64, f64, String)> = items
        .iter()
        .flat_map(|(left, top, height, text)| {
            // The lines are owned so the returned iterator does not borrow `text`.
            // Normalize CR before splitting, the same contract the flat fallback
            // applies: a `\r` that survived here would land mid-row in the grid —
            // beyond `trim_end`'s reach once a taller column shares the output
            // line — and re-parse as a row terminator on the markdown side.
            let lines: Vec<String> = text
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .split('\n')
                .map(str::to_string)
                .collect();
            let line_count = lines.len().max(1) as f64;
            let line_height = height / line_count;
            lines
                .into_iter()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .map(move |(index, line)| (*left, *top + index as f64 * line_height, line_height, line))
                .collect::<Vec<_>>()
        })
        .collect();

    if normalized.is_empty() {
        tracing::debug!("OCR grid refused: no lines to place");
        return None;
    }

    let line_height = median(normalized.iter().map(|item| item.2))
        .filter(|height| *height > 0.0)
        .unwrap_or(12.0);
    // A display column is half an em: Latin glyphs average ~0.5 em, a CJK glyph is 1 em wide
    // and occupies two columns, so the same divisor places both scripts correctly.
    let column_px = (line_height / 2.0).max(1.0);
    let min_left = normalized.iter().map(|item| item.0).fold(f64::INFINITY, f64::min);

    // Rows come from two rules, tried in order.
    //
    // A label/value listing — one column of names and one of their values — reports the value
    // column a constant offset below the labels it belongs to, because the picture draws the
    // values lower than the boxes the OCR measured. Vertical overlap then pairs every label with
    // the *next* label's value (`-mbist $ijtag`), inventing pairs the picture never showed, so
    // equal-length columns are joined by rank: the n-th line of each column shares a row, which
    // is the alignment the picture draws.
    //
    // Everything else falls back to vertical bands: clustering lines whose spans overlap by at
    // least half the smaller span, not dividing a top offset by the median height, because OCR
    // line boxes vary in height (tall glyph runs, sub/superscripts) and rounding a mixed-height
    // page onto a fixed grid parked lines from the same visual row on different rows. Unequal
    // columns — a wrapped cell, a paragraph beside a table — keep that path, which is what
    // preserves their mixed-height rows.
    //
    // Keep the OCR sequence as a guard for inferred rank pairs. Geometry-backed bands use
    // left-to-right order instead: detection order can differ within a visual row.
    let mut indexed: Vec<(usize, f64, f64, f64, String)> = normalized
        .into_iter()
        .enumerate()
        .map(|(index, (left, top, height, text))| (index, left, top, height, text))
        .collect();
    indexed.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    // The rank join *infers* which lines share a row (the n-th line of each column), so a row it
    // pairs can invert the OCR reading order; the bands are pure geometry. When the rank rows are
    // refused by the order check, retry with the bands before giving up on the grid: a grid that
    // keeps the reading order carries strictly more of the picture than the flat text, and the
    // bands place the same lines from their measured spans instead of from a rank.
    let mut grid =
        rank_rows(&indexed, line_height).and_then(|rows| place_rows(&indexed, rows, column_px, min_left, true));
    if grid.is_none() {
        let rows = band_rows(&indexed);
        grid = place_rows(&indexed, rows, column_px, min_left, false);
        if grid.is_some() {
            tracing::debug!("OCR grid rebuilt from vertical bands after the rank join reordered a row");
        }
    }
    grid
}

/// Place every line on the grid named by `row_of_item` (one row index per `indexed` entry, in
/// `indexed` order) and render it, or `None` when a complete row cannot be placed safely.
///
/// Split out of [`layout_boxes`] so the two row rules can be tried against the same checks: the
/// refusals below (row order, row cap, column cap) are what a rule has to satisfy, and the caller
/// falls back to the other rule — then to [`indented_ocr_text`] — instead of losing the layout.
fn place_rows(
    indexed: &[(usize, f64, f64, f64, String)],
    row_of_item: Vec<usize>,
    column_px: f64,
    min_left: f64,
    preserve_ocr_order: bool,
) -> Option<String> {
    // (index, row, column, text, width, measured_left) ordered top-to-bottom then left-to-right, which is
    // also the order collisions are resolved in. The column is the FINAL one: a line that
    // fits only from the grid's first column is re-based here — before the rows are ordered
    // and before the same-row order check below, which must see the column the line will
    // actually print at or a re-based line could slip past it and invert its row's
    // left-to-right order. A line starting past the last column is re-based to it for the
    // same reason. Truncating instead would lose the tail's glyphs: the paragraph-level
    // copy of the same OCR text is deleted downstream precisely on the promise that this
    // block still carries it.
    let mut placed: Vec<(usize, usize, usize, &str, usize, f64)> = indexed
        .iter()
        .zip(row_of_item)
        .map(|((index, left, _top, _height, text), row)| {
            let mut column = ((left - min_left) / column_px).round().max(0.0) as usize;
            let width = display_width(text);
            column = column.min(MAX_COLS.saturating_sub(1));
            if width > MAX_COLS - column && width <= MAX_COLS {
                column = 0;
            }
            (*index, row, column, text.as_str(), width, *left)
        })
        .collect();
    placed.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)).then(a.5.total_cmp(&b.5)));

    // Inferred rank pairs must still respect the OCR sequence. Measured bands may reorder
    // detector output left-to-right (e.g. a right-hand cell detected a pixel higher). Both
    // paths reject rebasing that would reverse the actual horizontal geometry.
    let mut previous_in_band = None;
    for (index, row, _, _, _, left) in &placed {
        if let Some((previous_row, previous_index, previous_left)) = previous_in_band
            && previous_row == *row
            && ((preserve_ocr_order && previous_index > *index) || previous_left > *left)
        {
            tracing::debug!(
                row = *row,
                index = *index,
                "OCR grid refused: the lines sharing a row would be reordered"
            );
            return None;
        }
        previous_in_band = Some((*row, *index, *left));
    }

    // Keep text verbatim and track its display width separately. A CJK glyph already
    // occupies two columns; serializing a placeholder space would add an unwanted third.
    let mut grid: Vec<(String, usize)> = Vec::new();
    for (_index, row, column, text, width, _left) in placed {
        if row >= MAX_ROWS || width > MAX_COLS {
            tracing::debug!(row, width, "OCR grid refused: row or column cap reached");
            return None;
        }
        if grid.len() <= row {
            grid.resize_with(row + 1, || (String::new(), 0));
        }
        let (line, used) = &mut grid[row];
        let column = if line.is_empty() { column } else { column.max(*used + 1) };
        if column + width > MAX_COLS {
            // Never split a visual row or truncate it to make it fit. The caller keeps
            // every recognized block through the complete-text fallback instead.
            tracing::debug!(
                row,
                MAX_COLS,
                "OCR grid refused: a complete visual row exceeds the column cap"
            );
            return None;
        }
        line.push_str(&" ".repeat(column - *used));
        line.push_str(text);
        *used = column + width;
    }

    let rendered: Vec<String> = grid.iter().map(|(line, _)| line.trim_end().to_string()).collect();

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
        .map(|row| {
            if row.len() >= margin {
                row[margin..].to_string()
            } else {
                row.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(block)
}

/// Render every recognized line at the display column its measured left edge falls on, one
/// output line each, in the OCR's own order.
///
/// The grid's last resort: the placement pass refuses unsafe rebasing (see [`place_rows`])
/// or rows that cannot fit the grid at all, and the flat OCR text the
/// caller falls back to carries no positions at all. This keeps each line's horizontal offset —
/// the alignment a screenshot's columns are read by — while changing nothing else: no two lines
/// are ever joined onto one row and no line is ever moved, so it cannot invent the pairs the grid
/// checks guard against. Vertical gaps are not padded: like the grid, it emits one line per
/// recognized line.
pub(crate) fn indented_ocr_text(items: &[(f64, f64, f64, String)]) -> String {
    let line_height = median(items.iter().map(|item| item.2))
        .filter(|height| *height > 0.0)
        .unwrap_or(12.0);
    let column_px = (line_height / 2.0).max(1.0);
    let min_left = items.iter().map(|item| item.0).fold(f64::INFINITY, f64::min);

    let mut lines: Vec<(usize, String)> = Vec::new();
    for (left, _top, _height, text) in items {
        let column = (((left - min_left) / column_px).round().max(0.0) as usize).min(MAX_COLS.saturating_sub(1));
        for line in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            lines.push((column, line.to_string()));
        }
    }

    // Re-base the left margin like the grid does, so a block that is uniformly indented in the
    // picture does not open with a run of spaces.
    let margin = lines.iter().map(|(column, _)| *column).min().unwrap_or(0);
    lines
        .iter()
        .map(|(column, line)| format!("{}{}", " ".repeat(column - margin), line))
        .collect::<Vec<_>>()
        .join("\n")
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

/// Cluster the top-sorted lines into vertical bands that overlap by at least half the smaller
/// span, and return the band index of each line.
fn band_rows(indexed: &[(usize, f64, f64, f64, String)]) -> Vec<usize> {
    let mut bands: Vec<(f64, f64)> = Vec::new();
    let mut rows: Vec<usize> = Vec::with_capacity(indexed.len());
    for (_, _, top, height, _) in indexed {
        let item_top = *top;
        let item_height = height.max(1.0);
        let item_bottom = item_top + item_height;
        if let Some((band_top, band_bottom)) = bands.last_mut() {
            let overlap = item_bottom.min(*band_bottom) - item_top.max(*band_top);
            let smaller = item_height.min(*band_bottom - *band_top);
            if overlap > 0.0 && overlap / smaller.max(1.0) >= 0.5 {
                *band_top = band_top.min(item_top);
                *band_bottom = band_bottom.max(item_bottom);
                rows.push(bands.len() - 1);
                continue;
            }
        }
        bands.push((item_top, item_bottom));
        rows.push(bands.len() - 1);
    }
    rows
}

/// Join equal-length display columns row by row, and return the row index of each line.
///
/// `None` when the lines do not form at least two repeated columns of the same length: a single column
/// already has the right rows from the bands, and unequal columns (a wrapped cell, a paragraph
/// beside a table) have no rank that pairs them.
fn rank_rows(indexed: &[(usize, f64, f64, f64, String)], line_height: f64) -> Option<Vec<usize>> {
    let gap = line_height.max(1.0);
    let mut by_left: Vec<usize> = (0..indexed.len()).collect();
    by_left.sort_by(|a, b| {
        indexed[*a]
            .1
            .partial_cmp(&indexed[*b].1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut columns: Vec<Vec<usize>> = Vec::new();
    for item in by_left {
        let starts_column = columns
            .last()
            .is_none_or(|column| indexed[item].1 - indexed[*column.last().expect("column is never empty")].1 >= gap);
        if starts_column {
            columns.push(vec![item]);
        } else {
            columns.last_mut().expect("column is never empty").push(item);
        }
    }
    // Pair each column's lines by vertical order, not by the left order the column was just
    // built in: a column's left edges jitter by a pixel or two (centred labels read
    // 0.0/2.0/1.0), and pairing the left-sorted ranks crossed lines that never shared a row.
    // `indexed` arrives sorted by top and this sort is stable, so equal tops keep that order.
    for column in &mut columns {
        column.sort_by(|a, b| {
            indexed[*a]
                .2
                .partial_cmp(&indexed[*b].2)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let count = columns.first()?.len();
    // A single item at each x position is not evidence of a repeated column layout.
    // Otherwise a heading and an indented paragraph hundreds of pixels below it merge.
    if columns.len() < 2 || count < 2 || columns.iter().any(|column| column.len() != count) {
        return None;
    }

    let mut rows = vec![0usize; indexed.len()];
    for rank in 0..count {
        for column in &columns {
            rows[column[rank]] = rank;
        }
    }
    Some(rows)
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
        let block = layout_boxes(&elements).expect("layout");

        let lines: Vec<&str> = block.lines().collect();
        let left_line = lines
            .iter()
            .position(|line| line.contains("top left"))
            .expect("top left");
        let right_line = lines
            .iter()
            .position(|line| line.contains("top right"))
            .expect("top right");
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
        assert!(layout_boxes(&Vec::new()).is_none());
        assert!(layout_boxes(&vec![element("   ", 0.0, 0.0, 10.0, 10.0)]).is_none());
    }

    /// A line with text but no measured box cannot be placed: the layout must be refused
    /// outright so the caller keeps the flat OCR text. A grid that silently omitted the
    /// line lost it from the fenced block while `content` still carried it.
    #[test]
    fn refuses_the_layout_when_a_line_has_no_bbox() {
        use crate::types::internal::{ElementKind, InternalDocument, InternalElement};

        let boxed = {
            let mut element = InternalElement::text(
                ElementKind::OcrText {
                    level: crate::types::OcrElementLevel::Line,
                },
                "boxed line",
                0,
            );
            element.bbox = Some(crate::types::extraction::BoundingBox {
                x0: 0.0,
                y0: 0.0,
                x1: 120.0,
                y1: 20.0,
            });
            element
        };
        let unboxed = InternalElement::text(
            ElementKind::OcrText {
                level: crate::types::OcrElementLevel::Line,
            },
            "unboxed line",
            0,
        );

        let mut document = InternalDocument::new("ocr");
        document.push_element(boxed);
        document.push_element(unboxed);

        assert!(
            layout_ocr_text(&document).is_none(),
            "a layout that would drop the unboxed line must be refused"
        );
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
        let lines: Vec<&str> = (0..8)
            .map(|_| "recognized paragraph line with sixty characters xxxxxxxx")
            .collect();
        let paragraph = lines.join("\n");
        assert!(
            display_width(&paragraph) > MAX_COLS,
            "the paragraph must exceed the column cap"
        );

        let elements = vec![element(&paragraph, 0.0, 0.0, 800.0, 96.0)];
        let block = layout_boxes(&elements).expect("layout");

        for (index, line) in lines.iter().enumerate() {
            assert!(
                block.contains(&line[..40]),
                "line {index} of the paragraph must survive: {block:?}"
            );
        }
        assert_eq!(block.lines().count(), lines.len(), "one row per line: {block:?}");
    }

    /// A single line wider than the whole grid refuses the layout instead of emitting a
    /// truncated grid: the paragraph-level copy of the same OCR text is deleted downstream
    /// precisely on the promise that the fenced block still carries it, so a truncated
    /// grid would leave the line's tail nowhere at all. `None` makes the caller keep the
    /// flat OCR text, whose every line is present.
    #[test]
    fn refuses_a_line_wider_than_the_column_cap() {
        let long = "a".repeat(MAX_COLS * 2);
        assert!(
            layout_boxes(&vec![element(&long, 0.0, 0.0, 4000.0, 20.0)]).is_none(),
            "a line past the column cap must be refused so the caller keeps every glyph"
        );
    }

    /// A line that fits the grid only from its first column is re-based there whole
    /// rather than truncated at its original column — same promise as above: the block
    /// must still carry every glyph of the line.
    #[test]
    fn re_bases_a_line_that_fits_only_from_the_first_column() {
        let wide = "b".repeat(50);
        // The left label anchors `min_left` at 0, putting the wide line at column 390;
        // rank pairing shares their row, and the re-based line settles on the next one.
        let elements = vec![
            element("anchor", 0.0, 0.0, 0.0, 20.0),
            element(&wide, 3900.0, 30.0, 0.0, 20.0),
        ];
        let block = layout_boxes(&elements).expect("layout");
        assert!(
            block.contains(&wide),
            "the line must survive whole (re-based to column 0, not truncated): {block:?}"
        );
    }

    /// A re-based line must not print to the LEFT of a line the OCR reported before it
    /// on the same row: the column it will actually print at has to feed the row's
    /// order check, or the grid would silently invert the pair. The layout is refused
    /// instead and the caller falls back to the flat OCR text.
    #[test]
    fn refuses_when_a_rebased_line_would_invert_its_row() {
        // The left anchor pins `min_left` at 0, so the wide line lands at column 390
        // and is re-based to 0 — ahead of the mid-row label at column 50.
        let elements = vec![
            element("anchor", 0.0, 30.0, 0.0, 20.0),
            element("right side label", 500.0, 0.0, 100.0, 20.0),
            element(&"b".repeat(50), 3900.0, 0.0, 0.0, 20.0),
        ];
        assert!(
            layout_boxes(&elements).is_none(),
            "a re-based line moving ahead of an earlier line on its row must refuse the layout"
        );
    }

    /// A page that needs more rows than the cap is refused rather than truncated: the previous
    /// behaviour returned a grid holding the first 400 rows and silently dropped the rest, so
    /// neither the caller nor its reader could tell text was missing. `None` makes the caller
    /// keep the flat OCR text, whose every line is present.
    #[test]
    fn refuses_a_page_taller_than_the_row_cap() {
        let elements: Vec<_> = (0..MAX_ROWS + 1)
            .map(|index| element(&format!("line {index}"), 0.0, index as f64 * 20.0, 60.0, 12.0))
            .collect();

        assert!(
            layout_boxes(&elements).is_none(),
            "a page past the row cap must be refused so the caller keeps every line"
        );
    }

    /// Two boxes on the same visual row whose heights differ (a tall glyph run
    /// next to a normal line) must share one grid row. Rounding a top offset
    /// onto a median-height grid parked such pairs on different rows, which is
    /// how table cells drifted off their labels.
    #[test]
    fn mixed_height_boxes_on_one_visual_row_share_a_row() {
        let tall = element("RATIO", 0.0, 30.0, 120.0, 40.0);
        let short = element("0.58 s/图", 200.0, 60.0, 120.0, 12.0);
        let heading = element("TITLE", 0.0, 0.0, 120.0, 12.0);
        let block = layout_boxes(&vec![heading, tall, short]).expect("layout");

        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "the title must get its own row and the mixed-height pair must share one; got {block:?}"
        );
        let ratio_row = lines.iter().position(|line| line.contains("RATIO")).expect("RATIO");
        let value_row = lines.iter().position(|line| line.contains("0.58")).expect("0.58");
        assert_eq!(
            ratio_row, value_row,
            "RATIO and its value must sit on the same row: {block:?}"
        );
        assert!(
            lines[value_row].contains("RATIO") && lines[value_row].contains("0.58"),
            "both fragments must be on the shared row: {block:?}"
        );
    }

    /// A value column is measured a constant offset below the labels it belongs to (the picture
    /// draws the values lower than their boxes). Overlap alone then pairs each label with the
    /// next label's value, printing `-mbist $ijtag` where the picture shows `-ijtag $ijtag` and
    /// `-mbist $mbist`; equal-length columns are joined by rank instead.
    #[test]
    fn a_value_column_offset_below_its_labels_keeps_every_pair_together() {
        let label_one = element("-ijtag", 0.0, 120.0, 60.0, 12.0);
        let value_one = element("$ijtag", 200.0, 140.0, 60.0, 12.0);
        let label_two = element("-mbist", 0.0, 145.0, 60.0, 12.0);
        let value_two = element("$mbist", 200.0, 151.0, 60.0, 12.0);

        let block = layout_boxes(&vec![label_one, value_one, label_two, value_two]).expect("layout");
        let lines: Vec<&str> = block.lines().collect();
        let ijtag_row = lines
            .iter()
            .position(|line| line.contains("-ijtag"))
            .expect("-ijtag kept");
        let mbist_row = lines
            .iter()
            .position(|line| line.contains("-mbist"))
            .expect("-mbist kept");
        assert!(
            lines[ijtag_row].contains("$ijtag"),
            "the value must sit on its own label's row: {block:?}"
        );
        assert!(
            lines[mbist_row].contains("$mbist"),
            "the value must sit on its own label's row: {block:?}"
        );
        assert!(ijtag_row < mbist_row, "the labels keep the OCR order: {block:?}");
    }

    /// A column's left edges can jitter by a pixel or two (centred labels read
    /// 0.0/2.0/1.0). Pairing the left-sorted ranks crossed lines that never shared a row —
    /// `beta` landed below `gamma` with the next label's value — so each column pairs by
    /// vertical order instead.
    #[test]
    fn jittered_column_edges_pair_by_vertical_order() {
        let label = |text: &str, left: f64, top: f64| element(text, left, top, 60.0, 12.0);
        let block = layout_boxes(&[
            label("alpha", 0.0, 0.0),
            label("beta", 2.0, 20.0),
            label("gamma", 1.0, 40.0),
            label("one", 200.0, 1.0),
            label("two", 200.0, 21.0),
            label("three", 200.0, 41.0),
        ])
        .expect("layout");

        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 3, "three visual rows: {block:?}");
        assert!(lines[0].contains("alpha") && lines[0].contains("one"), "{block:?}");
        assert!(lines[1].contains("beta") && lines[1].contains("two"), "{block:?}");
        assert!(lines[2].contains("gamma") && lines[2].contains("three"), "{block:?}");
    }

    /// The rank join pairs the n-th line of each column, which can invert the OCR order; the
    /// bands then place the same lines from their measured spans and the grid is kept instead of
    /// falling back to the flat text.
    #[test]
    fn vertical_bands_rescue_a_rank_join_that_inverts_the_reading_order() {
        let items = vec![
            element("a1", 0.0, 0.0, 60.0, 20.0),
            element("b2", 100.0, 50.0, 60.0, 20.0),
            element("a2", 0.0, 30.0, 60.0, 20.0),
            element("b1", 100.0, 20.0, 60.0, 20.0),
        ];
        let block = layout_boxes(&items).expect("the bands must rescue the grid");
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 3, "three bands: {block:?}");
        assert!(lines[0].contains("a1"), "{block:?}");
        assert!(
            lines[1].contains("a2") && lines[1].contains("b1"),
            "the vertically overlapping pair shares a row: {block:?}"
        );
        assert!(lines[2].contains("b2"), "{block:?}");
    }

    /// Detection order is not a reading-order contract: boxes from the same measured
    /// row must be sorted left-to-right. The fallback independently retains source order.
    #[test]
    fn measured_row_sorts_detector_order_and_fallback_retains_every_block() {
        let items = vec![
            element("right", 600.0, 0.0, 100.0, 20.0),
            element("left", 0.0, 0.0, 100.0, 20.0),
        ];
        let block = layout_boxes(&items).expect("geometry-backed row remains a grid");
        assert_eq!(block.lines().count(), 1);
        assert_eq!(block, format!("left{}right", " ".repeat(56)));

        let text = indented_ocr_text(&items);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "every line survives: {text:?}");
        assert_eq!(lines[0].trim_start(), "right");
        assert_eq!(lines[1].trim_start(), "left");
        // One display column is half the median line height (20 / 2 = 10 px) and the block is
        // re-based on the leftmost line, so "right" prints 60 columns in and "left" at 0.
        assert_eq!(lines[1], "left", "{text:?}");
        assert_eq!(lines[0].len() - lines[0].trim_start().len(), 60, "{text:?}");
    }

    #[test]
    fn jittered_table_cells_stay_on_their_visual_rows_despite_detector_order() {
        // Like the two tables in 测试识别.png: sparse cells and unequal column counts,
        // with the rightmost header detected before its left-hand neighbours.
        let block = layout_boxes(&[
            element("差距", 600.0, 1.0, 40.0, 20.0),
            element("口径", 0.0, 2.0, 40.0, 20.0),
            element("PaddleOCR", 400.0, 0.0, 100.0, 20.0),
            element("Tesseract", 200.0, 1.0, 100.0, 20.0),
            element("同一数据集", 0.0, 40.0, 100.0, 20.0),
            element("6.24 s/图", 400.0, 39.0, 90.0, 20.0),
            element("0.58 s/图", 200.0, 41.0, 90.0, 20.0),
            element("~10.8x", 600.0, 40.0, 60.0, 20.0),
            element("CPU", 0.0, 80.0, 40.0, 20.0),
            element("1.75 s/图", 400.0, 80.0, 90.0, 20.0),
        ])
        .expect("table grid");
        let rows: Vec<_> = block.lines().collect();
        assert_eq!(rows.len(), 3, "same-row cells must not turn into extra rows: {block:?}");
        for (row, expected) in rows.iter().zip([
            vec!["口径", "Tesseract", "PaddleOCR", "差距"],
            vec!["同一数据集", "0.58", "6.24", "~10.8x"],
            vec!["CPU", "1.75"],
        ]) {
            let mut end = 0;
            for token in expected {
                let start = row
                    .find(token)
                    .unwrap_or_else(|| panic!("missing {token} on row {row:?}"));
                assert!(start >= end, "cell order: {row:?}");
                end = start + token.len();
            }
        }
    }

    #[test]
    fn rounded_column_collisions_shift_right_without_splitting_a_row_or_chinese_words() {
        let block = layout_boxes(&[
            element("中文单元格", 0.0, 0.0, 90.0, 20.0),
            element("value", 90.0, 0.0, 50.0, 20.0),
            element("tail", 250.0, 0.0, 40.0, 20.0),
        ])
        .expect("a colliding cell stays on the row");
        assert_eq!(block, format!("中文单元格 value{}tail", " ".repeat(9)));
        assert_eq!(display_width(block.split("tail").next().unwrap()), 25);
    }

    #[test]
    fn boxes_rounding_to_the_same_column_keep_measured_left_to_right_order() {
        let block = layout_boxes(&[
            element("right", 2.0, 0.0, 50.0, 20.0),
            element("left", 0.0, 1.0, 50.0, 20.0),
        ])
        .expect("rounded collision remains on its visual row");
        assert_eq!(block, "left right");
    }

    #[test]
    fn a_complete_row_that_exceeds_capacity_falls_back_without_losing_cells() {
        let first = "a".repeat(250);
        let second = "中".repeat(100);
        let items = [
            element(&first, 0.0, 0.0, 2500.0, 20.0),
            element(&second, 200.0, 0.0, 2000.0, 20.0),
        ];
        assert!(layout_boxes(&items).is_none(), "do not split or truncate a visual row");
        let fallback = indented_ocr_text(&items);
        assert_eq!(fallback.lines().count(), 2);
        assert!(fallback.contains(&first));
        assert!(fallback.contains(&second));
    }

    #[test]
    fn singleton_columns_do_not_merge_a_heading_with_a_lower_indented_paragraph() {
        let block = layout_boxes(&[
            element("heading", 0.0, 0.0, 100.0, 20.0),
            element("paragraph", 200.0, 200.0, 100.0, 20.0),
        ])
        .expect("layout");
        assert_eq!(
            block.lines().count(),
            2,
            "unrelated vertical positions must stay separate"
        );
    }

    /// A line past the column cap refuses the grid; the fallback must carry it whole, because the
    /// caller deletes the paragraph-level copy of the same OCR text on the promise that this block
    /// still has it.
    #[test]
    fn a_line_wider_than_the_grid_still_reaches_the_indented_fallback_whole() {
        let long = "x".repeat(MAX_COLS + 10);
        let items = vec![
            element(&long, 0.0, 0.0, 0.0, 20.0),
            element("tail", 40.0, 40.0, 0.0, 20.0),
        ];
        assert!(
            layout_boxes(&items).is_none(),
            "a line past the column cap refuses the grid"
        );

        let text = indented_ocr_text(&items);
        assert!(
            text.contains(&long),
            "the over-wide line must survive intact: {text:.80}"
        );
        assert!(text.lines().any(|line| line.trim() == "tail"), "{text:.80}");
    }
}
