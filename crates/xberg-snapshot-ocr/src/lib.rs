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
//! # Model set: `snapshot-pp-ocrv6-small-textsnap`
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
use crate::pipeline::{assemble_spans, detect_candidates, recognize_records, OcrBackend, PipelineError};
use crate::types::Quad;

/// Snapshot model set name (SNAP-02).
pub const SNAPSHOT_MODEL_SET: &str = "snapshot-pp-ocrv6-small-textsnap";

/// Pinned det model digest: `PP-OCRv6_small_det/inference.onnx` (SNAP-03).
pub const DET_MODEL_SHA256: &str = "3914f972d833af87d23bb2338bd09238f978a48f3c4dbb8e1a4ee26a93869940";
/// Pinned det model size in bytes (SNAP-03).
pub const DET_MODEL_BYTES: u64 = 9_891_707;

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

/// Byte-pinned, ready-to-use snapshot OCR model set (`snapshot-pp-ocrv6-small-textsnap`).
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
