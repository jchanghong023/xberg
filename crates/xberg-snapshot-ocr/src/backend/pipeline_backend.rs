//! `crate::pipeline::OcrBackend` 的真实实现（O-23/O-24/O-26）。（移植自 JchTools
//! `snap-ocr-worker`，仅按新 crate 的模块路径调整 import。）
//!
//! 组合 [`DetPaddlex`]（检测）与 [`RecPaddlex`]（识别）两个常驻 ort 会话，
//! 图像句柄即 [`BgrImage`]；增强/拉伸/旋转直接映射到 [`crate::image_ops`]
//! 的 OpenCV 对照算子：
//!
//! - `tile`：`ocr.py:164-166` 的 `image[y:bottom, x:right]` 连续化视图；
//! - `enhance_small_crop`：2× INTER_CUBIC（`SMALL_TEXT_SCALE_FACTOR`）；
//! - `stretch_recognition_crop`：宽 ×1.5（半偶取整）INTER_LANCZOS4；
//! - `rotate_90s`：`numpy.rot90`；
//! - `recognize`：整批透传 [`RecPaddlex::recognize_crops`]（管线已按 8 切批，
//!   内部再按 8 幂等分批不改变批构成）；超宽裁剪（识别 imgW 钳制分支、横向
//!   压缩系数 < 0.5）先按 [`split_segment_count`] 等分拆段识别再拼接——
//!   压扁单次前向实测会整行丢失行内细笔画（如表格/代码行中的 `|`）。
//! - `detect`：透传 [`DetPaddlex::detect_tile_detached`]（瓦片局部坐标）。
//!
//! oracle 验收入口可按 `oracle_dump.py` 的 `_DetRecorder`/`_RecRecorder`
//! 语义记录逐调用明细；产品服务不保留截图裁剪摘要或正文副本。

use std::path::Path;
use std::sync::Mutex;

use crate::pipeline::{
    CropRecord, OcrBackend, PipelineError, SMALL_TEXT_SCALE_FACTOR, WIDE_TEXT_HORIZONTAL_SCALE_FACTOR,
};
use crate::types::Quad;
use crate::types::TileRegion;

use super::det::DetPaddlex;
use super::image_ops::{self, BgrImage};
use super::rec::{unclamped_img_w, RecPaddlex, MAX_IMG_W};
use super::sha256::sha256_hex;

/// 一次 det 调用的记录（`oracle_dump.py` det_calls 条目）。
#[derive(Debug, Clone)]
pub struct DetCallLog {
    /// 瓦片局部坐标多边形（原始输出，未做全局映射）。
    pub polys: Vec<Quad>,
    /// 与 `polys` 等长的检测分。
    pub scores: Vec<f64>,
}

/// 一个 rec 批调用的裁剪指纹。
#[derive(Debug, Clone)]
pub struct CropDigest {
    /// 裁剪宽（像素）。
    pub width: usize,
    /// 裁剪高（像素）。
    pub height: usize,
    /// BGR 行主序字节的 SHA-256（十六进制）。
    pub sha256: String,
}

/// 一次 rec 批调用的记录（`oracle_dump.py` rec_calls 条目）。
#[derive(Debug, Clone)]
pub struct RecCallLog {
    /// 批大小。
    pub batch: usize,
    /// 批内裁剪指纹（输入序）。
    pub crops: Vec<CropDigest>,
    /// 识别文本（与裁剪等长）。
    pub texts: Vec<String>,
    /// 识别分数（与裁剪等长）。
    pub scores: Vec<f64>,
}

/// 真实推理后端：DetPaddlex + RecPaddlex 常驻会话 + 调用记录。
pub struct WorkerOcrBackend {
    det: DetPaddlex,
    rec: RecPaddlex,
    record_calls: bool,
    det_log: Mutex<Vec<DetCallLog>>,
    rec_log: Mutex<Vec<RecCallLog>>,
}

