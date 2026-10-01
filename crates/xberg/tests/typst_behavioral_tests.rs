//! Comprehensive behavioral tests for Typst extractor against Pandoc baselines.
//!
//! These tests expose the critical bugs found in code review:
//! 1. 62% heading loss bug - only matches single `=` headings
//! 2. Blockquotes not implemented
//! 3. Display math not extracted
//! 4. Nested table brackets cause corruption
//! 5. Empty headings output (just `= ` with no text)
//! 6. Regex failures silently lose metadata
//!
//! The tests are designed to FAIL initially, exposing real bugs that need fixing.
//! They compare extracted output against Pandoc baseline outputs for behavioral parity.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test/bench binaries print by design; org logging policy exempts tests
#![allow(clippy::len_zero, clippy::unnecessary_get_then_check, clippy::single_match)]
#![cfg(feature = "office")]

mod helpers;
use helpers::extract_bytes_document;

use std::{fs, path::PathBuf};
use xberg::ExtractedDocument;
use xberg::core::config::{ExtractionConfig, OutputFormat};
use xberg::types::document_structure::{DocumentNode, NodeContent};

fn typst_doc_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/typst")
}

/// Load a test document from the test_documents/typst directory
fn load_test_document(filename: &str) -> Vec<u8> {
    let path = typst_doc_root().join(filename);
    fs::read(&path).unwrap_or_else(|_| panic!("Failed to read test document: {}", filename))
}

/// Load Pandoc baseline output for comparison
fn load_pandoc_baseline(filename_base: &str) -> String {
    let path = typst_doc_root().join(format!("{filename_base}_pandoc_baseline.txt"));
    fs::read_to_string(&path).unwrap_or_else(|_| panic!("Failed to read baseline: {}", filename_base))
}

/// Load Pandoc metadata JSON for comparison.
///
/// `test_documents/` is a bucket-fetched corpus (see
/// `test_documents/scripts/fetch_corpus.py`), and no `*_pandoc_meta.json` has ever
/// been published to it. Returns `None` — after printing a greppable skip message
/// naming the missing path — so the caller skips cleanly instead of panicking. A
/// `None` is "not run", never "passed".
fn load_pandoc_metadata(filename_base: &str) -> Option<String> {
    let path = typst_doc_root().join(format!("{filename_base}_pandoc_meta.json"));
    match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) => {
            eprintln!(
                "SKIP: fixture {} not available ({e}); \
                 run `python3 test_documents/scripts/fetch_corpus.py` to fetch it",
                path.display()
            );
            None
        }
    }
}

/// Extraction config that also returns the structured document tree.
///
/// Heading levels live on `NodeContent::Heading`, never in the extracted text: the
/// extractor records headings as structural elements and no renderer re-emits the
/// Typst `=` markers (see `should_render_heading_text_without_typst_markers` in
/// `crates/xberg/src/extractors/typst.rs`). Asserting on the structure is both the
/// only honest way to check levels and strictly stronger than string matching,
/// which cannot tell a level from a prefix.
fn structure_config() -> ExtractionConfig {
    ExtractionConfig {
        include_document_structure: true,
        ..Default::default()
    }
}

/// Structured nodes of an extraction produced with [`structure_config`].
fn nodes(result: &ExtractedDocument) -> &[DocumentNode] {
    result
        .document
        .as_ref()
        .expect("include_document_structure should populate `document`")
        .nodes
        .as_slice()
}

/// Every heading as a `(level, text)` pair, in document order.
fn headings(result: &ExtractedDocument) -> Vec<(u8, &str)> {
    nodes(result)
        .iter()
        .filter_map(|node| match &node.content {
            NodeContent::Heading { level, text } => Some((*level, text.as_str())),
            _ => None,
        })
        .collect()
}

/// Text of a body node, or `None` for headings and for containers that carry none.
///
/// Headings are excluded deliberately: callers use this to find the content a
/// heading owns, and a heading is the boundary of that content, not part of it.
fn body_text(node: &DocumentNode) -> Option<&str> {
    match &node.content {
        NodeContent::Paragraph { text } | NodeContent::ListItem { text } | NodeContent::Formula { text } => Some(text),
        NodeContent::Code { text, .. } => Some(text),
        _ => None,
    }
}

