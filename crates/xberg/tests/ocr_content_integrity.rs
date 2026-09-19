//! Regression test for https://github.com/xberg-io/xberg/issues/706
//!
//! Tesseract OCR was producing corrupted page content: the top-level `content`
//! field contained the coherent HOCR-rendered text followed by a word-by-word
//! dump of every OcrText element, effectively doubling the output.
//!
//! Root cause: `inject_ocr_elements_from_vec` pushed each OcrElement into
//! `InternalDocument::elements` as an `ElementKind::OcrText`. The rendering
//! pipeline (`render_plain`) iterated those elements and appended every word
//! token back into `content`, on top of the already-rendered HOCR string.
//!
//! Fix: OCR elements are now stored directly in `InternalDocument::prebuilt_ocr_elements`
//! (bypassing the rendering pipeline) and page content is set via
//! `InternalDocument::prebuilt_pages` (bypassing the word-grouped fallback in
//! `build_pages`).

#![cfg(feature = "ocr")]

mod helpers;
use helpers::extract_uri_document_blocking;

use helpers::*;
use xberg::core::config::{ExtractionConfig, OcrConfig, PageConfig};
use xberg::types::TesseractConfig;

/// Strip the fork image-OCR presentation scaffolding from rendered content:
/// image markers (`![...]`) and the ` ```text ` OCR layout fence blocks.
///
/// fork 默认行为（fork.md）：抽取的图片以 marker 段落引用，其 OCR 文本以 ```text
/// 布局围栏输出——这是需求要求保留的第二形态。上游按 Plain 总量写的断言在此
/// 改为「围栏外」口径：word-dump / 表格外段落重复等回归仍会在围栏外现形。
fn strip_image_ocr_scaffolding(content: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.starts_with("![") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Content must not be doubled when OCR is enabled.
///
/// Before the fix, `content` contained the HOCR-rendered paragraph text
/// immediately followed by a word-by-word dump of every OcrText element,
/// roughly doubling the word count.  After the fix the two representations
/// must be absent: `content` should equal approximately what is in `pages[0].content`.
#[test]
fn test_ocr_content_not_doubled() {
    if skip_if_missing("images/test_hello_world.png") {
        return;
    }

    let file_path = get_test_file_path("images/test_hello_world.png");
    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            language: vec!["eng".to_string()],
            ..Default::default()
        }),
        pages: Some(PageConfig {
            extract_pages: true,
            ..Default::default()
        }),
        use_cache: false,
        ..Default::default()
    };

    let result = extract_uri_document_blocking(&file_path, None, &config).expect("OCR extraction must succeed");

    let content_words: Vec<&str> = result.content.split_whitespace().collect();

    if content_words.is_empty() {
        return;
    }

    let pages = result
        .pages
        .as_ref()
        .expect("pages must be populated when extract_pages=true");
    assert!(!pages.is_empty(), "at least one page must be present");

    let page_content = &pages[0].content;
    let page_words: Vec<&str> = page_content.split_whitespace().collect();

    // fork 默认行为：content 为图片 marker + ```text OCR 围栏（fork.md），词数比被
    // 结构标记放大；#706 的「不双倍」契约改为逐词计数——content 中每个词的出现
    // 次数不得超过 page 中该词的次数（word dump 会使其翻倍）。
    if !page_words.is_empty() {
        for word in &page_words {
            let in_content = result.content.matches(*word).count();
            let in_page = page_content.matches(*word).count();
            assert!(
                in_content <= in_page,
                "word {:?} appears {} times in content but {} times in pages[0].content \
                 — word-token dump appended after HOCR text (issue #706)",
                word,
                in_content,
                in_page,
            );
        }
    }

    if page_content.trim().len() > 4 {
        let trimmed = page_content.trim();
        let doubled = format!("{trimmed}{trimmed}");
        assert!(
            !result.content.contains(doubled.as_str()),
            "content appears to contain page text concatenated with itself — doubled output (issue #706)",
        );
    }
}

