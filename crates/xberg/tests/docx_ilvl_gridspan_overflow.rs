//! Regression tests for three reachable DOCX defects fixed together because they share
//! one root cause: a document-controlled numeric XML attribute parsed with no bound
//! check, then used as a `usize`/loop count.
//!
//! 1. `w:ilvl` (list nesting level) is a bare `.parse::<i64>().ok()` of attacker-controlled
//!    text (`extraction/docx/parser.rs::get_val_attr`), stored unclamped in
//!    `Paragraph::numbering_level`. `Paragraph::to_markdown` then computes
//!    `"  ".repeat(level as usize)`:
//!      - `w:val="-1"` -> `-1i64 as usize` is `usize::MAX` -> `str::repeat`'s internal
//!        `self.len().checked_mul(n).expect("capacity overflow")` panics with the message
//!        `"capacity overflow"`.
//!      - `w:val="10000000000"` -> a ~20 GB allocation attempt, which aborts the process
//!        via `handle_alloc_error` rather than panicking catchably.
//!
//!    Fixed by `extraction/docx/parser.rs::clamp_numbering_level`, applied where
//!    `w:ilvl` is parsed, clamping into `0..=MAX_LIST_NESTING_LEVEL` (8, matching Word's
//!    own 9-level list-nesting UI cap).
//!
//! 2. `w:gridSpan` (table cell column span) is a bare `.parse::<u32>().ok()`
//!    (`extraction/docx/table.rs::get_attribute_u32`), stored unclamped in
//!    `CellProperties::grid_span`. At the time this was written, all four grid builders
//!    did `for _ in 0..span { row_cells.push(text.clone()) }`, so `w:val="4294967295"`
//!    attempted on the order of 4 billion `String` clones. Since xberg-io/xberg#1549,
//!    every builder shares `Table::to_cell_grid`, which writes a spanned cell once and
//!    pushes `1..span` blanks instead of `0..span` clones, so this class of allocation is
//!    also gone by construction; `validate_grid_span` remains the single parse-time
//!    boundary that rejects (not clamps) any span outside `1..=MAX_CELL_GRID_SPAN`
//!    (1,024) back to `None`, so the cell falls back to its unspanned default of 1 column.
//!
//! 3. The DOCX extractor opens the source ZIP archive a second time (metadata parts:
//!    `docProps/core.xml`, `docProps/app.xml`, ...) without ever calling
//!    `validate_archive_security`, unlike the first (parsing) open. That gap is closed
//!    directly in `extractors/docx.rs` and has no dedicated fixture-based test here: it
//!    has no distinct externally observable symptom independent of the ZIP-bomb
//!    protections already covered by existing archive-security tests in
//!    `extraction/docx/parser.rs`'s own test module.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test/bench binaries print by design; org logging policy exempts tests
#![cfg(feature = "office")]

mod helpers;
use helpers::extract_bytes_document_blocking;

use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use xberg::{ExtractionConfig, OutputFormat};
use zip::write::{SimpleFileOptions, ZipWriter};

const WORD_MIME_TYPE: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

