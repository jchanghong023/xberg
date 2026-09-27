//! 环境门控的真实模型测试（无资产时 early-return 并打印跳过原因，不算失败）。
//!
//! 合同编号（Xberg `docs/requirements/OCR-SNAPSHOT.md` 与 JchTools
//! `docs/requirements/SNAP2TEXT.md`）：
//! - SNAP-03/SNAP-04：模型集按字节钉定；错配明确失败、不回退、不推理。
//! - SNAP-05：det/rec/dict 任一成员错配（含互换路径、tiny 字典冒充）必须拒绝。
//! - SNAP-07/O-23：固定 CPU 推理、会话经进程门串行化构建、加载期空白探针预热。
//! - SNAP-13/O-24/O-26/O-27/O-28：同一输入两次识别输出逐字节一致（确定性）。
//! - SNAP-16/O-19：取消在瓦片/批次检查点生效，返回可区分的 Cancelled。
//!
//! 运行条件（全部满足才实际执行）：
//! - `XBERG_SNAPSHOT_TEST_MODELS` = 模型目录（文件名无关，按钉定 SHA-256 识别成员，
//!   例如 det.onnx / rec.onnx / dict/dict.txt 任意命名均可）；
//! - `ORT_DYLIB_PATH` = onnxruntime 动态库路径（会话构建前钉定，防抢加载）。
//! 可选：`XBERG_SNAPSHOT_TINY_DICT` = 一个非成套字典文件（如 v6 tiny 字典），
//! 用于验证字典错配被拒。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

use xberg_snapshot_ocr::backend::sha256::sha256_hex;
use xberg_snapshot_ocr::{
    AssetError, DICT_SHA256, DET_MODEL_SHA256, ModelMember, REC_MODEL_SHA256, REFERENCE_INTRA_THREADS,
    SnapshotOcrError, SnapshotOcrModels,
};

/// 按钉定摘要定位出的三个模型成员路径。
#[derive(Clone)]
struct ModelPaths {
    det: PathBuf,
    rec: PathBuf,
    dict: PathBuf,
}

/// 进程内一次性解析环境门控；skip（未设环境变量）与资产损坏（硬失败）分开。
static MODELS: OnceLock<Result<ModelPaths, String>> = OnceLock::new();

fn pinned_models() -> &'static Result<ModelPaths, String> {
    MODELS.get_or_init(|| {
        let Some(dir) = std::env::var_os("XBERG_SNAPSHOT_TEST_MODELS") else {
            return Err("skip: XBERG_SNAPSHOT_TEST_MODELS is not set".to_string());
        };
        if std::env::var_os("ORT_DYLIB_PATH").is_none() {
            return Err("skip: ORT_DYLIB_PATH is not set".to_string());
        }
        locate_by_digest(Path::new(&dir))
    })
}

/// 每个测试入口：返回 None 表示跳过（已打印原因）。
fn require_models() -> Option<ModelPaths> {
    match pinned_models() {
        Err(reason) => {
            println!("{reason}");
            assert!(!reason.starts_with("FAIL"), "{reason}");
            None
        }
        Ok(paths) => Some(paths.clone()),
    }
}

/// 扫描目录（含子目录，如 `dict/dict.txt`），按钉定 SHA-256 分派成员；
/// 文件名无关（SNAP-03 按字节校验）。
fn locate_by_digest(dir: &Path) -> Result<ModelPaths, String> {
    let mut det = None;
    let mut rec = None;
    let mut dict = None;
    scan_recursive(dir, 0, &mut det, &mut rec, &mut dict)?;
    let missing: Vec<&str> = [
        ("det (SHA-256 3914f972…)", det.is_none()),
        ("rec (SHA-256 3e3def68…)", rec.is_none()),
        ("dict (SHA-256 b5f2bfe2…)", dict.is_none()),
    ]
    .into_iter()
    .filter(|(_, absent)| *absent)
    .map(|(name, _)| name)
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "FAIL: {} does not contain the pinned model members, missing: {}",
            dir.display(),
            missing.join(", ")
        ));
    }
    Ok(ModelPaths {
        det: det.unwrap_or_default(),
        rec: rec.unwrap_or_default(),
        dict: dict.unwrap_or_default(),
    })
}

