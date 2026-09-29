use crate::table_core::{HocrWord, detect_rows, find_row_index, is_lone_cell_symbol, is_value, median_of};

/// A word under this share of its row's median height is thin enough to be a mark. ~keep
const THIN_MARK_HEIGHT_RATIO: f64 = 0.25;

/// A word over this share of its row's median height is tall enough to be a mark. ~keep
const TALL_MARK_HEIGHT_RATIO: f64 = 1.8;

/// Tesseract's word confidence (0 to 100) under which a word of mark height is a mark. The marks
/// measured on a shaded table read at 0 to 32. ~keep
const MARK_MAX_CONFIDENCE: f64 = 35.0;

/// Which words survive the filter on the marks Tesseract reads off the edge of a shaded table row,
/// in the same order and the same length as `words` (xberg-io/xberg#1858).
///
/// Such a mark (a tall `=`, a thin dash with a comma) sits in the gap between two values, and the
/// cell merge then joins both values into one cell. A word is a mark when all three tests hold: its
/// text has no letter and no digit and is not a lone cell symbol, its height is far from the median
/// height of its row, and its confidence is low. The text test keeps real content whose box went
/// wrong, such as a name read at twice its row's height or a nil dash. Rows are the ones table
/// reconstruction itself detects.
///
/// A mask rather than a filtered list because a caller holding a second vector parallel to `words`
/// must filter both on one decision -- filtering one desynchronises the indices -- and because such
/// a caller wants the decision made against the boxes Tesseract actually read: a later pass may
/// normalise a word's box onto its row's band and erase the very height anomaly this keys on
/// (GH#1834 against GH#1858). ~keep
pub(crate) fn shading_mark_keep_mask(words: &[HocrWord], row_threshold_ratio: f64) -> Vec<bool> {
    let row_positions = detect_rows(words, row_threshold_ratio);
    let rows: Vec<Option<usize>> = words.iter().map(|word| find_row_index(&row_positions, word)).collect();

    let mut row_heights: Vec<Vec<u32>> = vec![Vec::new(); row_positions.len()];
    for (word, row) in words.iter().zip(&rows) {
        if let Some(row) = *row {
            row_heights[row].push(word.height);
        }
    }
    let row_medians: Vec<u32> = row_heights.into_iter().map(median_of).collect();

    words
        .iter()
        .zip(rows)
        .map(|(word, row)| !row.is_some_and(|row| is_shading_mark(word, row_medians[row])))
        .collect()
}

fn is_shading_mark(word: &HocrWord, row_median_height: u32) -> bool {
    if row_median_height == 0 || word.confidence >= MARK_MAX_CONFIDENCE {
        return false;
    }
    let ratio = word.height as f64 / row_median_height as f64;
    !(THIN_MARK_HEIGHT_RATIO..=TALL_MARK_HEIGHT_RATIO).contains(&ratio) && is_mark_text(&word.text)
}