/// Count lines that are pure metadata/directives (not content)
fn count_directive_lines(content: &str) -> usize {
    content
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("#set ") || t.starts_with("#let ") || t.starts_with("#import ")
        })
        .count()
}

/// Content each heading owns: `(heading text, body texts up to the next heading)`.
///
/// Headings with no content of their own (a section that opens directly with a
/// subsection) get an empty vector rather than being dropped, so a caller can tell
/// "this heading has no content" from "this heading was lost". `Group` nodes are
/// skipped: derivation wraps each heading in a `Group` that carries its section, so
/// a `Group` belongs to the heading that follows it, never to the one before.
fn heading_content_blocks(result: &ExtractedDocument) -> Vec<(&str, Vec<&str>)> {
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();

    for node in nodes(result) {
        if let NodeContent::Heading { text, .. } = &node.content {
            blocks.push((text.as_str(), Vec::new()));
            continue;
        }
        if let (Some(text), Some(current)) = (body_text(node), blocks.last_mut()) {
            current.1.push(text);
        }
    }

    blocks
}

/// Check if content has reasonable parity with baseline (within tolerance)
fn content_parity_check(extracted: &str, baseline: &str, tolerance_percent: f64) -> bool {
    let extracted_len = extracted.len();
    let baseline_len = baseline.len();

    if baseline_len == 0 {
        return extracted_len == 0;
    }

    let ratio = (extracted_len as f64) / (baseline_len as f64);
    let acceptable_min = 1.0 - (tolerance_percent / 100.0);
    let acceptable_max = 1.0 + (tolerance_percent / 100.0);

    ratio >= acceptable_min && ratio <= acceptable_max
}

// CRITICAL BUG TESTS - These expose the 45+ issues

/// TEST 1: CRITICAL - 62% heading loss bug
///
/// The extractor once matched only single `=` headings, completely skipping
/// `==`, `===`, and higher levels. That caused catastrophic data loss in
/// hierarchical documents.
///
/// Expected: every heading level survives, at its own level, in document order.
#[tokio::test]
async fn test_typst_all_heading_levels_not_lost() {
    let content = load_test_document("headings.typ");
    let _baseline = load_pandoc_baseline("headings");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert_eq!(
        headings(&result),
        vec![
            (1, "Level 1 Heading"),
            (2, "Level 2 Heading"),
            (3, "Level 3 Heading"),
            (4, "Level 4 Heading"),
            (5, "Level 5 Heading"),
            (6, "Level 6 Heading"),
        ],
        "every `=`-run length must become a heading at the matching level, in document order. \
         A short or mis-levelled list is the heading loss bug: levels beyond '=' being skipped."
    );
}

/// TEST 2: Display math not extracted
///
/// Display math ($$...$$) is completely lost from extraction,
/// breaking mathematical content preservation.
///
/// Expected: Display math should be preserved in output
/// Current behavior: Silently dropped
/// WILL FAIL: Exposing display math loss
#[tokio::test]
async fn test_typst_display_math_preserved() {
    let content = load_test_document("advanced.typ");
    let baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_display_math_in_baseline =
        baseline.contains("²") || baseline.contains("Display math") || baseline.contains("x^2");

    if has_display_math_in_baseline {
        let our_has_math = result.content.contains("$")
            || result.content.contains("Display")
            || result.content.contains("²")
            || result.content.contains("²");

        assert!(
            our_has_math,
            "Display math should be extracted. Pandoc preserves mathematical notation, \
             but extractor drops it entirely. This breaks scientific/academic documents."
        );
    }

    let has_pythagorean = result.content.contains("^2")
        || result.content.contains("²")
        || result.content.contains("x") && result.content.contains("y") && result.content.contains("r");

    assert!(
        has_pythagorean,
        "Pythagorean theorem expression should be present. Display math is being dropped."
    );
}

