//! Centralised image re-encoding helper.
//!
//! This module provides [`re_encode`], which converts an [`ExtractedImage`] in place
//! to a caller-selected target format.  It is designed to be called from the extraction
//! pipeline after OCR has run and before post-processors that consume `data` + `format`.

use std::borrow::Cow;
use std::io::Cursor;

use bytes::Bytes;
use image::{DynamicImage, ImageFormat};
use tracing::warn;

use crate::core::config::extraction::ImageOutputFormat;
#[cfg(feature = "svg")]
use crate::core::config::extraction::SvgOptions;
use crate::error::XbergError;
#[cfg(feature = "heic")]
use crate::extraction::image_decode::{
    ImageDecodeBudget, copy_decoded_rows, decoded_byte_count, image_dimension_error,
};
use crate::extraction::image_decode::{
    decode_standard_image_with_format_and_security_limits, decode_standard_image_with_security_limits,
    validate_dynamic_image_additional_live_bytes,
};
use crate::extractors::security::SecurityLimits;
use crate::types::ExtractedImage;

/// Describes why a re-encode attempt was skipped or failed.
///
/// The pipeline converts `Err(EncodeWarning)` into a `ProcessingWarning` and leaves
/// the image bytes untouched — the caller is never left with a partially-written image.
#[derive(Debug)]
pub(crate) enum EncodeWarning {
    /// The source format cannot be decoded by any available decoder (vector/metafile formats).
    Undecodable {
        /// Format string of the source image (e.g. `"svg"`, `"emf"`).
        source_format: String,
    },
    /// The source bytes failed to decode despite the format being nominally supported.
    DecodeFailed {
        /// Format string that was attempted.
        source_format: String,
        /// Underlying error message from the decoder.
        message: String,
    },
    /// The decoded image could not be encoded to the target format.
    EncodeFailed {
        /// Name of the target format (e.g. `"jpeg"`, `"webp"`).
        target_format: &'static str,
        /// Underlying error message from the encoder.
        message: String,
    },
    /// The encoder for the target format is not available at runtime.
    #[cfg(feature = "heic")]
    EncoderUnavailable {
        /// Name of the target format.
        target_format: &'static str,
        /// Details about why the encoder is unavailable.
        message: String,
    },
    /// The requested conversion direction is not supported (e.g. raster → SVG).
    ///
    /// The image bytes are left untouched.  This is a non-fatal warning: the
    /// pipeline emits the warning and continues with the original image.
    ///
    /// Gated on `svg`: raster→SVG is the only direction the pipeline rejects with
    /// this variant, and that branch only exists when the `svg` feature is active.
    /// Without the gate, Windows/mobile aggregates that omit `svg` trip
    /// `-D dead-code` on the unconstructed variant.
    #[cfg(feature = "svg")]
    UnsupportedDirection {
        /// Format of the source image (e.g. `"jpeg"`, `"png"`).
        from_format: String,
        /// Name of the requested target format (e.g. `"svg"`).
        to_format: &'static str,
    },
}

impl std::fmt::Display for EncodeWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeWarning::Undecodable { source_format } => {
                write!(f, "cannot re-encode format '{source_format}': no decoder available")
            }
            EncodeWarning::DecodeFailed { source_format, message } => {
                write!(f, "failed to decode '{source_format}' image: {message}")
            }
            EncodeWarning::EncodeFailed { target_format, message } => {
                write!(f, "failed to encode image as {target_format}: {message}")
            }
            #[cfg(feature = "heic")]
            EncodeWarning::EncoderUnavailable { target_format, message } => {
                write!(f, "encoder for {target_format} is unavailable: {message}")
            }
            #[cfg(feature = "svg")]
            EncodeWarning::UnsupportedDirection { from_format, to_format } => {
                write!(
                    f,
                    "cannot re-encode '{from_format}' to '{to_format}': direction not supported \
                     (raster sources cannot be vectorized)"
                )
            }
        }
    }
}

/// Re-encode `image` in place to the requested `target` format.
///
/// Returns `Ok(true)` when the image was re-encoded and both `data` and `format`
/// have been updated.  Returns `Ok(false)` when no work was necessary (either the
/// target is [`ImageOutputFormat::Native`] or the image is already in the target
/// format).  On `Err(EncodeWarning)` the image is left completely untouched.
pub(crate) fn re_encode(
    image: &mut ExtractedImage,
    target: ImageOutputFormat,
    limits: &SecurityLimits,
    image_config: &crate::core::config::extraction::ImageExtractionConfig,
    #[cfg(feature = "svg")] svg_options: &SvgOptions,
) -> Result<bool, EncodeWarning> {
    if target == ImageOutputFormat::Native {
        #[cfg(feature = "svg")]
        if svg_options.sanitize && image.format.eq_ignore_ascii_case("svg") {
            let sanitized = sanitize_svg(&image.data)?;
            image.data = Bytes::from(sanitized.into_bytes());
            return Ok(true);
        }
        // Windows metafiles convert to PNG even under `Native`: no Markdown
        // preview renders an `.emf`/`.wmf` reference (the reason the GDI
        // rasterizer exists), so "keep the native bytes" would only leave a
        // dead reference on disk. Every other format keeps its bytes.
        if is_windows_metafile(image) {
            return re_encode_metafile(image, ImageOutputFormat::Png, image_config, limits);
        }
        return Ok(false);
    }

    if target_matches_format(target, &image.format) {
        return Ok(false);
    }

    #[cfg(feature = "svg")]
    if image.format.eq_ignore_ascii_case("svg") {
        if let ImageOutputFormat::Svg = target {
            let sanitized = sanitize_svg(&image.data)?;
            image.data = Bytes::from(sanitized.into_bytes());
            image.format = Cow::Borrowed("svg");
            return Ok(true);
        }
        let (new_bytes, new_format) = rasterize_svg(&image.data, target, svg_options)?;
        image.data = Bytes::from(new_bytes);
        image.format = Cow::Borrowed(new_format);
        return Ok(true);
    }

    #[cfg(feature = "svg")]
    if let ImageOutputFormat::Svg = target {
        return Err(EncodeWarning::UnsupportedDirection {
            from_format: image.format.to_string(),
            to_format: "svg",
        });
    }

    // Windows metafiles (EMF/WMF) have no standard decoder in the image crate, but the
    // GDI rasterizer can turn them into pixels. Prefer that over leaving `.emf` refs in
    // Markdown previews that cannot render them.
    if is_windows_metafile(image) {
        return re_encode_metafile(image, target, image_config, limits);
    }

    if is_untranslatable(&image.format) {
        return Err(EncodeWarning::Undecodable {
            source_format: image.format.to_string(),
        });
    }

    let dynamic = decode_source(image, limits)?;
    validate_reencode_peak(&dynamic, target, image.data.len(), limits)?;

    let (new_bytes, new_format) = encode_to_target(&dynamic, target)?;

    image.data = Bytes::from(new_bytes);
    image.format = Cow::Borrowed(new_format);

    Ok(true)
}

