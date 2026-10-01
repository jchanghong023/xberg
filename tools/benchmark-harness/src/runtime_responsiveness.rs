//! Cold-start Tokio scheduler responsiveness diagnostic for native Xberg batches.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::runtime::RuntimeFlavor;
use tokio::sync::oneshot;

use crate::batch_diagnostic::{
    BatchDiagnosticConfig, expanded_inputs, resolve_extraction_config, validate_batch_mapping, validate_config,
};
use crate::stats::percentile_r7;
use crate::{Error, Result, extract_xberg_files};

const REPORT_SCHEMA_VERSION: u32 = 1;
const PROBE_INTERVAL: Duration = Duration::from_millis(10);
const STALLED_TICK_THRESHOLD: Duration = Duration::from_millis(100);
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const PIPELINE_IDENTITY: &str = "xberg-rust-native-batch";

/// Inputs and concurrency controls for one cold native batch extraction.
#[derive(Debug, Clone)]
pub struct RuntimeResponsivenessConfig {
    pub inputs: Vec<PathBuf>,
    pub batch_size: usize,
    pub extraction_config_json: Option<String>,
    pub max_threads: Option<usize>,
    pub max_concurrent_extractions: Option<usize>,
}

/// Scheduler-lag percentiles observed while the first native batch extraction ran.
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerLagStatistics {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Versioned, machine-readable result for the cold responsiveness diagnostic.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeResponsivenessReport {
    pub schema_version: u32,
    pub pipeline: &'static str,
    pub extraction_config_blake3: String,
    pub expanded_input_content_blake3: String,
    pub executable_blake3: String,
    pub harness_version: &'static str,
    pub build_profile: &'static str,
    pub runtime_flavor: &'static str,
    pub target_os: &'static str,
    pub target_arch: &'static str,
    pub logical_cpu_count: usize,
    pub batch_size: usize,
    pub max_threads: Option<usize>,
    pub max_concurrent_extractions: Option<usize>,
    pub ocr_backend: Option<String>,
    pub ocr_model_version: Option<String>,
    pub ocr_model_tier: Option<String>,
    pub extraction_duration_ms: f64,
    pub probe_interval_ms: u64,
    pub wake_count: usize,
    pub missed_tick_count: u64,
    pub wake_lag_samples_ms: Vec<f64>,
    pub wake_lag_ms: SchedulerLagStatistics,
    pub stalled_tick_threshold_ms: u64,
    pub stalled_wake_event_count: usize,
}

#[derive(Debug)]
struct HeartbeatSamples {
    wake_lags: Vec<Duration>,
    missed_tick_count: u64,
}

/// Measure scheduler lag during the process's first native batch extraction.
///
/// This must run on a current-thread Tokio runtime: a multi-thread runtime could hide a blocked
/// async worker by scheduling the heartbeat on another worker. ~keep
pub async fn run_runtime_responsiveness_diagnostic(
    config: &RuntimeResponsivenessConfig,
) -> Result<RuntimeResponsivenessReport> {
    if !matches!(
        tokio::runtime::Handle::current().runtime_flavor(),
        RuntimeFlavor::CurrentThread
    ) {
        return Err(Error::Config(
            "runtime responsiveness diagnostic requires a current-thread Tokio runtime".into(),
        ));
    }

    let prepared = prepare_batch(config)?;

    let (armed_sender, armed_receiver) = oneshot::channel();
    let (stop_sender, stop_receiver) = oneshot::channel();
    let heartbeat = tokio::spawn(measure_scheduler_lag(PROBE_INTERVAL, armed_sender, stop_receiver));
    armed_receiver
        .await
        .map_err(|_| Error::Benchmark("scheduler heartbeat stopped before it was armed".into()))?;

    let started = Instant::now();
    let extraction = extract_xberg_files(&prepared.inputs, &prepared.extraction_config).await;
    let extraction_duration = started.elapsed();
    stop_sender
        .send(())
        .map_err(|_| Error::Benchmark("scheduler heartbeat stopped before extraction completed".into()))?;
    let heartbeat_samples = heartbeat
        .await
        .map_err(|error| Error::Benchmark(format!("scheduler heartbeat task failed: {error}")))?;

    let documents = extraction.map_err(|error| Error::Benchmark(format!("Xberg extraction failed: {error}")))?;
    validate_batch_mapping(&prepared.inputs, &documents)?;

    // Hash provenance after the timed extraction so reading the inputs cannot warm the measured
    // OS page cache. The blocking file reads stay off the async runtime. ~keep
    let provenance_inputs = prepared.inputs.clone();
    let (expanded_input_content_blake3, executable_blake3) = tokio::task::spawn_blocking(move || {
        Ok::<_, Error>((
            hash_expanded_input_content(&provenance_inputs)?,
            hash_current_executable()?,
        ))
    })
    .await
    .map_err(|error| Error::Benchmark(format!("provenance hashing task failed: {error}")))??;

    let identity = ReportIdentity {
        batch_size: prepared.inputs.len(),
        extraction_config_blake3: prepared.extraction_config_blake3,
        expanded_input_content_blake3,
        executable_blake3,
        max_threads: prepared.max_threads,
        max_concurrent_extractions: prepared.max_concurrent_extractions,
        ocr_backend: prepared.ocr_backend,
        ocr_model_version: prepared.ocr_model_version,
        ocr_model_tier: prepared.ocr_model_tier,
    };
    let report = build_report(extraction_duration, &heartbeat_samples, identity);
    validate_report(&report)?;
    Ok(report)
}

struct PreparedBatch {
    inputs: Vec<PathBuf>,
    extraction_config: xberg::ExtractionConfig,
    extraction_config_blake3: String,
    max_threads: Option<usize>,
    max_concurrent_extractions: Option<usize>,
    ocr_backend: Option<String>,
    ocr_model_version: Option<String>,
    ocr_model_tier: Option<String>,
}

struct ReportIdentity {
    batch_size: usize,
    extraction_config_blake3: String,
    expanded_input_content_blake3: String,
    executable_blake3: String,
    max_threads: Option<usize>,
    max_concurrent_extractions: Option<usize>,
    ocr_backend: Option<String>,
    ocr_model_version: Option<String>,
    ocr_model_tier: Option<String>,
}

fn prepare_batch(config: &RuntimeResponsivenessConfig) -> Result<PreparedBatch> {
    let batch_config = BatchDiagnosticConfig {
        inputs: config.inputs.clone(),
        batch_size: config.batch_size,
        warmup_iterations: 0,
        iterations: 1,
        extraction_config_json: config.extraction_config_json.clone(),
        max_threads: config.max_threads,
        max_concurrent_extractions: config.max_concurrent_extractions,
    };
    validate_config(&batch_config)?;
    let inputs = expanded_inputs(&batch_config);
    let extraction_config = resolve_extraction_config(&batch_config)?;
    let mut extraction_config_value = serde_json::to_value(&extraction_config)?;
    redact_sensitive_config_values(&mut extraction_config_value);
    canonicalize_json(&mut extraction_config_value);
    let extraction_config_bytes = serde_json::to_vec(&extraction_config_value)?;
    let extraction_config_blake3 = blake3::hash(&extraction_config_bytes).to_hex().to_string();
    let max_threads = extraction_config
        .concurrency
        .as_ref()
        .and_then(|value| value.max_threads);
    let max_concurrent_extractions = extraction_config.max_concurrent_extractions;
    let (ocr_backend, ocr_model_version, ocr_model_tier) = extraction_config
        .ocr
        .as_ref()
        .map(|ocr| {
            let paddle = ocr.paddle_ocr_settings.as_ref();
            (
                Some(ocr.backend.clone()),
                paddle.map(|value| value.model_version.clone()),
                paddle.map(|value| value.model_tier.clone()),
            )
        })
        .unwrap_or_default();
    Ok(PreparedBatch {
        inputs,
        extraction_config,
        extraction_config_blake3,
        max_threads,
        max_concurrent_extractions,
        ocr_backend,
        ocr_model_version,
        ocr_model_tier,
    })
}

fn redact_sensitive_config_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "api_key" | "passwords" if !value.is_null() => {
                        *value = serde_json::Value::String("<redacted-present>".to_owned());
                    }
                    "headers" => redact_header_values(value),
                    "base_url" => redact_url_credentials(value),
                    _ => redact_sensitive_config_values(value),
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_sensitive_config_values),
        _ => {}
    }
}

