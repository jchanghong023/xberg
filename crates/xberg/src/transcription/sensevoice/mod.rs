//! SenseVoice transcription backend (FFmpeg DLL decode → Silero VAD →
//! SenseVoice INT8 via sherpa-onnx C API), ported from the JchTools
//! `markdown-media-worker` chain so both stacks produce identical semantics:
//! 16 kHz mono decode, fixed VAD parameters, `use_itn=1` Chinese recognition,
//! the fixed 34-entry terminology map, and the SV-06 Markdown layout.
//!
//! This is the transcription feature's only backend (SV-01).
//!
//! Asset layout (all verified by pinned SHA-256 before any inference):
//! SenseVoice `model.int8.onnx` + `tokens.txt`, Silero `silero_vad.onnx`
//! under the model root, `sherpa-onnx-c-api.dll` (+ sibling `onnxruntime.dll`)
//! and the four FFmpeg shared libraries resolved from — in order — explicit
//! configuration, environment variables, exe-adjacent candidates, and `PATH`.

mod ffmpeg_dll;
mod output;
mod sherpa;
mod vad;

// Re-exports for the extractor's SV-06 element assembly (keeps `output`
// private while exposing the fixed wording helpers crate-wide).
pub(crate) use output::{NO_AUDIO_NOTE, NO_SPEECH_NOTE, duration_line, segment_line_ms};

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use sha2::Digest;

use vad::VadPipeline;

/// Model root override (config `transcription.model_dir`).
pub const MODEL_DIR_ENV: &str = "XBERG_SENSEVOICE_MODEL_DIR";
/// Directory holding `sherpa-onnx-c-api.dll` (+ sibling `onnxruntime.dll`).
pub const SHERPA_DLL_DIR_ENV: &str = "XBERG_SHERPA_DLL_DIR";
/// Directory holding the four pinned FFmpeg shared libraries.
pub const FFMPEG_DLL_DIR_ENV: &str = "XBERG_FFMPEG_DLL_DIR";

/// SenseVoice INT8 model, pinned build (`model.int8.onnx`, 239,233,841 bytes).
const SENSEVOICE_MODEL_RELATIVE: [&str; 2] = ["sense_voice_zh_en_ja_ko_yue_2024_07_17", "model.int8.onnx"];
const SENSEVOICE_MODEL_SHA256: &str = "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51";
const SENSEVOICE_MODEL_LEN: u64 = 239_233_841;
/// SenseVoice token table (`tokens.txt`, 315,894 bytes).
const TOKENS_RELATIVE: [&str; 2] = ["sense_voice_zh_en_ja_ko_yue_2024_07_17", "tokens.txt"];
const TOKENS_SHA256: &str = "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc";
const TOKENS_LEN: u64 = 315_894;
/// Silero VAD model (`silero_vad.onnx`, 643,854 bytes).
const VAD_MODEL_RELATIVE: [&str; 2] = ["vad", "silero_vad.onnx"];
const VAD_MODEL_SHA256: &str = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6";
const VAD_MODEL_LEN: u64 = 643_854;

/// Sample rate of the SenseVoice decode chain: everything is resampled to
/// 16 kHz mono before VAD (public for the extractor's `AudioMetadata`).
pub const SAMPLE_RATE_HZ: u32 = vad::SAMPLE_RATE as u32;

/// Result of one SenseVoice transcription: the SV-06 Markdown document plus
/// structured segments (millisecond timestamps) for JSON consumers.
#[cfg_attr(alef, alef(skip))]
#[derive(Debug, Clone)]
pub struct SenseVoiceResult {
    /// Name used in the SV-06 `# ` header (verbatim input to
    /// [`transcribe_bytes`]).
    pub name: String,
    /// One `(start_ms, end_ms, text)` tuple per transcript line, in time order.
    /// Empty-text VAD segments are dropped by the transcriber and absent here.
    pub segments: Vec<(u32, u32, String)>,
    /// The full SV-06 Markdown document (`# name`, duration line, segment
    /// count, `## 转录`, one `[start --> end] text` line per segment).
    pub markdown: String,
    /// Duration of the decoded audio in milliseconds (`0` when the container
    /// has no audio track).
    pub duration_ms: u64,
    /// Whether the container carried an audio track at all.
    pub has_audio: bool,
}

/// Transcribe in-memory media bytes through the SenseVoice chain.
///
/// Blocking and CPU-heavy — callers must run it on a blocking thread (the
/// extractor wraps it in `spawn_blocking` under the shared transcription
/// semaphore and `apply_timeout`).
///
/// `name` is used verbatim in the SV-06 `# ` header (the JchTools worker used
/// the input file name; `extract_content` only sees anonymous bytes, so the
/// extractor passes the document name). `model_dir` overrides the model root;
/// when `None`, `XBERG_SENSEVOICE_MODEL_DIR` / exe-adjacent locations apply.
///
/// The native engine (sherpa-onnx DLL symbols + the 239 MB SenseVoice INT8
/// recognizer session) is a process-level cache keyed by the resolved model /
/// token / DLL paths and the recognizer thread count — the same lifetime model
/// as the JchTools media worker — so repeated calls in one process skip the
/// model load. The VAD model also stays resident: its streaming state and queued
/// segments are reset under the session run lock before and after every input.
/// Digests for all three assets are verified before creating the cached sessions.
pub fn transcribe_bytes(
    content: &[u8],
    name: &str,
    mime_type: &str,
    model_dir: Option<&Path>,
    max_duration_ms: Option<u64>,
) -> Result<SenseVoiceResult, String> {
    transcribe_bytes_cancellable(
        content,
        name,
        mime_type,
        model_dir,
        max_duration_ms,
        &crate::cancellation::CancellationToken::new(),
    )
}