/// Re-encode every image in `images` to `target` — the blocking core of the pipeline's
/// image-format pass.
///
/// Shared by the sync pipeline (which runs it inline on its caller's thread) and the async
/// pipeline (which runs it inside `tokio::task::spawn_blocking`, see
/// `core::pipeline::apply_output_format_pass_offload`): decoding, the GDI rasterization a
/// Windows metafile goes through, and encoding are CPU/Win32-bound work that must stay off
/// async runtime workers.
///
/// Returns the re-encoded images, the `(image_index, old format, new format)` renames for
/// callers that bake `image_N.ext` references into pre-rendered content (only entries whose
/// format actually changed, so a sibling whose re-encode failed keeps its old extension on
/// disk and in the references), and one `ProcessingWarning` per failed image, in image order.
///
/// The rename key is [`ExtractedImage::image_index`] — the number the renderers bake into
/// `image_N.ext` and the CLI names the written file by — *not* the vector position: staging
/// can drop unreferenced images, leaving the positions dense while the field has gaps, and a
/// position-keyed rename then missed its reference or collided with another image's number.
/// `re_encode` never touches the field, so recording it after the call is exact.
/// The staging triple [`re_encode_images`] returns: the re-encoded images, the
/// `image_N` extension renames the content's references must follow, and the
/// warnings collected along the way.
pub(crate) type ReencodedImages = (
    Vec<ExtractedImage>,
    Vec<(u32, String, String)>,
    Vec<crate::types::ProcessingWarning>,
);

pub(crate) fn re_encode_images(
    mut images: Vec<ExtractedImage>,
    target: ImageOutputFormat,
    limits: &SecurityLimits,
    image_config: &crate::core::config::extraction::ImageExtractionConfig,
) -> ReencodedImages {
    let mut format_renames: Vec<(u32, String, String)> = Vec::new();
    let mut warnings = Vec::new();
    for image in images.iter_mut() {
        let previous_format = image.format.to_string();
        match re_encode(
            image,
            target,
            limits,
            image_config,
            #[cfg(feature = "svg")]
            &image_config.svg,
        ) {
            Ok(true) => {
                let next_format = image.format.to_string();
                if !previous_format.eq_ignore_ascii_case(&next_format) {
                    format_renames.push((image.image_index, previous_format, next_format));
                }
            }
            Ok(false) => {}
            Err(warning) => warnings.push(crate::types::ProcessingWarning {
                source: Cow::Borrowed("image_encoder"),
                message: Cow::Owned(warning.to_string()),
            }),
        }
    }
    (images, format_renames, warnings)
}

const ENCODE_FIXED_OVERHEAD_BYTES: u64 = 256 * 1024;
const PNG_WEBP_ENCODE_BYTES_PER_PIXEL: u64 = 4;
const JPEG_ENCODE_BYTES_PER_PIXEL: u64 = 3;
const HEIF_ENCODE_LIVE_BYTES_PER_PIXEL: u64 = 12;

fn validate_reencode_peak(
    image: &DynamicImage,
    target: ImageOutputFormat,
    encoded_source_bytes: usize,
    limits: &SecurityLimits,
) -> Result<(), EncodeWarning> {
    let additional_bytes_per_pixel = match target {
        ImageOutputFormat::Native => 0,
        ImageOutputFormat::Png | ImageOutputFormat::Webp { .. } => PNG_WEBP_ENCODE_BYTES_PER_PIXEL,
        ImageOutputFormat::Jpeg { .. } => JPEG_ENCODE_BYTES_PER_PIXEL,
        ImageOutputFormat::Heif { .. } => HEIF_ENCODE_LIVE_BYTES_PER_PIXEL,
        #[cfg(feature = "svg")]
        ImageOutputFormat::Svg => 0,
    };
    let encoded_source_bytes = u64::try_from(encoded_source_bytes).unwrap_or(u64::MAX);
    let fixed_live_bytes = ENCODE_FIXED_OVERHEAD_BYTES.saturating_add(encoded_source_bytes);
    validate_dynamic_image_additional_live_bytes(image, limits, additional_bytes_per_pixel, fixed_live_bytes).map_err(
        |error| EncodeWarning::EncodeFailed {
            target_format: "raster",
            message: error.to_string(),
        },
    )
}

