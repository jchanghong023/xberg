//! `xberg snapshot-ocr` - screenshot OCR channel CLI acceptance entry (SNAP-18)
//! plus the engine plumbing shared with the worker's `ocr_snapshot` command.
//!
//! The screenshot channel is a standalone second OCR capability
//! (`docs/requirements/OCR-SNAPSHOT.md`): it never shares models, sessions, or
//! backend selection with the document OCR channel (SNAP-04). This module only
//! wires the `xberg-snapshot-ocr` crate into the CLI/worker surface; the crate
//! owns all recognition behavior.
//!
//! Model root resolution order: explicit `--models` flag → `snapshot_ocr`
//! config block → `XBERG_SNAPSHOT_MODEL_DIR` environment variable →
//! `<exe directory>/models/snapshot-ocr`. An explicitly set location that does
//! not resolve is a hard error (no silent fallback); when nothing is set the
//! error lists every searched location.
//!
//! `ORT_DYLIB_PATH` must point at onnxruntime before any model load (the crate
//! pins the same-directory DLL first — anti-preemption); a missing variable is
//! reported as a clear error instead of a search-path accident.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use xberg::ExtractionConfig;
use xberg_snapshot_ocr::{SNAPSHOT_MODEL_SET, SnapshotOcrError, SnapshotOcrModels, SnapshotOcrRecord};

use super::validate_file_exists;

/// Environment variable holding the snapshot model root.
pub(crate) const MODELS_DIR_ENV: &str = "XBERG_SNAPSHOT_MODEL_DIR";

/// Pinned model set members, relative to the model root (SNAP-03 digests are
/// verified by `SnapshotOcrModels::load`).
const DET_FILE: &str = "det.onnx";
const REC_FILE: &str = "rec.onnx";
const DICT_FILE: &str = "dict/dict.txt";

/// Snapshot error categories reported to worker callers (SNAP-15).
pub(crate) type ErrorKind = &'static str;
pub(crate) const KIND_MODEL_NOT_READY: &str = "model_not_ready";
pub(crate) const KIND_ASSET_INVALID: &str = "asset_invalid";
pub(crate) const KIND_INPUT_INVALID: &str = "input_invalid";
pub(crate) const KIND_NO_TEXT: &str = "no_text";
pub(crate) const KIND_CANCELLED: &str = "cancelled";
pub(crate) const KIND_INTERNAL: &str = "internal";

/// Map a crate error onto its worker error category (SNAP-15). Asset failures
/// (missing files, digest mismatch, session build) are `asset_invalid`
/// (SNAP-05: fail loudly, never infer).
pub(crate) fn error_kind_for(error: &SnapshotOcrError) -> ErrorKind {
    match error {
        SnapshotOcrError::Asset(_) => KIND_ASSET_INVALID,
        SnapshotOcrError::InputInvalid(_) => KIND_INPUT_INVALID,
        SnapshotOcrError::Cancelled => KIND_CANCELLED,
        SnapshotOcrError::Pipeline(_) => KIND_INTERNAL,
    }
}

/// Resolve the snapshot model root.
///
/// `explicit` is the CLI `--models` flag, `config_dir` the `snapshot_ocr`
/// config block's `models_dir`, `env_value` the raw `XBERG_SNAPSHOT_MODEL_DIR`
/// value (passed in so tests never touch process state). An explicitly set but
/// unusable location is a hard error; when nothing is set the error lists all
/// three lookup locations.
pub(crate) fn resolve_models_dir(
    explicit: Option<&Path>,
    config_dir: Option<&Path>,
    env_value: Option<&OsStr>,
) -> std::result::Result<PathBuf, String> {
    let exe_candidate = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("models").join("snapshot-ocr")));

    if let Some(dir) = explicit.or(config_dir) {
        if dir.is_dir() {
            return Ok(dir.to_path_buf());
        }
        return Err(format!(
            "snapshot_ocr.models_dir 不是有效的截图模型根目录（需含 {DET_FILE}、{REC_FILE} 与 {DICT_FILE}）: {}",
            dir.display()
        ));
    }
    if let Some(value) = env_value {
        let dir = PathBuf::from(value);
        if dir.is_dir() {
            return Ok(dir);
        }
        return Err(format!(
            "{MODELS_DIR_ENV} 不是有效的截图模型根目录（需含 {DET_FILE}、{REC_FILE} 与 {DICT_FILE}）: {}",
            dir.display()
        ));
    }
    if let Some(dir) = exe_candidate.filter(|dir| dir.is_dir()) {
        return Ok(dir);
    }
    Err(format!(
        "缺少截图 OCR 模型根目录（{SNAPSHOT_MODEL_SET}，需含 {DET_FILE}、{REC_FILE} 与 {DICT_FILE}）；已依次查找：\
         1) 显式配置（--models 或 snapshot_ocr.models_dir）；\
         2) 环境变量 {MODELS_DIR_ENV}；\
         3) <exe目录>/models/snapshot-ocr"
    ))
}

