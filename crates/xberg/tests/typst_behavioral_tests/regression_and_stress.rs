use super::*;

/// TEST 26: Regression - Basic heading extraction
#[tokio::test]
async fn test_typst_basic_heading_regression() {
    let test_content = b"= Main Heading\n\nContent here";

    let config = structure_config();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert_eq!(
        headings(&result),
        vec![(1, "Main Heading")],
        "Basic level-1 heading should be extracted."
    );

    assert!(
        result.content.contains("Main Heading"),
        "Heading text should reach the extracted text."
    );

    assert!(result.content.contains("Content"), "Content should be extracted.");
}

/// TEST 27: Regression - Level 2 heading extraction
///
/// Also pins the rendered form: Markdown is where a heading's level becomes a
/// visible prefix, and it is `##`, not the Typst `==` of the source.
#[tokio::test]
async fn test_typst_level2_heading_regression() {
    let test_content = b"= Main\n\n== Subsection\n\nMore content";

    let config = structure_config();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert_eq!(
        headings(&result),
        vec![(1, "Main"), (2, "Subsection")],
        "Level 2 headings must be extracted, at level 2."
    );

    let markdown_config = ExtractionConfig {
        output_format: OutputFormat::Markdown,
        ..Default::default()
    };
    let markdown = extract_bytes_document(test_content, "application/x-typst", &markdown_config)
        .await
        .expect("Extraction failed");

    assert!(
        markdown.content.contains("## Subsection"),
        "Markdown rendering must emit a level-2 heading, got: {:?}",
        markdown.content
    );
}

/// TEST 28: Regression - Basic metadata
///
/// Covers the keyword-tuple form inline as well: `test_typst_metadata_extraction_completeness`
/// asserts the same three fields against `metadata.typ`, but skips whenever its Pandoc
/// metadata baseline is absent from the bucket-fetched corpus — which it always is.
#[tokio::test]
async fn test_typst_basic_metadata_regression() {
    let test_content = b"#set document(title: \"Test\", author: \"Me\", keywords: (\"alpha\", \"beta\"))\n\n= Heading";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert_eq!(
        result.metadata.title.as_deref(),
        Some("Test"),
        "Title metadata must be extracted."
    );

    assert_eq!(
        result.metadata.authors.as_deref(),
        Some(&["Me".to_string()][..]),
        "Author metadata must be extracted."
    );

    assert_eq!(
        result.metadata.keywords.as_deref(),
        Some(&["alpha".to_string(), "beta".to_string()][..]),
        "Keyword tuples must be extracted."
    );
}

/// TEST 29: Regression - Bold formatting
#[tokio::test]
async fn test_typst_bold_regression() {
    let test_content = b"This is *bold text* here";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("*bold*") || result.content.contains("bold"),
        "Bold text should be preserved."
    );
}

/// TEST 30: Regression - Inline code
///
/// The Typst extractor strips inline-code backticks from the rendered text
/// and stores the spans as `code` annotations on the InternalDocument. This
/// preserves the *information* (which words are code) without polluting the
/// plain-text output with format markers — same approach as our other format
/// extractors. This test asserts the content survives round-trip.
#[tokio::test]
async fn test_typst_inline_code_regression() {
    let test_content = b"Use `println!(\"hello\")` in Rust";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(result.content.contains("println"), "inline code content lost");
    assert!(
        result.content.contains("Use") && result.content.contains("Rust"),
        "surrounding text lost"
    );
}

/// TEST 31: Regression - Code blocks
///
/// Code-block content survives extraction. The triple-backtick fence and
/// language tag are tracked as a `Code` element with `language` attribute on
/// the InternalDocument; downstream renderers (markdown, djot, html) emit the
/// fence, plain-text omits it. This test asserts the program text round-trips.
#[tokio::test]
async fn test_typst_codeblock_regression() {
    let test_content = b"```rust\nfn main() {}\n```";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(result.content.contains("fn main"), "code block content lost");
}