/// Returns `true` when `target` already matches the source `format` string,
/// meaning no re-encode is needed.
fn target_matches_format(target: ImageOutputFormat, format: &str) -> bool {
    match target {
        ImageOutputFormat::Native => true,
        ImageOutputFormat::Png => format.eq_ignore_ascii_case("png"),
        ImageOutputFormat::Jpeg { .. } => format.eq_ignore_ascii_case("jpeg") || format.eq_ignore_ascii_case("jpg"),
        ImageOutputFormat::Webp { .. } => format.eq_ignore_ascii_case("webp"),
        #[cfg(feature = "heic")]
        ImageOutputFormat::Heif { .. } => {
            format.eq_ignore_ascii_case("heif")
                || format.eq_ignore_ascii_case("heic")
                || format.eq_ignore_ascii_case("HEIF")
                || format.eq_ignore_ascii_case("HEIC")
        }
        #[cfg(not(feature = "heic"))]
        ImageOutputFormat::Heif { .. } => false,
        #[cfg(feature = "svg")]
        ImageOutputFormat::Svg => false,
    }
}

/// Returns `true` for formats that cannot be decoded by any available decoder.
///
/// These are vector / Windows-metafile formats for which no raster pixel data
/// is accessible.  Returning `Err(EncodeWarning::Undecodable)` signals the
/// pipeline to skip re-encoding and emit a warning instead.
///
/// When the `svg` feature is active, SVG is handled separately (via `sanitize_svg` /
/// `rasterize_svg`) and is therefore **not** listed here.
///
/// EMF/WMF are still listed here so non-Windows builds (and failed GDI paths)
/// report Undecodable; Windows builds intercept them earlier via
/// [`re_encode_metafile`].
fn is_untranslatable(format: &str) -> bool {
    let lc = format.to_ascii_lowercase();
    let s = lc.as_str();
    #[cfg(not(feature = "svg"))]
    {
        matches!(s, "svg" | "emf" | "wmf" | "jpeg2000" | "jp2" | "j2k")
    }
    #[cfg(feature = "svg")]
    {
        matches!(s, "emf" | "wmf" | "jpeg2000" | "jp2" | "j2k")
    }
}

/// Whether a declared format string names a Windows metafile (EMF/WMF).
pub(crate) fn is_metafile_format(format: &str) -> bool {
    format.eq_ignore_ascii_case("emf") || format.eq_ignore_ascii_case("wmf")
}

/// Whether the image is a Windows metafile (EMF/WMF) by declared format string.
///
/// Office extractors set `format` from magic bytes; relying on that string keeps
/// this path free of the `office`-gated format detector.
fn is_windows_metafile(image: &ExtractedImage) -> bool {
    is_metafile_format(&image.format)
}

/// Rasterize EMF/WMF to pixels via the Windows GDI path, then encode to `target`.
///
/// Requires the same features as the shared metafile rasterizer (`ocr` +
/// `tokio-runtime`), which is where the GDI bridge lives. Builds without those
/// features leave metafiles untouched (Undecodable), matching pre-rasterize behaviour.
fn re_encode_metafile(
    image: &mut ExtractedImage,
    target: ImageOutputFormat,
    image_config: &crate::core::config::extraction::ImageExtractionConfig,
    limits: &SecurityLimits,
) -> Result<bool, EncodeWarning> {
    #[cfg(all(windows, feature = "ocr", feature = "tokio-runtime"))]
    {
        let source_format = image.format.to_string();
        let dynamic = crate::extraction::image_ocr::rasterize_metafile_to_dynamic_image(image, image_config, limits)
            .map_err(|error| EncodeWarning::DecodeFailed {
                source_format,
                message: error.to_string(),
            })?;
        validate_reencode_peak(&dynamic, target, image.data.len(), limits)?;
        let (new_bytes, new_format) = encode_to_target(&dynamic, target)?;
        image.data = Bytes::from(new_bytes);
        image.format = Cow::Borrowed(new_format);
        Ok(true)
    }

    #[cfg(not(all(windows, feature = "ocr", feature = "tokio-runtime")))]
    {
        let _ = (image_config, target, limits);
        Err(EncodeWarning::Undecodable {
            source_format: image.format.to_string(),
        })
    }
}

/// Parse SVG bytes through `usvg` and re-serialize, stripping external hrefs,
/// JS event handlers, and `foreignObject` elements that `usvg` does not model.
///
/// Minimum allowed `render_dpi` accepted before rasterization.  Values below
/// this are clamped up to avoid degenerate scaling and division-by-zero shapes.
#[cfg(feature = "svg")]
const SVG_RENDER_DPI_MIN: f32 = 1.0;

/// Maximum allowed `render_dpi` accepted before rasterization.  Caps the
/// blast radius of adversarial config combined with a large viewBox.  600 DPI
/// covers print-quality usage; anything beyond is rarely a legitimate need.
#[cfg(feature = "svg")]
const SVG_RENDER_DPI_MAX: f32 = 600.0;

/// Maximum number of output pixels permitted in the rasterized pixmap.
/// `tiny_skia::Pixmap` allocates 4 bytes per pixel (RGBA), so this corresponds
/// to roughly a 1 GB peak allocation — the upper bound at which we'd rather
/// fail loudly than let an adversarial SVG OOM the process.
#[cfg(feature = "svg")]
const SVG_MAX_PIXELS: u64 = 16_384 * 16_384;

