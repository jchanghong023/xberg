//! Regression test for https://github.com/xberg-io/xberg/issues/1829
//!
//! A candle backend's `backend_options` are parsed by the backend on each page. The automatic OCR
//! route keeps native text when a page fails, so an invalid value there used to surface only as a
//! warning. Configuration validation now runs the same check before any page, so these cases
//! extract a plain-text document: it needs no OCR, and only the up-front check can reject it.
//! Legacy `paddle_ocr_config` JSON is validated before extraction, while invalid typed
//! `paddle_ocr_settings` are rejected when the config file loads. ~keep
//!
//! The `cfg` below looks narrow but `--features full` satisfies all of it: `full` pulls in
//! `formats` (so `pdf`), `ocr`, `candle-vlm-ocr` (so both `candle-trocr` and
//! `candle-paddleocr-vl`) and `paddle-ocr` (so `paddle-ocr-ort`, which makes `build.rs` emit
//! `cfg(paddle_ocr)`). That is the feature set `scripts/ci/rust/run-unit-tests.sh` builds, so
//! these tests run on the x86-64 Linux and macOS legs. Only the aarch64-Linux leg substitutes
//! `full-no-heic`, which excludes candle deliberately, and skips them. Do not "fix" this cfg
//! without first checking a CI log for these test names -- GH#1893 was filed on the assumption
//! that no build compiled them. The two tests that need no candle or paddle feature at all live
//! in `issue_1829_per_page_failure_keeps_native_text.rs`. ~keep

#![allow(deprecated, clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test/bench binaries print by design; org logging policy exempts tests
#![cfg(all(
    feature = "ocr",
    feature = "pdf",
    feature = "candle-trocr",
    feature = "candle-paddleocr-vl",
    paddle_ocr
))]

use xberg::XbergError;
use xberg::core::config::{ExtractInput, ExtractionConfig, OcrConfig, OcrPipelineConfig, OcrPipelineStage};

const PLAIN_TEXT: &str = "Plain text that needs no OCR.";

async fn extract_plain_text(ocr: OcrConfig) -> xberg::Result<xberg::ExtractionResult> {
    let config = ExtractionConfig {
        ocr: Some(ocr),
        ..Default::default()
    };
    xberg::extract(
        ExtractInput::from_bytes(
            PLAIN_TEXT.as_bytes().to_vec(),
            "text/plain",
            Some("gh1829.txt".to_string()),
        ),
        &config,
    )
    .await
}

fn expect_validation_error(result: xberg::Result<xberg::ExtractionResult>, needle: &str) {
    match result {
        Err(XbergError::Validation { message, .. }) => assert!(
            message.contains(needle),
            "the validation error must name the rejected setting ({needle}): {message}"
        ),
        Err(other) => panic!("expected a validation error naming {needle}, got {other:?}"),
        Ok(_) => panic!("an invalid {needle} must fail the extraction before any page runs"),
    }
}

fn pipeline_with_stage(stage: OcrPipelineStage) -> OcrPipelineConfig {
    OcrPipelineConfig {
        stages: vec![stage],
        quality_thresholds: Default::default(),
    }
}

fn stage(backend: &str) -> OcrPipelineStage {
    OcrPipelineStage {
        backend: backend.to_string(),
        priority: 100,
        language: None,
        tesseract_config: None,
        paddle_ocr_config: None,
        paddle_ocr_settings: None,
        vlm_config: None,
        backend_options: None,
    }
}

