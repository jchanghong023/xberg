//! # xberg-snapshot-ocr
//!
//! Screenshot OCR channel for Xberg: PP-OCRv6 small det/rec with byte-pinned
//! models plus the deterministic tiling / seam-merge / dedup / re-crop /
//! line-clustering / grid-layout logic, coexisting with (and never touching)
//! the document OCR channel (`xberg-paddle-ocr`).
//!
//! # Behavior contract
//!
//! The authoritative behavior contract is the JchTools snapshot OCR spec
//! `docs/requirements/SNAP2TEXT.md`: O-23 (CPU inference constraints), O-24
//! (deterministic tiling), O-25 (seam merge, transitive dedup, re-crop), O-26
//! (orientation retries and batched recognition), O-27 (line clustering), O-28
//! (half-width grid layout text) with Appendix B fixing the determinism
//! details (rounding, tie-breaks, constants). Xberg carries these into
//! `docs/requirements/OCR-SNAPSHOT.md` as SNAP-01..SNAP-13. The port is
//! bit-faithful: logic, constants, thresholds, ordering and rounding are kept
//! unchanged; the pure-logic modules (`types`, `tiling`, `geometry`,
//! `detection`, `orientation`, `layout`, `pipeline`, `ucd_tables`) and the
//! backend modules (`backend::{det, rec, image_ops, pipeline_backend,
//! sha256}`) are byte-identical to their JchTools sources except import paths.
//!
//! # Model set: `snapshot-pp-ocrv6-medium-det-textsnap`
//!
//! [`SnapshotOcrModels::load`] verifies all three members against pinned
//! sizes and SHA-256 digests before building any session and refuses to fall
//! back or to run inference on mismatched bytes (SNAP-04/SNAP-05):
//!
//! | member | bytes | SHA-256 |
//! |---|---|---|
//! | det | 9,891,707 | [`DET_MODEL_SHA256`] |
//! | rec | 21,148,338 | [`REC_MODEL_SHA256`] |
//! | dict (18,708 entries) | 74,947 | [`DICT_SHA256`] |
//!
//! # Runtime requirements
//!
//! - with `load-dynamic` builds of `ort`, set `ORT_DYLIB_PATH` to the
//!   onnxruntime shared library BEFORE calling [`SnapshotOcrModels::load`]
//!   (statically linked builds resolve ORT at link time and need nothing)
//!   (pin the same-directory DLL first so no other copy is preemptively
//!   loaded through the search path).
//! - All session builds are serialized through the process-wide
//!   [`backend::det::SESSION_BUILD_GATE`].
//! - The reference `intra_threads` value is [`REFERENCE_INTRA_THREADS`] (10,
//!   the JchTools TextSnap engine config; O-23).
//!
//! Image decoding is out of scope: callers hand over BGR HxWx3 pixels
//! (row-major, 3 bytes per pixel). The `examples/snapshot_ocr_cli.rs` binary
//! shows the PNG decode + channel-swap recipe.

pub mod backend;
pub mod detection;
pub mod geometry;
pub mod layout;
pub mod orientation;
pub mod pipeline;
pub mod tiling;
pub mod types;
pub mod ucd_tables;

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::det::{DetError, DetPaddlex};
use crate::backend::image_ops::{self, BgrImage};
use crate::backend::pipeline_backend::{build_record, WorkerOcrBackend};
use crate::backend::rec::{RecError, RecPaddlex};
use crate::backend::sha256::sha256_hex;
use crate::layout::build_layout;
use crate::pipeline::{
    assemble_spans, detect_candidates, recognize_records, CropRecord, OcrBackend, PipelineError, RECOGNITION_BATCH_SIZE,
};
use crate::types::Quad;

/// Snapshot model set name (SNAP-02).
pub const SNAPSHOT_MODEL_SET: &str = "snapshot-pp-ocrv6-medium-det-textsnap";

/// Pinned det model digest: RapidAI ONNX export of `PP-OCRv6_det_medium` (SNAP-03).
pub const DET_MODEL_SHA256: &str = "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2";
/// Pinned det model size in bytes (SNAP-03).
pub const DET_MODEL_BYTES: u64 = 62_119_454;