/// Cooperative variant: stops between native decode/inference calls, retaining
/// the cached recognizer. A terminal return means this pipeline has stopped.
pub fn transcribe_bytes_cancellable(
    content: &[u8],
    name: &str,
    mime_type: &str,
    model_dir: Option<&Path>,
    max_duration_ms: Option<u64>,
    cancel: &crate::cancellation::CancellationToken,
) -> Result<SenseVoiceResult, String> {
    check_cancel(cancel)?;
    let model_root = resolve_model_dir(model_dir)?;

    let session = get_or_create_session(&model_root)?;
    check_cancel(cancel)?;

    let ffmpeg_dir = resolve_ffmpeg_dir(&model_root)?;

    // 字节先落临时文件（FFmpeg 按路径打开输入），用完即删。
    let staged = stage_input(content, mime_type)?;

    run_pipeline(&session, &ffmpeg_dir, &staged.path, name, max_duration_ms, cancel)
}

fn check_cancel(cancel: &crate::cancellation::CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("transcription cancelled".into())
    } else {
        Ok(())
    }
}

/// Recognizer thread count, fixed exactly as the JchTools media chain
/// (`create_recognizer(..., 1, cpu, zh)`; SV-04).
const RECOGNIZER_THREADS: i32 = 1;

/// One cached SenseVoice engine: the sherpa-onnx DLL symbols plus the
/// resident INT8 recognizer session. Created at most once per distinct
/// (model, tokens, sherpa DLL, threads) key and kept until process exit —
/// the same lifetime model as the JchTools media worker.
struct SenseVoiceSession {
    sherpa: sherpa::Sherpa,
    recognizer: *const sherpa::OfflineRecognizer,
    vad: *const sherpa::Vad,
    /// Serializes decode + VAD + inference runs over this session. The JchTools
    /// reference chain is strictly serial per engine; sharing one recognizer
    /// across concurrent caller threads is therefore funneled through this lock
    /// instead of relying on sherpa-onnx internal thread-safety. Sessions for
    /// distinct model roots never contend on each other's lock.
    run_lock: Mutex<()>,
}

// SAFETY: `recognizer` is a plain C heap handle returned by
// `SherpaOnnxCreateOfflineRecognizer`; it is not thread-bound. `Send` allows
// the owning `Arc` to move between the caller's blocking threads; `Sync` is
// sound because every dereference (stream create/accept/decode/result, and the
// final destroy in `Drop`) happens either under `run_lock` (pipeline runs) or
// with exclusive access (`&mut self` in `Drop`), never concurrently.
unsafe impl Send for SenseVoiceSession {}
unsafe impl Sync for SenseVoiceSession {}

impl std::fmt::Debug for SenseVoiceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Resident native sessions are not introspectable; identity only.
        f.debug_struct("SenseVoiceSession").finish_non_exhaustive()
    }
}

impl Drop for SenseVoiceSession {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `create_recognizer` with the same
        // `sherpa` symbol table, which outlives the process by design.
        self.sherpa.destroy_recognizer_session(self.recognizer);
        self.sherpa.destroy_vad_session(self.vad);
    }
}

impl SenseVoiceSession {
    /// Verify the pinned model/token assets, load the sherpa DLL, and build the
    /// recognizer session. Runs only on cache misses; the `sensevoice_model_load`
    /// perf span lives on [`create_recognizer`], the cold-start bulk.
    fn load(
        model_path: &Path,
        tokens_path: &Path,
        sherpa_path: &Path,
        vad_path: &Path,
        threads: i32,
    ) -> Result<Self, String> {
        // SAFETY：模型与 DLL 路径都来自摘要校验后的固定资产；识别器句柄由
        // 会话持有并在 `Drop` 中销毁。
        unsafe {
            let sherpa = sherpa::Sherpa::load(sherpa_path)?;
            let recognizer = create_recognizer(&sherpa, model_path, tokens_path, threads)?;
            let vad = match create_vad(&sherpa, vad_path) {
                Ok(vad) => vad,
                Err(error) => {
                    sherpa.destroy_recognizer_session(recognizer);
                    return Err(error);
                }
            };
            Ok(Self {
                sherpa,
                recognizer,
                vad,
                run_lock: Mutex::new(()),
            })
        }
    }
}

/// Process-level cache of loaded SenseVoice sessions, keyed by the resolved
/// model/token/DLL paths and the recognizer thread count. Created at most once
/// per distinct key and kept until process exit.
static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<SenseVoiceSession>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Non-blocking model query; loading holds the cache lock, so never wait for it.
pub fn model_state() -> serde_json::Value {
    match SESSIONS.try_lock() {
        Ok(sessions) => {
            let mut keys: Vec<_> = sessions.keys().cloned().collect();
            keys.sort();
            serde_json::json!({"state": if keys.is_empty() { "uninitialized" } else { "ready" }, "resident_sessions": keys})
        }
        Err(std::sync::TryLockError::WouldBlock) => serde_json::json!({"state": "loading"}),
        Err(_) => serde_json::json!({"state": "error", "error": "session cache poisoned"}),
    }
}