/// TEST 32: Regression - List extraction
#[tokio::test]
async fn test_typst_list_regression() {
    let test_content = b"- Item 1\n+ Item 2\n- Item 3";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("Item 1") && result.content.contains("Item 2") && result.content.contains("Item 3"),
        "All list items should be extracted."
    );
}

/// TEST 33: Regression - Math preservation
#[tokio::test]
async fn test_typst_math_regression() {
    let test_content = b"Formula: $E = mc^2$";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("$") && (result.content.contains("mc") || result.content.contains("E")),
        "Math formulas should be preserved."
    );
}

/// TEST 34: Regression - Link extraction
#[tokio::test]
async fn test_typst_link_regression() {
    let test_content = b"Visit #link(\"https://example.com\")[example]";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("example") || result.content.contains("example.com"),
        "Link text or URL should be preserved."
    );
}

/// TEST 35: Regression - Table basic extraction
#[tokio::test]
async fn test_typst_table_regression() {
    let test_content = b"#table(columns: 2, [A], [B], [1], [2])";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("A") || result.content.contains("TABLE"),
        "Table content should be extracted."
    );
}

/// TEST 36: Large document handling
#[tokio::test]
async fn test_typst_large_document_stress() {
    let mut large_content = String::new();

    for i in 1..=50 {
        large_content.push_str(&format!("= Heading {}\n\n", i));
        large_content.push_str(&format!("Content for section {}.\n\n", i));
    }

    let config = structure_config();
    let result = extract_bytes_document(large_content.as_bytes(), "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let expected: Vec<(u8, String)> = (1..=50).map(|i| (1, format!("Heading {}", i))).collect();
    let extracted: Vec<(u8, String)> = headings(&result)
        .into_iter()
        .map(|(level, text)| (level, text.to_string()))
        .collect();

    assert_eq!(
        extracted,
        expected,
        "Large documents should extract all 50 headings, in order, at level 1. Found {}.",
        extracted.len()
    );
}

/// TEST 37: Deep nesting stress test
#[tokio::test]
async fn test_typst_deep_nesting_stress() {
    let mut nested = String::new();

    for level in 1..=6 {
        nested.push_str(&format!("{} Level {} Heading\n\n", "=".repeat(level), level));
        nested.push_str(&format!("Content at level {}.\n\n", level));
    }

    let config = structure_config();
    let result = extract_bytes_document(nested.as_bytes(), "application/x-typst", &config)
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
        "Every level must survive deep nesting, at its own level."
    );
}

/// TEST 38: Mixed formatting stress
#[tokio::test]
async fn test_typst_mixed_formatting_stress() {
    let test_content = b"This text has *bold*, _italic_, `code`, and $math$ all mixed together!";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    let has_formatting = (result.content.contains("*") || result.content.contains("bold"))
        && (result.content.contains("_") || result.content.contains("italic"))
        && (result.content.contains("`") || result.content.contains("code"))
        && (result.content.contains("$") || result.content.contains("math"));

    assert!(has_formatting, "All mixed formatting should be preserved.");
}

/// TEST 39: Unicode stress test
#[tokio::test]
async fn test_typst_unicode_stress() {
    let test_content = "= Unicode Heading 中文 العربية\n\nContent with emojis: 🎉🚀💯\n\nGreek: α β γ δ ε ζ".as_bytes();

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("Unicode"),
        "Unicode content should be preserved."
    );
}

/// TEST 40: Pathological whitespace
#[tokio::test]
async fn test_typst_pathological_whitespace() {
    let test_content = b"= Heading\n\n\n\n\n\nContent with excessive blank lines\n\n\n\n\nMore content";

    let config = ExtractionConfig::default();
    let result = extract_bytes_document(test_content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.contains("Heading") && result.content.contains("Content"),
        "Should extract content even with excessive whitespace."
    );
}