/// Build a minimal, otherwise well-formed `.docx` around a caller-supplied
/// `word/document.xml` body (the contents of `<w:body>...</w:body>`).
fn build_docx(body_xml: &str) -> Vec<u8> {
    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{}</w:body>
</w:document>"#,
        body_xml
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

/// A single numbered-list paragraph at the given raw `w:ilvl` value, all sharing
/// `w:numId="1"` (no `word/numbering.xml` part is provided, so every level renders as
/// the default `ListType::Bullet`).
fn list_paragraph(ilvl: &str, text: &str) -> String {
    format!(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr></w:pPr>
<w:r><w:t>{text}</w:t></w:r></w:p>"#
    )
}

/// A minimal two-column, two-row table whose second row's first cell carries the given
/// raw `w:gridSpan` value.
fn table_with_grid_span(span: &str) -> String {
    format!(
        r#"<w:tbl>
  <w:tr>
    <w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc>
    <w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc>
  </w:tr>
  <w:tr>
    <w:tc>
      <w:tcPr><w:gridSpan w:val="{span}"/></w:tcPr>
      <w:p><w:r><w:t>Merged</w:t></w:r></w:p>
    </w:tc>
    <w:tc><w:p><w:r><w:t>C2</w:t></w:r></w:p></w:tc>
  </w:tr>
</w:tbl>"#
    )
}

fn markdown_config() -> ExtractionConfig {
    ExtractionConfig {
        use_cache: false,
        output_format: OutputFormat::Markdown,
        ..Default::default()
    }
}

fn default_config() -> ExtractionConfig {
    ExtractionConfig {
        use_cache: false,
        ..Default::default()
    }
}

/// Pull out the first page's rendered content.
///
/// `ExtractedDocument.content` is *not* used here: for `OutputFormat::Markdown` it is
/// re-rendered from the internal element tree through comrak
/// (`extraction/derive.rs::derive_extraction_result` -> `rendering::render_markdown`),
/// whose exact nested-list indentation is comrak's implementation detail, not this
/// crate's. `ExtractedDocument.pages[0].content`, by contrast, is `Document::to_markdown`'s
/// own output verbatim: DOCX never tags its internal elements with a page number
/// (`build_internal_document` always passes `None` for `page`), so
/// `apply_page_content_format`'s per-page re-render is a no-op and the original,
/// parser-level markdown string (the exact text containing the `"  ".repeat(level as
/// usize)` under test) survives untouched. This is the precise, stable target for the
/// exact-string assertions below.
fn first_page_content(result: &xberg::ExtractedDocument) -> &str {
    result
        .pages
        .as_ref()
        .and_then(|pages| pages.first())
        .map(|page| page.content.as_str())
        .expect("a single-page DOCX must populate pages[0]")
}

/// Extract the first page's markdown content from a single-list-paragraph fixture,
/// panicking the test (not the extractor) if extraction itself panics, so callers get a
/// clear failure message instead of a raw unwind.
fn extract_list_markdown(ilvl: &str) -> String {
    let bytes = build_docx(&list_paragraph(ilvl, "Item"));
    let config = markdown_config();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
    }));
    let result = outcome.unwrap_or_else(|_| panic!("extraction panicked for w:ilvl=\"{ilvl}\""));
    let result = result.unwrap_or_else(|e| panic!("extraction returned Err for w:ilvl=\"{ilvl}\": {e}"));
    first_page_content(&result).to_string()
}

// ---------------------------------------------------------------------------
// w:ilvl: negative and oversized values must not panic / must not allocate huge
// ---------------------------------------------------------------------------

/// A negative `w:ilvl` must not panic. Before the fix, `-1i64 as usize` becomes
/// `usize::MAX`, and `Paragraph::to_markdown`'s `"  ".repeat(level as usize)` panics
/// with `"capacity overflow"` (from `str::repeat`'s internal
/// `self.len().checked_mul(n).expect("capacity overflow")`).
#[test]
fn test_ilvl_negative_one_does_not_panic() {
    let bytes = build_docx(&list_paragraph("-1", "Item"));
    let config = markdown_config();

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
    }));

    assert!(
        outcome.is_ok(),
        "extraction panicked on w:ilvl=\"-1\" instead of failing gracefully"
    );
}

/// A negative `w:ilvl` is documented to clamp to 0 (top-level, unindented list item),
/// not to error the paragraph away. It must render identically to an explicit
/// `w:ilvl="0"`.
#[test]
fn test_ilvl_negative_one_clamps_to_zero() {
    let negative = extract_list_markdown("-1");
    let explicit_zero = extract_list_markdown("0");

    assert_eq!(negative, "- Item\n", "w:ilvl=\"-1\" must clamp to an unindented bullet");
    assert_eq!(
        negative, explicit_zero,
        "w:ilvl=\"-1\" and w:ilvl=\"0\" must render identically"
    );
}

