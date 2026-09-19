//! Regression test for xberg-io/xberg#1549: a DOCX table cell spanning `N` grid columns
//! (`w:gridSpan`) or `N` rows (`w:vMerge`) must appear exactly once, at its origin position,
//! with the columns/rows it covers left blank. Before the fix, `extractors/docx.rs` cloned a
//! `gridSpan` cell's text into every covered column, then copied a `vMerge`-continued row's
//! text down from the row above, so a cell merged across 4 columns and 3 rows came back 12
//! times in `result.tables[].cells`, in `result.tables[].markdown`, in the element stream (and
//! therefore `result.content`).
//!
//! This exercises the path consumers actually receive (`extract_bytes_document_blocking` ->
//! `result.tables` and `result.content`), not `Table::to_markdown` directly, which was already
//! correct and is covered by `extractors/docx.rs::test_vertical_merge_renders_empty_cells`.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test binaries print by design; org logging policy exempts tests
#![cfg(feature = "office")]

mod helpers;
use helpers::extract_bytes_document_blocking;

use std::io::Write;
use xberg::ExtractionConfig;
use zip::write::{SimpleFileOptions, ZipWriter};

const WORD_MIME_TYPE: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const ANSWER_TEXT: &str = "The platform has been deployed at the primary site with full redundancy across two availability zones, and the secondary site remains on standby for disaster recovery drills conducted every quarter under the current operations runbook.";

/// The three-column, five-row table from the issue's reproduction: a `gridSpan=3` title
/// row, a `vMerge`d two-row group, and a `gridSpan=2` + `vMerge`d two-row group.
fn merged_table_body_xml() -> String {
    format!(
        r#"<w:tbl>
  <w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
  <w:tr>
    <w:tc><w:tcPr><w:gridSpan w:val="3"/></w:tcPr><w:p><w:r><w:t>Overview</w:t></w:r></w:p></w:tc>
  </w:tr>
  <w:tr>
    <w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>Services</w:t></w:r></w:p></w:tc>
    <w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc>
    <w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc>
  </w:tr>
  <w:tr>
    <w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc>
    <w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc>
    <w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc>
  </w:tr>
  <w:tr>
    <w:tc><w:p><w:r><w:t>Reference</w:t></w:r></w:p></w:tc>
    <w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>{ANSWER_TEXT}</w:t></w:r></w:p></w:tc>
  </w:tr>
  <w:tr>
    <w:tc><w:p><w:r><w:t>Sector</w:t></w:r></w:p></w:tc>
    <w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge/></w:tcPr><w:p/></w:tc>
  </w:tr>
</w:tbl>"#
    )
}

/// Build a minimal, otherwise well-formed `.docx` around a caller-supplied
/// `word/document.xml` body (the contents of `<w:body>...</w:body>`).
fn build_docx(body_xml: &str) -> Vec<u8> {
    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body_xml}</w:body>
</w:document>"#
    );

    let mut buffer = Vec::new();
    {
        let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = SimpleFileOptions::default();

        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
    <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
    <Default Extension="xml" ContentType="application/xml"/>
    <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
        )
        .unwrap();

        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();

        let _ = zip.finish().unwrap();
    }
    buffer
}

/// `result.tables[0].cells` must match the issue's expected grid exactly: a spanned or
/// merged cell's text at its origin, blank everywhere it is merely covered.
#[test]
fn test_grid_span_and_vmerge_do_not_duplicate_cell_text_in_tables() {
    let bytes = build_docx(&merged_table_body_xml());
    let config = ExtractionConfig {
        use_cache: false,
        ..Default::default()
    };

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config).expect("extraction must succeed");

    let table = result.tables.first().expect("a table must be extracted");
    let expected: Vec<Vec<String>> = vec![
        vec!["Overview".to_string(), String::new(), String::new()],
        vec!["Services".to_string(), "A1".to_string(), "B1".to_string()],
        vec![String::new(), "A2".to_string(), "B2".to_string()],
        vec!["Reference".to_string(), ANSWER_TEXT.to_string(), String::new()],
        vec!["Sector".to_string(), String::new(), String::new()],
    ];
    assert_eq!(
        table.cells, expected,
        "a gridSpan/vMerge cell must appear once, at its origin, not cloned across every \
         covered column and copied down every covered row"
    );

    let answer_occurrences = table.markdown.matches(ANSWER_TEXT).count();
    assert_eq!(
        answer_occurrences, 1,
        "the merged answer cell must appear exactly once in the table's markdown, not once \
         per covered column/row: {}",
        table.markdown
    );
}

/// `result.content` is rendered from the element stream (`derive.rs::render_plain`), the
/// second consumer-visible grid the issue reports duplicating cell text.
#[test]
fn test_grid_span_and_vmerge_do_not_duplicate_cell_text_in_content() {
    let bytes = build_docx(&merged_table_body_xml());
    let config = ExtractionConfig {
        use_cache: false,
        ..Default::default()
    };

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config).expect("extraction must succeed");

    let answer_occurrences = result.content.matches(ANSWER_TEXT).count();
    assert_eq!(
        answer_occurrences, 1,
        "the merged answer cell must appear exactly once in result.content, not once per \
         covered column/row: {}",
        result.content
    );
}
