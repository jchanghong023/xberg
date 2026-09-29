//! A quantity that the page OCR pass misses is read again from its cell and lands in the table.

#![cfg(feature = "ocr")]

mod helpers;
use helpers::extract_bytes_document_blocking;
use xberg::core::config::{ExtractionConfig, OcrConfig};
use xberg::types::TesseractConfig;

/// An invoice whose lone single-digit quantity the page pass does not read. ~keep
const INVOICE: &[u8] = include_bytes!("fixtures/ocr/lone_quantity_invoice.png");

#[test]
fn a_missed_single_digit_quantity_is_recovered_into_its_row() {
    let config = ExtractionConfig {
        use_cache: false,
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            tesseract_config: Some(TesseractConfig {
                use_cache: false,
                enable_table_detection: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let document =
        extract_bytes_document_blocking(INVOICE, "image/png", &config).expect("OCR of the invoice must succeed");
    let table = document.tables.first().expect("the invoice must produce a table");
    let quantity = table
        .cells
        .first()
        .and_then(|header| header.iter().position(|cell| cell.trim().eq_ignore_ascii_case("qty")))
        .unwrap_or_else(|| panic!("no QTY header: {:?}", table.cells));
    let row = table
        .cells
        .iter()
        .find(|row| row.first().is_some_and(|cell| cell.trim().starts_with("Cleaning")))
        .unwrap_or_else(|| panic!("no Cleaning Tablets row: {:?}", table.cells));
    assert_eq!(
        row.get(quantity).map(|cell| cell.trim()),
        Some("5"),
        "the missed quantity must be read from its cell: {:?}",
        table.cells
    );
}