/// 递归哈希文件并按摘要分派（限深 4，防异常目录树）。
fn scan_recursive(
    dir: &Path,
    depth: usize,
    det: &mut Option<PathBuf>,
    rec: &mut Option<PathBuf>,
    dict: &mut Option<PathBuf>,
) -> Result<(), String> {
    if depth > 4 {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("FAIL: cannot read model dir {}: {error}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_recursive(&path, depth + 1, det, rec, dict)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("FAIL: cannot read {}: {error}", path.display()))?;
        match sha256_hex(&bytes).as_str() {
            DET_MODEL_SHA256 if det.is_none() => *det = Some(path),
            REC_MODEL_SHA256 if rec.is_none() => *rec = Some(path),
            DICT_SHA256 if dict.is_none() => *dict = Some(path),
            _ => {}
        }
    }
    Ok(())
}

/// 纯白图（无文字）。
fn white_image(width: u32, height: u32) -> Vec<u8> {
    vec![255_u8; width as usize * height as usize * 3]
}

/// 合成“文字状”图：白底 + 若干黑色竖条（确定性识别冒烟，不构造 oracle 文本）。
fn bar_pattern_image() -> (u32, u32, Vec<u8>) {
    let (width, height) = (480_u32, 120_u32);
    let mut data = white_image(width, height);
    let mut set_dark = |y: u32, x: u32| {
        let base = (y as usize * width as usize + x as usize) * 3;
        data[base] = 10;
        data[base + 1] = 10;
        data[base + 2] = 10;
    };
    for x in (20..width - 30).step_by(12) {
        for y in 30..90_u32 {
            for dx in 0..6_u32 {
                set_dark(y, x + dx);
            }
        }
    }
    (width, height, data)
}

// 覆盖 SNAP-04/SNAP-05：rec 与 dict 路径互换时，加载必须在构建会话前拒绝，
// 且错误明确指向 rec 成员并给出期望/实际摘要。
#[test]
fn load_rejects_swapped_rec_and_dict_paths() {
    let Some(m) = require_models() else {
        return;
    };
    let error = SnapshotOcrModels::load(&m.det, &m.dict, &m.rec, REFERENCE_INTRA_THREADS)
        .expect_err("swapped rec/dict paths must be rejected");
    match error {
        SnapshotOcrError::Asset(AssetError::DigestMismatch {
            member: ModelMember::Rec,
            expected_sha256,
            actual_sha256,
            expected_bytes,
            actual_bytes,
        }) => {
            assert_eq!(expected_sha256, REC_MODEL_SHA256);
            assert_eq!(actual_sha256, DICT_SHA256);
            assert_eq!(expected_bytes, 21_148_338);
            assert_eq!(actual_bytes, 74_947);
        }
        other => panic!("expected DigestMismatch for member rec, got {other:?}"),
    }
}

// 覆盖 SNAP-05：非成套字典（tiny 冒充）必须按字节校验拒绝。
#[test]
fn load_rejects_foreign_dict() {
    let Some(path) = std::env::var_os("XBERG_SNAPSHOT_TINY_DICT") else {
        println!("skip: XBERG_SNAPSHOT_TINY_DICT is not set (optional dict-mismatch probe)");
        return;
    };
    let Some(m) = require_models() else {
        return;
    };
    let error = SnapshotOcrModels::load(&m.det, &m.rec, Path::new(&path), REFERENCE_INTRA_THREADS)
        .expect_err("foreign dict must be rejected");
    assert!(
        matches!(
            error,
            SnapshotOcrError::Asset(AssetError::DigestMismatch {
                member: ModelMember::Dict,
                ..
            })
        ),
        "expected DigestMismatch for member dict, got {error:?}"
    );
}

// 覆盖 SNAP-03/SNAP-04/SNAP-07/O-23：钉定模型加载成功（含摘要校验、会话门、
// 空白探针预热），纯白图识别得到空文本与空记录。
#[test]
fn load_succeeds_and_blank_image_yields_no_text() {
    let Some(m) = require_models() else {
        return;
    };
    let models = SnapshotOcrModels::load(&m.det, &m.rec, &m.dict, REFERENCE_INTRA_THREADS)
        .expect("pinned model set must load");
    let cancel = AtomicBool::new(false);
    let output = models
        .recognize(&white_image(64, 64), 64, 64, &cancel)
        .expect("blank recognition must succeed");
    assert!(output.layout_text.is_empty());
    assert!(output.records.is_empty());
}

// 覆盖 SNAP-13/O-24/O-26/O-27/O-28：同一输入两次识别输出逐字节一致（确定
// 性），且记录四边形都在图像范围内。
#[test]
fn recognize_is_deterministic_on_synthetic_pattern() {
    let Some(m) = require_models() else {
        return;
    };
    let models = SnapshotOcrModels::load(&m.det, &m.rec, &m.dict, REFERENCE_INTRA_THREADS)
        .expect("pinned model set must load");
    let (width, height, image) = bar_pattern_image();
    let cancel = AtomicBool::new(false);
    let first = models
        .recognize(&image, width, height, &cancel)
        .expect("first recognition must succeed");
    let second = models
        .recognize(&image, width, height, &cancel)
        .expect("second recognition must succeed");
    assert_eq!(first, second, "same input must give byte-identical output");
    for record in &first.records {
        for &(x, y) in record.quad.iter() {
            assert!(
                x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0,
                "quad coordinate ({x}, {y}) out of domain"
            );
        }
    }
}

// 覆盖 SNAP-16/O-19：取消标志在首个瓦片检查点生效，返回可区分的 Cancelled。
#[test]
fn cancelled_flag_yields_cancelled() {
    let Some(m) = require_models() else {
        return;
    };
    let models = SnapshotOcrModels::load(&m.det, &m.rec, &m.dict, REFERENCE_INTRA_THREADS)
        .expect("pinned model set must load");
    let cancel = AtomicBool::new(true);
    let error = models
        .recognize(&white_image(64, 64), 64, 64, &cancel)
        .expect_err("pre-cancelled recognition must not succeed");
    assert!(
        matches!(error, SnapshotOcrError::Cancelled),
        "expected Cancelled, got {error:?}"
    );
}
