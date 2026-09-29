//! Right-aligned amounts of one to eight digits stay in their columns in a scanned table (GH#1909).

#![cfg(feature = "ocr")]

mod helpers;
use helpers::extract_bytes_document_blocking;
use xberg::core::config::ExtractionConfig;

/// A synthetic scan with no text layer: a label column, then five right-aligned amount columns
/// whose values run from one to eight digits, with empty cells and nil dashes. The columns sit so
/// close that a long amount starts near where a one-digit amount of the column before starts, and
/// Tesseract at its default page segmentation reads the labels and the amounts as separate
/// blocks. ~keep
const SCAN: &[u8] = include_bytes!("fixtures/ocr/right_aligned_amounts_scan.png");

/// The grid as printed. Every amount column is numeric, so table normalisation empties each nil
/// dash (GH#1914).
const EXPECTED: [[&str; 6]; 7] = [
    ["Item", "Year 1", "Year 2", "Year 3", "Year 4", "Year 5"],
    ["Alpha", "7", "40,218,965", "2", "58,730,142", ""],
    ["Bravo", "26,904,317", "4", "71,265,803", "9", "372"],
    ["Charlie", "846", "", "6,027", "", "92,416,570"],
    ["Delta", "5", "17,382,649", "", "1,540", "8"],
    ["Echo", "63,091,728", "250", "3", "44,617,209", ""],
    ["Foxtrot", "19", "8", "30,952,871", "6", "7,213,406"],
];

#[test]
fn every_amount_stays_in_its_row_and_column() {
    let document = extract_bytes_document_blocking(SCAN, "image/png", &ExtractionConfig::default())
        .expect("the scanned table must extract with the default config");
    let table = document.tables.first().expect("the scan must produce a table");
    let cells: Vec<Vec<&str>> = table
        .cells
        .iter()
        .map(|row| row.iter().map(|cell| cell.trim()).collect())
        .collect();

    assert_eq!(cells, EXPECTED, "tables were: {:#?}", document.tables);
}