/// Pinned rec model digest: `PP-OCRv6_small_rec/inference.onnx` (SNAP-03).
pub const REC_MODEL_SHA256: &str = "3e3def686ac9a1676b59bc9749ad896263d8f68b53f352060774de359a2e23ed";
/// Pinned rec model size in bytes (SNAP-03).
pub const REC_MODEL_BYTES: u64 = 21_148_338;

/// Pinned ordered dictionary digest (18,708 entries, SNAP-03/SNAP-06).
pub const DICT_SHA256: &str = "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d";
/// Pinned dictionary size in bytes (SNAP-03).
pub const DICT_BYTES: u64 = 74_947;

/// Reference intra-op thread count (JchTools TextSnap `_DEFAULT_ENGINE_CONFIG`).
pub const REFERENCE_INTRA_THREADS: usize = 10;

/// Which pinned model member an asset error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMember {
    /// Detection model.
    Det,
    /// Recognition model.
    Rec,
    /// Ordered recognition dictionary.
    Dict,
}

impl fmt::Display for ModelMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Det => write!(f, "det"),
            Self::Rec => write!(f, "rec"),
            Self::Dict => write!(f, "dict"),
        }
    }
}

/// Model asset validation / session failures (SNAP-04/SNAP-05: fail loudly,
/// never fall back, never infer on mismatched bytes).
#[derive(Debug)]
pub enum AssetError {
    /// File read failed.
    Io {
        /// Offending member.
        member: ModelMember,
        /// Underlying io error.
        source: std::io::Error,
    },
    /// Size or SHA-256 digest does not match the pinned model set.
    DigestMismatch {
        /// Offending member.
        member: ModelMember,
        /// Pinned SHA-256 for the member.
        expected_sha256: &'static str,
        /// SHA-256 actually observed on disk.
        actual_sha256: String,
        /// Pinned size in bytes.
        expected_bytes: u64,
        /// Size actually observed on disk.
        actual_bytes: u64,
    },
    /// ONNX Runtime session build or warm-up inference failed.
    Session {
        /// Offending member when attributable.
        member: Option<ModelMember>,
        /// Sanitized backend message (no image content, no user paths).
        message: String,
    },
    /// CTC class count does not match the `[blank] + dict + space` layout
    /// (SNAP-05/SNAP-06).
    DictLayout {
        /// Dictionary entries (excluding blank/space).
        dict_entries: usize,
        /// Model output class count.
        class_count: usize,
    },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { member, source } => {
                write!(f, "snapshot model asset read failed ({member}): {source}")
            }
            Self::DigestMismatch {
                member,
                expected_sha256,
                actual_sha256,
                expected_bytes,
                actual_bytes,
            } => write!(
                f,
                "snapshot model asset mismatch ({member}, model set \
                 {SNAPSHOT_MODEL_SET}): expected SHA-256 {expected_sha256} \
                 ({expected_bytes} bytes), got {actual_sha256} ({actual_bytes} \
                 bytes); refusing to fall back or infer (SNAP-04/SNAP-05)"
            ),
            Self::Session { member, message } => match member {
                Some(member) => write!(f, "snapshot {member} session error: {message}"),
                None => write!(f, "snapshot session error: {message}"),
            },
            Self::DictLayout {
                dict_entries,
                class_count,
            } => write!(
                f,
                "rec dictionary/class mismatch: dict has {dict_entries} entries but \
                 model output has {class_count} classes; expected classes == entries \
                 + 2 ([blank] + dict + [space]) (SNAP-05/SNAP-06)"
            ),
        }
    }
}

impl std::error::Error for AssetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Top-level snapshot OCR error.
#[derive(Debug)]
pub enum SnapshotOcrError {
    /// Model asset validation or session failure (load path).
    Asset(AssetError),
    /// User cancellation observed at a tile/batch checkpoint (O-19/SNAP-16:
    /// cancellation is not a failure).
    Cancelled,
    /// Caller-provided image is invalid (wrong buffer length, zero extent).
    InputInvalid(String),
    /// Pipeline/inference failure (sanitized messages; O-29/O-30).
    Pipeline(PipelineError),
}

impl fmt::Display for SnapshotOcrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asset(error) => write!(f, "snapshot model asset error: {error}"),
            Self::Cancelled => write!(f, "snapshot ocr cancelled"),
            Self::InputInvalid(message) => write!(f, "invalid image input: {message}"),
            Self::Pipeline(error) => write!(f, "snapshot ocr pipeline error: {error}"),
        }
    }
}