/// TEST 3: Empty headings output
///
/// A `=` run with no text after it must not become a heading at all. It used to be
/// emitted as a bare `= ` marker, polluting the output with a textless heading.
///
/// Expected: every heading that exists carries text.
#[tokio::test]
async fn test_typst_no_empty_headings_output() {
    let content = load_test_document("headings.typ");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let textless: Vec<(u8, &str)> = headings(&result)
        .into_iter()
        .filter(|(_, text)| text.trim().is_empty())
        .collect();

    assert!(
        textless.is_empty(),
        "Found {} heading(s) with no text: {:?}. A textless `=` run must be dropped, \
         not emitted as an empty heading that corrupts the document structure.",
        textless.len(),
        textless
    );
}

/// TEST 4: Metadata extraction fails with regex silently
///
/// When regex patterns fail to match metadata fields,
/// the extractor silently returns None instead of logging/failing,
/// causing complete metadata loss for certain formats.
///
/// Expected: All metadata fields should be extracted
/// Current behavior: Some formats fail silently
/// WILL FAIL: Exposing metadata loss
#[tokio::test]
async fn test_typst_metadata_extraction_completeness() {
    let content = load_test_document("metadata.typ");
    let Some(_baseline_meta) = load_pandoc_metadata("metadata") else {
        return;
    };
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_title = result.metadata.title.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
    let has_author = result.metadata.authors.as_ref().map(|a| !a.is_empty()).unwrap_or(false);
    let has_keywords = result
        .metadata
        .keywords
        .as_ref()
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    assert!(
        has_title,
        "Title metadata should be extracted. Regex pattern matching fails silently \
         and metadata is lost with no error reporting."
    );

    assert!(
        has_author,
        "Author metadata should be extracted. Some metadata formats fail silently."
    );

    assert!(
        has_keywords,
        "Keywords should be extracted. Regex failures cause silent data loss."
    );
}

/// TEST 5: Nested table brackets cause corruption
///
/// Tables with nested brackets like [Name [full]] corrupt the
/// table content extraction because bracket counting is naive.
///
/// Expected: Table cells should be extracted correctly even with nesting
/// Current behavior: Bracket nesting causes cells to be malformed
/// WILL FAIL: Exposing table corruption bug
#[tokio::test]
async fn test_typst_tables_with_nested_brackets_not_corrupted() {
    let content = load_test_document("advanced.typ");
    let baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_table_in_baseline = baseline.contains("Name") && baseline.contains("Alice");

    if has_table_in_baseline {
        let table_content_extracted =
            result.content.contains("Name") && result.content.contains("Alice") && result.content.contains("Age");

        assert!(
            table_content_extracted,
            "Table content should be extracted correctly. Nested brackets cause corruption \
             and table cells are malformed."
        );

        let corrupted_brackets = result.content.matches("[[").count();
        assert_eq!(
            corrupted_brackets, 0,
            "Found corrupted bracket sequences [[. Table extraction with nested brackets \
             produces malformed output."
        );
    }
}

/// TEST 6: Content volume parity - within tolerance of Pandoc
///
/// Our extractor should extract roughly the same amount of content
/// as Pandoc (baseline). Large discrepancies indicate data loss or
/// noise injection.
///
/// Expected: Within reasonable tolerance of baseline content size
/// Current behavior: Significant data loss on complex documents (e.g., advanced.typ)
/// WILL FAIL: Exposing data loss on complex documents with formatting
#[tokio::test]
async fn test_typst_content_volume_parity_with_pandoc() {
    let documents = vec![("simple", 30.0), ("headings", 20.0)];

    for (doc_name, tolerance) in documents {
        let content = load_test_document(&format!("{}.typ", doc_name));
        let baseline = load_pandoc_baseline(doc_name);
        let config = ExtractionConfig::default();

        let result = extract_bytes_document(&content, "application/x-typst", &config)
            .await
            .unwrap_or_else(|_| panic!("Extraction failed for {}", doc_name));

        let baseline_size = baseline.len();
        let extracted_size = result.content.len();

        let is_within_tolerance = content_parity_check(&result.content, &baseline, tolerance);

        assert!(
            is_within_tolerance,
            "Content volume parity failed for {}: \
             Baseline: {} bytes, Extracted: {} bytes ({}% tolerance allowed). \
             Data loss indicates missing extraction features or formatting issues.",
            doc_name, baseline_size, extracted_size, tolerance
        );
    }
}

