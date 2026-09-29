//! A multi-word row label stays in its row when shading stretches one of its word boxes
//! (GH#1834).

#![cfg(all(feature = "ocr", feature = "pdf"))]

mod helpers;
use helpers::extract_bytes_document_blocking;
use xberg::core::config::{ExtractionConfig, OcrConfig};
use xberg::types::TesseractConfig;

const SCANNED_TABLE: &[u8] = include_bytes!("fixtures/ocr/shaded_table_scan.pdf");

/// A shaded row of the fixture: its label, then its Year 1 value. ~keep
const STRETCHED_ROW: (&str, &str) = ("EXPECTED SURPLUS", "1,012");

/// At PSM 11 with shaded-row normalisation on, Tesseract reads the label's first word with a box
/// stretched over the next row but on one text line with the second word. Both words must share
/// the row of the row's values. ~keep
#[test]
fn a_label_word_with_a_stretched_box_stays_in_the_row_of_its_line() {
    let config = ExtractionConfig {
        force_ocr: true,
        use_cache: false,
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            tesseract_config: Some(TesseractConfig {
                psm: Some(11),
                use_cache: false,
                enable_table_detection: true,
                preprocessing: Some(xberg::types::ImagePreprocessingConfig {
                    normalize_shaded_rows: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let document = extract_bytes_document_blocking(SCANNED_TABLE, "application/pdf", &config)
        .expect("forced OCR of the scanned table must succeed");
    let table = document.tables.first().expect("the page must produce a table");
    let (label, value) = STRETCHED_ROW;
    let row = table
        .cells
        .iter()
        .find(|row| row.iter().any(|cell| cell.trim() == value))
        .unwrap_or_else(|| panic!("no row holds {value}: {:?}", table.cells));
    assert_eq!(
        row.first().map(|cell| cell.trim()),
        Some(label),
        "the whole label must share the row of its values: {:?}",
        table.cells
    );
}