impl std::error::Error for SnapshotOcrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Asset(error) => Some(error),
            Self::Pipeline(error) => Some(error),
            Self::Cancelled | Self::InputInvalid(_) => None,
        }
    }
}

impl From<AssetError> for SnapshotOcrError {
    fn from(value: AssetError) -> Self {
        Self::Asset(value)
    }
}

impl From<PipelineError> for SnapshotOcrError {
    fn from(value: PipelineError) -> Self {
        match value {
            PipelineError::Cancelled => Self::Cancelled,
            other => Self::Pipeline(other),
        }
    }
}

/// One recognized text record with its final image-global geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotOcrRecord {
    /// Detection quad in image-global pixel coordinates (TL/TR/BR/BL).
    pub quad: Quad,
    /// Recognized text (empty text records are dropped by the pipeline).
    pub text: String,
    /// Detection confidence in [0, 1].
    pub detection_score: f64,
    /// Recognition confidence in [0, 1] (best orientation attempt).
    pub recognition_score: f64,
    /// Winning orientation: 0/90/180/270 degrees.
    pub rotation_degrees: u16,
}

/// Result of one screenshot recognition.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotOcrOutput {
    /// Layout-preserving plain text (O-27/O-28; no trailing newline).
    pub layout_text: String,
    /// Structured records, one per non-empty recognized span.
    pub records: Vec<SnapshotOcrRecord>,
}

/// Byte-pinned, ready-to-use snapshot OCR model set (`snapshot-pp-ocrv6-medium-det-textsnap`).
///
/// Wraps the resident det/rec ort sessions. Load via [`SnapshotOcrModels::load`];
/// recognize via [`SnapshotOcrModels::recognize`]. The load path mirrors the
/// JchTools service: verify digests, build sessions under the process-wide
/// gate, then warm both sessions with blank-white probes so session/dict
/// problems surface at load time and never mid-recognition.
pub struct SnapshotOcrModels {
    backend: WorkerOcrBackend,
}

impl fmt::Debug for SnapshotOcrModels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Resident ort sessions are not introspectable; show identity only.
        f.debug_struct("SnapshotOcrModels")
            .field("model_set", &SNAPSHOT_MODEL_SET)
            .finish_non_exhaustive()
    }
}

impl SnapshotOcrModels {
    /// Load the three model members after SHA-256 + size verification.
    ///
    /// For `load-dynamic` builds of `ort`, `ORT_DYLIB_PATH` must point at
    /// onnxruntime before this call. On any
    /// mismatch the load fails with an [`AssetError`] naming the member and
    /// expected/actual digests; there is no fallback path (SNAP-04/SNAP-05).
    ///
    /// # Errors
    /// [`SnapshotOcrError::Asset`] on any asset/session/dict-layout failure.
    pub fn load(
        det_path: &Path,
        rec_path: &Path,
        dict_path: &Path,
        intra_threads: usize,
    ) -> Result<Self, SnapshotOcrError> {
        verify_member(det_path, ModelMember::Det, DET_MODEL_SHA256, DET_MODEL_BYTES)?;
        verify_member(rec_path, ModelMember::Rec, REC_MODEL_SHA256, REC_MODEL_BYTES)?;
        verify_member(dict_path, ModelMember::Dict, DICT_SHA256, DICT_BYTES)?;

        let det = DetPaddlex::from_path(det_path, intra_threads).map_err(|error| {
            SnapshotOcrError::Asset(match error {
                DetError::Io(source) => AssetError::Io {
                    member: ModelMember::Det,
                    source,
                },
                other => AssetError::Session {
                    member: Some(ModelMember::Det),
                    message: other.to_string(),
                },
            })
        })?;
        let rec = RecPaddlex::load(rec_path, dict_path, intra_threads).map_err(|error| {
            SnapshotOcrError::Asset(match error {
                RecError::Io(source) => AssetError::Io {
                    member: ModelMember::Rec,
                    source,
                },
                RecError::DictLayout {
                    dict_entries,
                    class_count,
                } => AssetError::DictLayout {
                    dict_entries,
                    class_count,
                },
                other => AssetError::Session {
                    member: Some(ModelMember::Rec),
                    message: other.to_string(),
                },
            })
        })?;
        let backend = WorkerOcrBackend::from_parts(det, rec);

        // Warm-up probes (same blank-white shapes as the JchTools service load):
        // both sessions must complete one real inference before we call the
        // model set ready.
        let white_det = BgrImage::from_vec(64, 64, vec![255; 64 * 64 * 3])
            .map_err(|error| warmup_error(None, error.to_string()))?;
        backend
            .detect(&white_det)
            .map_err(|error| warmup_error(Some(ModelMember::Det), error.to_string()))?;
        let white_rec = BgrImage::from_vec(64, 32, vec![255; 64 * 32 * 3])
            .map_err(|error| warmup_error(None, error.to_string()))?;
        backend
            .recognize(&[white_rec])
            .map_err(|error| warmup_error(Some(ModelMember::Rec), error.to_string()))?;

        Ok(Self { backend })
    }