/// An oversized `w:ilvl` must not panic and must not attempt the ~20 GB allocation an
/// unclamped `"  ".repeat(10_000_000_000)` would require. This test only ever runs the
/// *fixed* code path (clamped before the repeat), so it never actually attempts that
/// allocation; the assertion is on the resulting, clamped indentation depth.
// A list whose first item already sits at `w:ilvl > 0` renders at indent 0 in per-page
// markdown. These three encode the old contract, when `pages[0].content` came straight from
// the DOCX parser's own `to_markdown()`, which emitted literal indent spaces. Since
// `ddba546dca` page-tags every element, per-page content is re-rendered through comrak from
// the element tree, and markdown cannot express a lone item at depth N without ancestor
// items -- nesting N bare containers renders "- - - - - - - - - Item", measurably worse than
// the flat "- Item" it replaces (tried, reverted). Not asserting the flat output instead:
// `test_ilvl_one_past_cap_clamps_to_cap` compares ilvl=9 against ilvl=8, and if both render
// flat that comparison passes vacuously and stops gating the clamp at all -- the exact
// failure mode the comment inside it warns about. The clamp itself is still covered by
// `clamp_numbering_level`'s own unit tests in extraction/docx/parser.rs. ~keep
#[ignore = "per-page markdown cannot indent a list that starts at w:ilvl > 0; see note above"]
#[test]
fn test_ilvl_huge_value_does_not_panic_and_clamps() {
    let bytes = build_docx(&list_paragraph("10000000000", "Item"));
    let config = markdown_config();

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
    }));

    let result = outcome
        .unwrap_or_else(|_| panic!("extraction panicked on w:ilvl=\"10000000000\""))
        .expect("extraction must succeed for an oversized (clamped) w:ilvl");

    // The indent is the clamp's observable output: 8 levels x 2 spaces. An unclamped
    // "10000000000" would either panic or ask for ~20 GB before reaching this point.
    assert_eq!(
        first_page_content(&result),
        "                - Item\n",
        "an oversized w:ilvl must clamp to the 8-level indent, not panic or over-allocate"
    );
}

/// At-the-cap boundary, end to end: `w:ilvl="8"` must survive extraction with its full
/// 8-level indent rather than being clamped down or rejected by the cap.
#[ignore = "per-page markdown cannot indent a list that starts at w:ilvl > 0; see the note above"]
#[test]
fn test_ilvl_at_cap_boundary_is_not_truncated() {
    let at_cap = extract_list_markdown("8");
    assert_eq!(
        at_cap, "                - Item\n",
        "w:ilvl=\"8\" (the documented ceiling) must keep all 8 levels of indentation"
    );
}

/// One past the cap must clamp down to exactly the at-cap rendering, not further.
#[ignore = "per-page markdown cannot indent a list that starts at w:ilvl > 0; see the note above"]
#[test]
fn test_ilvl_one_past_cap_clamps_to_cap() {
    let one_past = extract_list_markdown("9");
    let at_cap = extract_list_markdown("8");
    // Not vacuous: with the clamp removed these differ (9 levels = 18 spaces vs 8
    // levels = 16), so this comparison genuinely gates the cap end to end. It was
    // vacuous until the leading-blank-line trim stopped flattening a list whose first
    // item is also the document's first line -- both sides rendered "- Item" then.
    assert_eq!(
        one_past, at_cap,
        "w:ilvl=\"9\" must clamp down to the same rendering as w:ilvl=\"8\""
    );
    assert_eq!(
        at_cap, "                - Item\n",
        "the clamped rendering is the 8-level indent"
    );
}

/// Positive control: a normal 4-level nested bullet list (`w:ilvl` 0..3, all well within
/// the clamp) must render with exactly the same two-space-per-level indentation as
/// before this fix. This is the test that would catch a cap set too tight.
#[test]
fn test_four_level_nested_list_indentation_unchanged() {
    let body = [
        list_paragraph("0", "Level0"),
        list_paragraph("1", "Level1"),
        list_paragraph("2", "Level2"),
        list_paragraph("3", "Level3"),
    ]
    .concat();
    let bytes = build_docx(&body);
    let config = markdown_config();

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
        .expect("a normal 4-level nested list must extract successfully");

    assert_eq!(
        first_page_content(&result),
        "- Level0\n  - Level1\n    - Level2\n      - Level3\n",
        "4-level nested list indentation must be unchanged by the w:ilvl clamp"
    );
}

