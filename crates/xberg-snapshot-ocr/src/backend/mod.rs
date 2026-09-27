//! Real-inference backend for the screenshot OCR channel.
//!
//! Submodules are bit-faithful ports of the JchTools snapshot OCR worker
//! (`optional/snap-ocr-worker/src`); only import paths were adjusted, all
//! logic, constants, ordering and comments are preserved verbatim:
//!
//! - [`det`]: DB text detection pre/post-processing (PaddleX 3.7.2
//!   `DetResizeForTest` type-0 resize, `NormalizeImage`, `DBPostProcess.
//!   boxes_from_bitmap`, OpenCV 4.10 contours/minAreaRect/fillPoly/pyclipper
//!   unclip semantics) plus the process-wide [`det::SESSION_BUILD_GATE`]
//!   serializing every ort session build (ort 2.0.0-rc.13 `load-dynamic`
//!   first-load is not thread-safe).
//! - [`rec`]: CRNN recognition chain (`OCRReisizeNormImg` 48-height batching,
//!   CTC decode over the `[blank] + 18708 dict entries + space` = 18710 class
//!   layout, SNAP-06).
//! - [`image_ops`]: BGR image container and OpenCV 4.10 operators
//!   (INTER_LINEAR/CUBIC/LANCZOS4 resize, warpPerspective CUBIC+REPLICATE,
//!   `numpy.rot90`).
//! - [`pipeline_backend`]: the [`crate::pipeline::OcrBackend`] implementation
//!   combining resident det/rec ort sessions.
//! - [`sha256`]: dependency-free SHA-256 (model pinning + crop digests).
//!
//! The vendored `xberg-paddle-ocr` crate in the JchTools tree is NOT used by
//! these modules (verified by import audit before porting); the Xberg-side
//! crate therefore depends only on `ort` (load-dynamic), `geo-clipper` and
//! `geo-types`.

pub mod det;
pub mod image_ops;
pub mod pipeline_backend;
pub mod rec;
pub mod sha256;
