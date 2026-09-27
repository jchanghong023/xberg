//! Headless screenshot-OCR smoke/acceptance entry (PNG in, layout text out).
//!
//! Usage:
//! ```text
//! cargo run -p xberg-snapshot-ocr --example snapshot_ocr_cli -- \
//!     --det <det.onnx> --rec <rec.onnx> --dict <dict.txt> \
//!     --image <image.png> [--json] [--runs N]
//! ```
//!
//! Environment: `ORT_DYLIB_PATH` must point at the onnxruntime shared library
//! and is pinned BEFORE any session is built, so no search-path DLL can be
//! loaded first (anti-preemption). Model files are verified against the pinned
//! `snapshot-pp-ocrv6-small-textsnap` digests at load; mismatch aborts.
//!
//! Output: layout text on stdout (or a JSON report with `--json`), timings on
//! stderr. Repeated `--runs` must produce byte-identical text (determinism
//! contract, OCR-SNAPSHOT.md SNAP-13).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use xberg_snapshot_ocr::{SnapshotOcrModels, REFERENCE_INTRA_THREADS, SNAPSHOT_MODEL_SET};

#[derive(Default)]
struct Args {
    det: Option<PathBuf>,
    rec: Option<PathBuf>,
    dict: Option<PathBuf>,
    image: Option<PathBuf>,
    json: bool,
    runs: usize,
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next().ok_or_else(|| format!("{flag} expects a value"))
}

fn parse_args() -> Result<Args, String> {
    const USAGE: &str = "usage: snapshot_ocr_cli --det <det.onnx> --rec <rec.onnx> \
                         --dict <dict.txt> --image <image.png> [--json] [--runs N]";
    let mut args = Args {
        runs: 1,
        ..Args::default()
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--det" => args.det = Some(PathBuf::from(next_value(&mut iter, &arg)?)),
            "--rec" => args.rec = Some(PathBuf::from(next_value(&mut iter, &arg)?)),
            "--dict" => args.dict = Some(PathBuf::from(next_value(&mut iter, &arg)?)),
            "--image" => args.image = Some(PathBuf::from(next_value(&mut iter, &arg)?)),
            "--json" => args.json = true,
            "--runs" => {
                let value = next_value(&mut iter, &arg)?;
                let runs: usize = value
                    .parse()
                    .map_err(|_| format!("--runs expects a number, got {value}"))?;
                if runs == 0 {
                    return Err("--runs must be positive".to_string());
                }
                args.runs = runs;
            }
            other => return Err(format!("unknown argument {other}; {USAGE}")),
        }
    }
    if args.det.is_none() || args.rec.is_none() || args.dict.is_none() || args.image.is_none() {
        return Err(format!("missing required arguments; {USAGE}"));
    }
    Ok(args)
}

/// PNG (RGB storage) -> BGR row-major HxWx3.
fn load_bgr(path: &Path) -> Result<(u32, u32, Vec<u8>), Box<dyn Error>> {
    let rgb = image::open(path)?.to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut data = Vec::with_capacity(rgb.len());
    for pixel in rgb.pixels() {
        data.push(pixel[2]);
        data.push(pixel[1]);
        data.push(pixel[0]);
    }
    Ok((width, height, data))
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args().map_err(|message| -> Box<dyn Error> {
        eprintln!("{message}");
        message.into()
    })?;
    // Pin the onnxruntime library before any session build (anti-preemption).
    if std::env::var_os("ORT_DYLIB_PATH").is_none() {
        return Err("ORT_DYLIB_PATH is not set; point it at onnxruntime before loading"
            .into());
    }

    let image_path = args.image.clone().expect("checked in parse_args");
    let (width, height, bgr) = load_bgr(&image_path)?;
    println!(
        "image: {} ({}x{}), model set: {}",
        image_path.display(),
        width,
        height,
        SNAPSHOT_MODEL_SET
    );

    let load_started = Instant::now();
    let models = SnapshotOcrModels::load(
        args.det.as_ref().expect("checked in parse_args"),
        args.rec.as_ref().expect("checked in parse_args"),
        args.dict.as_ref().expect("checked in parse_args"),
        REFERENCE_INTRA_THREADS,
    )?;
    let load_seconds = load_started.elapsed().as_secs_f64();

    let cancel = AtomicBool::new(false);
    let mut recognize_seconds = Vec::with_capacity(args.runs);
    let mut output = None;
    for run in 0..args.runs {
        let started = Instant::now();
        let current = models.recognize(&bgr, width, height, &cancel)?;
        recognize_seconds.push(started.elapsed().as_secs_f64());
        if let Some(previous) = &output {
            if previous != &current {
                return Err(format!("run {run} diverged from run 0 (nondeterministic)").into());
            }
        }
        output = Some(current);
    }
    let output = output.expect("runs >= 1 guarantees at least one recognition");

    if args.json {
        let report = serde_json::json!({
            "model_set": SNAPSHOT_MODEL_SET,
            "image": args.image.expect("checked in parse_args"),
            "width": width,
            "height": height,
            "load_seconds": load_seconds,
            "recognize_seconds": recognize_seconds,
            "record_count": output.records.len(),
            "text": output.layout_text,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", output.layout_text);
        eprintln!(
            "load={load_seconds:.3}s recognize={:?}s records={}",
            recognize_seconds,
            output.records.len()
        );
    }
    Ok(())
}