/// Cache key for one loaded session — stable across calls that resolve to the
/// same assets and engine configuration.
fn session_cache_key(model_path: &Path, tokens_path: &Path, sherpa_path: &Path, threads: i32) -> String {
    format!(
        "sensevoice|{threads}|{}|{}|{}",
        model_path.display(),
        tokens_path.display(),
        sherpa_path.display()
    )
}

/// Return the cached session for `model_root`, creating (digest-verified) and
/// caching it on the first call. Asset verification and any load failure happen
/// before the cache is touched, so a failed load never poisons the map.
fn get_or_create_session(model_root: &Path) -> Result<Arc<SenseVoiceSession>, String> {
    let model_path = sherpa::model_file(model_root, &SENSEVOICE_MODEL_RELATIVE);
    let tokens_path = sherpa::model_file(model_root, &TOKENS_RELATIVE);
    let vad_path = sherpa::model_file(model_root, &VAD_MODEL_RELATIVE);
    let sherpa_path = resolve_sherpa_dll(model_root)?;
    let key = session_cache_key(&model_path, &tokens_path, &sherpa_path, RECOGNIZER_THREADS);
    let mut sessions = SESSIONS.lock().map_err(|e| format!("会话缓存损坏: {e}"))?;
    if let Some(session) = sessions.get(&key) {
        return Ok(Arc::clone(session));
    }
    for path in [&model_path, &tokens_path] {
        if !path.is_file() {
            return Err(format!("媒体模型缺失: {}", path.display()));
        }
    }
    // 加载前离线校验摘要，不符即失败不推理（仅缓存未命中时执行）。
    verify_asset(
        &model_path,
        SENSEVOICE_MODEL_SHA256,
        SENSEVOICE_MODEL_LEN,
        "SenseVoice 模型",
    )?;
    verify_asset(&tokens_path, TOKENS_SHA256, TOKENS_LEN, "SenseVoice tokens")?;
    verify_asset(&vad_path, VAD_MODEL_SHA256, VAD_MODEL_LEN, "Silero VAD 模型")?;
    let session = Arc::new(SenseVoiceSession::load(
        &model_path,
        &tokens_path,
        &sherpa_path,
        &vad_path,
        RECOGNIZER_THREADS,
    )?);
    sessions.insert(key, Arc::clone(&session));
    tracing::info!("SenseVoice and Silero VAD sessions initialized successfully");
    Ok(session)
}

/// Create the resident Silero VAD session; RealVad borrows it under run_lock and
/// resets streaming state without unloading the model.
unsafe fn create_vad(sherpa_engine: &sherpa::Sherpa, vad_model_path: &Path) -> Result<*const sherpa::Vad, String> {
    // SAFETY：VAD 模型路径来自摘要校验后的固定资产；指针在本函数栈上
    // 存活到 FFI 调用结束，返回的会话由 RealVad 的 Drop 销毁。
    unsafe {
        let vad_c = sherpa::cstring(vad_model_path)?;
        let cpu = sherpa::cstr("cpu")?;
        sherpa_engine.create_vad(&vad_c, &cpu)
    }
}

// (fork) perf-tracing：识别器会话创建（缓存未命中时的冷启动大头）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "sensevoice_model_load", skip_all)
)]
unsafe fn create_recognizer(
    sherpa_engine: &sherpa::Sherpa,
    model_path: &Path,
    tokens_path: &Path,
    threads: i32,
) -> Result<*const sherpa::OfflineRecognizer, String> {
    // SAFETY：模型路径都来自摘要校验后的固定资产；FFI 调用期间 sherpa 句柄
    // 与配置字符串存活。
    unsafe {
        let model_c = sherpa::cstring(model_path)?;
        let tokens_c = sherpa::cstring(tokens_path)?;
        let zh = sherpa::cstr("zh")?;
        let cpu = sherpa::cstr("cpu")?;
        // JchTools 链路固定：language=zh、use_itn=1、provider cpu。
        sherpa_engine.create_recognizer(&model_c, &tokens_c, threads.max(1), &cpu, &zh)
    }
}

