//! Tests verifying CSV extraction uses canonical plain table rendering.

mod helpers;
use helpers::extract_bytes_document;

use xberg::core::config::ExtractionConfig;

/// Header and data rows should use the shared space-separated table rendering.
#[tokio::test]
async fn test_csv_preserves_header_value_association() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age,City\nAlice,30,NYC\nBob,25,LA\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    // fork 默认 Markdown 渲染（fork.md）：CSV 渲染为管道表格；上游断言按 Plain 空格拼接写
    assert_eq!(
        result.content,
        "| Name | Age | City |\n| --- | --- | --- |\n| Alice | 30 | NYC |\n| Bob | 25 | LA |\n"
    );
}

/// Rows should remain one line each without extractor-specific labels.
#[tokio::test]
async fn test_csv_row_grouping() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Score\nAlice,95\nBob,88\nCarol,72\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    // fork 默认 Markdown 渲染（fork.md）：CSV 渲染为管道表格；上游断言按 Plain 空格拼接写
    assert_eq!(
        result.content,
        "| Name | Score |\n| --- | --- |\n| Alice | 95 |\n| Bob | 88 |\n| Carol | 72 |\n"
    );
}

/// Empty cells should retain their column position in canonical table rendering.
#[tokio::test]
async fn test_csv_skips_empty_values() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age,City\nAlice,,NYC\nBob,25,LA\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    // fork 默认 Markdown 渲染（fork.md）：空单元格保留列位（空管道单元格）；上游断言按 Plain 空格拼接写
    assert_eq!(
        result.content,
        "| Name | Age | City |\n| --- | --- | --- |\n| Alice |  | NYC |\n| Bob | 25 | LA |\n"
    );
}

/// The tables field should still contain the full parsed structure.
#[tokio::test]
async fn test_csv_tables_field_unchanged() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age\nAlice,30\nBob,25\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].cells.len(), 3);
    assert_eq!(result.tables[0].cells[0], vec!["Name", "Age"]);
    assert!(!result.tables[0].markdown.is_empty());
}

/// Rows shorter than the header should not panic; extra headers are skipped.
#[tokio::test]
async fn test_csv_short_row_no_panic() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age,City\nAlice,30\nBob,25,LA\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    // fork 默认 Markdown 渲染（fork.md）：短行补空单元格对齐表头列数；上游断言按 Plain 空格拼接写
    assert_eq!(
        result.content,
        "| Name | Age | City |\n| --- | --- | --- |\n| Alice | 30 |  |\n| Bob | 25 | LA |\n"
    );
}

/// Rows where all cells are empty should be omitted entirely.
#[tokio::test]
async fn test_csv_all_empty_data_rows() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age\n,,\nAlice,30\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    // fork 默认 Markdown 渲染（fork.md）：全空数据行在管道表格中保留为空行；上游断言按 Plain 空格拼接写
    assert_eq!(
        result.content,
        "| Name | Age |  |\n| --- | --- | --- |\n|  |  |  |\n| Alice | 30 |  |\n"
    );
}

/// When no header is detected, should fall back to space-separated output.
#[tokio::test]
async fn test_csv_no_header_fallback() {
    let config = ExtractionConfig::default();
    let csv = b"Alice,NYC,Engineer\nBob,LA,Designer\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    assert!(
        !result.content.contains("Row 1:"),
        "No header detected — should not label rows"
    );
    assert!(result.content.contains("Alice"));
    assert!(result.content.contains("Bob"));
}

/// Header-only CSV (no data rows) should still produce output.
#[tokio::test]
async fn test_csv_header_only() {
    let config = ExtractionConfig::default();
    let csv = b"Name,Age,City\n";

    let result = extract_bytes_document(csv, "text/csv", &config).await.unwrap();

    assert!(!result.content.is_empty());
    assert!(result.content.contains("Name"));
}

/// Real CSV files use the same canonical table rendering.
#[tokio::test]
async fn test_csv_real_file_header_value() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/csv/data_table.csv");
    if !path.exists() {
        return;
    }
    let content = std::fs::read(&path).unwrap();
    let config = ExtractionConfig::default();

    let result = extract_bytes_document(&content, "text/csv", &config).await.unwrap();

    assert!(result.content.contains("Alice Johnson"));
    assert!(result.content.contains("Engineering"));
    assert!(!result.content.contains("Row 1:"));
}