/// Maximum input byte length accepted for SVG parse.  usvg expands the source
/// into an in-memory tree synchronously; a small zip-bomb-shape SVG (lots of
/// `<use>` references, gradient stops, or nested `<g>` elements) can blow up
/// CPU and memory before pixmap allocation is even considered.  10 MB is well
/// above any realistic embedded-document SVG.
#[cfg(feature = "svg")]
const SVG_MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;

/// Returns the sanitized SVG as a `String`.  On parse failure returns
/// `Err(EncodeWarning::DecodeFailed)`.
#[cfg(feature = "svg")]
fn sanitize_svg(data: &[u8]) -> Result<String, EncodeWarning> {
    use resvg::usvg;

    if data.len() > SVG_MAX_INPUT_BYTES {
        return Err(EncodeWarning::DecodeFailed {
            source_format: "svg".into(),
            message: format!("SVG input size {} exceeds {SVG_MAX_INPUT_BYTES}-byte cap", data.len()),
        });
    }

    let opts = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };

    let tree = usvg::Tree::from_data(data, &opts).map_err(|e| EncodeWarning::DecodeFailed {
        source_format: "svg".to_string(),
        message: e.to_string(),
    })?;

    Ok(tree.to_string(&usvg::WriteOptions::default()))
}

/// Rasterize SVG bytes to a pixel-based format (PNG, JPEG, WebP, HEIF).
///
/// The SVG viewBox is scaled by `svg_options.render_dpi / 96.0` to produce the
/// output pixel dimensions.  The resulting pixel buffer is then handed to the
/// existing raster encode path.
///
/// Returns the encoded bytes and the canonical format name string on success.
#[cfg(feature = "svg")]
fn rasterize_svg(
    data: &[u8],
    target: ImageOutputFormat,
    svg_options: &SvgOptions,
) -> Result<(Vec<u8>, &'static str), EncodeWarning> {
    use resvg::{tiny_skia, usvg};

    if data.len() > SVG_MAX_INPUT_BYTES {
        return Err(EncodeWarning::DecodeFailed {
            source_format: "svg".into(),
            message: format!("SVG input size {} exceeds {SVG_MAX_INPUT_BYTES}-byte cap", data.len()),
        });
    }

    let opts = usvg::Options {
        resources_dir: None,
        dpi: svg_options.render_dpi,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };

    let tree = usvg::Tree::from_data(data, &opts).map_err(|e| EncodeWarning::DecodeFailed {
        source_format: "svg".to_string(),
        message: e.to_string(),
    })?;

    let dpi = svg_options.render_dpi.clamp(SVG_RENDER_DPI_MIN, SVG_RENDER_DPI_MAX);
    let scale = dpi / 96.0;
    let svg_size = tree.size();
    let width = ((svg_size.width() * scale) as u32).max(1);
    let height = ((svg_size.height() * scale) as u32).max(1);

    let pixel_count = u64::from(width) * u64::from(height);
    if pixel_count > SVG_MAX_PIXELS {
        return Err(EncodeWarning::EncodeFailed {
            target_format: "raster",
            message: format!("SVG render dimensions {width}×{height} exceed {SVG_MAX_PIXELS}-pixel cap"),
        });
    }

    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or_else(|| EncodeWarning::EncodeFailed {
        target_format: "raster",
        message: format!("cannot allocate {width}×{height} pixmap for SVG rasterization"),
    })?;

    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());

    let rgba_bytes = pixmap.take_demultiplied();
    let rgba_img =
        image::RgbaImage::from_raw(width, height, rgba_bytes).ok_or_else(|| EncodeWarning::EncodeFailed {
            target_format: "raster",
            message: format!("RGBA buffer size mismatch for {width}×{height} SVG rasterization"),
        })?;
    let dynamic = DynamicImage::ImageRgba8(rgba_img);

    encode_to_target(&dynamic, target)
}

/// Decode the source bytes inside `image` to a [`DynamicImage`].
///
/// The dispatch order is:
/// 1. Known format strings → the format-specific `image` decoder
/// 2. `"heic"` / `"heif"` / `"HEIC"` / `"HEIF"` → `xberg-libheif` (feature `heic`)
/// 3. `"unknown"` or anything else → magic-byte auto-detect
fn decode_source(image: &ExtractedImage, limits: &SecurityLimits) -> Result<DynamicImage, EncodeWarning> {
    let format_lc = image.format.to_ascii_lowercase();

    #[cfg(feature = "heic")]
    if matches!(format_lc.as_str(), "heic" | "heif") {
        return decode_heic(&image.data, &format_lc, limits);
    }

    #[cfg(not(feature = "heic"))]
    if matches!(format_lc.as_str(), "heic" | "heif") {
        return Err(EncodeWarning::Undecodable {
            source_format: image.format.to_string(),
        });
    }

    let maybe_fmt: Option<ImageFormat> = match format_lc.as_str() {
        "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        "pnm" | "pbm" | "pgm" | "ppm" => Some(ImageFormat::Pnm),
        _ => None,
    };

    match maybe_fmt {
        Some(format) => {
            decode_standard_image_with_format_and_security_limits(&image.data, format, limits).map_err(|error| {
                EncodeWarning::DecodeFailed {
                    source_format: image.format.to_string(),
                    message: error.to_string(),
                }
            })
        }
        None => decode_standard_image_with_security_limits(&image.data, limits).map_err(|error| match error {
            XbergError::Validation { .. } => EncodeWarning::DecodeFailed {
                source_format: image.format.to_string(),
                message: error.to_string(),
            },
            _ => EncodeWarning::Undecodable {
                source_format: image.format.to_string(),
            },
        }),
    }
}

#[cfg(feature = "heic")]
const HEIF_DECODE_BUFFER_COUNT: u64 = 2;