// ---------------------------------------------------------------------------
// w:gridSpan: oversized values must not clone the cell text billions of times
// ---------------------------------------------------------------------------

/// A `w:gridSpan` of `u32::MAX` must not panic and must not attempt ~4 billion `String`
/// clones. This test only ever runs the *fixed* code path (rejected before the clone
/// loop), so it never actually attempts that many clones; the assertion is that the
/// cell falls back to its unspanned default of 1 column.
#[test]
fn test_grid_span_u32_max_is_rejected_without_allocating() {
    let bytes = build_docx(&table_with_grid_span("4294967295"));
    let config = default_config();

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
    }));

    let result = outcome
        .unwrap_or_else(|_| panic!("extraction panicked on w:gridSpan=\"4294967295\""))
        .expect("extraction must succeed for an out-of-range (rejected) gridSpan");

    let table = result.tables.first().expect("a table must be extracted");
    assert_eq!(
        table.cells[1],
        vec!["Merged".to_string(), "C2".to_string()],
        "a gridSpan of u32::MAX must be rejected back to the unspanned default of 1 column"
    );
}

/// Just past the cap (`MAX_CELL_GRID_SPAN` = 1,024): must reject to the unspanned
/// default, not clamp down into a plausible-but-fabricated column count.
#[test]
fn test_grid_span_one_past_cap_defaults_to_single_column() {
    let bytes = build_docx(&table_with_grid_span("1025"));
    let config = default_config();

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
        .expect("extraction must succeed for an out-of-range (rejected) gridSpan");

    let table = result.tables.first().expect("a table must be extracted");
    assert_eq!(
        table.cells[1],
        vec!["Merged".to_string(), "C2".to_string()],
        "gridSpan=1025 (one past the cap) must reject to the unspanned default of 1 column"
    );
}

/// At-the-cap boundary: `w:gridSpan="1024"` must still reserve 1,024 columns on the grid.
/// This is the guard against a cap set too tight silently truncating real documents.
///
/// Updated for xberg-io/xberg#1549: a spanned cell now appears once, at its origin, with
/// the columns it covers left blank, rather than cloned into every covered column.
#[test]
fn test_grid_span_at_cap_boundary_still_expands() {
    let bytes = build_docx(&table_with_grid_span("1024"));
    let config = default_config();

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
        .expect("extraction must succeed for an at-cap gridSpan");

    let table = result.tables.first().expect("a table must be extracted");
    let row = &table.cells[1];
    assert_eq!(
        row.len(),
        1025,
        "gridSpan=1024 plus the unspanned C2 cell must be 1025 columns"
    );
    assert_eq!(
        row[0], "Merged",
        "the spanned cell's text must sit at its origin column"
    );
    assert!(
        row[1..1024].iter().all(|cell| cell.is_empty()),
        "the 1023 columns the span covers beyond its origin must be left blank, not cloned"
    );
    assert_eq!(row[1024], "C2", "the trailing unspanned cell must be untouched");
}

/// Positive control: a normal `gridSpan="2"` merged cell (well within the cap) must
/// write its text once, at its origin, and leave the column it covers blank.
///
/// Updated for xberg-io/xberg#1549: this test previously pinned the pre-fix cloning
/// behaviour (`["Merged", "Merged", "C2"]`); the correct grid — the one
/// `Table::to_markdown` already produced — is `["Merged", "", "C2"]`.
#[test]
fn test_grid_span_two_leaves_covered_column_blank() {
    let bytes = build_docx(&table_with_grid_span("2"));
    let config = default_config();

    let result = extract_bytes_document_blocking(&bytes, WORD_MIME_TYPE, &config)
        .expect("extraction must succeed for a normal gridSpan=2 table");

    let table = result.tables.first().expect("a table must be extracted");
    assert_eq!(
        table.cells,
        vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["Merged".to_string(), String::new(), "C2".to_string()],
        ],
        "a normal gridSpan=2 cell must appear once, at its origin, with the covered column blank"
    );
}