    /// Recognize one BGR HxWx3 image (row-major pixels).
    ///
    /// Flow is identical to the JchTools worker service path (O-23..O-28):
    /// tile detection with cancellation checkpoints, seam merge + transitive
    /// dedup, per-box perspective re-crop from the original image, batched
    /// recognition (batch 8) with small-crop enhancement / dense-code stretch
    /// / orientation retries, span assembly, then grid layout text.
    ///
    /// Cancellation is honored at tile and batch checkpoints; once the cancel
    /// flag is observed, [`SnapshotOcrError::Cancelled`] is returned instead
    /// of a (possibly empty) result.
    ///
    /// # Errors
    /// [`SnapshotOcrError::Cancelled`], [`SnapshotOcrError::InputInvalid`],
    /// or [`SnapshotOcrError::Pipeline`] on backend failures.
    pub fn recognize(
        &self,
        image_bgr_hwc: &[u8],
        width: u32,
        height: u32,
        cancel: &AtomicBool,
    ) -> Result<SnapshotOcrOutput, SnapshotOcrError> {
        let expected_len = u64::from(width) * u64::from(height) * 3;
        if width == 0 || height == 0 || expected_len != image_bgr_hwc.len() as u64 {
            return Err(SnapshotOcrError::InputInvalid(format!(
                "image buffer length {} does not match {width}x{height}x3",
                image_bgr_hwc.len()
            )));
        }
        let image = BgrImage::from_vec(width as usize, height as usize, image_bgr_hwc.to_vec())
            .map_err(|error| SnapshotOcrError::InputInvalid(error.to_string()))?;

        let candidates = detect_candidates(&self.backend, &image, width, height, Some(cancel))?;
        let mut records = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if cancel.load(Ordering::Acquire) {
                return Err(SnapshotOcrError::Cancelled);
            }
            // Degenerate quads are skipped exactly like the JchTools service
            // (`if let Ok(crop) = warp_perspective_cubic_replicate(...)`).
            if let Ok(crop) = image_ops::warp_perspective_cubic_replicate(&image, candidate.quad()) {
                records.push(build_record(candidate, crop));
            }
        }
        recognize_records(&self.backend, &mut records, Some(cancel))?;
        punct_extension_retry(&self.backend, &image, &mut records, Some(cancel))?;
        let spans = assemble_spans(&records, Some(cancel))?;
        let layout_text = if spans.is_empty() {
            String::new()
        } else {
            build_layout(&spans).text
        };
        if cancel.load(Ordering::Acquire) {
            return Err(SnapshotOcrError::Cancelled);
        }
        let out_records = spans
            .iter()
            .map(|span| SnapshotOcrRecord {
                quad: *span.quad(),
                text: span.text().to_string(),
                detection_score: span.detection_score(),
                recognition_score: span.recognition_score(),
                rotation_degrees: span.rotation_degrees(),
            })
            .collect();
        Ok(SnapshotOcrOutput {
            layout_text,
            records: out_records,
        })
    }
}

/// Warm-up session failure with best-effort member attribution.
fn warmup_error(member: Option<ModelMember>, message: String) -> SnapshotOcrError {
    SnapshotOcrError::Asset(AssetError::Session { member, message })
}