#[cfg(feature = "heic")]
fn validate_heic_decode_budget(
    width: u32,
    height: u32,
    source_format: &str,
    encoded_source_bytes: usize,
    limits: &SecurityLimits,
) -> Result<(), EncodeWarning> {
    let rgba_bytes = decoded_byte_count(width, height, u64::from(image::ColorType::Rgba8.bytes_per_pixel()))
        .and_then(|bytes| {
            bytes
                .checked_mul(HEIF_DECODE_BUFFER_COUNT)
                .and_then(|peak| peak.checked_add(u64::try_from(encoded_source_bytes).unwrap_or(u64::MAX)))
                .ok_or_else(|| image_dimension_error(width, height, u64::MAX, u64::MAX))
        })
        .and_then(|peak| ImageDecodeBudget::from_security_limits(limits).validate(width, height, peak));
    rgba_bytes.map_err(|error| EncodeWarning::DecodeFailed {
        source_format: source_format.to_string(),
        message: error.to_string(),
    })
}

#[cfg(feature = "heic")]
fn validate_heic_encoded_input_budget(
    encoded_source_bytes: usize,
    source_format: &str,
    limits: &SecurityLimits,
) -> Result<(), EncodeWarning> {
    ImageDecodeBudget::from_security_limits(limits)
        .validate(1, 1, u64::try_from(encoded_source_bytes).unwrap_or(u64::MAX))
        .map_err(|error| EncodeWarning::DecodeFailed {
            source_format: source_format.to_string(),
            message: error.to_string(),
        })
}

/// Decode a HEIC/HEIF image via `xberg-libheif` into a [`DynamicImage`].
///
/// The decoded output is always RGBA8 so that the subsequent encode step has a
/// uniform input regardless of the source chroma.
#[cfg(feature = "heic")]
fn decode_heic(data: &[u8], source_format: &str, limits: &SecurityLimits) -> Result<DynamicImage, EncodeWarning> {
    use xberg_libheif::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    validate_heic_encoded_input_budget(data.len(), source_format, limits)?;
    let context = HeifContext::read_from_bytes(data).map_err(|err| EncodeWarning::DecodeFailed {
        source_format: source_format.to_string(),
        message: format!("{err:?}"),
    })?;

    let handle = context
        .primary_image_handle()
        .map_err(|err| EncodeWarning::DecodeFailed {
            source_format: source_format.to_string(),
            message: format!("{err:?}"),
        })?;

    let width = handle.width();
    let height = handle.height();
    validate_heic_decode_budget(width, height, source_format, data.len(), limits)?;

    let lib = LibHeif::new();
    let heif_img = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .map_err(|err| EncodeWarning::DecodeFailed {
            source_format: source_format.to_string(),
            message: format!("{err:?}"),
        })?;

    let planes = heif_img.planes();
    let plane = planes.interleaved.as_ref().ok_or_else(|| EncodeWarning::DecodeFailed {
        source_format: source_format.to_string(),
        message: "HEIF image has no interleaved plane".to_string(),
    })?;

    let decoded_width = heif_img.width();
    let decoded_height = heif_img.height();
    if decoded_width != width || decoded_height != height {
        return Err(EncodeWarning::DecodeFailed {
            source_format: source_format.to_string(),
            message: format!(
                "HEIF decoded dimensions {decoded_width}x{decoded_height} do not match declared dimensions {width}x{height}"
            ),
        });
    }

    let rgba_bytes = copy_decoded_rows(
        plane.data,
        plane.stride,
        width,
        height,
        u64::from(image::ColorType::Rgba8.bytes_per_pixel()),
    )
    .map_err(|error| EncodeWarning::DecodeFailed {
        source_format: source_format.to_string(),
        message: error.to_string(),
    })?;

    let rgba_img =
        image::RgbaImage::from_raw(width, height, rgba_bytes).ok_or_else(|| EncodeWarning::DecodeFailed {
            source_format: source_format.to_string(),
            message: format!("RGBA buffer does not fit {width}×{height} image"),
        })?;

    Ok(DynamicImage::ImageRgba8(rgba_img))
}

/// Encode `img` into `target` format and return the raw bytes plus the canonical
/// format name string.
///
/// Returns `Err(EncodeWarning)` if the encode step fails.
fn encode_to_target(img: &DynamicImage, target: ImageOutputFormat) -> Result<(Vec<u8>, &'static str), EncodeWarning> {
    match target {
        ImageOutputFormat::Native => {
            unreachable!("Native target must be handled before encode dispatch")
        }
        ImageOutputFormat::Png => {
            let bytes = encode_png(img)?;
            Ok((bytes, "png"))
        }
        ImageOutputFormat::Jpeg { quality } => {
            let clamped = clamp_quality(quality, "jpeg");
            let bytes = encode_jpeg(img, clamped)?;
            Ok((bytes, "jpeg"))
        }
        ImageOutputFormat::Webp { quality: _ } => {
            let bytes = encode_webp_lossless(img)?;
            Ok((bytes, "webp"))
        }
        #[cfg(feature = "heic")]
        ImageOutputFormat::Heif { quality } => {
            let clamped = clamp_quality(quality, "heif");
            let bytes = encode_heif(img, clamped)?;
            Ok((bytes, "heif"))
        }
        #[cfg(not(feature = "heic"))]
        ImageOutputFormat::Heif { quality: _ } => Err(EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: "heic feature is not enabled in this build".to_string(),
        }),
        #[cfg(feature = "svg")]
        ImageOutputFormat::Svg => {
            unreachable!("raster → SVG must be rejected before reaching encode_to_target")
        }
    }
}