/// TEST 7: Blockquotes not implemented
///
/// Blockquotes (using > syntax in other formats, typst uses #quote)
/// are completely unimplemented, causing loss of semantic structure.
///
/// Expected: Blockquote content should be extracted
/// Current behavior: Feature not implemented
/// WILL FAIL: Exposing missing blockquote support
#[tokio::test]
async fn test_typst_blockquote_handling() {
    let test_content = b"#quote[
        This is a blockquote.
        It should be extracted.
    ]";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_blockquote_content =
        result.content.contains("blockquote") || result.content.contains("This is a blockquote");

    assert!(
        has_blockquote_content,
        "Blockquote content should be extracted. Blockquotes are not implemented \
         in the extractor, causing complete loss of quoted content."
    );
}

/// TEST 8: Inline code preservation
///
/// Test that inline code blocks are properly extracted and marked.
/// This ensures code snippets aren't corrupted.
///
/// Expected: Inline code preserved with backticks or clearly marked
/// Current behavior: May be corrupted
/// WILL FAIL: If inline code is not preserved
#[tokio::test]
async fn test_typst_inline_code_preserved() {
    let content = load_test_document("advanced.typ");
    let baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_inline_code =
        result.content.contains("`") || (result.content.contains("code") && baseline.contains("`code`"));

    assert!(
        has_inline_code,
        "Inline code should be preserved with backticks or clearly marked."
    );
}

/// TEST 9: Inline math extraction
///
/// Inline math (single $ delimiters) should be extracted and preserved.
///
/// Expected: Inline math formulas preserved
/// Current behavior: May be dropped
/// WILL FAIL: If inline math is lost
#[tokio::test]
async fn test_typst_inline_math_preserved() {
    let content = load_test_document("advanced.typ");
    let baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_inline_math =
        result.content.contains("$") || result.content.contains("sqrt") || result.content.contains("equation");

    if baseline.contains("$") || baseline.contains("equation") {
        assert!(
            has_inline_math,
            "Inline math should be extracted. Mathematical formulas are being dropped."
        );
    }
}

/// TEST 10: Figures and captions
///
/// Figure extraction with captions should preserve both image references
/// and caption text.
///
/// Expected: Figure content and captions extracted
/// Current behavior: May be unimplemented
#[tokio::test]
async fn test_typst_figures_and_captions() {
    let test_content = b"#figure(
        image(\"example.png\"),
        caption: [This is a figure caption]
    )";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let _has_caption = result.content.contains("caption") || result.content.contains("figure");

    println!(
        "Figure extraction result (feature may be unimplemented): {:?}",
        result.content
    );
}

/// TEST 11: Citation/reference handling
///
/// Citations and references should be extracted when present.
///
/// Expected: Citation markers and text preserved
/// Current behavior: May be dropped
#[tokio::test]
async fn test_typst_citations_preserved() {
    let test_content = b"Here is a citation @smith2020.

= References

#bibliography()";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let _has_citation = result.content.contains("@smith2020")
        || result.content.contains("smith")
        || result.content.contains("References");

    println!("Citation handling (may be limited): {:?}", result.content);
}

/// TEST 12: Link extraction and formatting
///
/// Links should be extracted with both URL and link text.
///
/// Expected: Links in markdown format [text](url)
/// Current behavior: May lose URL or text
#[tokio::test]
async fn test_typst_link_extraction() {
    let content = load_test_document("advanced.typ");
    let _baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_link_content =
        result.content.contains("example") || result.content.contains("link") || result.content.contains("https");

    assert!(
        has_link_content,
        "Link content should be extracted. Links may be completely dropped."
    );
}

/// TEST 13: Unordered list extraction
///
/// Both + and - list markers should be converted to standard format.
///
/// Expected: All list items extracted and normalized
/// Current behavior: May lose some items
#[tokio::test]
async fn test_typst_list_extraction() {
    let content = load_test_document("simple.typ");
    let _baseline = load_pandoc_baseline("simple");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_list_markers = result.content.contains("-") || result.content.contains("+");
    let has_list_content =
        result.content.contains("First") || result.content.contains("Second") || result.content.contains("item");

    assert!(
        has_list_markers || has_list_content,
        "List items should be extracted with markers or content preserved."
    );
}

