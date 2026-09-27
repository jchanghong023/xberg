//! Integration tests for the transcription extractor end-to-end path
//! (SV-01: FFmpeg DLL decode → Silero VAD → SenseVoice INT8).
//!
//! Tests that need the pinned models / native DLLs / real speech media are
//! runtime-gated: they print the missing variable and return when absent, so
//! `cargo test` stays green on machines without the assets. Environment
//! variables (same contract as `transcription::sensevoice` unit tests):
//! - `XBERG_TEST_SENSEVOICE_ROOT` — model root directory
//! - `XBERG_TEST_SHERPA_DLL_DIR` / `XBERG_TEST_FFMPEG_DLL_DIR` — native libraries
//! - `XBERG_TEST_MEDIA_MP4` — real speech MP4
//! - `XBERG_TEST_MEDIA_NO_AUDIO_MP4` — MP4 container without an audio track
//!
//! Run with:
//!
//! ```text
//! cargo test -p xberg --features transcription --test transcription_extractor
//! ```

#![cfg(feature = "transcription")]

mod helpers;
use helpers::{extract_bytes_document, extract_bytes_document_blocking};

use xberg::core::config::ExtractionConfig;
use xberg::core::config::transcription::TranscriptionConfig;

fn config_with_transcription() -> ExtractionConfig {
    ExtractionConfig {
        transcription: Some(TranscriptionConfig {
            enabled: true,
            // Real model load + decode needs more than the 10 s default? No —
            // the default is 10 minutes; keep it explicit for clarity.
            timeout_ms: Some(600_000),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Resolve the SenseVoice test assets, printing which one is missing.
/// Returns `None` when anything is absent (runtime gating).
fn sensevoice_assets() -> Option<()> {
    let root = std::env::var_os("XBERG_TEST_SENSEVOICE_ROOT")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir());
    let sherpa = std::env::var_os("XBERG_TEST_SHERPA_DLL_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir());
    let ffmpeg = std::env::var_os("XBERG_TEST_FFMPEG_DLL_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir());
    let (Some(root), Some(sherpa), Some(ffmpeg)) = (&root, &sherpa, &ffmpeg) else {
        if root.is_none() {
            println!("skip：XBERG_TEST_SENSEVOICE_ROOT 不在场");
        }
        if sherpa.is_none() {
            println!("skip：XBERG_TEST_SHERPA_DLL_DIR 不在场");
        }
        if ffmpeg.is_none() {
            println!("skip：XBERG_TEST_FFMPEG_DLL_DIR 不在场");
        }
        return None;
    };
    // Map test-gate variables onto the runtime variables the backend reads.
    // SAFETY：测试进程内、其它测试写入的是相同值（Windows std 内部有锁）。
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var(xberg::transcription::sensevoice::MODEL_DIR_ENV, &root);
        std::env::set_var(xberg::transcription::sensevoice::SHERPA_DLL_DIR_ENV, &sherpa);
        std::env::set_var(xberg::transcription::sensevoice::FFMPEG_DLL_DIR_ENV, &ffmpeg);
    }
    Some(())
}

/// SV-06 structural assertions over the extracted Markdown content.
///
/// `ExtractedDocument.content` is the rendered Markdown, whose writer escapes
/// leading `-` as `\-` and arrows as `--\>`; assertions run on a backslash-
/// normalized copy so the SV-06 wording is checked independent of escaping.
fn assert_sv06_structure(content: &str) {
    let flat = content.replace('\\', "");
    assert!(
        flat.starts_with("# "),
        "SV-06 must open with a `# ` header: {content:?}"
    );
    assert!(
        flat.contains("\n- 音频时长: "),
        "SV-06 duration line missing: {content:?}"
    );
    assert!(
        flat.contains("\n- 语音片段: "),
        "SV-06 segment-count line missing: {content:?}"
    );
    assert!(
        flat.contains("## 转录"),
        "SV-06 transcript heading missing: {content:?}"
    );
    let segments = flat
        .lines()
        .filter(|line| line.starts_with('[') && line.contains(" --> "))
        .count();
    assert!(
        segments > 0,
        "SV-06 must carry at least one `[start --> end] text` segment"
    );
}

/// 解析一段 `[HH:MM:SS.mmm --> HH:MM:SS.mmm] ...` 行的毫秒起止（转义归一后）。
fn parse_segment_bounds(line: &str) -> Option<(u32, u32)> {
    let line = &line.replace('\\', "");
    let inner = line.strip_prefix('[')?;
    let (start, rest) = inner.split_once(" --> ")?;
    let end = rest.split(']').next()?;
    let to_ms = |ts: &str| -> Option<u32> {
        let mut it = ts.split(':');
        let h: u32 = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        let s: f64 = it.next()?.parse().ok()?;
        Some(h * 3_600_000 + m * 60_000 + (s * 1000.0).round() as u32)
    };
    Some((to_ms(start)?, to_ms(end)?))
}

/// （资产门控）真实语音 MP4 经公开 `extract` 入口转录：SV-06 结构、段按时间
/// 顺序、时间戳合法、转录文本非空。真实断言覆盖文本与段起止，不以「模型加载
/// 成功 / 未崩溃」代替。
#[tokio::test]
async fn async_extract_real_mp4_produces_sv06_markdown() {
    let Some(()) = sensevoice_assets() else { return };
    let media = match std::env::var_os("XBERG_TEST_MEDIA_MP4")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
    {
        Some(m) => m,
        None => {
            println!("skip：XBERG_TEST_MEDIA_MP4 不在场");
            return;
        }
    };
    let bytes = std::fs::read(&media).unwrap_or_else(|e| panic!("missing test media {media:?}: {e}"));
    let result = extract_bytes_document(&bytes, "video/mp4", &config_with_transcription())
        .await
        .expect("sensevoice extraction of a real MP4 must succeed");
    assert_sv06_structure(&result.content);

    // 段行按时间顺序且起止合法（start < end）；转义归一后匹配。
    let flat = result.content.replace('\\', "");
    let mut last_start_ms = 0u32;
    for line in flat.lines().filter(|l| l.starts_with('[') && l.contains(" --> ")) {
        let (start, end) = parse_segment_bounds(line).expect("segment bounds");
        assert!(end > start, "segment end must exceed start: {line}");
        assert!(start >= last_start_ms, "segments must be in time order: {line}");
        last_start_ms = start;
    }
    // 转录文本非空（至少一个段带有非空正文）。
    assert!(
        flat.lines().any(|l| l.starts_with('[')
            && l.contains(" --> ")
            && l.split("] ").nth(1).is_some_and(|t| !t.trim().is_empty())),
        "at least one segment must carry non-empty text: {:?}",
        result.content
    );
}

/// （资产门控）同步公开入口（`extract_bytes_document_blocking`）走同一链路：
/// 仓库内置 hello-world.wav 真实语音转写成功且满足 SV-06 结构。
#[test]
fn sync_extract_real_wav_produces_sv06_markdown() {
    let Some(()) = sensevoice_assets() else { return };
    let path = helpers::get_test_file_path("audio/hello-world.wav");
    let bytes = std::fs::read(&path).expect("fixture");
    let result = extract_bytes_document_blocking(&bytes, "audio/wav", &config_with_transcription())
        .expect("sensevoice extraction of a real WAV must succeed");
    assert_sv06_structure(&result.content);
}

/// （资产门控）仓库内置静音 WAV：有音轨但 VAD 零出段 → 「未检测到语音」
/// 说明行，不是失败（SV-06）。
#[test]
fn sync_extract_silent_wav_reports_no_speech() {
    let Some(()) = sensevoice_assets() else { return };
    let path = helpers::get_test_file_path("audio/silence-1s.wav");
    let bytes = std::fs::read(&path).expect("fixture");
    let result = extract_bytes_document_blocking(&bytes, "audio/wav", &config_with_transcription())
        .expect("silent input is not a failure");
    let flat = result.content.replace('\\', "");
    assert!(flat.contains("（未检测到语音）"), "unexpected: {flat:?}");
    assert!(
        !flat.contains(" --> "),
        "silent input must not produce transcript segments: {flat:?}"
    );
}

/// （资产门控）无音轨容器：has_audio=false 语义走「无音频轨道」说明，转换
/// 成功而非报错（SV-06/SV-02）。
#[tokio::test]
async fn async_extract_no_audio_track_reports_missing_audio() {
    let Some(()) = sensevoice_assets() else { return };
    let media = match std::env::var_os("XBERG_TEST_MEDIA_NO_AUDIO_MP4")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
    {
        Some(m) => m,
        None => {
            println!("skip：XBERG_TEST_MEDIA_NO_AUDIO_MP4 不在场");
            return;
        }
    };
    let bytes = std::fs::read(&media).unwrap_or_else(|e| panic!("missing test media {media:?}: {e}"));
    let result = extract_bytes_document(&bytes, "video/mp4", &config_with_transcription())
        .await
        .expect("a trackless container is not a failure");
    let flat = result.content.replace('\\', "");
    assert!(flat.contains("- 音频时长: 无音频轨道"), "unexpected: {flat:?}");
    assert!(flat.contains("（无音频轨道）"), "unexpected: {flat:?}");
}

/// （资产门控）损坏输入必须可区分地失败（SV-02：损坏/截断输入优雅失败），
/// 而不是产出半成品文档或挂死。
#[tokio::test]
async fn async_extract_corrupt_input_fails_distinctly() {
    let Some(()) = sensevoice_assets() else { return };
    let mut bytes = vec![0u8; 4096];
    bytes.extend_from_slice(b"this is not a media file");
    let result = extract_bytes_document(&bytes, "video/mp4", &config_with_transcription()).await;
    let error = result.expect_err("corrupt input must fail");
    let msg = error.to_string();
    assert!(
        msg.contains("Transcription"),
        "error must surface through the transcription channel: {msg}"
    );
}

/// 退役的 Whisper 配置键（SV-11）：解析必须在配置层报出带迁移指引的错误，
/// 不允许静默映射——这是 extract 链路之外的纯配置契约。
#[test]
fn config_rejects_retired_whisper_keys() {
    for cfg_json in [
        r#"{"transcription": {"enabled": true, "model": "tiny"}}"#,
        r#"{"transcription": {"enabled": true, "language": "zh"}}"#,
        r#"{"transcription": {"enabled": true, "timestamps": true}}"#,
    ] {
        let err = serde_json::from_str::<ExtractionConfig>(cfg_json).expect_err("retired Whisper key must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("Whisper 已移除"), "unexpected: {msg}");
        assert!(msg.contains("backend:\"sensevoice\""), "unexpected: {msg}");
    }
}

/// 无 `transcription` 配置块的音视频输入必须报错并给出当前配置形态。
#[tokio::test]
async fn async_extract_no_transcription_config_returns_error() {
    let bytes = std::fs::read(helpers::get_test_file_path("audio/hello-world.wav")).expect("fixture");
    let config = ExtractionConfig::default();
    let result = extract_bytes_document(&bytes, "audio/wav", &config).await;
    assert!(result.is_err(), "expected error with no transcription config");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("config") || msg.contains("disabled") || msg.contains("enabled"),
        "unexpected error: {msg}"
    );
    assert!(msg.contains("backend = \"sensevoice\""), "unexpected error: {msg}");
}

/// `enabled = false` 的配置块同样报错（不静默跳过到其他提取器）。
#[tokio::test]
async fn async_extract_disabled_transcription_returns_error() {
    let bytes = std::fs::read(helpers::get_test_file_path("audio/hello-world.wav")).expect("fixture");
    let config = ExtractionConfig {
        transcription: Some(TranscriptionConfig {
            enabled: false,
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = extract_bytes_document(&bytes, "audio/wav", &config).await;
    assert!(result.is_err(), "expected error when transcription disabled");
}

/// `max_bytes` 上限在进入模型链路前生效（无需任何资产）。
#[tokio::test]
async fn async_extract_size_limit_enforced() {
    let bytes = vec![0u8; 64];
    let config = ExtractionConfig {
        transcription: Some(TranscriptionConfig {
            max_bytes: Some(10),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = extract_bytes_document(&bytes, "audio/mpeg", &config).await;
    assert!(result.is_err(), "expected error when input exceeds max_bytes");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("exceed") || msg.contains("limit") || msg.contains("size"),
        "unexpected: {msg}"
    );
}

/// 超时接线的归属：`extract_content` 确实读取并应用 `timeout_ms`（#278 回归）。
/// 该契约的确定性覆盖在库内测试 `extractors::transcription::tests::
/// extract_content_enforces_timeout_ms`（直连提取器、0ms 截止在首 poll 内确定
/// 胜出）与 `apply_timeout_*` 单测；经公开 `extract()` 包装层复刻 0ms 用例会
/// 与「极快失败的管线」产生真实时间竞态（快失败与 0ms 截止同时就绪时以先
/// 完成者为准），不构成稳定的集成级断言，故此处不重复该用例。
#[test]
fn timeout_enforcement_coverage_is_owned_by_extractor_level_tests() {
    let config = config_with_transcription();
    let tcfg = config.transcription.as_ref().expect("transcription block present");
    assert_eq!(
        tcfg.timeout_ms,
        Some(600_000),
        "integration config keeps the explicit long deadline"
    );
}