impl WorkerOcrBackend {
    /// 加载检测/识别模型与字典。
    ///
    /// # Errors
    /// 任一模型或字典加载失败时返回 [`PipelineError::Backend`]（消息不含
    /// 截图内容与用户数据，O-29/O-30）。
    pub fn load(det_model: &Path, rec_model: &Path, dict: &Path, intra_threads: usize) -> Result<Self, PipelineError> {
        Self::load_with_logging(det_model, rec_model, dict, intra_threads, true)
    }

    /// 常驻产品服务只保留模型会话，不计算/缓存用于开发对照的裁剪指纹和正文副本。
    pub fn load_for_service(
        det_model: &Path,
        rec_model: &Path,
        dict: &Path,
        intra_threads: usize,
    ) -> Result<Self, PipelineError> {
        Self::load_with_logging(det_model, rec_model, dict, intra_threads, false)
    }

    fn load_with_logging(
        det_model: &Path,
        rec_model: &Path,
        dict: &Path,
        intra_threads: usize,
        record_calls: bool,
    ) -> Result<Self, PipelineError> {
        let det = DetPaddlex::from_path(det_model, intra_threads).map_err(|e| PipelineError::Backend(e.to_string()))?;
        let rec =
            RecPaddlex::load(rec_model, dict, intra_threads).map_err(|e| PipelineError::Backend(e.to_string()))?;
        Ok(Self {
            det,
            rec,
            record_calls,
            det_log: Mutex::new(Vec::new()),
            rec_log: Mutex::new(Vec::new()),
        })
    }