/// Load the FFmpeg shared libraries, decode + segment + transcribe, and
/// assemble the SV-06 output. The recognizer comes from the process-level
/// session cache; VAD streaming state is reset per run. Split from
/// [`transcribe_bytes`] so the perf spans wrap the exact stages named in
/// PERFORMANCE.md.
#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    session: &SenseVoiceSession,
    ffmpeg_dir: &Path,
    media_path: &Path,
    name: &str,
    max_duration_ms: Option<u64>,
    cancel: &crate::cancellation::CancellationToken,
) -> Result<SenseVoiceResult, String> {
    // 同一会话上的解码与推理严格串行（JchTools 参考语义）；锁内不涉及其它锁。
    let _run_guard = session.run_lock.lock().map_err(|_| "会话运行锁损坏".to_string())?;
    check_cancel(cancel)?;
    // SAFETY：模型与 DLL 路径都来自摘要校验后的固定资产；FFI 指针在
    // 本函数栈上存活到全部 FFI 调用结束。
    unsafe {
        let libs = ffmpeg_dll::FfmpegLibs::load(ffmpeg_dir)?;
        let mut real_vad = sherpa::RealVad::new(&session.sherpa, session.vad);
        let mut transcriber = sherpa::SenseVoiceTranscriber::new(&session.sherpa, session.recognizer);
        let mut pipeline = VadPipeline::new(&mut real_vad, &mut transcriber).with_cancel(cancel.clone());

        let mut sink = |samples: &[f32]| -> Result<(), String> { vad_push(&mut pipeline, samples) };
        let outcome = decode_file(&libs, media_path, &mut sink, cancel)?;
        check_cancel(cancel)?;
        vad_finish(&mut pipeline)?;
        check_cancel(cancel)?;

        // 时长以实际喂入 VAD 的样本为准（与片段时间戳同源）。
        let total_samples = pipeline.total_samples();
        let lines = pipeline.into_lines();

        if outcome.has_audio
            && let Some(max_ms) = max_duration_ms
        {
            let duration_ms = samples_to_ms(total_samples);
            if duration_ms > max_ms {
                return Err(format!(
                    "Decoded audio duration {duration_ms} ms exceeds transcription.max_duration_ms limit of {max_ms}"
                ));
            }
        }

        let duration_seconds = total_samples as f32 / vad::SAMPLE_RATE as f32;
        let markdown = output::markdown(name, duration_seconds, &lines, outcome.has_audio);
        let segments = lines
            .iter()
            .map(|line| (seconds_to_ms(line.start), seconds_to_ms(line.end), line.text.clone()))
            .collect();
        Ok(SenseVoiceResult {
            name: name.to_string(),
            segments,
            markdown,
            duration_ms: samples_to_ms(total_samples),
            has_audio: outcome.has_audio,
        })
    }
}

// (fork) perf-tracing：一次解码全程（打开输入 → 流式解码 → 双重 flush）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "ffmpeg_decode", skip_all)
)]
fn decode_file(
    libs: &ffmpeg_dll::FfmpegLibs,
    path: &Path,
    sink: &mut dyn FnMut(&[f32]) -> Result<(), String>,
    cancel: &crate::cancellation::CancellationToken,
) -> Result<ffmpeg_dll::DecodeOutcome, String> {
    ffmpeg_dll::decode_audio_cancellable(libs, path, |samples| sink(samples), cancel)
}

// (fork) perf-tracing：VAD 喂入窗口（含出段即转写）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "vad_segment", skip_all)
)]
fn vad_push(pipeline: &mut VadPipeline, samples: &[f32]) -> Result<(), String> {
    pipeline.push_samples(samples)
}

// (fork) perf-tracing：VAD EOF flush 与尾段收割。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "vad_segment", skip_all)
)]
fn vad_finish(pipeline: &mut VadPipeline) -> Result<(), String> {
    pipeline.finish()
}

fn seconds_to_ms(seconds: f32) -> u32 {
    (seconds.max(0.0) * 1000.0).round() as u32
}

fn samples_to_ms(total_samples: u64) -> u64 {
    (total_samples as f64 * 1000.0 / vad::SAMPLE_RATE as f64).round() as u64
}