/// TEST 41: Full document comparison - simple.typ
#[tokio::test]
async fn test_typst_full_simple_document_comparison() {
    let content = load_test_document("simple.typ");
    let _baseline = load_pandoc_baseline("simple");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.len() > 50,
        "simple.typ should extract substantial content"
    );

    assert_eq!(
        headings(&result),
        vec![
            (1, "Introduction"),
            (2, "Subsection"),
            (1, "Features"),
            (2, "Lists"),
            (2, "Code"),
            (2, "Tables"),
            (2, "Links"),
            (1, "Conclusion"),
        ],
        "simple.typ's section structure must survive intact"
    );
}

/// TEST 42: Full document comparison - advanced.typ
#[tokio::test]
async fn test_typst_full_advanced_document_comparison() {
    let content = load_test_document("advanced.typ");
    let _baseline = load_pandoc_baseline("advanced");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(
        result.content.len() > 100,
        "advanced.typ should extract comprehensive content"
    );

    assert_eq!(
        headings(&result),
        vec![
            (1, "Mathematical Notation"),
            (1, "Formatting Showcase"),
            (1, "Structured Content"),
            (2, "Code Blocks"),
            (2, "Nested Headings"),
            (3, "Level 3 heading"),
            (4, "Level 4 heading"),
            (1, "Tables and Data"),
            (1, "Multiple Paragraphs"),
            (1, "Conclusion"),
        ],
        "advanced.typ should preserve heading structure, including the nested levels 3 and 4"
    );
}

/// TEST 43: MIME type consistency
///
/// The extractor should support both standard MIME types for Typst.
/// Currently only supports application/x-typst, not text/x-typst.
#[tokio::test]
async fn test_typst_mime_type_consistency() {
    let content = load_test_document("simple.typ");
    let config = ExtractionConfig::default();

    let result_primary = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Primary MIME type should work");

    assert!(
        result_primary.content.len() > 0,
        "Primary MIME type should extract content"
    );

    match extract_bytes_document(&content, "text/x-typst", &config).await {
        Ok(result) => {
            assert!(
                result.content.len() > 0,
                "Alternative MIME type should extract content if supported"
            );
        }
        Err(_e) => {
            println!("Note: text/x-typst is not currently supported (may be added in future)");
        }
    }
}

/// TEST 44: Config parameter impact
#[tokio::test]
async fn test_typst_config_parameter_handling() {
    let content = load_test_document("simple.typ");
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    assert!(!result.content.is_empty(), "Extraction with default config should work");

    assert_eq!(result.mime_type, "application/x-typst", "MIME type should be preserved");
}

/// TEST 45: Comparative heading analysis
///
/// This final comprehensive test checks heading extraction
/// against the baseline to identify the exact scope of the heading loss bug.
#[tokio::test]
async fn test_typst_heading_loss_bug_analysis() {
    let content = load_test_document("headings.typ");
    let baseline = load_pandoc_baseline("headings");
    let config = structure_config();

    let result = extract_bytes_document(&content, "application/x-typst", &config)
        .await
        .expect("Extraction failed");

    println!("\n===== HEADING EXTRACTION ANALYSIS =====");
    println!("Baseline content:");
    println!("{}", baseline);
    println!("\nExtracted content:");
    println!("{}", result.content);

    let extracted_headings = headings(&result);
    println!("\nExtracted headings: {}", extracted_headings.len());
    for (i, (level, text)) in extracted_headings.iter().enumerate() {
        println!("  {}: level {} — {}", i + 1, level, text);
    }

    let levels: Vec<u8> = extracted_headings.iter().map(|(level, _)| *level).collect();
    assert_eq!(
        levels,
        vec![1, 2, 3, 4, 5, 6],
        "Heading loss detected: expected one heading per level 1-6, found {:?}. \
         The historical bug matched only a single '=', skipping '==' and deeper entirely.",
        extracted_headings
    );
}
