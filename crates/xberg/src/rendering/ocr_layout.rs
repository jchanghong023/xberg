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
    for element in &document.elements {
        if !matches!(element.kind, ElementKind::OcrText { .. }) {
            continue;
        }
        let text = element.text.trim();
        if text.is_empty() {
            continue;
        }
        // A line with text but no measured box cannot be placed. Emitting a grid without it
        // would silently drop the line from the fenced block while the flat `content` still
        // carries it, so the whole layout is refused and the caller keeps the flat OCR text
        // — the same "refuse rather than lose lines" contract as the row/column caps below.
        let bbox = element.bbox?;
        let height = (bbox.y1 - bbox.y0).abs();
        items.push((bbox.x0, bbox.y0, height, text.to_string()));
    }
    layout_boxes(items)
}

/// Place `(left, top, height, text)` lines on a monospace grid, or `None` when there is
/// nothing to place. `None` is also returned when the grid would not hold every line — a row
/// past `MAX_ROWS`, a line past `MAX_COLS`, or a row whose lines would be reordered — so the
/// caller keeps the flat OCR text instead of a layout that silently lost part of the page.
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
    // Keep each line's original position: the grid below is a reconstruction, and it may only
    // re-emit what the OCR reported in the order the OCR reported it (checked once the rows are
    // assigned).
    let mut indexed: Vec<(usize, f64, f64, f64, String)> = items
        .into_iter()
        .enumerate()
        .map(|(index, (left, top, height, text))| (index, left, top, height, text))
        .collect();
    indexed.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    let row_of_item = rank_rows(&indexed, line_height).unwrap_or_else(|| band_rows(&indexed));

    // (index, row, column, text, width) ordered top-to-bottom then left-to-right, which is
    // also the order collisions are resolved in. The column is the FINAL one: a line that
    // fits only from the grid's first column is re-based here — before the rows are ordered
    // and before the same-row order check below, which must see the column the line will
    // actually print at or a re-based line could slip past it and invert its row's
    // left-to-right order. A line starting past the last column is re-based to it for the
    // same reason. Truncating instead would lose the tail's glyphs: the paragraph-level
    // copy of the same OCR text is deleted downstream precisely on the promise that this
    // block still carries it.
    let mut placed: Vec<(usize, usize, usize, String, usize)> = indexed
        .into_iter()
        .zip(row_of_item)
        .map(|((index, left, _top, _height, text), row)| {
            let mut column = ((left - min_left) / column_px).round().max(0.0) as usize;
            let width = display_width(&text);
            column = column.min(MAX_COLS.saturating_sub(1));
            if width > MAX_COLS - column && width <= MAX_COLS {
                column = 0;
            }
            (index, row, column, text, width)
        })
        .collect();
    placed.sort_by(|a, b| (a.1, a.2).cmp(&(b.1, b.2)));

    // A reconstruction that reorders the lines sharing one row would invent a different
    // document (`-mbist $ijtag` where the OCR reported `-ijtag`, `$ijtag`, `-mbist`, `$mbist`),
    // so those rows are refused outright and the caller falls back to the raw OCR text, whose
    // order is correct by construction. Only a *row* is checked: a document whose OCR reading
    // order is column-major, or one where collision handling pushes a line down, may legitimately
    // reorder rows against each other, and rejecting those would throw away the layout the bands
    // exist to reconstruct. ~keep
    let mut previous_in_band: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (index, row, _, _, _) in &placed {
        if let Some(previous) = previous_in_band.insert(*row, *index)
            && previous > *index
        {
            return None;
        }
    }

    let mut grid: Vec<Vec<char>> = Vec::new();
    let mut occupied_rows = 0usize;
    for (_index, row, column, text, width) in placed {
        if occupied_rows >= MAX_ROWS {
            // Refuse the layout instead of truncating it: the caller falls back to the flat OCR
            // text, so the lines past the cap still reach the output rather than disappearing
            // from a grid that looks complete.
            return None;
        }
        // The column already carries the re-base decided above. A line wider than the whole
        // grid cannot be kept whole anywhere, so the layout is refused and the caller falls
        // back to the flat OCR text — the same "refuse rather than lose text" contract as
        // the row cap above.
        if width > MAX_COLS {
            return None;
        }
        let mut row_index = row;
        loop {
            if row_index >= MAX_ROWS {
                // Past the cap (the line already sat there, or collision handling pushed it
                // down): the same refusal as above keeps its text in the output.
                return None;
            }
            let needed = column + width;
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
/// `None` when the lines do not form at least two columns of the same length: a single column
/// already has the right rows from the bands, and unequal columns (a wrapped cell, a paragraph
/// beside a table) have no rank that pairs them.
fn rank_rows(indexed: &[(usize, f64, f64, f64, String)], line_height: f64) -> Option<Vec<usize>> {
    let gap = line_height.max(1.0);
    let mut by_left: Vec<usize> = (0..indexed.len()).collect();
    by_left.sort_by(|a, b| indexed[*a].1.partial_cmp(&indexed[*b].1).unwrap_or(std::cmp::Ordering::Equal));

    let mut columns: Vec<Vec<usize>> = Vec::new();
    for item in by_left {
        let starts_column = columns.last().is_none_or(|column| {
            indexed[item].1 - indexed[*column.last().expect("column is never empty")].1 >= gap
        });
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
        column.sort_by(|a, b| indexed[*a].2.partial_cmp(&indexed[*b].2).unwrap_or(std::cmp::Ordering::Equal));
    }
    let count = columns.first()?.len();
    if columns.len() < 2 || count == 0 || columns.iter().any(|column| column.len() != count) {
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

    /// A single line wider than the whole grid refuses the layout instead of emitting a
    /// truncated grid: the paragraph-level copy of the same OCR text is deleted downstream
    /// precisely on the promise that the fenced block still carries it, so a truncated
    /// grid would leave the line's tail nowhere at all. `None` makes the caller keep the
    /// flat OCR text, whose every line is present.
    #[test]
    fn refuses_a_line_wider_than_the_column_cap() {
        let long = "a".repeat(MAX_COLS * 2);
        assert!(
            layout_boxes(vec![element(&long, 0.0, 0.0, 4000.0, 20.0)]).is_none(),
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
        let block = layout_boxes(elements).expect("layout");
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
            layout_boxes(elements).is_none(),
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
            layout_boxes(elements).is_none(),
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
        let block = layout_boxes(vec![heading, tall, short]).expect("layout");

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

        let block = layout_boxes(vec![label_one, value_one, label_two, value_two]).expect("layout");
        let lines: Vec<&str> = block.lines().collect();
        let ijtag_row = lines.iter().position(|line| line.contains("-ijtag")).expect("-ijtag kept");
        let mbist_row = lines.iter().position(|line| line.contains("-mbist")).expect("-mbist kept");
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
        let block = layout_boxes(vec![
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
}