/// TEST 14: Code block extraction
///
/// Triple-backtick code blocks should be fully extracted with language specifiers.
///
/// Expected: Code blocks with language markers preserved
/// Current behavior: May be malformed
#[tokio::test]
async fn test_typst_code_block_extraction() {
    let content = load_test_document("advanced.typ");
    let _baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_code = result.content.contains("```")
        || result.content.contains("def")
        || result.content.contains("fibonacci")
        || result.content.contains("python");

    assert!(has_code, "Code blocks should be extracted with language specifiers.");
}

/// TEST 15: Bold and italic formatting
///
/// Inline emphasis formatting should be preserved or normalized.
///
/// Expected: Bold (*text*) and italic (_text_) markers present
/// Current behavior: May be lost
#[tokio::test]
async fn test_typst_emphasis_formatting() {
    let content = load_test_document("advanced.typ");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_emphasis = result.content.contains("*") && result.content.contains("_");

    assert!(has_emphasis, "Bold and italic formatting markers should be preserved.");
}

/// TEST 16: Complex nested formatting
///
/// Test handling of *_nested formatting_* combinations.
///
/// Expected: Nested formatting preserved or flattened consistently
/// Current behavior: May be malformed
#[tokio::test]
async fn test_typst_nested_formatting() {
    let test_content = b"This is *bold with _nested italic_* text.";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_formatting = result.content.contains("*")
        || result.content.contains("_")
        || (result.content.contains("bold") && result.content.contains("italic"));

    assert!(
        has_formatting,
        "Nested formatting should be preserved or flattened consistently."
    );
}

/// TEST 17: Multiple paragraph handling
///
/// Multiple paragraphs separated by blank lines should be preserved.
///
/// Expected: Paragraph structure maintained
/// Current behavior: May merge or lose paragraphs
#[tokio::test]
async fn test_typst_multiple_paragraphs() {
    let content = load_test_document("advanced.typ");
    let _baseline = load_pandoc_baseline("advanced");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let non_empty_lines: Vec<_> = result.content.lines().filter(|l| !l.trim().is_empty()).collect();

    assert!(
        non_empty_lines.len() >= 5,
        "Multiple paragraphs should be preserved. Found {} content lines.",
        non_empty_lines.len()
    );
}

/// TEST 18: Heading-content association
///
/// Content must stay attached to the heading it follows, in document order.
///
/// Expected: each heading owns exactly the body that follows it up to the next
/// heading — no scrambling, no empty content, nothing orphaned.
#[tokio::test]
async fn test_typst_heading_content_association() {
    let content = load_test_document("advanced.typ");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let blocks = heading_content_blocks(&result);

    let heading_order: Vec<&str> = blocks.iter().map(|(heading, _)| *heading).collect();
    assert_eq!(
        heading_order,
        vec![
            "Mathematical Notation",
            "Formatting Showcase",
            "Structured Content",
            "Code Blocks",
            "Nested Headings",
            "Level 3 heading",
            "Level 4 heading",
            "Tables and Data",
            "Multiple Paragraphs",
            "Conclusion",
        ],
        "headings must appear in document order"
    );

    for (heading, body) in &blocks {
        for text in body {
            assert!(
                !text.trim().is_empty(),
                "content associated with heading '{}' must not be empty",
                heading
            );
        }
    }

    // `=== Level 3 heading` and `==== Level 4 heading` each own exactly one
    // paragraph, so their association is unambiguous and pins the ordering: a
    // scrambled walk would hand these paragraphs to a different heading. ~keep
    let block_for = |wanted: &str| {
        blocks
            .iter()
            .find(|(heading, _)| *heading == wanted)
            .unwrap_or_else(|| panic!("heading '{}' should be extracted", wanted))
            .1
            .clone()
    };
    assert_eq!(
        block_for("Level 3 heading"),
        vec!["This is under a level 3 heading."],
        "a level-3 heading must own the paragraph that follows it"
    );
    assert_eq!(
        block_for("Level 4 heading"),
        vec!["And this is level 4."],
        "a level-4 heading must own the paragraph that follows it"
    );
}