fn redact_header_values(value: &mut serde_json::Value) {
    let serde_json::Value::Object(headers) = value else {
        if !value.is_null() {
            *value = serde_json::Value::String("<redacted-present>".to_owned());
        }
        return;
    };
    for value in headers.values_mut() {
        *value = serde_json::Value::String("<redacted-present>".to_owned());
    }
}

fn redact_url_credentials(value: &mut serde_json::Value) {
    let Some(raw) = value.as_str() else {
        return;
    };
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return;
    };
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("redacted-present");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(Some("redacted-present"));
    }
    if parsed.query().is_some() {
        parsed.set_query(Some("redacted-present"));
    }
    if parsed.fragment().is_some() {
        parsed.set_fragment(Some("redacted-present"));
    }
    *value = serde_json::Value::String(parsed.into());
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.values_mut().for_each(canonicalize_json);
            object.sort_keys();
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalize_json),
        _ => {}
    }
}

fn hash_expanded_input_content(inputs: &[PathBuf]) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"xberg-runtime-responsiveness-inputs-v1\0");
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    for path in inputs {
        hash_file_content(&mut hasher, path, &mut buffer, "diagnostic input")?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_current_executable() -> Result<String> {
    let path = std::env::current_exe()
        .map_err(|error| Error::Benchmark(format!("failed to locate the diagnostic executable: {error}")))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"xberg-runtime-responsiveness-executable-v1\0");
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    hash_file_content(&mut hasher, &path, &mut buffer, "diagnostic executable")?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_file_content(hasher: &mut blake3::Hasher, path: &Path, buffer: &mut [u8], description: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| Error::Benchmark(format!("failed to open {description} for hashing: {error}")))?;
    let length = file
        .metadata()
        .map_err(|error| Error::Benchmark(format!("failed to inspect {description} for hashing: {error}")))?
        .len();
    hasher.update(&length.to_le_bytes());
    loop {
        let count = file
            .read(buffer)
            .map_err(|error| Error::Benchmark(format!("failed to read {description} for hashing: {error}")))?;
        if count == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..count]);
    }
}

