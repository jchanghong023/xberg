//! 辅助细线检测：为 DBNet 概率图漏检的水平分隔线合成候选框。
//!
//! 背景（2026-10-16 用户授权的识别质量优化；**不属于** paddlex oracle 语义，
//! 与 [`super::det`] 的逐位对齐实现分离）：钉定的 `PP-OCRv6_small_det` 对
//! 高度 ≤4px 的长水平笔画（表格 `----` 分隔行、成排下划线）概率响应接近零
//! （实测破折号行 maxp≈0.04，二值化阈值 0.3 之下无轮廓），这类元素整行丢失。
//! 而识别模型对合成框内的虚线/下划线可正常读出（score≈0.83-0.99），实线
//! 边框读出空串、由既有的空文本过滤自然丢弃。
//!
//! 算法（逐行墨迹剖面，纯整数运算、确定性）：
//! 1. 灰度暗像素（整数均值 < 160）按行计数，得每行墨迹数与 x 范围；
//! 2. 连续墨迹行构成 band，过滤「高度 ≤ 4px 且 x 跨度 ≥ 48px」的细长 band；
//! 3. band 上方/下方 4px 内必须有近空白行（墨迹 ≤ 2px），排除文字行内的
//!    破折号等已有检测框覆盖的元素；
//! 4. 与本瓦片已有检测框（含 2px 外扩）相交的候选剔除；
//! 5. 合成四边形：y 方向外扩 `min(6, 邻带间距-1)`、x 方向外扩 4px。
//!
//! 合成框走既有管线（全局映射、接缝合并、去重、重裁识别），识别为空的
//! 框被 [`crate::pipeline::assemble_spans`] 丢弃，因此对实线边框、
//! 下划线装饰无害。

/// band 内单行最小墨迹像素数（低于该值视为空白行）。
const MIN_ROW_INK: u32 = 16;
/// band 高度上限（细线语义）。
const MAX_BAND_HEIGHT: u32 = 4;
/// band x 跨度下限（长线语义；排除字符级短横线）。
const MIN_BAND_WIDTH: u32 = 48;
/// band 与最近邻 band 的最小垂直空白（隔行）。
const MIN_ISOLATION_GAP: u32 = 4;
/// 隔离行的允许墨迹（噪声容限）。
const ISOLATION_MAX_INK: u32 = 2;
/// x 方向外扩（两侧各）。
const PAD_X: f64 = 4.0;
/// y 方向最大外扩（受邻带间距约束）。
const MAX_PAD_Y: f64 = 6.0;
/// 与既有检测框判定相交时的外扩余量。
const OVERLAP_MARGIN: f64 = 2.0;
/// 合成框的固定检测分（仅信息展示，不参与下游门控）。
pub(crate) const THIN_LINE_SCORE: f64 = 0.5;

/// 一条候选细线的 band（瓦片局部坐标，含行范围与 x 跨度）。
struct InkBand {
    top: u32,
    bottom: u32,
    min_x: u32,
    max_x: u32,
}

/// 行墨迹统计（每行暗像素计数 + 首/末暗像素 x）。
struct RowInk {
    count: Vec<u32>,
    first: Vec<u32>,
    last: Vec<u32>,
}

// Integer luma average and per-row scans are intentionally branch-light;
// bounds come from the padded runs below.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn row_ink_profile(bgr: &[u8], w: u32, h: u32) -> RowInk {
    let (w, h) = (w as usize, h as usize);
    let mut count = vec![0_u32; h];
    let mut first = vec![0_u32; h];
    let mut last = vec![0_u32; h];
    for y in 0..h {
        let row = &bgr[y * w * 3..(y + 1) * w * 3];
        let mut seen = 0_u32;
        for (x, pixel) in row.chunks_exact(3).enumerate() {
            // 整数平均灰度：< 160 视为暗像素（抗锯齿笔画）。
            let mean = (u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2])) / 3;
            if mean < 160 {
                if seen == 0 {
                    first[y] = x as u32;
                }
                last[y] = x as u32;
                seen += 1;
            }
        }
        count[y] = seen;
    }
    RowInk { count, first, last }
}