/// Read one model member and check size + SHA-256 against the pinned values.
fn verify_member(
    path: &Path,
    member: ModelMember,
    expected_sha256: &'static str,
    expected_bytes: u64,
) -> Result<(), AssetError> {
    let bytes = std::fs::read(path).map_err(|source| AssetError::Io { member, source })?;
    let actual_bytes = bytes.len() as u64;
    let actual_sha256 = sha256_hex(&bytes);
    if actual_bytes != expected_bytes || actual_sha256 != expected_sha256 {
        return Err(AssetError::DigestMismatch {
            member,
            expected_sha256,
            actual_sha256,
            expected_bytes,
            actual_bytes,
        });
    }
    Ok(())
}

/// 行首标点扩展重试的接受判定（纯逻辑，供单测）。
///
/// 门控（全部满足才接受扩展结果）：新文本非空且严格更长；原文本作为整体
/// 出现在新文本端部（前缀或后缀，中间插入一律拒绝）；新增非空白字符全部
/// 属于标点/线条/连接符类且至少 2 个（单字符扩展无法与图标边缘读出的
/// `+`/`-` 区分）；新分不低于原分减 [`crate::pipeline::CODE_STRETCH_SCORE_TOLERANCE`]。
#[must_use]
fn punct_extension_allowed(old_text: &str, new_text: &str, old_score: f64, new_score: f64) -> bool {
    fn is_connector(c: char) -> bool {
        c.is_whitespace()
            || c.is_ascii_punctuation()
            || matches!(
                c,
                '·' | '—' | '–' | '「' | '」' | '『' | '』' | '，' | '。' | '：' | '；' | '、'
            )
    }
    if new_text.is_empty() || new_text.chars().count() <= old_text.chars().count() {
        return false;
    }
    if new_score < old_score - crate::pipeline::CODE_STRETCH_SCORE_TOLERANCE {
        return false;
    }
    let added: String = if new_text.ends_with(old_text) {
        new_text[..new_text.len() - old_text.len()].to_string()
    } else if new_text.starts_with(old_text) {
        new_text[old_text.len()..].to_string()
    } else {
        return false;
    };
    added.chars().filter(|c| !c.is_whitespace()).count() >= 2 && added.chars().all(is_connector)
}

/// 行首标点扩展重试（2026-10-17 用户授权，SNAP-10 第四次增补配套）。
///
/// DBNet 对紧贴正文左侧的短破折号/连字符（如 `-- flag` 的 `--`）概率响应
/// 接近零（与表格分隔线同类盲区），这些字形从检测框中消失；而识别模型在
/// 包含左侧破折号的更宽裁剪上可正常读出（ide_dark 实测：合并框
/// `-- dark theme console` 识别分 0.998，独立碎框读空）。本重试对同行最左
/// 的文本框向左扩展 1.5×框高重新识别，几何门控依次为：
/// 1. 框宽高比 ≤ 20（超宽长行的扩展探测无产出且代价高）；
/// 2. 同行没有更靠左的框（行首标点只可能出现在行首；聊天气泡的作者列等
///    左侧已有框时本框不可能是行首）；
/// 3. 扩展区未被其他检测框覆盖（真实 UI 图标都会被 det 独立检出成框）。
/// 内容门控见 [`punct_extension_allowed`]；接受时替换 0° 首试（与密集代码
/// 拉伸重试同一替换位）。
///
/// # Errors
/// 后端识别失败或用户取消时透传 [`PipelineError`]。
fn punct_extension_retry(
    backend: &WorkerOcrBackend,
    image: &BgrImage,
    records: &mut [CropRecord<BgrImage>],
    cancel: Option<&AtomicBool>,
) -> Result<(), PipelineError> {
    struct Job {
        index: usize,
        old_text: String,
        old_score: f64,
    }
    let mut jobs: Vec<Job> = Vec::new();
    let mut crops: Vec<BgrImage> = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let best = crate::orientation::select_best(&record.attempts);
        if best.text.is_empty() || best.rotation_degrees != 0 {
            continue;
        }
        let quad = *record.candidate.quad();
        let (left, top, right, bottom) = crate::geometry::quad_bounds(&quad);
        let height = bottom - top;
        let width = right - left;
        if !height.is_finite() || height <= 1.0 || !(width / height).is_finite() || width / height > 20.0 {
            continue;
        }
        // 行首先验：同行（y 范围 ± 半个框高容差）已有更靠左的框时跳过。
        let leftmost_in_row = !records.iter().enumerate().any(|(other, record)| {
            other != index && {
                let (other_left, other_top, _, other_bottom) = crate::geometry::quad_bounds(record.candidate.quad());
                other_left < left - 2.0 && other_top < bottom + height * 0.5 && other_bottom > top - height * 0.5
            }
        });
        if !leftmost_in_row {
            continue;
        }
        let ext = height * 1.5;
        // det 已有框覆盖扩展区时不必重读（真实 UI 图标都会被 det 独立检出）。
        let zone_blocked = records.iter().enumerate().any(|(other, record)| {
            other != index && {
                let (other_left, other_top, other_right, other_bottom) =
                    crate::geometry::quad_bounds(record.candidate.quad());
                let intersect_x = (other_right.min(left) - other_left.max(left - ext)).max(0.0);
                let intersect_y = (other_bottom.min(bottom) - other_top.max(top)).max(0.0);
                intersect_x > 2.0 && intersect_y > 2.0
            }
        });
        if zone_blocked {
            continue;
        }
        let extended = [(left - ext, top), (right, top), (right, bottom), (left - ext, bottom)];
        let Ok(crop) = image_ops::warp_perspective_cubic_replicate(image, &extended) else {
            continue;
        };
        jobs.push(Job {
            index,
            old_text: best.text.clone(),
            old_score: best.score,
        });
        crops.push(crop);
    }
    if jobs.is_empty() {
        return Ok(());
    }
    let mut results: Vec<(String, f64)> = Vec::with_capacity(jobs.len());
    for chunk in crops.chunks(RECOGNITION_BATCH_SIZE) {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(PipelineError::Cancelled);
        }
        results.extend(backend.recognize(chunk)?);
    }
    for (job, (text, score)) in jobs.into_iter().zip(results) {
        if !punct_extension_allowed(&job.old_text, &text, job.old_score, score) {
            continue;
        }
        records[job.index].attempts[0] = crate::orientation::RecognitionAttempt {
            text,
            score,
            rotation_degrees: 0,
        };
    }
    Ok(())
}