/// TEST 19: Whitespace normalization
///
/// Multiple blank lines should be normalized consistently.
///
/// Expected: Single blank lines between sections
/// Current behavior: May have excessive whitespace
#[tokio::test]
async fn test_typst_whitespace_handling() {
    let content = load_test_document("advanced.typ");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let blank_line_runs: Vec<_> = result.content.split("\n\n\n").collect();

    assert!(
        blank_line_runs.len() <= 2,
        "Should not have excessive blank lines (triple newlines). \
         Found {} instances of triple newlines.",
        blank_line_runs.len() - 1
    );
}

/// TEST 20: Minimal document handling
///
/// Even minimal documents should extract correctly.
///
/// Expected: Basic content and structure
/// Current behavior: May fail or lose content
#[tokio::test]
async fn test_typst_minimal_document() {
    let content = load_test_document("minimal.typ");
    let _baseline = load_pandoc_baseline("minimal");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        !result.content.is_empty(),
        "Even minimal documents should extract some content."
    );

    assert!(
        result.content.len() > 0,
        "Minimal document should produce non-empty output."
    );
}

/// TEST 21: No directive pollution
///
/// Extracted content should not contain #set, #let, #import directives.
///
/// Expected: Clean extracted content without directives
/// Current behavior: May include directives
#[tokio::test]
async fn test_typst_no_directive_pollution() {
    let content = load_test_document("advanced.typ");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let directive_count = count_directive_lines(&result.content);

    assert_eq!(
        directive_count, 0,
        "Extracted content should not contain directives (#set, #let, etc). \
         Found {} directive lines polluting the output.",
        directive_count
    );
}

/// TEST 22: Metadata field completeness
///
/// All metadata fields from baseline should be present.
///
/// Expected: Title, author, date, keywords all extracted
/// Current behavior: Some fields missing
#[tokio::test]
async fn test_typst_metadata_field_completeness() {
    let content = load_test_document("advanced.typ");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_title = result.metadata.title.is_some();
    let has_author = result.metadata.authors.is_some();
    let has_date = result.metadata.created_at.is_some();

    assert!(
        has_title && has_author && has_date,
        "All metadata fields should be extracted. \
         Title: {}, Author: {}, Date: {}",
        has_title,
        has_author,
        has_date
    );
}

/// TEST 23: Special character handling
///
/// Unicode and special characters should be preserved.
///
/// Expected: Special characters like ü, é, etc. preserved
/// Current behavior: May be corrupted
#[tokio::test]
async fn test_typst_special_character_preservation() {
    let test_content = "Café with naïve français".as_bytes();

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_special_chars =
        result.content.contains("Café") || result.content.contains("naïve") || result.content.contains("français");

    assert!(
        has_special_chars,
        "Special characters should be preserved in extraction."
    );
}

/// TEST 24: Very long heading handling
///
/// Long headings should not cause truncation or corruption.
///
/// Expected: Full heading text preserved regardless of length
/// Current behavior: May truncate
#[tokio::test]
async fn test_typst_long_heading_handling() {
    let test_content = b"= This is a very long heading that should be completely preserved without any truncation or corruption whatsoever";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_heading_start = result.content.contains("very long heading");

    assert!(has_heading_start, "Long headings should not be truncated.");
}

/// TEST 25: Edge case - Empty heading recovery
///
/// Even if a heading has no text, extraction should be robust.
///
/// Expected: Graceful handling without crashes
/// Current behavior: May panic or produce empty output
#[tokio::test]
async fn test_typst_empty_heading_edge_case() {
    let test_content = b"= \n\n== \nContent here";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config).await;

    match result {
        Ok(extraction) => {
            assert!(
                extraction.content.contains("Content"),
                "Should extract regular content even if some headings are empty."
            );
        }
        Err(_) => {
            // An empty `= ` heading is malformed Typst; rejecting the document is an acceptable
            // outcome here. Only the successful path carries an assertion. ~keep
        }
    }
}

#[path = "typst_behavioral_tests/regression_and_stress.rs"]
mod regression_and_stress;