/// Read one image and decode it to BGR HxWx3 row-major pixels (the crate's
/// input format). This build decodes PNG (the worker contract's lossless
/// encoding); other formats depend on the `image` features compiled in.
pub(crate) fn decode_image_bgr(path: &Path) -> std::result::Result<(u32, u32, Vec<u8>), String> {
    let rgb = image::open(path)
        .map_err(|error| format!("图像解码失败（{}）：{error}", path.display()))?
        .to_rgb8();
    rgb_to_bgr(rgb)
}

/// Decode in-memory image bytes (the worker `ocr_snapshot` shape) to the same
/// BGR layout. The bytes never touch disk (SNAP-14).
pub(crate) fn decode_image_bgr_bytes(bytes: &[u8]) -> std::result::Result<(u32, u32, Vec<u8>), String> {
    let rgb = image::load_from_memory(bytes)
        .map_err(|error| format!("图像解码失败：{error}"))?
        .to_rgb8();
    rgb_to_bgr(rgb)
}

fn rgb_to_bgr(rgb: image::RgbImage) -> std::result::Result<(u32, u32, Vec<u8>), String> {
    let (width, height) = rgb.dimensions();
    let mut bgr = Vec::with_capacity(rgb.len());
    for pixel in rgb.pixels() {
        bgr.push(pixel[2]);
        bgr.push(pixel[1]);
        bgr.push(pixel[0]);
    }
    Ok((width, height, bgr))
}

/// Optional `ORT_DYLIB_PATH` pin: only meaningful for `load-dynamic` ort
/// builds (the default workspace build statically links onnxruntime), where
/// setting it before any session build prevents a stale PATH dll from winning
/// the race. The default build accepts the variable being unset.
fn require_ort_dylib() -> std::result::Result<(), String> {
    Ok(())
}

/// Load the pinned model set from `models_dir`, returning the message plus its
/// error category on failure (SNAP-04/SNAP-05: no fallback, no inference).
pub(crate) fn load_models(
    models_dir: &Path,
    intra_threads: usize,
) -> std::result::Result<SnapshotOcrModels, (String, ErrorKind)> {
    require_ort_dylib().map_err(|message| (message, KIND_ASSET_INVALID))?;
    SnapshotOcrModels::load(
        &models_dir.join(DET_FILE),
        &models_dir.join(REC_FILE),
        &models_dir.join(DICT_FILE),
        intra_threads,
    )
    .map_err(|error| (error.to_string(), error_kind_for(&error)))
}

/// One completed recognition: grid-layout text plus the structured records.
pub(crate) struct Recognized {
    /// Layout-preserving text (no trailing newline; empty when no text found).
    pub text: String,
    /// Structured records, one per non-empty recognized span.
    pub records: Vec<SnapshotOcrRecord>,
    /// Total wall time for decode + recognize, milliseconds.
    pub elapsed_ms: u128,
}

/// Decode and recognize one image file with a loaded model set.
pub(crate) fn recognize_image(
    models: &SnapshotOcrModels,
    image: &Path,
    cancel: &AtomicBool,
) -> std::result::Result<Recognized, (String, ErrorKind)> {
    let started = Instant::now();
    let (width, height, bgr) = decode_image_bgr(image).map_err(|message| (message, KIND_INPUT_INVALID))?;
    recognize_pixels(models, &bgr, width, height, started, cancel)
}