#[cfg(test)]
mod punct_extension_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use super::punct_extension_allowed;

    // ide_dark 实测：`--` 碎框死亡、合并框识别分 0.998 ≥ 0.999 − 0.02，接受。
    #[test]
    fn accepts_leading_dash_run_with_equal_score() {
        assert!(punct_extension_allowed(
            "dark theme console",
            "-- dark theme console",
            0.999,
            0.998,
        ));
    }

    // 前缀扩展同样成立（行尾破折号/省略号场景）。
    #[test]
    fn accepts_trailing_dash_run() {
        assert!(punct_extension_allowed("row 1", "row 1 --", 0.95, 0.95));
    }

    // 单字符扩展拒绝：real_ui 实测图标边缘读出的 `-→`、`+新建任务` 损伤。
    #[test]
    fn rejects_single_character_extension() {
        assert!(!punct_extension_allowed("→", "-→", 0.754, 0.9));
        assert!(!punct_extension_allowed("新建任务", "+新建任务", 1.0, 1.0));
    }

    // 中间插入（原文本不是新文本端部）拒绝。
    #[test]
    fn rejects_middle_insertion() {
        assert!(!punct_extension_allowed("abc", "a--bc", 0.9, 0.95));
    }

    // 非连接符字符（字母/数字/CJK 正文）拒绝。
    #[test]
    fn rejects_non_connector_added_characters() {
        assert!(!punct_extension_allowed("abc", "x abc", 0.9, 0.95));
        assert!(!punct_extension_allowed("搜索", "Q搜索", 0.9, 0.95));
    }

    // 分数跌出容差拒绝。
    #[test]
    fn rejects_score_drop_beyond_tolerance() {
        assert!(!punct_extension_allowed("abc", "-- abc", 0.99, 0.9));
        assert!(punct_extension_allowed("abc", "-- abc", 0.99, 0.97 + 1e-9));
    }

    // 更短或等长的“扩展”拒绝。
    #[test]
    fn rejects_shorter_or_equal_length() {
        assert!(!punct_extension_allowed("-- abc", "abc", 0.9, 0.95));
        assert!(!punct_extension_allowed("-- abc", "-- abc", 0.9, 0.95));
    }

    // 全空白扩展拒绝（无新增非空白连接符）。
    #[test]
    fn rejects_whitespace_only_extension() {
        assert!(!punct_extension_allowed("abc", "   abc", 0.9, 0.95));
    }
}