async fn measure_scheduler_lag(
    interval: Duration,
    armed_sender: oneshot::Sender<()>,
    stop_receiver: oneshot::Receiver<()>,
) -> HeartbeatSamples {
    let deadline = tokio::time::Instant::now() + interval;
    measure_scheduler_lag_from_deadline(interval, deadline, armed_sender, stop_receiver).await
}

async fn measure_scheduler_lag_from_deadline(
    interval: Duration,
    mut deadline: tokio::time::Instant,
    armed_sender: oneshot::Sender<()>,
    mut stop_receiver: oneshot::Receiver<()>,
) -> HeartbeatSamples {
    let mut wake_lags = Vec::new();
    let mut missed_tick_count = 0_u64;
    if armed_sender.send(()).is_err() {
        return HeartbeatSamples {
            wake_lags,
            missed_tick_count,
        };
    }

    loop {
        tokio::select! {
            // Record an overdue deadline before accepting stop, so a blocking extraction cannot
            // erase the lag it caused when both futures become ready together. ~keep
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                let now = tokio::time::Instant::now();
                let lag = now.saturating_duration_since(deadline);
                wake_lags.push(lag);
                let missed = missed_intervals(lag, interval);
                missed_tick_count = missed_tick_count.saturating_add(missed);
                // Preserve the fixed cadence, but keep one lag sample per actual wake. Missed
                // deadlines are reported separately instead of becoming correlated samples. ~keep
                let intervals_to_advance = u32::try_from(missed.saturating_add(1)).unwrap_or(u32::MAX);
                deadline += interval.saturating_mul(intervals_to_advance);
            }
            _ = &mut stop_receiver => break,
        }
    }
    HeartbeatSamples {
        wake_lags,
        missed_tick_count,
    }
}