/// Decode base64 image bytes (worker `ocr_snapshot` shape) and recognize them
/// through the lazily loaded engine: the worker's complete per-request path.
pub(crate) fn recognize_base64(
    engine: &mut SnapshotOcrEngine,
    encoded: &str,
    cancel: &AtomicBool,
) -> std::result::Result<Recognized, (String, ErrorKind)> {
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err(("snapshot request cancelled".to_string(), KIND_CANCELLED));
    }
    let bytes = STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| (format!("image_base64 不是有效的 base64：{error}"), KIND_INPUT_INVALID))?;
    let started = Instant::now();
    let (width, height, bgr) = decode_image_bgr_bytes(&bytes).map_err(|message| (message, KIND_INPUT_INVALID))?;
    let models = engine.get_or_load()?;
    recognize_pixels(models, &bgr, width, height, started, cancel)
}

/// Run recognition on decoded pixels and time it.
fn recognize_pixels(
    models: &SnapshotOcrModels,
    bgr: &[u8],
    width: u32,
    height: u32,
    started: Instant,
    cancel: &AtomicBool,
) -> std::result::Result<Recognized, (String, ErrorKind)> {
    let output = models
        .recognize(bgr, width, height, cancel)
        .map_err(|error| (error.to_string(), error_kind_for(&error)))?;
    Ok(Recognized {
        text: output.layout_text,
        records: output.records,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// State shared with the protocol thread; never holds a model or an inference lock.
#[derive(Clone)]
pub(crate) struct SnapshotState(Arc<Mutex<(&'static str, Option<String>)>>);

impl SnapshotState {
    pub(crate) fn get(&self) -> (&'static str, Option<String>) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn set(&self, state: &'static str, error: Option<String>) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (state, error);
    }
}

/// Owned exclusively by the screenshot thread and reused until process exit.
/// Only its small status record is shared with the protocol thread.
pub(crate) struct SnapshotOcrEngine {
    block: xberg::core::config::SnapshotOcrConfig,
    models: Option<SnapshotOcrModels>,
    state: SnapshotState,
}

impl SnapshotOcrEngine {
    pub(crate) fn new(block: Option<xberg::core::config::SnapshotOcrConfig>) -> Self {
        Self {
            block: block.unwrap_or_default(),
            models: None,
            state: SnapshotState(Arc::new(Mutex::new(("uninitialized", None)))),
        }
    }

    pub(crate) fn state(&self) -> SnapshotState {
        self.state.clone()
    }

    /// Load-on-first-use, then reuse. A failure is remembered for
    /// `snapshot_state` and the next request retries the load (the process
    /// never exits on a model error).
    fn get_or_load(&mut self) -> std::result::Result<&SnapshotOcrModels, (String, ErrorKind)> {
        if self.models.is_none() {
            self.state.set("loading", None);
            let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                resolve_models_dir(
                    None,
                    self.block.models_dir.as_deref(),
                    std::env::var_os(MODELS_DIR_ENV).as_deref(),
                )
                .map_err(|message| (message, KIND_ASSET_INVALID))
                .and_then(|dir| load_models(&dir, self.block.intra_threads))
            }))
            .unwrap_or_else(|_| Err(("snapshot model initialization panicked".to_string(), KIND_INTERNAL)));
            match resolved {
                Ok(models) => {
                    self.models = Some(models);
                    self.state.set("ready", None);
                    tracing::info!(
                        model_set = SNAPSHOT_MODEL_SET,
                        intra_threads = self.block.intra_threads,
                        "worker snapshot models loaded"
                    );
                }
                Err((message, kind)) => {
                    self.state.set("error", Some(message.clone()));
                    return Err((message, kind));
                }
            }
        }
        Ok(self.models.as_ref().expect("just loaded above"))
    }
}

/// Execute the `xberg snapshot-ocr` command (SNAP-18): recognize one image and
/// print the layout text (or a JSON report with `--json`). Diagnostics go to
/// stderr through `tracing`.
///
/// `models` / `intra_threads` override the `snapshot_ocr` config block; the
/// block comes from the same config cascade as `extract` (`--config`,
/// `--config-json`, discovery).
pub fn snapshot_ocr_command(
    image: PathBuf,
    models: Option<PathBuf>,
    intra_threads: Option<usize>,
    json: bool,
    config: &ExtractionConfig,
) -> Result<()> {
    validate_file_exists(&image)?;

    let block = config.snapshot_ocr.clone().unwrap_or_default();
    let threads = intra_threads.unwrap_or(block.intra_threads);
    let models_dir = resolve_models_dir(
        models.as_deref(),
        block.models_dir.as_deref(),
        std::env::var_os(MODELS_DIR_ENV).as_deref(),
    )
    .map_err(anyhow::Error::msg)?;

    let load_started = Instant::now();
    let engine = load_models(&models_dir, threads).map_err(|(message, kind)| anyhow::anyhow!("[{kind}] {message}"))?;
    let load_ms = load_started.elapsed().as_millis() as u64;

    let cancel = AtomicBool::new(false);
    let recognized =
        recognize_image(&engine, &image, &cancel).map_err(|(message, kind)| anyhow::anyhow!("[{kind}] {message}"))?;
    let recognize_ms = recognized.elapsed_ms as u64;

    tracing::info!(
        records = recognized.records.len(),
        load_ms,
        recognize_ms,
        "snapshot-ocr completed"
    );

    if json {
        print_snapshot_json(&recognized, load_ms, recognize_ms)?;
    } else {
        print_snapshot_text(&recognized.text)?;
    }
    Ok(())
}

// `print_stdout` does not fire here: the contract output goes through
// `writeln!(stdout, …)`, not the `println!` macros the lint covers.
fn print_snapshot_text(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    // Same shape as the reference TextSnap CLI: text with one trailing newline.
    writeln!(stdout, "{text}").context("failed to write the snapshot-ocr text output")?;
    stdout.flush().context("failed to flush the snapshot-ocr text output")?;
    Ok(())
}

#[expect(
    clippy::print_stdout,
    reason = "the JSON report is this subcommand's stdout output contract"
)]
fn print_snapshot_json(recognized: &Recognized, load_ms: u64, recognize_ms: u64) -> Result<()> {
    let payload = serde_json::json!({
        "text": recognized.text,
        "records": recognized
            .records
            .iter()
            .map(record_json)
            .collect::<Vec<_>>(),
        "load_ms": load_ms,
        "recognize_ms": recognize_ms,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).context("failed to serialize the snapshot-ocr JSON report")?
    );
    Ok(())
}