    /// 取走 det 调用记录（`detect_candidates` 之后调用一次）。
    pub fn take_det_calls(&self) -> Vec<DetCallLog> {
        self.det_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// 取走 rec 批调用记录（`recognize_records` 之后调用一次）。
    pub fn take_rec_calls(&self) -> Vec<RecCallLog> {
        self.rec_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// 识别一批已就绪的裁剪（无拆段；调试转储与调用记录以此为口径）。
    fn recognize_flat(&self, batch: &[BgrImage]) -> Result<Vec<(String, f64)>, PipelineError> {
        let results = self
            .rec
            .recognize_crops(batch)
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        Self::debug_dump_crops(batch, &results);
        if self.record_calls {
            let log = RecCallLog {
                batch: batch.len(),
                crops: batch
                    .iter()
                    .map(|crop| CropDigest {
                        width: crop.width(),
                        height: crop.height(),
                        sha256: sha256_hex(crop.data()),
                    })
                    .collect(),
                texts: results.iter().map(|(t, _)| t.clone()).collect(),
                scores: results.iter().map(|(_, s)| *s).collect(),
            };
            self.rec_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(log);
        }
        Ok(results)
    }
}

/// 超宽裁剪的分段数：unclamped imgW 整除 3200，≥2 才拆段。
fn split_segment_count(width: usize, height: usize) -> usize {
    if height == 0 {
        return 1; // 零高裁剪由 rec 侧 InvalidCrop 拒绝，这里不拆段
    }
    (unclamped_img_w(width, height) / MAX_IMG_W).max(1)
}

/// 等分横向切片（前 `width % count` 段各多 1 像素）；段宽 ≥ 1 由分段数
/// 上界（≤ 48·w/h/3200）保证。
fn split_wide_crop(image: &BgrImage, count: usize) -> Result<Vec<BgrImage>, PipelineError> {
    let width = image.width();
    let height = image.height();
    let base = width / count;
    let extra = width % count;
    let mut segments = Vec::with_capacity(count);
    let mut x0 = 0;
    for index in 0..count {
        let seg_w = base + usize::from(index < extra);
        let mut data = Vec::with_capacity(seg_w * height * 3);
        for row in 0..height {
            let row_base = (row * width + x0) * 3;
            data.extend_from_slice(&image.data()[row_base..row_base + seg_w * 3]);
        }
        segments.push(
            BgrImage::from_vec(seg_w, height, data)
                .map_err(|_| PipelineError::Backend("wide-crop segment allocation failed".into()))?,
        );
        x0 += seg_w;
    }
    Ok(segments)
}

/// 分段结果归并：文本按段序拼接，分数按段文本长度（空段计 1）加权平均；
/// 全空段的分数仍为 0（与空串尝试语义一致，`select_best` 天然弃用）。
#[allow(clippy::cast_precision_loss)] // 权重为段内字符数，量级远小于 2^53。
fn merge_segment_results(groups: &[(usize, usize)], results: &[(String, f64)]) -> Vec<(String, f64)> {
    groups
        .iter()
        .map(|&(start, len)| {
            let segments = &results[start..start + len];
            let text: String = segments.iter().map(|(t, _)| t.as_str()).collect();
            let mut weight_sum = 0.0;
            let mut score_sum = 0.0;
            for (segment_text, segment_score) in segments {
                let weight = segment_text.chars().count().max(1) as f64;
                weight_sum += weight;
                score_sum += segment_score * weight;
            }
            let score = if weight_sum > 0.0 { score_sum / weight_sum } else { 0.0 };
            (text, score)
        })
        .collect()
}

impl OcrBackend for WorkerOcrBackend {
    type Image = BgrImage;

    fn enhance_small_crop(&self, image: &Self::Image) -> Self::Image {
        image_ops::resize_inter_cubic(
            image,
            image.width() * SMALL_TEXT_SCALE_FACTOR as usize,
            image.height() * SMALL_TEXT_SCALE_FACTOR as usize,
        )
    }

    // OpenCV fixture path retains the original usize → u32 narrowing and Python
    // round-to-even float → integer conversion (including Rust's saturating cast).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn stretch_recognition_crop(&self, image: &Self::Image) -> Self::Image {
        // ocr.py:262 用 Python round()（半偶）后取整。
        let width = (f64::from(image.width() as u32) * WIDE_TEXT_HORIZONTAL_SCALE_FACTOR).round_ties_even() as usize;
        image_ops::resize_inter_lanczos4(image, width, image.height())
    }

    fn rotate_90s(&self, image: &Self::Image, degrees: u16) -> Self::Image {
        match degrees {
            0 => image.clone(),
            // 管线只传 additional_rotations 的 90/180/270；rot90 对 1..=3 不可能
            // 失败，防御分支按契约违规原样返回（不产生错误数据流）。
            degrees => image_ops::rot90(image, u32::from(degrees / 90)).unwrap_or_else(|_| image.clone()),
        }
    }

    fn recognize(&self, batch: &[Self::Image]) -> Result<Vec<(String, f64)>, PipelineError> {
        // 超宽裁剪按钳制几何等分拆段：单次压扁前向（横向压缩系数 < 0.5）实测
        // 会整行丢失行内细笔画；段数取 unclamped imgW / 3200（≥2 才拆），使每
        // 段落回钳制分支时的压缩系数保持在 (0.5, 1]，无超宽裁剪时零开销透传。
        let counts: Vec<usize> = batch
            .iter()
            .map(|image| split_segment_count(image.width(), image.height()))
            .collect();
        if counts.iter().all(|&k| k == 1) {
            return self.recognize_flat(batch);
        }
        let mut flat: Vec<BgrImage> = Vec::with_capacity(batch.len());
        let mut groups: Vec<(usize, usize)> = Vec::with_capacity(batch.len());
        for (image, &count) in batch.iter().zip(&counts) {
            let start = flat.len();
            if count == 1 {
                flat.push(image.clone());
            } else {
                flat.extend(split_wide_crop(image, count)?);
            }
            groups.push((start, flat.len() - start));
        }
        let results = self.recognize_flat(&flat)?;
        Ok(merge_segment_results(&groups, &results))
    }

    fn tile(&self, image: &Self::Image, tile: &TileRegion) -> Result<Self::Image, PipelineError> {
        let (x, y) = (tile.x() as usize, tile.y() as usize);
        let (width, height) = (tile.width() as usize, tile.height() as usize);
        if x + width > image.width() || y + height > image.height() {
            return Err(PipelineError::Backend("tile out of image bounds".into()));
        }
        let mut data = Vec::with_capacity(width * height * 3);
        for row in y..y + height {
            let base = (row * image.width() + x) * 3;
            data.extend_from_slice(&image.data()[base..base + width * 3]);
        }
        BgrImage::from_vec(width, height, data).map_err(|_| PipelineError::PredictorOutput)
    }

    // The detector interface takes u32 dimensions; retain the original narrowing.
    #[allow(clippy::cast_possible_truncation)]
    fn detect(&self, tile_image: &Self::Image) -> Result<(Vec<Quad>, Vec<f64>), PipelineError> {
        let (mut raw_polys, mut scores) = self
            .det
            .detect_tile_detached(tile_image.data(), tile_image.width() as u32, tile_image.height() as u32)
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        // 辅助细线合成（line_detect 模块文档）：补齐 DBNet 漏检的水平分隔线。
        let extra = super::line_detect::synthesize_thin_line_boxes(
            tile_image.data(),
            tile_image.width() as u32,
            tile_image.height() as u32,
            &raw_polys,
        );
        if !extra.is_empty() {
            for quad in extra {
                raw_polys.push(quad);
                scores.push(super::line_detect::THIN_LINE_SCORE);
            }
        }
        let polys = raw_polys
            .into_iter()
            .map(|poly| {
                [
                    (poly[0][0], poly[0][1]),
                    (poly[1][0], poly[1][1]),
                    (poly[2][0], poly[2][1]),
                    (poly[3][0], poly[3][1]),
                ]
            })
            .collect::<Vec<_>>();
        if self.record_calls {
            self.det_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(DetCallLog {
                    polys: polys.clone(),
                    scores: scores.clone(),
                });
        }
        Ok((polys, scores))
    }
}

/// 为透视裁剪后的候选框组装初始识别记录。
///
/// 增强由 [`crate::pipeline::recognize_records`] 按源高判定并经
/// [`OcrBackend::enhance_small_crop`] 原地替换，此处只装填增强前裁剪。
#[allow(clippy::cast_possible_truncation)] // CropRecord stores dimensions as u32.
#[must_use]
pub fn build_record(candidate: crate::types::DetectionCandidate, crop: BgrImage) -> CropRecord<BgrImage> {
    let source_width = crop.width() as u32;
    let source_height = crop.height() as u32;
    CropRecord {
        candidate,
        crop,
        source_width,
        source_height,
        attempts: Vec::new(),
    }
}

// ─────────────────── Xberg glue: assembly from validated parts ───────────────────

impl WorkerOcrBackend {
    /// 调试转储（环境变量 `XBERG_SNAPSHOT_DEBUG_CROPS` 指向输出目录时启用）：
    /// 每张进入识别的裁剪存为 `{序号}_{宽}x{高}_{分数}.ppm`（P6/RGB），同名
    /// `.txt` 存识别文本。仅服务于离线调参，正常路径（未设环境变量）零开销。
    fn debug_dump_crops(batch: &[BgrImage], results: &[(String, f64)]) {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let Some(dir) = std::env::var_os("XBERG_SNAPSHOT_DEBUG_CROPS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        for (crop, (text, score)) in batch.iter().zip(results) {
            let seq = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let (width, height) = (crop.width(), crop.height());
            let mut ppm = Vec::with_capacity(width * height * 3 + 32);
            ppm.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
            for pixel in crop.data().chunks_exact(3) {
                ppm.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            let stem = format!("{seq:04}_{width}x{height}_{score}");
            let _ = std::fs::write(dir.join(format!("{stem}.ppm")), &ppm);
            let _ = std::fs::write(dir.join(format!("{stem}.txt")), text.as_bytes());
        }
    }

    /// Assemble a backend from already-constructed det/rec sessions
    /// (Xberg `SnapshotOcrModels::load` glue; call logging stays disabled so
    /// the resident service path keeps no crop fingerprints, mirroring
    /// `load_for_service`).
    pub(crate) fn from_parts(det: DetPaddlex, rec: RecPaddlex) -> Self {
        Self {
            det,
            rec,
            record_calls: false,
            det_log: Mutex::new(Vec::new()),
            rec_log: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn sha256_matches_reference_vectors() {
        // FIPS 180-4 / RFC 6234 标准向量。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn backend_ops_geometry() {
        // 无模型的算子几何：拉伸宽度半偶取整、旋转与瓦片裁剪。
        let img = BgrImage::from_vec(
            3,
            2,
            vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 170, 180,
            ],
        )
        .unwrap();
        let tile = crate::types::TileRegion::new(0, 1, 0, 2, 2, 3, 2).unwrap();
        // tile() 与 build_record 需要 backend 实例（会话），几何由 image_ops
        // 单测覆盖；此处仅校验 build_record 的字段装填。
        let candidate = crate::types::DetectionCandidate::new(
            [(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0)],
            0.5,
            tile,
            1.0,
            false,
            Vec::new(),
        )
        .unwrap();
        let record = build_record(candidate, img.clone());
        assert_eq!(record.source_width, 3);
        assert_eq!(record.source_height, 2);
        assert!(record.attempts.is_empty());
        assert_eq!(record.crop, img);
    }

    #[test]
    fn split_segment_count_follows_clamp_geometry() {
        // 3842×23：unclamped imgW = int(48×167.04) = 8017 → k = 2（long_line 语料）。
        assert_eq!(split_segment_count(3842, 23), 2);
        // 2008×22：imgW = 4381 → k = 1，不拆（现有压扁读取正确，保持不变）。
        assert_eq!(split_segment_count(2008, 22), 1);
        // imgW 恰为 2×3200 → k = 2；差 1 像素 → k = 1。
        assert_eq!(split_segment_count(6400, 48), 2);
        assert_eq!(split_segment_count(6399, 48), 1);
        // imgW 未超上限 → k = 1。
        assert_eq!(split_segment_count(1000, 100), 1);
        assert_eq!(split_segment_count(100, 1000), 1);
        // 防御分支：零高不拆（rec 侧 InvalidCrop 拒绝）。
        assert_eq!(split_segment_count(100, 0), 1);
    }

    #[test]
    fn split_wide_crop_preserves_pixels_and_order() {
        // 5×2 三段（base=1、extra=2 → 段宽 2/2/1），逐像素还原原图。
        let width = 5;
        let height = 2;
        let data: Vec<u8> = (0..width * height * 3)
            .map(|v| u8::try_from(v % 251).unwrap())
            .collect();
        let img = BgrImage::from_vec(width, height, data.clone()).unwrap();
        let segments = split_wide_crop(&img, 3).unwrap();
        assert_eq!(segments.iter().map(BgrImage::width).collect::<Vec<_>>(), vec![2, 2, 1]);
        let mut rebuilt = vec![0u8; data.len()];
        let mut x0 = 0;
        for segment in &segments {
            for row in 0..height {
                let src = row * segment.width() * 3;
                let dst = (row * width + x0) * 3;
                rebuilt[dst..dst + segment.width() * 3]
                    .copy_from_slice(&segment.data()[src..src + segment.width() * 3]);
            }
            x0 += segment.width();
        }
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn merge_segment_results_weights_by_text_length() {
        let groups = [(0, 2), (2, 1)];
        let results = vec![("abc".to_string(), 0.9), ("de".to_string(), 0.6), (String::new(), 0.0)];
        let merged = merge_segment_results(&groups, &results);
        // 第一组：(0.9×3 + 0.6×2) / 5 = 0.78。
        assert!((merged[0].1 - 0.78).abs() < 1e-9);
        assert_eq!(merged[0].0, "abcde");
        // 第二组：单段空文本 → 空串、分数 0（权重和为正、分数和恰为 0.0）。
        assert_eq!(merged[1].0, "");
        assert!(merged[1].1.abs() < 1e-12);
    }
}