/// Clamp a quality value to `1..=100` and emit a warning when clamping occurs.
fn clamp_quality(quality: u8, format_name: &'static str) -> u8 {
    if quality == 0 {
        warn!(
            target: "xberg::image_encode",
            quality,
            format = format_name,
            "quality 0 is out of range (1–100); clamped to 1"
        );
        return 1;
    }
    if quality > 100 {
        warn!(
            target: "xberg::image_encode",
            quality,
            format = format_name,
            "quality {quality} is out of range (1–100); clamped to 100"
        );
        return 100;
    }
    quality
}

/// Encode `img` as PNG (lossless).
fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, EncodeWarning> {
    let mut buf: Vec<u8> = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "png",
            message: err.to_string(),
        })?;
    Ok(buf)
}

/// Encode `img` as JPEG at the given quality (1–100).
fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, EncodeWarning> {
    use image::codecs::jpeg::JpegEncoder;
    let mut buf: Vec<u8> = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
    encoder.encode_image(img).map_err(|err| EncodeWarning::EncodeFailed {
        target_format: "jpeg",
        message: err.to_string(),
    })?;
    Ok(buf)
}

/// Encode `img` as lossless WebP using the `image` crate's built-in VP8L encoder.
///
/// The `quality` field from [`ImageOutputFormat::Webp`] is intentionally ignored:
/// `image` 0.25 exposes only lossless WebP (VP8L) via `WebPEncoder::new_lossless`.
/// Lossy encoding would require the `webp` crate (libwebp FFI) or a future `image`
/// release that exposes a quality knob on its VP8 encode path.
fn encode_webp_lossless(img: &DynamicImage) -> Result<Vec<u8>, EncodeWarning> {
    let mut buf: Vec<u8> = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::WebP)
        .map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "webp",
            message: err.to_string(),
        })?;
    Ok(buf)
}