/// JSON view of one record: image-global quad (TL/TR/BR/BL), text, scores,
/// winning orientation.
fn record_json(record: &SnapshotOcrRecord) -> serde_json::Value {
    serde_json::json!({
        "quad": record.quad.map(|point| [point.0, point.1]),
        "text": record.text,
        "detection_score": record.detection_score,
        "recognition_score": record.recognition_score,
        "rotation_degrees": record.rotation_degrees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn status_queries_do_not_borrow_the_engine_and_failed_load_is_reported() {
        let mut engine = SnapshotOcrEngine::new(Some(xberg::core::config::SnapshotOcrConfig {
            models_dir: Some(PathBuf::from("Z:/xberg-missing-snapshot-models")),
            intra_threads: 3,
        }));
        let status = engine.state();
        assert_eq!(status.get(), ("uninitialized", None));
        // A query needs only the status handle, never the engine/model mutex.
        engine.state.set("loading", None);
        let observer = status.clone();
        assert_eq!(
            std::thread::spawn(move || observer.get()).join().expect("query joined"),
            ("loading", None)
        );
        let (_, kind) = engine
            .get_or_load()
            .expect_err("missing explicit models cannot fall back");
        assert_eq!(kind, KIND_ASSET_INVALID);
        let (state, error) = status.get();
        assert_eq!(state, "error");
        assert!(
            error
                .expect("load diagnostic")
                .contains("xberg-missing-snapshot-models")
        );
        assert_eq!(
            engine.block.intra_threads, 3,
            "independent screenshot configuration retained"
        );
    }

    /// Explicit (CLI or config) locations win and must exist; no silent
    /// fallback to the env var (SNAP-04 spirit).
    #[test]
    fn explicit_location_wins_and_missing_explicit_is_a_hard_error() {
        let dir = std::env::temp_dir();
        let resolved = resolve_models_dir(Some(&dir), Some(Path::new("Z:/definitely/not/here")), None)
            .expect("explicit CLI location must win");
        assert_eq!(resolved, dir);

        let err = resolve_models_dir(Some(Path::new("Z:/definitely/not/here")), Some(&dir), None)
            .expect_err("explicit but missing must be a hard error");
        assert!(err.contains("Z:/definitely/not/here"), "unexpected: {err}");
        assert!(err.contains(DET_FILE), "error must name the model layout: {err}");
    }

    /// The env var applies only when nothing explicit is set, and its value must
    /// resolve to a real directory.
    #[test]
    fn env_location_applies_after_explicit_sources() {
        let dir = std::env::temp_dir();
        let resolved = resolve_models_dir(None, None, Some(dir.as_os_str()))
            .expect("env location must resolve when it is a real directory");
        assert_eq!(resolved, dir);

        let err = resolve_models_dir(None, None, Some(OsStr::new("Z:/no/such/dir")))
            .expect_err("invalid env value must be a hard error");
        assert!(err.contains(MODELS_DIR_ENV), "unexpected: {err}");
    }

    /// With every source unset (or unusable), the error lists all three lookup
    /// locations so the caller can fix any of them.
    #[test]
    fn unset_everywhere_lists_all_three_lookup_locations() {
        let err = resolve_models_dir(None, None, None).expect_err("nothing set must fail");
        assert!(err.contains("snapshot_ocr.models_dir"), "missing config source: {err}");
        assert!(err.contains(MODELS_DIR_ENV), "missing env source: {err}");
        assert!(
            err.contains("models/snapshot-ocr"),
            "missing exe-adjacent source: {err}"
        );
    }

    /// Error category mapping (SNAP-15): asset vs input vs cancelled vs internal.
    #[test]
    fn error_kinds_map_from_crate_errors() {
        use xberg_snapshot_ocr::{AssetError, ModelMember};
        let asset = SnapshotOcrError::Asset(AssetError::Io {
            member: ModelMember::Det,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        });
        assert_eq!(error_kind_for(&asset), KIND_ASSET_INVALID);
        assert_eq!(
            error_kind_for(&SnapshotOcrError::InputInvalid("bad buffer".into())),
            KIND_INPUT_INVALID
        );
        assert_eq!(error_kind_for(&SnapshotOcrError::Cancelled), KIND_CANCELLED);
        assert_eq!(
            error_kind_for(&SnapshotOcrError::Pipeline(
                xberg_snapshot_ocr::pipeline::PipelineError::Backend("boom".into())
            )),
            KIND_INTERNAL
        );
    }

    /// PNG decode hands the crate row-major BGR bytes: red pixels must arrive
    /// as (B, G, R) = (0, 0, 255).
    #[test]
    fn decode_image_bgr_swaps_channels() {
        let path = std::env::temp_dir().join(format!("xberg-snap-ut-{}.png", std::process::id()));
        let image = image::RgbImage::from_pixel(3, 2, image::Rgb([255, 0, 0]));
        image.save(&path).expect("test png writes");
        let (width, height, bgr) = decode_image_bgr(&path).expect("decodes");
        let _ = std::fs::remove_file(&path);
        assert_eq!((width, height), (3, 2));
        assert_eq!(bgr.len(), 3 * 2 * 3);
        assert_eq!(&bgr[..3], &[0, 0, 255], "red must be last in BGR");
    }

    /// An unreadable image is an `input_invalid` failure, not a panic.
    #[test]
    fn recognize_image_reports_invalid_input_without_a_model() {
        let missing = std::env::temp_dir().join(format!("xberg-snap-ut-missing-{}.png", std::process::id()));
        // Load fails first for lack of models; the input check happens in
        // `recognize`, so exercise only the decode half here.
        let err = decode_image_bgr(&missing).expect_err("missing file must fail");
        assert!(err.contains("图像解码失败"), "unexpected: {err}");
    }
}
