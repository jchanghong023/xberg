//! Confidence-threshold regressions for external redaction findings (GH#1941).

#![cfg(all(feature = "redaction", feature = "tokio-runtime"))]

use std::borrow::Cow;

use xberg::text::redaction::redact_with_entities;
use xberg::{
    ExternalRedactionFinding, ExtractInput, ExtractedDocument, ExtractionConfig, RedactionConfig,
    extract_with_external_redaction,
};

const MASK: &str = "[REDACTED]";

fn document(content: &str) -> ExtractedDocument {
    let mut document = ExtractedDocument::default();
    document.content = content.to_string();
    document.mime_type = Cow::Borrowed("text/plain");
    document
}

fn finding(label: &str, text: &str, score: Option<f32>) -> ExternalRedactionFinding {
    ExternalRedactionFinding {
        label: label.to_string(),
        text: Some(text.to_string()),
        score,
        ..Default::default()
    }
}

#[test]
fn should_reject_invalid_external_finding_score_thresholds() {
    for min_score in [-0.1, 1.1, f32::NAN] {
        let error = RedactionConfig {
            min_score: Some(min_score),
            ..Default::default()
        }
        .validate()
        .expect_err("an external finding score threshold must be within [0, 1]");

        assert!(error.to_string().contains("RedactionConfig.min_score"), "{error}");
    }
}

#[test]
fn should_filter_inline_findings_below_the_score_threshold() {
    let mut output = document("Zarnak Quorlim met Blorp Nazzle and Miskatonic.");
    let config = RedactionConfig {
        min_score: Some(0.8),
        findings: vec![
            finding("LOW", "Zarnak Quorlim", Some(0.79)),
            finding("EXACT", "Blorp Nazzle", Some(0.8)),
            finding("UNSCORED", "Miskatonic", None),
        ],
        ..Default::default()
    };

    redact_with_entities(&mut output, &config, &[]).expect("inline findings must redact");

    assert_eq!(output.content, format!("Zarnak Quorlim met {MASK} and {MASK}."));
}

#[test]
fn should_filter_file_findings_below_the_score_threshold() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("findings.json");
    std::fs::write(
        &path,
        r#"[
            {"label":"LOW","text":"Zarnak Quorlim","score":0.7},
            {"label":"HIGH","text":"Blorp Nazzle","score":0.9}
        ]"#,
    )
    .expect("write findings");
    let mut output = document("Zarnak Quorlim met Blorp Nazzle.");
    let config = RedactionConfig {
        min_score: Some(0.8),
        findings_path: Some(path),
        ..Default::default()
    };

    redact_with_entities(&mut output, &config, &[]).expect("file findings must redact");

    assert_eq!(output.content, format!("Zarnak Quorlim met {MASK}."));
}

#[tokio::test]
async fn should_filter_request_scoped_findings_below_the_score_threshold() {
    let config = ExtractionConfig {
        redaction: Some(RedactionConfig {
            min_score: Some(0.8),
            ..Default::default()
        }),
        ..Default::default()
    };
    let output = extract_with_external_redaction(
        ExtractInput::from_bytes(b"Zarnak Quorlim met Blorp Nazzle.".to_vec(), "text/plain", None),
        &config,
        r#"[
            {"category":"LOW","text":"Zarnak Quorlim","confidenceScore":0.7,"warnings":[]},
            {"category":"HIGH","text":"Blorp Nazzle","confidenceScore":0.9,"providerMetadata":{"model":"v2"}}
        ]"#,
        None,
        Some(10),
    )
    .await
    .expect("request-scoped findings must redact");

    assert_eq!(output.results[0].content, format!("Zarnak Quorlim met {MASK}."));
}