/// Verify one pinned asset: exact byte length and exact SHA-256. Mismatch is a
/// hard error before any model load or inference.
fn verify_asset(path: &Path, expected_sha256: &str, expected_len: u64, label: &str) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读取 {label} 元数据失败（{}）：{e}", path.display()))?;
    if meta.len() != expected_len {
        return Err(format!(
            "{label} 摘要校验失败（{}）：大小 {actual} 字节，钉定 {expected} 字节；拒绝加载",
            path.display(),
            actual = meta.len(),
            expected = expected_len,
        ));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("打开 {label} 失败（{}）：{e}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("读取 {label} 失败（{}）：{e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        return Err(format!(
            "{label} 摘要校验失败（{}）：SHA-256 {actual}，钉定 {expected_sha256}；拒绝加载",
            path.display()
        ));
    }
    Ok(())
}

/// Model root resolution order: explicit config → `XBERG_SENSEVOICE_MODEL_DIR`
/// → exe-adjacent candidates. An explicitly requested directory that does not
/// exist is a hard error (no silent fallback).
fn resolve_model_dir(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        if dir.is_dir() {
            return Ok(dir.to_path_buf());
        }
        return Err(format!(
            "transcription.model_dir 不是有效的模型根目录: {}",
            dir.display()
        ));
    }
    if let Ok(value) = std::env::var(MODEL_DIR_ENV) {
        let dir = PathBuf::from(value);
        if dir.is_dir() {
            return Ok(dir);
        }
        return Err(format!("{MODEL_DIR_ENV} 不是有效的模型根目录: {}", dir.display()));
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    if let Some(dir) = exe_dir {
        for candidate in [dir.join("media-models"), dir.join("models"), dir] {
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "缺少 SenseVoice 模型根目录：请设置 transcription.model_dir 或环境变量 {MODEL_DIR_ENV}（需含 {}/{}、{}/{} 与 {}/{}）",
        SENSEVOICE_MODEL_RELATIVE[0],
        SENSEVOICE_MODEL_RELATIVE[1],
        SENSEVOICE_MODEL_RELATIVE[0],
        TOKENS_RELATIVE[1],
        VAD_MODEL_RELATIVE[0],
        VAD_MODEL_RELATIVE[1],
    ))
}

/// sherpa-onnx DLL resolution order: `XBERG_SHERPA_DLL_DIR` (file or
/// directory) → exe-adjacent candidates → model-root candidates (JchTools
/// layout) → `PATH`. `onnxruntime.dll` must sit next to the sherpa DLL; that
/// is enforced by [`sherpa::Sherpa::load`].
fn resolve_sherpa_dll(model_root: &Path) -> Result<PathBuf, String> {
    const DLL_NAMES: [&str; 2] = ["sherpa-onnx-c-api.dll", "sherpa-onnx.dll"];
    let is_sherpa = |p: &Path| {
        p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| DLL_NAMES.contains(&n))
    };

    if let Ok(value) = std::env::var(SHERPA_DLL_DIR_ENV) {
        let candidate = PathBuf::from(&value);
        if is_sherpa(&candidate) {
            return Ok(candidate);
        }
        if candidate.is_dir() {
            for name in DLL_NAMES {
                let inner = candidate.join(name);
                if inner.is_file() {
                    return Ok(inner);
                }
            }
        }
        return Err(format!(
            "{SHERPA_DLL_DIR_ENV} 不是有效的 sherpa-onnx DLL 路径（需为 {} 所在目录或文件）: {value}",
            DLL_NAMES[0]
        ));
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    if let Some(dir) = exe_dir {
        for sub in ["", "bin", "native", "media-dlls"] {
            for name in DLL_NAMES {
                let candidate = dir.join(sub).join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Some(found) = sherpa::find_sherpa(model_root).filter(|p| p.is_file()) {
        return Ok(found);
    }

    if let Some(dir) = std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).find(|p| DLL_NAMES.iter().any(|n| p.join(n).is_file())))
    {
        for name in DLL_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "缺少 sherpa-onnx DLL（{}）：请设置环境变量 {SHERPA_DLL_DIR_ENV} 或将其放入模型根目录",
        DLL_NAMES[0]
    ))
}

/// FFmpeg shared-library directory resolution order:
/// `XBERG_FFMPEG_DLL_DIR` → exe-adjacent candidates → model-root candidates
/// (JchTools layout) → `PATH`. The directory must hold the full pinned set.
fn resolve_ffmpeg_dir(model_root: &Path) -> Result<PathBuf, String> {
    let valid = |p: &Path| ffmpeg_dll::library_dir(p).is_some();

    if let Ok(value) = std::env::var(FFMPEG_DLL_DIR_ENV) {
        return ffmpeg_dll::library_dir(Path::new(&value))
            .ok_or_else(|| format!("{FFMPEG_DLL_DIR_ENV} 不是有效的 FFmpeg 共享库目录: {value}"));
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    if let Some(dir) = exe_dir {
        for sub in ["", "ffmpeg", "media-dlls", "media-dlls/ffmpeg", "bin"] {
            let candidate = dir.join(sub);
            if valid(&candidate) {
                return ffmpeg_dll::library_dir(&candidate).ok_or_else(|| "FFmpeg 共享库目录校验不一致".to_string());
            }
        }
    }

    let candidates = [
        model_root.join("ffmpeg"),
        model_root.join("bin"),
        model_root.join("../ffmpeg"),
    ];
    for candidate in candidates {
        if valid(&candidate) {
            return ffmpeg_dll::library_dir(&candidate).ok_or_else(|| "FFmpeg 共享库目录校验不一致".to_string());
        }
    }

    if let Some(dir) = std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).find(|p| valid(p))) {
        return ffmpeg_dll::library_dir(&dir).ok_or_else(|| "FFmpeg 共享库目录校验不一致".to_string());
    }

    Err(format!(
        "缺少固定版本 FFmpeg 共享库（{}）；请初始化资产或设置环境变量 {FFMPEG_DLL_DIR_ENV}",
        ffmpeg_dll::FFMPEG_DLLS.join("、")
    ))
}

/// Extension applied to the staged temp file so FFmpeg's demuxer gets a hint;
/// probing still works without one (empty string appended is a no-op).
fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "audio/mp4" | "video/mp4" => ".mp4",
        "audio/x-m4a" => ".m4a",
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/webm" | "video/webm" => ".webm",
        "video/x-ms-wmv" => ".wmv",
        "video/x-ms-asf" | "application/vnd.ms-asf" => ".asf",
        _ => "",
    }
}

/// RAII guard for the staged input: the temp file is removed on every path,
/// including decode failures (a failed transcription never leaves halves).
struct StagedMedia {
    path: PathBuf,
}