fn missed_intervals(lag: Duration, interval: Duration) -> u64 {
    let missed = lag.as_nanos() / interval.as_nanos();
    u64::try_from(missed).unwrap_or(u64::MAX)
}

fn build_report(
    extraction_duration: Duration,
    heartbeat: &HeartbeatSamples,
    identity: ReportIdentity,
) -> RuntimeResponsivenessReport {
    let wake_lag_samples_ms: Vec<f64> = heartbeat
        .wake_lags
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect();
    let mut lag_ms = wake_lag_samples_ms.clone();
    lag_ms.sort_by(f64::total_cmp);
    let wake_lag_ms = SchedulerLagStatistics {
        p50: percentile_r7(&lag_ms, 0.50),
        p95: percentile_r7(&lag_ms, 0.95),
        p99: percentile_r7(&lag_ms, 0.99),
        max: lag_ms.last().copied().unwrap_or(0.0),
    };
    RuntimeResponsivenessReport {
        schema_version: REPORT_SCHEMA_VERSION,
        pipeline: PIPELINE_IDENTITY,
        extraction_config_blake3: identity.extraction_config_blake3,
        expanded_input_content_blake3: identity.expanded_input_content_blake3,
        executable_blake3: identity.executable_blake3,
        harness_version: env!("CARGO_PKG_VERSION"),
        build_profile: if cfg!(debug_assertions) { "debug" } else { "release" },
        runtime_flavor: "current_thread",
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
        logical_cpu_count: std::thread::available_parallelism().map_or(1, usize::from),
        batch_size: identity.batch_size,
        max_threads: identity.max_threads,
        max_concurrent_extractions: identity.max_concurrent_extractions,
        ocr_backend: identity.ocr_backend,
        ocr_model_version: identity.ocr_model_version,
        ocr_model_tier: identity.ocr_model_tier,
        extraction_duration_ms: extraction_duration.as_secs_f64() * 1_000.0,
        probe_interval_ms: PROBE_INTERVAL.as_millis() as u64,
        wake_count: heartbeat.wake_lags.len(),
        missed_tick_count: heartbeat.missed_tick_count,
        wake_lag_samples_ms,
        wake_lag_ms,
        stalled_tick_threshold_ms: STALLED_TICK_THRESHOLD.as_millis() as u64,
        stalled_wake_event_count: heartbeat
            .wake_lags
            .iter()
            .filter(|duration| **duration >= STALLED_TICK_THRESHOLD)
            .count(),
    }
}