/// Page content must match the top-level content (after trimming) when there
/// is only one page, for any image that produces non-empty OCR output.
#[test]
fn test_ocr_page_content_matches_top_level_content() {
    if skip_if_missing("images/ocr_image.jpg") {
        return;
    }

    let file_path = get_test_file_path("images/ocr_image.jpg");
    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            language: vec!["eng".to_string()],
            ..Default::default()
        }),
        pages: Some(PageConfig {
            extract_pages: true,
            ..Default::default()
        }),
        use_cache: false,
        ..Default::default()
    };

    let result = extract_uri_document_blocking(&file_path, None, &config).expect("OCR extraction must succeed");

    if result.content.trim().is_empty() {
        return;
    }

    let pages = result
        .pages
        .as_ref()
        .expect("pages must be populated when extract_pages=true");
    assert!(!pages.is_empty(), "at least one page must be present");

    // fork 默认行为：top-level content 在常规段落外还带图片 marker 与 ```text OCR
    // 围栏（fork.md 要求保留）；#706 契约改为围栏外词数不显著超过 page。
    let top_words = strip_image_ocr_scaffolding(&result.content).split_whitespace().count();
    let page_words = pages[0].content.split_whitespace().count();

    if top_words > 0 && page_words > 0 {
        let ratio = top_words as f64 / page_words.max(1) as f64;
        assert!(
            ratio <= 1.3,
            "top-level content ({} words) is more than 30% larger than pages[0].content ({} words). \
             Indicates word-dump appended to top-level content but missing from page — issue #706.",
            top_words,
            page_words,
        );
    }
}

/// A detected table's text must not also survive as ordinary paragraph text (issue #1571).
///
/// `hocr_document` is parsed from the raw hOCR before table detection runs, so nothing
/// removes a table's words from it once `tables` is computed. Every consumer built from
/// `internal_document` therefore carries the table's words twice: once as `tables[].markdown`
/// and once as prose. Regression coverage for #706 above uses a table-free image and never
/// exercises this path, so this test uses `images/simple_table.png` with table detection on.
#[test]
fn test_ocr_table_text_not_duplicated_in_content() {
    if skip_if_missing("images/simple_table.png") {
        return;
    }

    let file_path = get_test_file_path("images/simple_table.png");
    let config = ExtractionConfig {
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            language: vec!["eng".to_string()],
            tesseract_config: Some(TesseractConfig {
                enable_table_detection: true,
                table_min_confidence: 0.0,
                // The default 50px column threshold is too tight for this fixture's font and
                // splits it into extra spurious columns that fail `post_process_table`'s
                // well-formedness check, so table detection never fires and the test can't
                // reproduce #1571. 80px merges those back into the real 4 columns.
                table_column_threshold: 80,
                table_row_threshold_ratio: 0.5,
                ..Default::default()
            }),
            ..Default::default()
        }),
        force_ocr: false,
        use_cache: false,
        ..Default::default()
    };

    let result = extract_uri_document_blocking(&file_path, None, &config).expect("OCR extraction must succeed");

    assert!(
        !result.tables.is_empty(),
        "table detection must fire on this fixture for the test to exercise the #1571 path"
    );

    // Pick a distinctive cell value from the detected table and confirm it appears in
    // `doc.content` only as many times as it appears across the table cells themselves
    // (usually once) -- not once more as a duplicated paragraph outside the table.
    let table = &result.tables[0];
    let mut cell_values: Vec<&str> = table
        .cells
        .iter()
        .flatten()
        .map(String::as_str)
        .filter(|cell| cell.trim().len() >= 3)
        .collect();
    cell_values.sort_unstable();
    cell_values.dedup();

    for cell in cell_values {
        let occurrences_in_table_cells = table.cells.iter().flatten().filter(|c| c.as_str() == cell).count();
        // fork 默认行为：```text OCR 围栏含全部 OCR 文本（含表格内容，fork.md 要求
        // 保留）；#1571 契约改为围栏外文本中表格 cell 不得重复出现。
        let occurrences_in_content = strip_image_ocr_scaffolding(&result.content).matches(cell).count();
        assert!(
            occurrences_in_content <= occurrences_in_table_cells,
            "table cell {cell:?} appears {occurrences_in_content} times in doc.content but only \
             {occurrences_in_table_cells} times among table cells -- its text is duplicated outside \
             the table (issue #1571)",
        );
    }
}
