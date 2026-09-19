//! Regression tests for issue #137: the Typst extractor must correctly handle
//! multi-line display math blocks, `#figure(image(...), caption: [...])`
//! figure/caption pairs, and `#cite(<key>)` / `@key` citation references
//! instead of leaking raw Typst source or dropping the content.
//!
//! Uses the default (plain-text) `ExtractionConfig`, whose rendering is exact
//! and predictable: `ElementKind::Formula`/`Citation` render as
//! `"{text}\n\n"`, `ElementKind::Paragraph` renders as `"{text}\n\n"`, so the
//! resulting `extraction.content` string can be asserted on exactly.
//!
//! fork 默认 Markdown 渲染（fork.md）：公式为 `$$…$$` 块、引用 key 为脚注定义 `[^key]:`、
//! 引用块为 `> ` 前缀、标题带 `#`；断言按实际 Markdown 形态写（上游按 Plain 写）。

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test/bench binaries print by design; org logging policy exempts tests
#![cfg(feature = "office")]

mod helpers;
use helpers::extract_bytes_document;

use xberg::core::config::ExtractionConfig;

const TYPST_MIME: &str = "text/x-typst";

#[tokio::test]
async fn should_extract_multiline_display_math_as_its_own_formula_block() {
    let content = b"Text before.\n\n$\na^2 + b^2 = c^2\n$\n\nText after.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "Text before.\n\n$$a^2 + b^2 = c^2$$\n\nText after.\n",
        "multi-line display math should be captured as its own formula block, not merged into paragraph text"
    );
}

#[tokio::test]
async fn should_extract_single_line_display_math_distinct_from_surrounding_prose() {
    let content = b"Text before.\n\n$ a^2 + b^2 = c^2 $\n\nText after.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "Text before.\n\n$$a^2 + b^2 = c^2$$\n\nText after.\n",
        "single-line display math block should still be captured as a formula block"
    );
}

#[tokio::test]
async fn should_not_lose_display_math_body_spanning_many_lines() {
    let content = b"$\nx = 1 \\\ny = 2 \\\nz = x + y\n$".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "$$x = 1 \\\\ y = 2 \\\\ z = x + y$$\n",
        "every line of a multi-line display math block must survive, as LaTeX line breaks"
    );
}

#[tokio::test]
async fn should_extract_figure_image_reference_and_caption_as_a_pairing() {
    let content = br#"#figure(
  image("diagram.png"),
  caption: [System architecture overview]
)"#
    .to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "[Image: diagram.png]\n\nSystem architecture overview\n",
        "figure image path and caption text must both be extracted, with the caption preserved verbatim"
    );
    assert!(
        !extraction.content.contains("#figure"),
        "raw Typst #figure(...) source must not leak into extracted content: {}",
        extraction.content
    );
}

#[tokio::test]
async fn should_extract_single_line_figure_caption() {
    let content = br#"#figure(image("chart.png"), caption: [Quarterly results])"#.to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "[Image: chart.png]\n\nQuarterly results\n",
        "single-line #figure(...) must still pair the image reference with its caption"
    );
}

#[tokio::test]
async fn should_extract_cite_function_call_as_citation_reference() {
    let content = b"This follows prior work #cite(<smith2020>) closely.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "This follows prior work [smith2020] closely.\n\n[^smith2020]:\n    smith2020\n",
        "a #cite(<key>) reference must be recognized and surfaced, not left as raw Typst source"
    );
    assert!(
        !extraction.content.contains("#cite"),
        "raw #cite(...) syntax must not leak into extracted content: {}",
        extraction.content
    );
}

#[tokio::test]
async fn should_extract_at_key_citation_shorthand() {
    let content = b"As shown by @jones2021, the effect is significant.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "As shown by [jones2021], the effect is significant.\n\n[^jones2021]:\n    jones2021\n",
        "an @key citation shorthand must be recognized and surfaced, not left as raw Typst source"
    );
}

#[tokio::test]
async fn should_extract_multiple_citations_in_one_paragraph() {
    let content = b"See #cite(<a2020>) and also @b2021 for background.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content,
        "See [a2020] and also [b2021] for background.\n\n[^a2020]:\n    a2020\n\n[^b2021]:\n    b2021\n",
        "both citation forms in the same paragraph must be extracted in encounter order"
    );
}

#[tokio::test]
async fn should_treat_bibliography_directive_as_skipped_not_leaked_text() {
    let content = b"= References\n\n#bibliography(\"refs.bib\")\n\nDone.".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert!(
        !extraction.content.contains("#bibliography"),
        "raw #bibliography(...) directive must not leak into extracted content: {}",
        extraction.content
    );
    assert_eq!(extraction.content, "# References\n\nDone.\n");
}

#[tokio::test]
async fn should_extract_quote_block_content() {
    let content = b"#quote[This is a memorable quotation.]".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "> This is a memorable quotation.\n",
        "#quote[...] content must be extracted as quoted text, not raw Typst source"
    );
}

#[tokio::test]
async fn should_extract_term_description_list_entry() {
    let content = b"/ RAM: Random Access Memory".to_vec();
    let config = ExtractionConfig::default();

    let extraction = extract_bytes_document(&content, TYPST_MIME, &config)
        .await
        .expect("Typst extraction should succeed");

    assert_eq!(
        extraction.content, "RAM\n\n: Random Access Memory\n",
        "/ term: description syntax must be recognized as a definition list entry"
    );
}