/// Encode `img` as HEIF/HEVC using `xberg-libheif`.
///
/// The pixel data is first converted to RGBA8 (via `DynamicImage::to_rgba8`)
/// and then written into a libheif interleaved plane before encoding.
#[cfg(feature = "heic")]
fn encode_heif(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, EncodeWarning> {
    use xberg_libheif::{
        Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, LibHeif, RgbChroma,
    };

    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    let mut context = HeifContext::new().map_err(|err| EncodeWarning::EncoderUnavailable {
        target_format: "heif",
        message: format!("HeifContext::new failed: {err:?}"),
    })?;

    let mut heif_img =
        Image::new(width, height, ColorSpace::Rgb(RgbChroma::Rgba)).map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: format!("Image::new failed: {err:?}"),
        })?;

    heif_img
        .create_plane(Channel::Interleaved, width, height, 8)
        .map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: format!("create_plane failed: {err:?}"),
        })?;

    {
        let mut planes = heif_img.planes_mut();
        let plane = planes.interleaved.as_mut().ok_or(EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: "interleaved plane missing after create_plane".to_string(),
        })?;
        let row_size = (width as usize) * 4;
        for (dst_row, src_row) in plane.data.chunks_mut(plane.stride).zip(rgba.chunks_exact(row_size)) {
            dst_row[..row_size].copy_from_slice(src_row);
        }
    }

    let lib = LibHeif::new();
    let mut encoder =
        lib.encoder_for_format(CompressionFormat::Hevc)
            .map_err(|err| EncodeWarning::EncoderUnavailable {
                target_format: "heif",
                message: format!("no HEVC encoder available: {err:?}"),
            })?;

    encoder
        .set_quality(EncoderQuality::Lossy(quality))
        .map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: format!("set_quality failed: {err:?}"),
        })?;

    context
        .encode_image(&heif_img, &mut encoder, None)
        .map_err(|err| EncodeWarning::EncodeFailed {
            target_format: "heif",
            message: format!("encode_image failed: {err:?}"),
        })?;

    context.write_to_bytes().map_err(|err| EncodeWarning::EncodeFailed {
        target_format: "heif",
        message: format!("write_to_bytes failed: {err:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thin wrapper around `re_encode` that supplies default `SvgOptions` when the
    /// `svg` feature is active, so callers in this test module need not repeat the
    /// cfg-gated argument at every call site.
    fn re_encode_default(image: &mut ExtractedImage, target: ImageOutputFormat) -> Result<bool, EncodeWarning> {
        re_encode(
            image,
            target,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            #[cfg(feature = "svg")]
            &SvgOptions::default(),
        )
    }

    #[test]
    fn reencode_honors_request_security_limit() {
        let original = make_png_bytes();
        let mut image = make_image(original.clone(), "png");
        let limits = SecurityLimits {
            max_content_size: 1_024,
            ..Default::default()
        };

        let result = re_encode(
            &mut image,
            ImageOutputFormat::Jpeg { quality: 85 },
            &limits,
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            #[cfg(feature = "svg")]
            &SvgOptions::default(),
        );

        assert!(matches!(result, Err(EncodeWarning::EncodeFailed { .. })));
        assert_eq!(image.data, original, "a rejected re-encode must preserve source bytes");
    }

    /// Create a minimal valid 4×4 PNG image as `Bytes` for use in tests.
    fn make_png_bytes() -> Bytes {
        let img = image::RgbImage::new(4, 4);
        let mut buf: Vec<u8> = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .expect("test PNG encode");
        Bytes::from(buf)
    }

    /// Create a minimal valid 4×4 JPEG image as `Bytes` for use in tests.
    fn make_jpeg_bytes() -> Bytes {
        use image::codecs::jpeg::JpegEncoder;
        let img = image::RgbImage::new(4, 4);
        let mut buf: Vec<u8> = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, 85)
            .encode_image(&DynamicImage::ImageRgb8(img))
            .expect("test JPEG encode");
        Bytes::from(buf)
    }

    /// Create a minimal `ExtractedImage` from raw bytes and a format string.
    fn make_image(data: Bytes, format: &'static str) -> ExtractedImage {
        ExtractedImage {
            data,
            format: Cow::Borrowed(format),
            ..Default::default()
        }
    }

    #[test]
    fn native_target_no_op() {
        let original_data = make_png_bytes();
        let mut image = make_image(original_data.clone(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Native);
        assert!(matches!(result, Ok(false)), "Native must return Ok(false)");
        assert_eq!(image.data, original_data, "bytes must be untouched");
        assert_eq!(image.format.as_ref(), "png", "format must be untouched");
    }

    /// `Native` must not swallow metafiles: an `.emf` declared image routes to the
    /// GDI rasterizer even without a configured `output_format`, because no
    /// Markdown preview renders an `.emf` reference. The bytes here are not a
    /// playable metafile, so the rasterizer reports a decode failure — the point
    /// under test is that the call reaches it instead of returning `Ok(false)`.
    #[test]
    fn native_target_still_routes_metafiles_to_the_rasterizer() {
        let mut image = make_image(Bytes::from_static(&[0x01, 0x00, 0x00, 0x00]), "emf");
        let result = re_encode_default(&mut image, ImageOutputFormat::Native);
        assert!(
            matches!(
                result,
                Err(EncodeWarning::DecodeFailed { .. }) | Err(EncodeWarning::Undecodable { .. })
            ),
            "Native must route metafiles to the rasterizer; got {result:?}"
        );
        assert_eq!(
            image.format.as_ref(),
            "emf",
            "a failed rasterize leaves the entry untouched"
        );
    }

    #[test]
    fn same_format_no_op() {
        let original_data = make_png_bytes();
        let mut image = make_image(original_data.clone(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Png);
        assert!(matches!(result, Ok(false)), "already-PNG → Png must return Ok(false)");
        assert_eq!(image.data, original_data, "bytes must be untouched");
    }

    #[test]
    fn png_to_jpeg() {
        let mut image = make_image(make_png_bytes(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Jpeg { quality: 85 });
        assert!(
            matches!(result, Ok(true)),
            "png→jpeg must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "jpeg");
        let guessed = image::guess_format(&image.data).expect("should detect valid JPEG");
        assert_eq!(guessed, ImageFormat::Jpeg);
    }

    #[test]
    fn jpeg_to_png() {
        let mut image = make_image(make_jpeg_bytes(), "jpeg");
        let result = re_encode_default(&mut image, ImageOutputFormat::Png);
        assert!(
            matches!(result, Ok(true)),
            "jpeg→png must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "png");
        let guessed = image::guess_format(&image.data).expect("should detect valid PNG");
        assert_eq!(guessed, ImageFormat::Png);
    }

    #[test]
    fn png_to_webp() {
        let mut image = make_image(make_png_bytes(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Webp { quality: 80 });
        assert!(
            matches!(result, Ok(true)),
            "png→webp must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "webp");
        let guessed = image::guess_format(&image.data).expect("should detect valid WebP");
        assert_eq!(guessed, ImageFormat::WebP);
    }

    /// Without `svg` feature: SVG is untranslatable → `Err(Undecodable)`.
    /// With `svg` feature: SVG → PNG rasterizes successfully → `Ok(true)`.
    #[test]
    fn svg_to_png_behaviour() {
        let svg_bytes = Bytes::from_static(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>");
        let original_data = svg_bytes.clone();
        let mut image = make_image(svg_bytes, "svg");
        let result = re_encode_default(&mut image, ImageOutputFormat::Png);
        #[cfg(not(feature = "svg"))]
        assert!(
            matches!(result, Err(EncodeWarning::Undecodable { ref source_format }) if source_format == "svg"),
            "svg (no svg feature) must return Err(Undecodable); got {result:?}",
        );
        #[cfg(not(feature = "svg"))]
        {
            assert_eq!(image.data, original_data);
            assert_eq!(image.format.as_ref(), "svg");
        }
        #[cfg(feature = "svg")]
        assert!(
            matches!(result, Ok(true)),
            "svg→png (svg feature) must return Ok(true); got {result:?}",
        );
        #[cfg(feature = "svg")]
        {
            assert_eq!(image.format.as_ref(), "png");
            let _ = original_data;
        }
    }

    #[test]
    fn corrupt_png_decode_fails() {
        let corrupt = Bytes::from_static(b"\x89PNG\r\n\x1a\ncorrupt garbage bytes here");
        let original_data = corrupt.clone();
        let mut image = make_image(corrupt, "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Jpeg { quality: 85 });
        assert!(
            matches!(result, Err(EncodeWarning::DecodeFailed { ref source_format, .. }) if source_format == "png"),
            "corrupt PNG must return Err(DecodeFailed); got {result:?}",
        );
        assert_eq!(image.data, original_data, "bytes must be untouched on decode failure");
    }

    #[test]
    fn should_reject_oversized_declared_dimensions_before_reencoding() {
        let oversized = crate::extraction::image_decode::bmp_with_declared_dimensions(6_000, 6_000);
        let mut image = make_image(Bytes::from(oversized), "bmp");

        let result = re_encode_default(&mut image, ImageOutputFormat::Png);

        assert!(
            matches!(result, Err(EncodeWarning::DecodeFailed { ref message, .. }) if message.contains("security_limits.max_content_size")),
            "oversized image must fail at the decoded-image budget; got {result:?}"
        );
    }

    #[test]
    fn unknown_format_auto_detects() {
        let png_bytes = make_png_bytes();
        let mut image = make_image(png_bytes, "unknown");
        let result = re_encode_default(&mut image, ImageOutputFormat::Jpeg { quality: 85 });
        assert!(
            matches!(result, Ok(true)),
            "unknown-format valid PNG→jpeg must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "jpeg");
    }

    #[test]
    fn quality_out_of_range_clamps() {
        let mut image = make_image(make_png_bytes(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Jpeg { quality: 200 });
        assert!(
            matches!(result, Ok(true)),
            "quality 200 should clamp and encode; got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "jpeg");
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_sanitize_pass_on_native_target() {
        let svg_bytes = Bytes::from_static(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>");
        let mut image = make_image(svg_bytes, "svg");
        let opts = SvgOptions {
            sanitize: true,
            render_dpi: 96.0,
        };
        let result = re_encode(
            &mut image,
            ImageOutputFormat::Native,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            &opts,
        );
        assert!(
            matches!(result, Ok(true)),
            "SVG sanitize on Native must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "svg", "format must remain 'svg'");
        std::str::from_utf8(&image.data).expect("sanitized SVG must be valid UTF-8");
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_no_sanitize_native_is_noop() {
        let svg_bytes = Bytes::from_static(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>");
        let original_data = svg_bytes.clone();
        let mut image = make_image(svg_bytes, "svg");
        let opts = SvgOptions {
            sanitize: false,
            render_dpi: 96.0,
        };
        let result = re_encode(
            &mut image,
            ImageOutputFormat::Native,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            &opts,
        );
        assert!(
            matches!(result, Ok(false)),
            "SVG no-sanitize on Native must return Ok(false); got {result:?}"
        );
        assert_eq!(image.data, original_data);
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_to_svg_sanitize_roundtrip() {
        let svg_bytes = Bytes::from_static(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>");
        let mut image = make_image(svg_bytes, "svg");
        let result = re_encode(
            &mut image,
            ImageOutputFormat::Svg,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            &SvgOptions::default(),
        );
        assert!(
            matches!(result, Ok(true)),
            "svg→svg must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "svg");
        std::str::from_utf8(&image.data).expect("output must be valid UTF-8");
    }

    #[cfg(feature = "svg")]
    #[test]
    fn raster_to_svg_returns_unsupported_direction() {
        let mut image = make_image(make_png_bytes(), "png");
        let result = re_encode(
            &mut image,
            ImageOutputFormat::Svg,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            &SvgOptions::default(),
        );
        assert!(
            matches!(result, Err(EncodeWarning::UnsupportedDirection { ref from_format, to_format: "svg" }) if from_format == "png"),
            "png→svg must return Err(UnsupportedDirection); got {result:?}",
        );
        let guessed = image::guess_format(&image.data).expect("data must still be valid PNG");
        assert_eq!(guessed, ImageFormat::Png);
    }

    #[cfg(feature = "svg")]
    #[test]
    fn svg_to_jpeg_rasterizes() {
        let svg_bytes = Bytes::from_static(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>");
        let mut image = make_image(svg_bytes, "svg");
        let result = re_encode(
            &mut image,
            ImageOutputFormat::Jpeg { quality: 85 },
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
            &SvgOptions::default(),
        );
        assert!(
            matches!(result, Ok(true)),
            "svg→jpeg must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "jpeg");
        let guessed = image::guess_format(&image.data).expect("output must be valid JPEG");
        assert_eq!(guessed, ImageFormat::Jpeg);
    }

    #[cfg(feature = "heic")]
    #[test]
    fn png_to_heif_round_trip() {
        let mut image = make_image(make_png_bytes(), "png");
        let result = re_encode_default(&mut image, ImageOutputFormat::Heif { quality: 80 });
        assert!(
            matches!(result, Ok(true)),
            "png→heif must return Ok(true); got {result:?}"
        );
        assert_eq!(image.format.as_ref(), "heif");
        let context = xberg_libheif::HeifContext::read_from_bytes(&image.data).expect("output should be valid HEIF");
        let handle = context.primary_image_handle().expect("should have primary image");
        assert_eq!(handle.width(), 4);
        assert_eq!(handle.height(), 4);
    }

    #[cfg(feature = "heic")]
    #[test]
    fn heif_same_format_no_op() {
        let mut image = make_image(Bytes::from_static(b"placeholder"), "heif");
        let result = re_encode_default(&mut image, ImageOutputFormat::Heif { quality: 80 });
        assert!(matches!(result, Ok(false)), "heif→heif must return Ok(false)");
    }

    #[cfg(feature = "heic")]
    #[test]
    fn heic_format_string_matches() {
        let mut image = make_image(Bytes::from_static(b"placeholder"), "heic");
        let result = re_encode_default(&mut image, ImageOutputFormat::Heif { quality: 80 });
        assert!(matches!(result, Ok(false)), "heic→Heif must return Ok(false)");
    }

    #[cfg(feature = "heic")]
    #[test]
    fn should_reject_oversized_heic_dimensions_before_reencoding_decode() {
        let error = validate_heic_decode_budget(6_000, 6_000, "heic", 0, &SecurityLimits::default())
            .expect_err("oversized HEIC must fail at the decoded-image budget");

        assert!(
            matches!(error, EncodeWarning::DecodeFailed { ref message, .. } if message.contains("security_limits.max_content_size")),
            "unexpected error: {error}"
        );
    }
}
