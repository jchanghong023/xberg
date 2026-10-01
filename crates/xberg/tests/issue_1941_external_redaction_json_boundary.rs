#![cfg(all(feature = "redaction", feature = "tokio-runtime"))]

use std::borrow::Cow;
use std::future::Future;

use xberg::extractors::security::SecurityLimits;
use xberg::{
    ExtractInput, ExtractedDocument, ExtractionConfig, RedactionConfig, extract_with_external_redaction,
    redact_external,
};

const MASK: &str = "[REDACTED]";

fn run_extraction_test<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::Builder::new()
        .name("external-redaction-json-boundary-test".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(future)
        })
        .expect("test thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn document(content: &str) -> ExtractedDocument {
    let mut document = ExtractedDocument::default();
    document.content = content.to_string();
    document.mime_type = Cow::Borrowed("text/plain");
    document
}

#[tokio::test]
async fn should_preserve_raw_azure_offsets_unknown_fields_and_min_score_at_the_boundary() {
    let config = RedactionConfig {
        min_score: Some(0.8),
        ..Default::default()
    };
    let redacted = redact_external(
        document("Zarnak Quorlim met Blorp Nazzle."),
        config,
        r#"[
            {"category":"Person","offset":0,"length":14,"confidenceScore":0.79,"warnings":["review"]},
            {"category":"Person","offset":19,"length":12,"confidenceScore":0.8,"providerMetadata":{"model":"v2"}}
        ]"#,
        None,
        Some(10),
    )
    .await
    .expect("raw Azure findings must be parsed by Rust");

    assert_eq!(redacted.content, format!("Zarnak Quorlim met {MASK}."));
}

#[tokio::test]
async fn should_preserve_nested_gcp_fields_at_the_boundary() {
    let redacted = redact_external(
        document("Zoë emailed."),
        RedactionConfig::default(),
        r#"[{
            "infoType":{"name":"PERSON_NAME"},
            "likelihood":"VERY_LIKELY",
            "location":{
                "codepointRange":{"start":"0","end":"3"},
                "byteRange":{"start":"0","end":"4"}
            },
            "quote":"Zoë"
        }]"#,
        None,
        Some(10),
    )
    .await
    .expect("raw GCP findings must be parsed by Rust");

    assert_eq!(redacted.content, format!("{MASK} emailed."));
}

#[tokio::test]
async fn should_bound_boundary_parsing_before_a_later_malformed_entry() {
    let error = redact_external(
        document("Zarnak Quorlim"),
        RedactionConfig::default(),
        r#"[
            {"entity_type":"PERSON","text":"Zarnak"},
            {"entity_type":"PERSON","text":"Quorlim"},
            not-json
        ]"#,
        None,
        Some(1),
    )
    .await
    .expect_err("the boundary must stop at the item limit");

    assert!(error.to_string().contains("maximum of 1"), "{error}");
    assert!(!error.to_string().contains("expected ident"), "{error}");
}

#[test]
fn should_apply_the_extraction_security_limit_while_parsing_the_boundary_payload() {
    run_extraction_test(async {
        let config = ExtractionConfig {
            security_limits: Some(SecurityLimits {
                max_iterations: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let error = extract_with_external_redaction(
            ExtractInput::from_bytes(b"Zarnak Quorlim".to_vec(), "text/plain", None),
            &config,
            r#"[
                {"entity_type":"PERSON","text":"Zarnak"},
                {"entity_type":"PERSON","text":"Quorlim"},
                not-json
            ]"#,
            None,
            Some(8),
        )
        .await
        .expect_err("the configured security limit must bound parsing");

        assert!(error.to_string().contains("maximum of 1"), "{error}");
        assert!(!error.to_string().contains("expected ident"), "{error}");
    });
}