/// Whether `text` can be a mark: it holds no letter, it is not a value by the test the underscore
/// mark cut uses (#1833), and it is not a lone cell symbol. ~keep
fn is_mark_text(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    !chars.iter().any(|ch| ch.is_alphabetic()) && !is_value(&chars) && !is_lone_cell_symbol(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask applied, which is what every case below asserts on.
    fn drop_shading_marks(words: Vec<HocrWord>, row_threshold_ratio: f64) -> Vec<HocrWord> {
        let keep = shading_mark_keep_mask(&words, row_threshold_ratio);
        words
            .into_iter()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(word, _)| word)
            .collect()
    }

    fn word(text: &str, left: u32, top: u32, height: u32, confidence: f64) -> HocrWord {
        HocrWord {
            text: text.to_string(),
            left,
            top,
            width: 60,
            height,
            confidence,
        }
    }

    /// A row of three values 100 px apart, 30 px high at confidence 90, with `mark` after the first.
    fn row_with(mark: HocrWord) -> Vec<HocrWord> {
        vec![
            word("7,812", 100, 200, 30, 90.0),
            mark,
            word("7,968", 200, 200, 30, 90.0),
            word("8,127", 300, 200, 30, 90.0),
        ]
    }

    fn texts(words: &[HocrWord]) -> Vec<&str> {
        words.iter().map(|word| word.text.as_str()).collect()
    }

    const ROW_THRESHOLD_RATIO: f64 = 0.5;

    #[test]
    fn drops_a_tall_low_confidence_mark() {
        let kept = drop_shading_marks(row_with(word("=", 165, 184, 62, 13.0)), ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["7,812", "7,968", "8,127"]);
    }

    #[test]
    fn drops_a_thin_low_confidence_mark() {
        let kept = drop_shading_marks(row_with(word("\u{2014},", 165, 213, 5, 0.0)), ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["7,812", "7,968", "8,127"]);
    }

    /// The highest confidence measured on a shaded table mark.
    #[test]
    fn drops_a_thin_mark_read_at_confidence_32() {
        let kept = drop_shading_marks(row_with(word(":", 165, 214, 1, 32.0)), ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["7,812", "7,968", "8,127"]);
    }

    /// The #1649 shape: a dash 10 px high in a row of 60 px words, read at confidence 46.
    #[test]
    fn keeps_a_thin_dash_read_at_a_moderate_confidence() {
        let row = vec![
            word("Jan 1", 100, 200, 60, 95.0),
            word("-", 165, 225, 10, 46.0),
            word("Jan 31,", 200, 200, 60, 95.0),
            word("2026", 300, 200, 60, 95.0),
        ];
        let kept = drop_shading_marks(row, ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["Jan 1", "-", "Jan 31,", "2026"]);
    }

    #[test]
    fn keeps_a_tall_mark_read_at_a_high_confidence() {
        let kept = drop_shading_marks(row_with(word("=", 165, 184, 62, 90.0)), ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["7,812", "=", "7,968", "8,127"]);
    }

    #[test]
    fn keeps_a_value_at_confidence_zero_and_normal_height() {
        let kept = drop_shading_marks(row_with(word("(2,100)", 165, 200, 30, 0.0)), ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["7,812", "(2,100)", "7,968", "8,127"]);
    }

    /// A name cell read at low confidence with a box more than twice its row's height.
    #[test]
    fn keeps_a_low_confidence_word_with_a_letter_at_mark_height() {
        let row = vec![
            word("19.82", 100, 200, 11, 90.0),
            word("10.47", 200, 200, 11, 90.0),
            word("ATI", 300, 200, 11, 90.0),
            word("Tech", 400, 191, 29, 27.6),
            word("17.89", 500, 200, 11, 90.0),
        ];
        let kept = drop_shading_marks(row, ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["19.82", "10.47", "ATI", "Tech", "17.89"]);
    }

    /// A value whose box the shading grew or cropped.
    #[test]
    fn keeps_a_low_confidence_value_at_mark_height() {
        for (top, height) in [(184, 62), (211, 7)] {
            let kept = drop_shading_marks(row_with(word("22,636", 165, top, height, 20.0)), ROW_THRESHOLD_RATIO);
            assert_eq!(texts(&kept), ["7,812", "22,636", "7,968", "8,127"], "height {height}");
        }
    }

    #[test]
    fn keeps_a_lone_cell_symbol_at_mark_height() {
        for symbol in ["-", "\u{2014}", "(", "{", "$", "%", "*"] {
            for (top, height) in [(213, 4), (180, 70)] {
                let kept = drop_shading_marks(row_with(word(symbol, 165, top, height, 20.0)), ROW_THRESHOLD_RATIO);
                assert_eq!(
                    texts(&kept),
                    ["7,812", symbol, "7,968", "8,127"],
                    "{symbol} at height {height}"
                );
            }
        }
    }

    /// A mark is measured against its own row, not the region: the 20 px `=` is twice its row of
    /// 10 px words, and the 60 px `=` matches its row of 60 px words, though the region median is 20.
    #[test]
    fn measures_a_mark_against_the_median_of_its_own_row() {
        let region = vec![
            word("1,101", 100, 100, 10, 90.0),
            word("=", 165, 95, 20, 10.0),
            word("1,102", 200, 100, 10, 90.0),
            word("1,103", 300, 100, 10, 90.0),
            word("1,104", 400, 100, 10, 90.0),
            word("2,201", 100, 300, 60, 90.0),
            word("=", 165, 300, 60, 10.0),
            word("2,202", 200, 300, 60, 90.0),
            word("2,203", 300, 300, 60, 90.0),
        ];
        let kept = drop_shading_marks(region, ROW_THRESHOLD_RATIO);
        assert_eq!(
            texts(&kept),
            ["1,101", "1,102", "1,103", "1,104", "2,201", "=", "2,202", "2,203"]
        );
    }

    /// A row whose words mostly report a height of 0 has no median to measure a mark against.
    #[test]
    fn keeps_every_word_of_a_row_whose_median_height_is_zero() {
        let row = vec![
            word("-", 100, 200, 0, 0.0),
            word("=", 200, 200, 0, 0.0),
            word("7,812", 300, 200, 30, 0.0),
        ];
        let kept = drop_shading_marks(row, ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["-", "=", "7,812"]);
    }

    #[test]
    fn keeps_every_word_of_an_empty_or_one_word_region() {
        assert!(drop_shading_marks(Vec::new(), ROW_THRESHOLD_RATIO).is_empty());
        let kept = drop_shading_marks(vec![word("=", 100, 200, 5, 0.0)], ROW_THRESHOLD_RATIO);
        assert_eq!(texts(&kept), ["="]);
    }
}