impl Drop for StagedMedia {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn stage_input(content: &[u8], mime_type: &str) -> Result<StagedMedia, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "xberg-sensevoice-{}-{nanos:x}{}",
        std::process::id(),
        extension_for_mime(mime_type)
    ));
    std::fs::write(&path, content).map_err(|e| format!("写入临时媒体文件失败（{}）：{e}", path.display()))?;
    Ok(StagedMedia { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_mapping_covers_supported_mime_types() {
        assert_eq!(extension_for_mime("video/mp4"), ".mp4");
        assert_eq!(extension_for_mime("audio/x-m4a"), ".m4a");
        assert_eq!(extension_for_mime("audio/mpeg"), ".mp3");
        assert_eq!(extension_for_mime("audio/wav"), ".wav");
        assert_eq!(extension_for_mime("video/x-ms-asf"), ".asf");
        assert_eq!(extension_for_mime("application/octet-stream"), "");
    }

    #[test]
    fn seconds_to_ms_rounds_half_up() {
        assert_eq!(seconds_to_ms(1.5), 1500);
        assert_eq!(seconds_to_ms(0.0005), 1);
        assert_eq!(seconds_to_ms(-1.0), 0);
    }

    #[test]
    fn samples_to_ms_converts_at_16khz() {
        assert_eq!(samples_to_ms(16_000), 1_000);
        assert_eq!(samples_to_ms(0), 0);
    }

    // 覆盖摘要钉定：篡改副本一个字节必须在加载前被拒绝（真实文件版见
    // 环境门控测试）。
    #[test]
    fn verify_asset_rejects_tampered_copy() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("xberg-sv-verify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("tampered.bin");
        // Use the real tokens.txt when available (small, pinned); otherwise a
        // synthetic file must also be rejected against the pinned tokens hash.
        let real = std::env::var("XBERG_TEST_SENSEVOICE_ROOT")
            .ok()
            .map(PathBuf::from)
            .map(|root| sherpa::model_file(&root, &TOKENS_RELATIVE));
        let bytes: Vec<u8> = match &real {
            Some(p) if p.is_file() => {
                let mut b = std::fs::read(p).expect("read tokens");
                let last = b.len() - 1;
                b[last] ^= 0xFF;
                b
            }
            _ => b"not the pinned tokens".to_vec(),
        };
        std::fs::write(&path, &bytes).expect("write tampered copy");
        let result = verify_asset(&path, TOKENS_SHA256, bytes.len() as u64, "SenseVoice tokens");
        std::fs::remove_file(&path).ok();
        let err = result.expect_err("篡改副本必须被拒绝");
        assert!(err.contains("摘要校验失败"), "unexpected: {err}");
    }

    #[test]
    fn verify_asset_rejects_wrong_size() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("xberg-sv-size-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("wrong-size.bin");
        std::fs::write(&path, b"short").expect("write");
        let result = verify_asset(&path, TOKENS_SHA256, TOKENS_LEN, "SenseVoice tokens");
        std::fs::remove_file(&path).ok();
        let err = result.expect_err("大小不符必须被拒绝");
        assert!(err.contains("大小") && err.contains("拒绝加载"), "unexpected: {err}");
    }

    #[test]
    fn resolve_model_dir_explicit_invalid_is_hard_error() {
        let err = resolve_model_dir(Some(Path::new("Z:/definitely/not/a/dir"))).expect_err("显式无效目录必须报错");
        assert!(err.contains("model_dir"), "unexpected: {err}");
    }

    /// (fork) 协作取消的入口检查点（WORKER.md：排队/运行任务在检查点停止）：
    /// 已取消的 token 必须在任何资产解析之前返回取消错误——即使模型目录不可用，
    /// 错误也是「transcription cancelled」而不是缺模型诊断，证明停止不等待模型工作。
    #[test]
    fn cancelled_request_stops_before_asset_resolution() {
        let cancel = crate::cancellation::CancellationToken::new();
        cancel.cancel();
        let err = transcribe_bytes_cancellable(
            b"",
            "speech.mp4",
            "video/mp4",
            Some(Path::new("Z:/definitely/not/a/dir")),
            None,
            &cancel,
        )
        .expect_err("已取消的请求必须在入口检查点停止");
        assert_eq!(err, "transcription cancelled");
    }

    // 覆盖进程级会话缓存：键对相同输入稳定、对路径与线程配置区分。
    #[test]
    fn session_cache_key_is_stable_and_discriminating() {
        let a = session_cache_key(
            Path::new("E:/m/model.onnx"),
            Path::new("E:/m/tokens.txt"),
            Path::new("E:/d/sherpa.dll"),
            1,
        );
        let again = session_cache_key(
            Path::new("E:/m/model.onnx"),
            Path::new("E:/m/tokens.txt"),
            Path::new("E:/d/sherpa.dll"),
            1,
        );
        assert_eq!(a, again, "same inputs must produce the same cache key");
        assert_ne!(
            a,
            session_cache_key(
                Path::new("E:/other/model.onnx"),
                Path::new("E:/m/tokens.txt"),
                Path::new("E:/d/sherpa.dll"),
                1
            ),
            "model path must be part of the key"
        );
        assert_ne!(
            a,
            session_cache_key(
                Path::new("E:/m/model.onnx"),
                Path::new("E:/m/tokens.txt"),
                Path::new("E:/d2/sherpa.dll"),
                1
            ),
            "sherpa DLL path must be part of the key"
        );
        assert_ne!(
            a,
            session_cache_key(
                Path::new("E:/m/model.onnx"),
                Path::new("E:/m/tokens.txt"),
                Path::new("E:/d/sherpa.dll"),
                4
            ),
            "thread configuration must be part of the key"
        );
    }

    // 覆盖进程级会话缓存：加载失败不得给缓存留下条目（下一次调用可重试），
    // 且缺失模型的报错可定位到具体文件。
    #[test]
    fn session_cache_stays_usable_after_a_failed_load() {
        let missing = std::env::temp_dir().join(format!("xberg-sv-session-missing-{}", std::process::id()));
        let err = get_or_create_session(&missing).expect_err("缺失模型必须报错");
        assert!(err.contains("媒体模型缺失"), "unexpected: {err}");

        let sessions = SESSIONS.lock().expect("cache lock");
        assert!(
            !sessions
                .iter()
                .any(|(key, _)| key.contains(&missing.to_string_lossy().to_string())),
            "a failed load must not insert a cache entry"
        );
    }

    /// 真实链路集成测试：只在固定资产与测试媒体齐备时执行（运行时条件跳过，
    /// 缺哪个资产就打印哪个）。环境变量：
    /// - `XBERG_TEST_SENSEVOICE_ROOT`：模型根目录（含 models/ 或直接布局）
    /// - `XBERG_TEST_SHERPA_DLL_DIR` / `XBERG_TEST_FFMPEG_DLL_DIR`：原生库目录
    /// - `XBERG_TEST_MEDIA_MP4` / `XBERG_TEST_MEDIA_M4A`：真实语音媒体
    /// - `XBERG_TEST_MEDIA_NO_AUDIO_MP4`：无音轨 MP4
    /// - `XBERG_TEST_MEDIA_SILENT_M4A`：纯静音 M4A
    mod real_assets {
        use super::*;
        use std::sync::Mutex;

        /// 这些测试写进程环境变量（edition 2024 下 unsafe）：用互斥锁串行化，
        /// 保证没有任何其它测试在写入的同时读取环境。
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        struct Env {
            root: PathBuf,
            sherpa: PathBuf,
            ffmpeg: PathBuf,
        }

        impl Env {
            fn load() -> Option<Env> {
                let root = std::env::var_os("XBERG_TEST_SENSEVOICE_ROOT")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())?;
                let sherpa = std::env::var_os("XBERG_TEST_SHERPA_DLL_DIR")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())?;
                let ffmpeg = std::env::var_os("XBERG_TEST_FFMPEG_DLL_DIR")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())?;
                Some(Env { root, sherpa, ffmpeg })
            }
        }

        fn media(name: &str) -> Option<PathBuf> {
            std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_file())
        }

        fn set_env(env: &Env) -> std::sync::MutexGuard<'static, ()> {
            let guard = ENV_LOCK.lock().unwrap();
            // SAFETY：持有 ENV_LOCK，其它测试不会并发读写进程环境。
            unsafe {
                std::env::set_var(MODEL_DIR_ENV, &env.root);
                std::env::set_var(SHERPA_DLL_DIR_ENV, &env.sherpa);
                std::env::set_var(FFMPEG_DLL_DIR_ENV, &env.ffmpeg);
            }
            guard
        }

        // 覆盖真实链路：MP4 进 → 有段、时间戳递增、文本非空、术语映射生效
        //（识别文本中不允许再出现已被映射的源词「扫描链」；SAPI 合成音对该
        // 词的识别质量由运行报告如实给出，不作为断言）。
        #[test]
        fn real_mp4_transcribes_with_segments_and_terms() {
            let (Some(env), Some(media)) = (Env::load(), media("XBERG_TEST_MEDIA_MP4")) else {
                println!(
                    "skip：真实资产不在场（XBERG_TEST_SENSEVOICE_ROOT/_SHERPA_DLL_DIR/_FFMPEG_DLL_DIR/XBERG_TEST_MEDIA_MP4）"
                );
                return;
            };
            let _env_guard = set_env(&env);
            let result = transcribe_bytes(
                &std::fs::read(&media).expect("读取测试媒体"),
                "sample.mp4",
                "video/mp4",
                None,
                None,
            )
            .expect("真实 MP4 转录不应失败");
            assert!(result.has_audio);
            assert!(!result.segments.is_empty(), "必须有至少一个语音片段");
            let mut last_start = 0u32;
            for (start_ms, end_ms, text) in &result.segments {
                assert!(!text.trim().is_empty(), "片段文本非空");
                assert!(end_ms > start_ms, "段时间戳递增：{start_ms} < {end_ms}");
                assert!(*start_ms >= last_start, "片段按时间顺序输出");
                last_start = *start_ms;
            }
            // 术语映射作用于片段正文：映射源词不得原样出现在输出里。
            for (_, _, text) in &result.segments {
                assert!(!text.contains("扫描链"), "「扫描链」必须已映射为 scan chain：{text}");
            }
            println!("MP4 转录文本：{:#?}", result.segments);
            println!("MP4 SV-06 markdown：\n{}", result.markdown);
            assert!(result.markdown.starts_with("# sample.mp4\n"));
            assert!(result.markdown.contains("## 转录\n"));
        }

        // 覆盖真实链路：M4A 进（同链路不同容器）。
        #[test]
        fn real_m4a_transcribes_with_segments() {
            let (Some(env), Some(media)) = (Env::load(), media("XBERG_TEST_MEDIA_M4A")) else {
                println!("skip：真实资产不在场（…/XBERG_TEST_MEDIA_M4A）");
                return;
            };
            let _env_guard = set_env(&env);
            let result = transcribe_bytes(
                &std::fs::read(&media).expect("读取测试媒体"),
                "sample.m4a",
                "audio/x-m4a",
                None,
                None,
            )
            .expect("真实 M4A 转录不应失败");
            assert!(result.has_audio);
            assert!(!result.segments.is_empty(), "M4A 必须有语音片段");
            assert!(result.segments.iter().all(|(_, _, t)| !t.trim().is_empty()));
            println!("M4A 转录文本：{:#?}", result.segments);
        }

        // 覆盖：无音轨容器 → has_audio=false、「无音频轨道」说明，不是失败。
        #[test]
        fn real_no_audio_container_reports_missing_track() {
            let (Some(env), Some(media)) = (Env::load(), media("XBERG_TEST_MEDIA_NO_AUDIO_MP4")) else {
                println!("skip：真实资产不在场（…/XBERG_TEST_MEDIA_NO_AUDIO_MP4）");
                return;
            };
            let _env_guard = set_env(&env);
            let result = transcribe_bytes(
                &std::fs::read(&media).expect("读取测试媒体"),
                "noaudio.mp4",
                "video/mp4",
                None,
                None,
            )
            .expect("无音轨不是失败");
            assert!(!result.has_audio);
            assert!(result.segments.is_empty());
            assert_eq!(result.duration_ms, 0);
            assert!(result.markdown.contains("- 音频时长: 无音频轨道"));
            assert!(result.markdown.contains("（无音频轨道）"));
        }

        // 覆盖：纯静音媒体 → 有音轨但零片段、「未检测到语音」说明，不是失败。
        #[test]
        fn real_silent_media_reports_no_speech() {
            let (Some(env), Some(media)) = (Env::load(), media("XBERG_TEST_MEDIA_SILENT_M4A")) else {
                println!("skip：真实资产不在场（…/XBERG_TEST_MEDIA_SILENT_M4A）");
                return;
            };
            let _env_guard = set_env(&env);
            let result = transcribe_bytes(
                &std::fs::read(&media).expect("读取测试媒体"),
                "silent.m4a",
                "audio/x-m4a",
                None,
                None,
            )
            .expect("纯静音不是失败");
            assert!(result.has_audio, "静音媒体有音轨");
            assert!(result.segments.is_empty(), "静音不得产生片段");
            assert!(result.markdown.contains("（未检测到语音）"));
            assert!(result.duration_ms > 0);
        }

        // 覆盖摘要钉定：模型副本被篡改一个字节 → 加载被拒（摘要校验失败），
        // 真资产不动（tokens.txt 用副本，model/vad 用硬链接只读）。
        #[test]
        fn tampered_model_copy_is_rejected_before_inference() {
            let Some(env) = Env::load() else {
                println!("skip：真实资产不在场（XBERG_TEST_SENSEVOICE_ROOT）");
                return;
            };
            let temp_root = std::env::temp_dir().join(format!("xberg-sv-tamper-{}", std::process::id()));
            let nested = temp_root.join("models").join(super::SENSEVOICE_MODEL_RELATIVE[0]);
            std::fs::create_dir_all(&nested).expect("创建临时模型目录");
            let real_root = env.root.join("models").join(super::SENSEVOICE_MODEL_RELATIVE[0]);

            // model.int8.onnx 与 silero_vad.onnx 用硬链接（只读，零拷贝）。
            std::fs::hard_link(
                real_root.join(super::SENSEVOICE_MODEL_RELATIVE[1]),
                nested.join(super::SENSEVOICE_MODEL_RELATIVE[1]),
            )
            .or_else(|_| {
                std::fs::copy(
                    real_root.join(super::SENSEVOICE_MODEL_RELATIVE[1]),
                    nested.join(super::SENSEVOICE_MODEL_RELATIVE[1]),
                )
                .map(|_| ())
            })
            .expect("链接模型文件");
            let vad_real = env
                .root
                .join("models")
                .join(super::VAD_MODEL_RELATIVE[0])
                .join(super::VAD_MODEL_RELATIVE[1]);
            let vad_nested = temp_root.join("models").join(super::VAD_MODEL_RELATIVE[0]);
            std::fs::create_dir_all(&vad_nested).expect("创建 VAD 目录");
            std::fs::hard_link(&vad_real, vad_nested.join(super::VAD_MODEL_RELATIVE[1]))
                .or_else(|_| std::fs::copy(&vad_real, vad_nested.join(super::VAD_MODEL_RELATIVE[1])).map(|_| ()))
                .expect("链接 VAD 文件");

            // tokens.txt 复制后翻转最后一个字节：大小不变、摘要必错。
            let tokens_src = real_root.join(super::TOKENS_RELATIVE[1]);
            let mut tokens = std::fs::read(&tokens_src).expect("读取 tokens");
            let last = tokens.len() - 1;
            tokens[last] ^= 0xFF;
            std::fs::write(nested.join(super::TOKENS_RELATIVE[1]), &tokens).expect("写篡改副本");

            let err = transcribe_bytes(b"\x00\x00", "t.mp4", "video/mp4", Some(&temp_root), None)
                .expect_err("篡改副本必须被拒绝");
            assert!(err.contains("摘要校验失败"), "unexpected: {err}");
            assert!(err.contains("tokens"), "unexpected: {err}");
            let _ = std::fs::remove_dir_all(&temp_root);
        }

        // 覆盖超预算拒绝：max_duration_ms 小于实际时长时报错（不再进入推理）。
        #[test]
        fn duration_limit_is_enforced() {
            let (Some(env), Some(media)) = (Env::load(), media("XBERG_TEST_MEDIA_MP4")) else {
                println!("skip：真实资产不在场（…/XBERG_TEST_MEDIA_MP4）");
                return;
            };
            let _env_guard = set_env(&env);
            let err = transcribe_bytes(
                &std::fs::read(&media).expect("读取测试媒体"),
                "sample.mp4",
                "video/mp4",
                None,
                Some(1),
            )
            .expect_err("时长超限必须报错");
            assert!(err.contains("max_duration_ms"), "unexpected: {err}");
        }
    }
}