/// 提取满足「细长 + 上下隔离」的 band（瓦片局部坐标）。
// Band coordinates stay within the u32 tile dimensions; the usize→u32 casts
// mirror the det/layout modules' tile-coordinate convention.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn thin_bands(ink: &RowInk, h: u32) -> Vec<InkBand> {
    let h = h as usize;
    let mut bands: Vec<InkBand> = Vec::new();
    let mut y = 0;
    while y < h {
        if ink.count[y] < MIN_ROW_INK {
            y += 1;
            continue;
        }
        let top = y;
        while y < h && ink.count[y] >= MIN_ROW_INK {
            y += 1;
        }
        let bottom = y - 1; // 含
        if (bottom - top) as u32 + 1 > MAX_BAND_HEIGHT {
            continue;
        }
        let min_x = ink.first[top..=bottom].iter().copied().min().unwrap_or(0);
        let max_x = ink.last[top..=bottom].iter().copied().max().unwrap_or(0);
        let extent = max_x.saturating_sub(min_x).saturating_add(1);
        if extent < MIN_BAND_WIDTH {
            continue;
        }
        // 上下隔离：band 外 4 行内不得有实质墨迹。
        let isolated =
            |range: std::ops::Range<usize>| range.filter(|&r| r < h).all(|r| ink.count[r] <= ISOLATION_MAX_INK);
        let below_start = bottom + 1;
        let above = isolated(top.saturating_sub(MIN_ISOLATION_GAP as usize)..top);
        let below = isolated(below_start..below_start + MIN_ISOLATION_GAP as usize);
        if above && below {
            bands.push(InkBand {
                top: top as u32,
                bottom: bottom as u32,
                min_x,
                max_x,
            });
        }
    }
    bands
}

/// 候选框（外扩后）与既有检测框的包围盒是否相交。
fn overlaps_existing(band: &InkBand, pad_y: f64, w: u32, h: u32, existing: &[[[f64; 2]; 4]]) -> bool {
    let x0 = (f64::from(band.min_x) - PAD_X - OVERLAP_MARGIN).max(0.0);
    let x1 = (f64::from(band.max_x) + PAD_X + 1.0 + OVERLAP_MARGIN).min(f64::from(w));
    let y0 = (f64::from(band.top) - pad_y - OVERLAP_MARGIN).max(0.0);
    let y1 = (f64::from(band.bottom) + pad_y + 1.0 + OVERLAP_MARGIN).min(f64::from(h));
    existing.iter().any(|quad| {
        let (qx0, qx1) = quad
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
        let (qy0, qy1) = quad
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        x0 < qx1 + OVERLAP_MARGIN && qx0 < x1 + OVERLAP_MARGIN && y0 < qy1 + OVERLAP_MARGIN && qy0 < y1 + OVERLAP_MARGIN
    })
}

/// 对单张瓦片合成细线候选框（瓦片局部坐标，TL/TR/BR/BL）。
///
/// `existing` 为本瓦片 DBNet 已产出的检测框（同坐标系）；与其相交的候选
/// 直接剔除，避免与真实检测重复。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn synthesize_thin_line_boxes(
    tile_bgr: &[u8],
    tile_w: u32,
    tile_h: u32,
    existing: &[[[f64; 2]; 4]],
) -> Vec<[[f64; 2]; 4]> {
    if tile_w == 0 || tile_h == 0 {
        return Vec::new();
    }
    let ink = row_ink_profile(tile_bgr, tile_w, tile_h);
    let bands = thin_bands(&ink, tile_h);
    let mut quads = Vec::new();
    for band in &bands {
        // y 外扩受相邻 band 约束：扩不到邻带里；无邻带时取满额外扩。
        let pad_above = prev_band_bottom(&bands, band).map_or(MAX_PAD_Y, |gap| (f64::from(gap) - 1.0).max(1.0));
        let pad_below = next_band_top(&bands, band).map_or(MAX_PAD_Y, |gap| (f64::from(gap) - 1.0).max(1.0));
        let pad_y = MAX_PAD_Y.min(pad_above).min(pad_below);
        if overlaps_existing(band, pad_y, tile_w, tile_h, existing) {
            continue;
        }
        let x0 = (f64::from(band.min_x) - PAD_X).max(0.0);
        let x1 = (f64::from(band.max_x) + 1.0 + PAD_X).min(f64::from(tile_w));
        let y0 = (f64::from(band.top) - pad_y).max(0.0);
        let y1 = (f64::from(band.bottom) + 1.0 + pad_y).min(f64::from(tile_h));
        if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
            continue;
        }
        quads.push([[x0, y0], [x1, y0], [x1, y1], [x0, y1]]);
    }
    quads
}

/// 排序前的 band 列表按发现序（top 升序）生成，这里直接线性查找邻带；
/// 返回该 band 与相邻 band 的垂直间距（`None` 表示该侧没有邻带）。
fn prev_band_bottom(bands: &[InkBand], band: &InkBand) -> Option<u32> {
    bands
        .iter()
        .filter(|other| other.bottom < band.top)
        .map(|other| band.top - other.bottom)
        .min()
}

fn next_band_top(bands: &[InkBand], band: &InkBand) -> Option<u32> {
    bands
        .iter()
        .filter(|other| other.top > band.bottom)
        .map(|other| other.top - band.bottom)
        .min()
}