fn validate_report(report: &RuntimeResponsivenessReport) -> Result<()> {
    let lag = &report.wake_lag_ms;
    let finite_non_negative = [report.extraction_duration_ms, lag.p50, lag.p95, lag.p99, lag.max]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0);
    if report.schema_version != REPORT_SCHEMA_VERSION
        || report.pipeline != PIPELINE_IDENTITY
        || !is_blake3_hex(&report.extraction_config_blake3)
        || !is_blake3_hex(&report.expanded_input_content_blake3)
        || !is_blake3_hex(&report.executable_blake3)
        || report.harness_version.is_empty()
        || !matches!(report.build_profile, "debug" | "release")
        || report.runtime_flavor != "current_thread"
        || report.logical_cpu_count == 0
        || report.batch_size == 0
        || report.probe_interval_ms != PROBE_INTERVAL.as_millis() as u64
        || report.stalled_tick_threshold_ms != STALLED_TICK_THRESHOLD.as_millis() as u64
        || report.wake_count != report.wake_lag_samples_ms.len()
        || report.stalled_wake_event_count > report.wake_count
        || !report
            .wake_lag_samples_ms
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        || !finite_non_negative
        || !(lag.p50 <= lag.p95 && lag.p95 <= lag.p99 && lag.p99 <= lag.max)
    {
        return Err(Error::Benchmark(
            "runtime responsiveness diagnostic produced an invalid report".into(),
        ));
    }
    Ok(())
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8) -> String {
        std::iter::repeat_n(char::from(byte), 64).collect()
    }

    fn test_report(samples: &[Duration]) -> RuntimeResponsivenessReport {
        build_report(
            Duration::from_millis(20),
            &HeartbeatSamples {
                wake_lags: samples.to_vec(),
                missed_tick_count: 3,
            },
            ReportIdentity {
                batch_size: 2,
                extraction_config_blake3: fingerprint(b'a'),
                expanded_input_content_blake3: fingerprint(b'b'),
                executable_blake3: fingerprint(b'c'),
                max_threads: Some(2),
                max_concurrent_extractions: Some(1),
                ocr_backend: Some("paddleocr".to_owned()),
                ocr_model_version: Some("pp-ocrv6".to_owned()),
                ocr_model_tier: Some("small".to_owned()),
            },
        )
    }

    #[test]
    fn lag_statistics_use_sorted_r7_percentiles_and_count_stalls() {
        let samples = [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            Duration::from_millis(5),
            Duration::from_millis(6),
            Duration::from_millis(7),
            Duration::from_millis(8),
            Duration::from_millis(9),
            Duration::from_millis(100),
        ];
        let report = test_report(&samples);

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.wake_count, 10);
        assert_eq!(report.missed_tick_count, 3);
        assert_eq!(report.stalled_tick_threshold_ms, 100);
        assert_eq!(report.stalled_wake_event_count, 1);
        assert_eq!(report.wake_lag_ms.p50, 5.5);
        assert!((report.wake_lag_ms.p95 - 59.05).abs() < 0.001);
        assert!((report.wake_lag_ms.p99 - 91.81).abs() < 0.001);
        assert_eq!(report.wake_lag_ms.max, 100.0);
        assert!(validate_report(&report).is_ok());
    }

    #[test]
    fn report_validation_accepts_short_runs_and_rejects_invalid_values() {
        let mut report = test_report(&[]);
        assert!(validate_report(&report).is_ok());

        report.wake_count = 1;
        assert!(validate_report(&report).is_err());

        report.wake_count = 0;
        report.wake_lag_ms.p99 = f64::NAN;
        assert!(validate_report(&report).is_err());

        report.wake_lag_ms.p99 = 0.0;
        report.extraction_config_blake3.pop();
        assert!(validate_report(&report).is_err());
    }

    #[test]
    fn report_serializes_the_version_one_contract() {
        let config_fingerprint = fingerprint(b'a');
        let input_fingerprint = fingerprint(b'b');
        let mut report = test_report(&[Duration::from_millis(1); 10]);
        report.extraction_config_blake3.clone_from(&config_fingerprint);
        report.expanded_input_content_blake3.clone_from(&input_fingerprint);
        let value = serde_json::to_value(report).expect("report should serialize");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["pipeline"], PIPELINE_IDENTITY);
        assert_eq!(value["extraction_config_blake3"], config_fingerprint);
        assert_eq!(value["expanded_input_content_blake3"], input_fingerprint);
        assert_eq!(value["executable_blake3"], fingerprint(b'c'));
        assert_eq!(value["harness_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["runtime_flavor"], "current_thread");
        assert_eq!(value["batch_size"], 2);
        assert_eq!(value["max_threads"], 2);
        assert_eq!(value["max_concurrent_extractions"], 1);
        assert_eq!(value["ocr_backend"], "paddleocr");
        assert_eq!(value["ocr_model_version"], "pp-ocrv6");
        assert_eq!(value["ocr_model_tier"], "small");
        assert_eq!(value["probe_interval_ms"], 10);
        assert_eq!(value["stalled_tick_threshold_ms"], 100);
        assert_eq!(value["wake_lag_samples_ms"].as_array().map(Vec::len), Some(10));
        assert_eq!(value["wake_lag_ms"]["max"], 1.0);
    }

    #[test]
    fn config_fingerprint_redacts_secrets_but_keeps_quality_settings() {
        let mut value = serde_json::json!({
            "llm": {
                "api_key": "secret-value",
                "base_url": "https://user:password@example.com/v1?access_token=secret#private",
                "headers": {"x-api-key": "custom-secret", "cookie": "session-secret"},
                "max_tokens": 2048,
                "model": "model-a"
            },
            "pdf": { "passwords": ["guess-me"] },
            "ocr": { "drop_score": 0.4, "token_reduction": true, "tokenizer": "quality-tokenizer" }
        });
        redact_sensitive_config_values(&mut value);

        assert_eq!(value["llm"]["api_key"], "<redacted-present>");
        assert_eq!(value["llm"]["headers"]["x-api-key"], "<redacted-present>");
        assert_eq!(value["llm"]["headers"]["cookie"], "<redacted-present>");
        assert_eq!(
            value["llm"]["base_url"],
            "https://redacted-present:redacted-present@example.com/v1?redacted-present#redacted-present"
        );
        assert_eq!(value["pdf"]["passwords"], "<redacted-present>");
        assert_eq!(value["llm"]["model"], "model-a");
        assert_eq!(value["llm"]["max_tokens"], 2048);
        assert_eq!(value["ocr"]["drop_score"], 0.4);
        assert_eq!(value["ocr"]["token_reduction"], true);
        assert_eq!(value["ocr"]["tokenizer"], "quality-tokenizer");
    }

    #[test]
    fn canonical_json_sorting_is_independent_of_insertion_order() {
        let mut first = serde_json::json!({"headers": {"z": "secret", "a": "secret"}, "b": 2, "a": 1});
        let mut second = serde_json::json!({"a": 1, "b": 2, "headers": {"a": "other", "z": "other"}});
        redact_sensitive_config_values(&mut first);
        redact_sensitive_config_values(&mut second);
        canonicalize_json(&mut first);
        canonicalize_json(&mut second);

        assert_eq!(
            serde_json::to_vec(&first).expect("first value should serialize"),
            serde_json::to_vec(&second).expect("second value should serialize")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overdue_deadline_is_recorded_before_ready_stop() {
        let interval = Duration::from_millis(10);
        let overdue_by = Duration::from_millis(25);
        let deadline = tokio::time::Instant::now() - overdue_by;
        let (armed_sender, armed_receiver) = oneshot::channel();
        let (stop_sender, stop_receiver) = oneshot::channel();
        stop_sender.send(()).expect("heartbeat receiver should remain open");

        let samples = measure_scheduler_lag_from_deadline(interval, deadline, armed_sender, stop_receiver).await;

        armed_receiver.await.expect("heartbeat should arm");
        assert_eq!(samples.wake_lags.len(), 1);
        assert!(samples.wake_lags[0] >= overdue_by);
        assert!(samples.missed_tick_count >= 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fast_extraction_returns_a_valid_zero_or_low_sample_report() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let input = directory.path().join("input.txt");
        std::fs::write(&input, "short diagnostic input").expect("input should be written");
        let report = run_runtime_responsiveness_diagnostic(&RuntimeResponsivenessConfig {
            inputs: vec![input],
            batch_size: 1,
            extraction_config_json: Some(r#"{"disable_ocr":true}"#.to_owned()),
            max_threads: Some(1),
            max_concurrent_extractions: Some(1),
        })
        .await
        .expect("fast extraction should still produce a report");

        assert_eq!(report.batch_size, 1);
        assert_eq!(report.wake_count, report.wake_lag_samples_ms.len());
        assert!(validate_report(&report).is_ok());
    }
}