#[tokio::test]
async fn should_fail_before_any_page_when_candle_backend_options_are_invalid() {
    let result = extract_plain_text(OcrConfig {
        backend: "candle-trocr".to_string(),
        backend_options: Some(serde_json::json!({"variant": "no-such-variant"})),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "candle-trocr backend_options");
}

#[tokio::test]
async fn should_fail_before_any_page_when_trocr_hf_revision_is_blank() {
    let result = extract_plain_text(OcrConfig {
        backend: "candle-trocr".to_string(),
        backend_options: Some(serde_json::json!({"hf_revision": "  "})),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "candle-trocr backend_options.hf_revision");
}

#[tokio::test]
async fn should_fail_before_any_page_when_a_pipeline_stage_has_invalid_candle_backend_options() {
    let result = extract_plain_text(OcrConfig {
        pipeline: Some(pipeline_with_stage(OcrPipelineStage {
            backend_options: Some(serde_json::json!({"model_id": "  "})),
            ..stage("candle-paddleocr-vl")
        })),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "candle-paddleocr-vl backend_options.model_id");
}

#[cfg(feature = "candle-glm-ocr")]
#[tokio::test]
async fn should_fail_before_any_page_when_glm_ocr_backend_options_are_invalid() {
    let result = extract_plain_text(OcrConfig {
        backend: "candle-glm-ocr".to_string(),
        backend_options: Some(serde_json::json!({"cache_dir": "  "})),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "candle-glm-ocr backend_options.cache_dir");
}

#[cfg(feature = "candle-deepseek-ocr")]
#[tokio::test]
async fn should_fail_before_any_page_when_deepseek_ocr_backend_options_are_invalid() {
    let result = extract_plain_text(OcrConfig {
        backend: "candle-deepseek-ocr".to_string(),
        backend_options: Some(serde_json::json!({"version": 3})),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "candle-deepseek-ocr backend_options.version");
}

fn load_config_file(file_name: &str, contents: &str) -> xberg::Result<ExtractionConfig> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(file_name);
    std::fs::write(&path, contents).expect("write config file");
    ExtractionConfig::from_file(&path)
}

fn expect_config_error(result: xberg::Result<ExtractionConfig>, needle: &str) {
    match result {
        Err(XbergError::Validation { message, .. }) => assert!(
            message.contains(needle),
            "the config error must name the rejected setting ({needle}): {message}"
        ),
        Err(other) => panic!("expected a validation error naming {needle}, got {other:?}"),
        Ok(_) => panic!("an invalid {needle} must fail when the config loads"),
    }
}

#[test]
fn should_fail_config_loading_naming_the_key_when_paddle_ocr_settings_are_invalid() {
    let result = load_config_file(
        "xberg.toml",
        "[ocr.paddle_ocr_settings]\ndet_db_thresh = \"not a number\"\n",
    );

    expect_config_error(result, "det_db_thresh");
}

#[test]
fn should_fail_config_loading_naming_the_key_when_a_pipeline_stage_has_invalid_paddle_ocr_settings() {
    let result = load_config_file(
        "xberg.toml",
        "[[ocr.pipeline.stages]]\nbackend = \"paddle-ocr\"\n\n[ocr.pipeline.stages.paddle_ocr_settings]\nuse_angle_cls = \"yes\"\n",
    );

    expect_config_error(result, "use_angle_cls");
}

#[test]
fn should_fail_config_loading_naming_the_key_when_paddle_ocr_settings_have_an_unknown_key() {
    let result = load_config_file(
        "xberg.json",
        r#"{"ocr": {"paddle_ocr_settings": {"useAngleCls": true}}}"#,
    );

    expect_config_error(result, "useAngleCls");
}

#[tokio::test]
async fn should_extract_when_backend_options_and_paddle_ocr_settings_are_valid() {
    let result = extract_plain_text(OcrConfig {
        backend: "candle-trocr".to_string(),
        backend_options: Some(serde_json::json!({"variant": "large-printed", "cache_dir": "/tmp/models"})),
        paddle_ocr_settings: Some(xberg::PaddleOcrConfig {
            det_db_thresh: 0.4,
            use_angle_cls: false,
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("valid backend options must not fail validation");

    assert!(
        result.results.iter().any(|doc| doc.content.contains(PLAIN_TEXT)),
        "the document must still be extracted: {:?}",
        result.results.first().map(|doc| doc.content.clone())
    );
}

#[tokio::test]
async fn should_fail_before_any_page_when_legacy_paddle_ocr_config_is_invalid() {
    let result = extract_plain_text(OcrConfig {
        backend: "paddle-ocr".to_string(),
        paddle_ocr_config: Some(serde_json::json!({"det_db_thresh": "not a number"})),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "paddle_ocr_config");
}

#[tokio::test]
async fn should_fail_before_any_page_when_pipeline_legacy_paddle_ocr_config_is_invalid() {
    let result = extract_plain_text(OcrConfig {
        pipeline: Some(pipeline_with_stage(OcrPipelineStage {
            paddle_ocr_config: Some(serde_json::json!({"use_angle_cls": "yes"})),
            ..stage("paddle-ocr")
        })),
        ..Default::default()
    })
    .await;

    expect_validation_error(result, "paddle_ocr_config");
}

#[tokio::test]
async fn should_extract_when_legacy_paddle_ocr_config_has_unknown_extension_keys() {
    let result = extract_plain_text(OcrConfig {
        backend: "paddle-ocr".to_string(),
        paddle_ocr_config: Some(serde_json::json!({
            "model_tier": "server",
            "vendor_extension": {"enabled": true}
        })),
        ..Default::default()
    })
    .await
    .expect("unknown legacy PaddleOCR extension keys must remain compatible");

    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].content, PLAIN_TEXT);
}
