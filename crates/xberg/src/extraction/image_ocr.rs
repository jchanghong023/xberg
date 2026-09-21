//! Centralized image OCR processing.
//!
//! Provides a shared function for processing extracted images with OCR,
//! used by DOCX, PPTX, Jupyter, Markdown, and other extractors.
//!
//! # Recursion Prevention
//!
//! The OCR results produced here set `images: None` to prevent any
//! downstream consumer from triggering further image extraction on
//! OCR output. This breaks the potential cycle:
//! document → extract images → OCR images → (no further image extraction).
//!
//! # Concurrency
//!
//! Image OCR tasks within one extraction operation are processed with a bounded
//! concurrency limit derived from the general thread budget
//! (`core::config::concurrency::resolve_thread_budget`) to prevent resource
//! exhaustion when documents contain many embedded images.
//!
//! This limit is deliberately *not* derived from any VLM-specific request limit
//! (e.g. `OcrConfig::vlm_config::max_concurrency`), even when the configured backend
//! or fallback policy can reach a VLM: this call site mixes CPU-bound raster/OCR work
//! with potential remote requests, and a per-extraction VLM knob would still leave
//! aggregate provider concurrency across concurrent extractions unbounded (see #1465).
//! A real, global provider-side limit is enforced once per shared LLM client instead —
//! see [`crate::llm::client::create_client`].

use std::borrow::Cow;

use crate::types::{ExtractedDocument, ExtractedImage};

/// Why a Windows metafile could not be prepared for OCR, tagged with the pipeline stage so
/// the resulting warning says whether format detection or rasterization failed. Dimension
/// bounding failures share the rasterization stage — both answer "this metafile cannot be
/// turned into a bitmap within its limits", and the `reason` carries the specific bound
/// that was exceeded.
#[derive(Debug)]
pub(crate) struct ImageOcrPreprocessError {
    stage: &'static str,
    reason: String,
}

impl ImageOcrPreprocessError {
    fn new(stage: &'static str, reason: impl Into<String>) -> Self {
        Self {
            stage,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ImageOcrPreprocessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.reason)
    }
}

fn read_i32_le(data: &[u8], offset: usize) -> Option<i64> {
    let end = offset.checked_add(4)?;
    let bytes = data.get(offset..end)?;
    Some(i64::from(i32::from_le_bytes(bytes.try_into().ok()?)))
}

fn read_u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = data.get(offset..end)?;
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

/// Read the intrinsic pixel size of an EMF/WMF straight from its header.
///
/// Returns `None` for anything that is not a metafile, or whose header does not yield a
/// positive size. Used only when the shape carries no usable width/height.
fn infer_metafile_dimensions(data: &[u8], format: &str) -> Option<(u32, u32)> {
    match format {
        "emf" => {
            let width = read_i32_le(data, 16)?.checked_sub(read_i32_le(data, 8)?)?.abs();
            let height = read_i32_le(data, 20)?.checked_sub(read_i32_le(data, 12)?)?.abs();
            Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
                .filter(|(width, height)| *width > 0 && *height > 0)
        }
        "wmf" if data.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) => {
            let left = i64::from(i16::from_le_bytes([*data.get(6)?, *data.get(7)?]));
            let top = i64::from(i16::from_le_bytes([*data.get(8)?, *data.get(9)?]));
            let right = i64::from(i16::from_le_bytes([*data.get(10)?, *data.get(11)?]));
            let bottom = i64::from(i16::from_le_bytes([*data.get(12)?, *data.get(13)?]));
            let units_per_inch = u64::from(read_u16_le(data, 14)?);
            let width_units = right.checked_sub(left)?;
            let height_units = bottom.checked_sub(top)?;
            if width_units <= 0 || height_units <= 0 || units_per_inch == 0 {
                return None;
            }
            let to_pixels = |units: i64| {
                u32::try_from((u64::try_from(units).ok()?.saturating_mul(96) + units_per_inch / 2) / units_per_inch)
                    .ok()
                    .filter(|value| *value > 0)
            };
            Some((to_pixels(width_units)?, to_pixels(height_units)?))
        }
        _ => None,
    }
}

/// Resolve the target raster size for a metafile: the shape's declared size when present,
/// otherwise the intrinsic header size, scaled to `ImageExtractionConfig::target_dpi` and
/// clamped to `max_image_dimension`. The whole result is rejected up front if the RGBA
/// buffer (or a conservative PNG bound) would exceed the configured content limit, so GDI is
/// never asked to allocate a surface larger than the caller permits.
fn bounded_metafile_dimensions(
    image: &ExtractedImage,
    image_config: &crate::core::config::ImageExtractionConfig,
    security_limits: &crate::extractors::security::SecurityLimits,
) -> Result<(u32, u32), ImageOcrPreprocessError> {
    let detected_format = crate::extraction::image_format::detect_image_format(&image.data);
    let intrinsic = infer_metafile_dimensions(&image.data, detected_format.as_ref());
    let width = image
        .width
        .filter(|value| *value > 0)
        .or_else(|| intrinsic.as_ref().map(|dimensions| dimensions.0))
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "shape width is unavailable"))?;
    let height = image
        .height
        .filter(|value| *value > 0)
        .or_else(|| intrinsic.as_ref().map(|dimensions| dimensions.1))
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "shape height is unavailable"))?;
    let dpi = u64::try_from(image_config.target_dpi)
        .map_err(|_| ImageOcrPreprocessError::new("rasterize_decode", "target DPI is invalid"))?;
    let mut width = u64::from(width)
        .checked_mul(dpi)
        .and_then(|value| value.checked_add(48))
        .map(|value| value / 96)
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "scaled width overflow"))?
        .max(1);
    let mut height = u64::from(height)
        .checked_mul(dpi)
        .and_then(|value| value.checked_add(48))
        .map(|value| value / 96)
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "scaled height overflow"))?
        .max(1);
    let maximum = u64::try_from(image_config.max_image_dimension)
        .map_err(|_| ImageOcrPreprocessError::new("rasterize_decode", "maximum image dimension is invalid"))?;
    if maximum == 0 {
        return Err(ImageOcrPreprocessError::new(
            "rasterize_decode",
            "maximum image dimension is zero",
        ));
    }
    let largest = width.max(height);
    if largest > maximum {
        width = width
            .checked_mul(maximum)
            .map(|value| value / largest)
            .unwrap_or(0)
            .max(1);
        height = height
            .checked_mul(maximum)
            .map(|value| value / largest)
            .unwrap_or(0)
            .max(1);
    }
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "pixel count overflow"))?;
    let rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "RGBA allocation overflow"))?;
    let png_bound = rgba_bytes
        .checked_add(rgba_bytes / 16)
        .and_then(|value| value.checked_add(65_536))
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "PNG buffer bound overflow"))?;
    let content_limit = u64::try_from(security_limits.max_content_size).unwrap_or(u64::MAX);
    if rgba_bytes > content_limit || png_bound > content_limit {
        return Err(ImageOcrPreprocessError::new(
            "rasterize_decode",
            "metafile raster exceeds configured content limit",
        ));
    }
    Ok((
        u32::try_from(width).map_err(|_| ImageOcrPreprocessError::new("rasterize_decode", "width exceeds u32"))?,
        u32::try_from(height).map_err(|_| ImageOcrPreprocessError::new("rasterize_decode", "height exceeds u32"))?,
    ))
}

/// Return the bytes to hand to the OCR backend: the original raster image unchanged, or a
/// freshly rasterized PNG for an EMF/WMF. On non-Windows hosts, or when a declared metafile
/// fails header validation, this is an error rather than a silently misdecoded buffer.
fn prepare_image_for_ocr<'a>(
    image: &'a ExtractedImage,
    image_config: &crate::core::config::ImageExtractionConfig,
    security_limits: &crate::extractors::security::SecurityLimits,
) -> Result<Cow<'a, [u8]>, ImageOcrPreprocessError> {
    let detected = crate::extraction::image_format::detect_image_format(&image.data);
    let declared_vector = matches!(image.format.as_ref(), "emf" | "wmf");
    if !matches!(detected.as_ref(), "emf" | "wmf") {
        if declared_vector {
            return Err(ImageOcrPreprocessError::new(
                "format_detect",
                "declared metafile failed header validation",
            ));
        }
        return Ok(Cow::Borrowed(&image.data));
    }

    let (width, height) = bounded_metafile_dimensions(image, image_config, security_limits)?;

    #[cfg(windows)]
    {
        use image::ImageEncoder;
        use xberg_windows_metafile::{MetafileKind, rasterize};

        let kind = match detected.as_ref() {
            "emf" => MetafileKind::Emf,
            "wmf" if image.data.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) => MetafileKind::PlaceableWmf,
            "wmf" => MetafileKind::StandardWmf,
            _ => unreachable!("metafile format checked above"),
        };
        let raster = rasterize(&image.data, kind, width, height)
            .map_err(|error| ImageOcrPreprocessError::new("rasterize_decode", error.to_string()))?;
        let expected = usize::try_from(raster.width)
            .ok()
            .and_then(|w| usize::try_from(raster.height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "RGBA result size overflow"))?;
        if raster.rgba.len() != expected {
            return Err(ImageOcrPreprocessError::new(
                "rasterize_decode",
                "rasterizer returned an invalid RGBA length",
            ));
        }
        let png_capacity = expected
            .checked_add(expected / 16)
            .and_then(|value| value.checked_add(65_536))
            .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "PNG buffer bound overflow"))?;
        if png_capacity > security_limits.max_content_size {
            return Err(ImageOcrPreprocessError::new(
                "rasterize_decode",
                "PNG buffer bound exceeds configured content limit",
            ));
        }
        let mut png = Vec::new();
        png.try_reserve(png_capacity)
            .map_err(|error| ImageOcrPreprocessError::new("rasterize_decode", error.to_string()))?;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &raster.rgba,
                raster.width,
                raster.height,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|error| ImageOcrPreprocessError::new("rasterize_decode", error.to_string()))?;
        if png.len() > security_limits.max_content_size {
            return Err(ImageOcrPreprocessError::new(
                "rasterize_decode",
                "encoded PNG exceeds configured content limit",
            ));
        }
        Ok(Cow::Owned(png))
    }

    #[cfg(not(windows))]
    {
        let _ = (width, height);
        Err(ImageOcrPreprocessError::new(
            "rasterize_decode",
            "Windows metafile rasterization is unavailable on this platform",
        ))
    }
}

/// Rasterize an EMF/WMF image to a [`image::DynamicImage`] for consumers other than OCR
/// (e.g. the `images.output_format` re-encode pass). Non-metafile inputs are an error.
pub(crate) fn rasterize_metafile_to_dynamic_image(
    image: &ExtractedImage,
    image_config: &crate::core::config::ImageExtractionConfig,
    security_limits: &crate::extractors::security::SecurityLimits,
) -> Result<image::DynamicImage, ImageOcrPreprocessError> {
    let detected = crate::extraction::image_format::detect_image_format(&image.data);
    if !matches!(detected.as_ref(), "emf" | "wmf") {
        return Err(ImageOcrPreprocessError::new(
            "format_detect",
            "input is not a Windows metafile",
        ));
    }

    let (width, height) = bounded_metafile_dimensions(image, image_config, security_limits)?;

    #[cfg(windows)]
    {
        use xberg_windows_metafile::{MetafileKind, rasterize};

        let kind = match detected.as_ref() {
            "emf" => MetafileKind::Emf,
            "wmf" if image.data.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) => MetafileKind::PlaceableWmf,
            "wmf" => MetafileKind::StandardWmf,
            _ => unreachable!("metafile format checked above"),
        };
        let raster = rasterize(&image.data, kind, width, height)
            .map_err(|error| ImageOcrPreprocessError::new("rasterize_decode", error.to_string()))?;
        let expected = usize::try_from(raster.width)
            .ok()
            .and_then(|w| usize::try_from(raster.height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "RGBA result size overflow"))?;
        if raster.rgba.len() != expected {
            return Err(ImageOcrPreprocessError::new(
                "rasterize_decode",
                "rasterizer returned an invalid RGBA length",
            ));
        }
        let rgba_bytes = u64::try_from(expected).unwrap_or(u64::MAX);
        let content_limit = u64::try_from(security_limits.max_content_size).unwrap_or(u64::MAX);
        if rgba_bytes > content_limit {
            return Err(ImageOcrPreprocessError::new(
                "rasterize_decode",
                "metafile raster exceeds configured content limit",
            ));
        }
        let rgba_img = image::RgbaImage::from_raw(raster.width, raster.height, raster.rgba)
            .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "RGBA buffer size mismatch"))?;
        Ok(image::DynamicImage::ImageRgba8(rgba_img))
    }

    #[cfg(not(windows))]
    {
        let _ = (width, height);
        Err(ImageOcrPreprocessError::new(
            "rasterize_decode",
            "Windows metafile rasterization is unavailable on this platform",
        ))
    }
}

/// Process extracted images with OCR if configured.
///
/// For each image, spawns an async OCR task using the backend from the registry
/// and stores the result in `image.ocr_result`. If OCR is not configured or
/// fails for an individual image, that image's `ocr_result` remains `None`.
///
/// This function is the single shared implementation used by all
/// document extractors (DOCX, PPTX, Jupyter, Markdown, etc.).
///
/// # Recursion Safety
///
/// The produced `ExtractedDocument` for each image explicitly sets
/// `images: None`, preventing further image extraction cycles when
/// OCR results are consumed by archive or recursive extraction paths.
///
/// # Concurrency
///
/// Concurrency within the current extraction is bounded by the general thread
/// budget (never a VLM-specific request limit — see the module docs) using a
/// replenished task set, so queued images do not create an unbounded number of
/// futures. Concurrent document extractions each enforce their own limit.
// (fork) perf-tracing：图片 OCR 批处理阶段 span（一次调用 = 一份文档的全部图片）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(
        target = "perf",
        name = "image_ocr",
        skip_all,
        fields(images = images.len())
    )
)]
#[cfg(all(feature = "ocr", feature = "tokio-runtime"))]
pub(crate) async fn process_images_with_ocr(
    mut images: Vec<ExtractedImage>,
    config: &crate::core::config::ExtractionConfig,
    warnings: &mut Vec<crate::types::ProcessingWarning>,
) -> crate::Result<Vec<ExtractedImage>> {
    if images.is_empty() {
        return Ok(images);
    }

    // A caller that disabled OCR (`disable_ocr` or `ocr.enabled = false`) must not get image
    // OCR out of this shared helper just because it was handed images anyway: defend the hard
    // switch here instead of trusting every call site's gate. ~keep
    if config.effective_disable_ocr() {
        return Ok(images);
    }

    // Image OCR is on by default (`ImageExtractionConfig::run_ocr_on_images`), so a caller
    // who never configured an `ocr` section still gets their images OCR'd: fall back to the
    // default backend rather than silently skipping every image. ~keep
    let default_ocr_config;
    let ocr_config = match config.ocr.as_ref() {
        Some(cfg) => cfg,
        None => {
            default_ocr_config = crate::core::config::OcrConfig::default();
            &default_ocr_config
        }
    };

    // No usable backend must not become a per-image error storm: report once and return the
    // images unprocessed so every image entry — and its placeholder in the output — survives.
    crate::plugins::ensure_ocr_backends_initialized();
    if ocr_config.pipeline.is_none()
        && crate::plugins::registry::get_ocr_backend_registry()
            .read()
            .get(&ocr_config.backend)
            .is_err()
    {
        warnings.push(crate::types::ProcessingWarning {
            source: std::borrow::Cow::Borrowed("image_ocr"),
            message: std::borrow::Cow::Owned(format!(
                "OCR backend '{}' is not registered; images were extracted without OCR text",
                ocr_config.backend
            )),
        });
        return Ok(images);
    }
    let output_format = config.output_format.clone();
    let acceleration = ocr_config.acceleration.clone();
    // GH#1554: `OcrConfig::security_limits` has no other way to reach a caller's configured
    // limits — the `OcrBackend::process_image` trait method takes only `OcrConfig`, not
    // `ExtractionConfig` — so it must be copied onto each per-image clone here, mirroring
    // `acceleration` immediately above. `ExtractionConfig::security_limits` is the source of
    // truth; a backend seeing `None` here must fall back to `SecurityLimits::default()`,
    // never disable the check. ~keep
    let security_limits = config.security_limits.clone();
    // Rasterization runs before any backend sees the image, so it must bound the GDI surface
    // itself; a caller that configured no `security_limits` gets the crate defaults rather
    // than an unbounded allocation. `OcrConfig::security_limits` keeps the `Option` above.
    let raster_security_limits = security_limits.clone().unwrap_or_default();
    // Metafile rasterization needs the same image-extraction knobs the extractors already
    // honour (`target_dpi`, `max_image_dimension`), so pull the section once for every task.
    let image_config = config.images.clone().unwrap_or_default();

    use std::collections::VecDeque;
    use tokio::task::JoinSet;

    let max_tasks = crate::core::config::concurrency::resolve_thread_budget(config.concurrency.as_ref());

    type OcrTaskResult = (usize, crate::Result<ExtractedDocument>);
    type PendingOcrTask = (
        usize,
        ExtractedImage,
        crate::core::config::OcrConfig,
        crate::core::config::ImageExtractionConfig,
        crate::extractors::security::SecurityLimits,
    );
    let mut join_set: JoinSet<OcrTaskResult> = JoinSet::new();
    let mut pending: VecDeque<PendingOcrTask> = VecDeque::with_capacity(images.len());

    for (idx, image) in images.iter().cloned().enumerate() {
        let mut ocr_config_clone = ocr_config.clone();
        ocr_config_clone.output_format = Some(output_format.clone());
        ocr_config_clone.acceleration = acceleration.clone();
        // Conditional for the same reason as the standalone-image route (GH#1651): do not
        // replace a directly-set `OcrConfig::security_limits` with `None`. ~keep
        if let Some(limits) = security_limits.clone() {
            ocr_config_clone.security_limits = Some(limits);
        }
        pending.push_back((
            idx,
            image,
            ocr_config_clone,
            image_config.clone(),
            raster_security_limits.clone(),
        ));
    }

    let spawn_task = |join_set: &mut JoinSet<OcrTaskResult>, task: PendingOcrTask| {
        join_set.spawn(async move {
            let (idx, image, ocr_config_clone, image_config, security_limits) = task;
            let ocr_result = async {
                // EMF/WMF are vector formats no OCR backend can decode. Rasterize them to a
                // PNG first, off the async executor (GDI work is CPU-bound and blocking).
                // The declared format is also honoured because a caller may label the bytes
                // `emf`/`wmf` while the header is subtly invalid; that mismatch must fail
                // loudly instead of being fed to the backend as an opaque blob.
                let detected = crate::extraction::image_format::detect_image_format(&image.data);
                let is_metafile =
                    matches!(detected.as_ref(), "emf" | "wmf") || matches!(image.format.as_ref(), "emf" | "wmf");
                let prepared: bytes::Bytes = if is_metafile {
                    tokio::task::spawn_blocking(move || {
                        prepare_image_for_ocr(&image, &image_config, &security_limits)
                            .map(|cow| bytes::Bytes::from(cow.into_owned()))
                    })
                    .await
                    .map_err(|error| crate::XbergError::Ocr {
                        message: format!("metafile rasterization task panicked: {}", error),
                        source: None,
                    })?
                    .map_err(|error| crate::XbergError::Ocr {
                        message: format!("metafile rasterization failed at {}: {}", error.stage, error.reason),
                        source: None,
                    })?
                } else {
                    image.data.clone()
                };

                let backend = {
                    let registry = crate::plugins::registry::get_ocr_backend_registry();
                    let registry = registry.read();
                    match registry.get(&ocr_config_clone.backend) {
                        Ok(b) => b.clone(),
                        Err(e) => {
                            return Err(crate::XbergError::Ocr {
                                message: format!("OCR backend '{}' not found: {}", ocr_config_clone.backend, e),
                                source: None,
                            });
                        }
                    }
                };

                backend.process_image(&prepared, &ocr_config_clone).await
            }
            .await;
            (idx, ocr_result)
        });
    };

    while join_set.len() < max_tasks {
        let Some(task) = pending.pop_front() else {
            break;
        };
        spawn_task(&mut join_set, task);
    }

    while let Some(join_result) = join_set.join_next().await {
        let (idx, ocr_result) = join_result.map_err(|e| crate::XbergError::Ocr {
            message: format!("OCR task panicked: {}", e),
            source: None,
        })?;

        match ocr_result {
            Ok(extraction_result) => {
                // Keep the backend's result whole. Rebuilding it field-by-field silently
                // dropped everything the backend populated besides content/mime_type/
                // ocr_elements — tables, metadata (OCR language, PSM, confidence),
                // formulas, llm_usage (VLM cost accounting), detected_languages and
                // processing_warnings. The PDF inline-image path already stores the
                // backend result unmodified; mirror it here.
                let mut ocr_document = extraction_result;
                // Recursion guard: OCR output must never carry nested images, or an
                // archive/recursive consumer would extract images out of OCR output.
                ocr_document.images = None;
                ocr_config.apply_public_element_policy(&mut ocr_document);
                images[idx].ocr_result = Some(Box::new(ocr_document));
            }
            Err(e) => {
                warnings.push(crate::types::ProcessingWarning {
                    source: std::borrow::Cow::Borrowed("image_ocr"),
                    message: std::borrow::Cow::Owned(format!("Image {} OCR failed: {}", idx, e)),
                });
                images[idx].ocr_result = None;
            }
        }

        if let Some(task) = pending.pop_front() {
            spawn_task(&mut join_set, task);
        }
    }

    Ok(images)
}

#[cfg(all(test, feature = "ocr", feature = "tokio-runtime"))]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::Notify;

    use super::*;
    use crate::core::config::{ConcurrencyConfig, LlmConfig, OcrConfig, VlmFallbackPolicy};
    use crate::plugins::{OcrBackend, OcrBackendType, Plugin};

    const BACKEND_NAME: &str = "thread-budget-concurrency-test-backend";
    const POLICY_BACKEND_NAME: &str = "embedded-image-element-policy-test-backend";

    struct RegistrationGuard;

    impl Drop for RegistrationGuard {
        fn drop(&mut self) {
            let _ = crate::plugins::unregister_ocr_backend(BACKEND_NAME);
        }
    }

    /// An OCR backend that counts every call that starts, then parks forever on a
    /// [`Notify`] the test never fires.
    ///
    /// `process_images_with_ocr` spawns exactly `min(max_tasks, images.len())` tasks in a
    /// synchronous loop before its first `.await` point (the `while join_set.len() <
    /// max_tasks` loop), and only spawns a replacement task once an existing one
    /// completes. Since every call here blocks forever, no replacement is ever spawned, so
    /// the final call count is a deterministic fact about `max_tasks` — not a race won by
    /// however many tasks happen to be "in flight" at some observed instant.
    struct GatedBackend {
        calls: Arc<AtomicUsize>,
        gate: Arc<Notify>,
    }

    const SECURITY_LIMITS_CAPTURE_BACKEND_NAME: &str = "security-limits-capture-test-backend";

    /// Captures the `security_limits` on the `OcrConfig` it receives, so the test can assert
    /// on what actually reached the backend rather than trusting the caller's intent.
    struct SecurityLimitsCaptureBackend {
        observed_max_content_size: Arc<std::sync::Mutex<Option<Option<usize>>>>,
    }

    impl Plugin for SecurityLimitsCaptureBackend {
        fn name(&self) -> &str {
            SECURITY_LIMITS_CAPTURE_BACKEND_NAME
        }

        fn version(&self) -> String {
            "1.0.0".to_string()
        }

        fn initialize(&self) -> crate::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl OcrBackend for SecurityLimitsCaptureBackend {
        async fn process_image(&self, _image_bytes: &[u8], config: &OcrConfig) -> crate::Result<ExtractedDocument> {
            let observed = config.security_limits.as_ref().map(|limits| limits.max_content_size);
            *self.observed_max_content_size.lock().unwrap() = Some(observed);
            Ok(ExtractedDocument::default())
        }

        fn supports_language(&self, _lang: &str) -> bool {
            true
        }

        fn backend_type(&self) -> OcrBackendType {
            OcrBackendType::Custom
        }
    }

    /// GH#1554 regression: `ExtractionConfig::security_limits` must reach the `OcrConfig`
    /// handed to `OcrBackend::process_image` for embedded-image OCR (DOCX/PPTX/etc.), the
    /// same way `OcrConfig::acceleration` already does. Before this fix, `OcrConfig` had no
    /// `security_limits` field at all, so a caller's configured, possibly higher, limit could
    /// never reach a backend through this path — every backend silently decoded under
    /// `SecurityLimits::default()` regardless of what the caller configured.
    #[tokio::test]
    async fn extraction_config_security_limits_reach_embedded_image_ocr_config() {
        let observed_max_content_size = Arc::new(std::sync::Mutex::new(None));
        crate::plugins::register_ocr_backend(Arc::new(SecurityLimitsCaptureBackend {
            observed_max_content_size: Arc::clone(&observed_max_content_size),
        }))
        .expect("register security-limits capture OCR backend");
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = crate::plugins::unregister_ocr_backend(SECURITY_LIMITS_CAPTURE_BACKEND_NAME);
            }
        }
        let _guard = Guard;

        let configured_limit = 200 * 1024 * 1024;
        let config = crate::core::config::ExtractionConfig {
            ocr: Some(OcrConfig {
                backend: SECURITY_LIMITS_CAPTURE_BACKEND_NAME.to_string(),
                ..Default::default()
            }),
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_content_size: configured_limit,
                ..Default::default()
            }),
            ..Default::default()
        };
        let images = vec![ExtractedImage {
            data: Bytes::from_static(b"image"),
            ..Default::default()
        }];
        let mut warnings = Vec::new();

        process_images_with_ocr(images, &config, &mut warnings)
            .await
            .expect("embedded-image OCR must succeed");

        assert!(warnings.is_empty());
        let observed = observed_max_content_size.lock().unwrap();
        assert_eq!(
            *observed,
            Some(Some(configured_limit)),
            "OcrConfig::security_limits must carry the caller's ExtractionConfig::security_limits"
        );
    }

    struct PolicyIgnoringBackend;

    impl Plugin for PolicyIgnoringBackend {
        fn name(&self) -> &str {
            POLICY_BACKEND_NAME
        }

        fn version(&self) -> String {
            "1.0.0".to_string()
        }

        fn initialize(&self) -> crate::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl OcrBackend for PolicyIgnoringBackend {
        async fn process_image(&self, _image_bytes: &[u8], _config: &OcrConfig) -> crate::Result<ExtractedDocument> {
            Ok(ExtractedDocument {
                content: "embedded OCR".to_string(),
                ocr_elements: Some(vec![crate::types::OcrElement {
                    text: "backend element".to_string(),
                    page_number: 1,
                    ..Default::default()
                }]),
                ..Default::default()
            })
        }

        fn supports_language(&self, _lang: &str) -> bool {
            true
        }

        fn backend_type(&self) -> OcrBackendType {
            OcrBackendType::Custom
        }
    }

    impl Plugin for GatedBackend {
        fn name(&self) -> &str {
            BACKEND_NAME
        }

        fn version(&self) -> String {
            "1.0.0".to_string()
        }

        fn initialize(&self) -> crate::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl OcrBackend for GatedBackend {
        async fn process_image(&self, _image_bytes: &[u8], _config: &OcrConfig) -> crate::Result<ExtractedDocument> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Never notified: this call parks here for the rest of the test.
            self.gate.notified().await;
            Ok(ExtractedDocument {
                content: "unreachable".to_string(),
                mime_type: Cow::Borrowed("text/plain"),
                ..Default::default()
            })
        }

        fn supports_language(&self, _lang: &str) -> bool {
            true
        }

        fn backend_type(&self) -> OcrBackendType {
            OcrBackendType::Custom
        }
    }

    /// Regression test for GH#1465.
    ///
    /// Before the fix, image OCR concurrency was `resolve_ocr_concurrency`, which prefers
    /// `OcrConfig::vlm_config::max_concurrency` over the general thread budget whenever
    /// `vlm_fallback` is not `Disabled` — even though this call site mixes CPU-bound OCR
    /// work with, at most, occasional remote VLM requests (see the module docs). A small
    /// general thread budget (2) paired with a much larger VLM limit (6) must now bound
    /// concurrency at 2, not 6: the general thread budget governs this CPU-bound batch
    /// size unconditionally.
    #[tokio::test]
    async fn general_thread_budget_bounds_image_ocr_batch_not_vlm_max_concurrency() {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        crate::plugins::register_ocr_backend(Arc::new(GatedBackend {
            calls: Arc::clone(&calls),
            gate: Arc::clone(&gate),
        }))
        .expect("register gated OCR backend");
        let _registration = RegistrationGuard;

        let config = crate::core::config::ExtractionConfig {
            ocr: Some(OcrConfig {
                backend: BACKEND_NAME.to_string(),
                vlm_fallback: VlmFallbackPolicy::Always,
                vlm_config: Some(LlmConfig {
                    model: "test/model".to_string(),
                    max_concurrency: Some(6),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            concurrency: Some(ConcurrencyConfig { max_threads: Some(2) }),
            ..Default::default()
        };
        let images = (0..6)
            .map(|_| ExtractedImage {
                data: Bytes::from_static(b"image"),
                ..Default::default()
            })
            .collect();
        let mut warnings = Vec::new();

        // None of the 6 spawned tasks can ever complete (the gate is never notified), so
        // this always times out. The timeout only gives the runtime a chance to run every
        // task that was actually spawned before the test inspects `calls`; dropping the
        // timed-out future aborts them via `JoinSet`'s `Drop` impl.
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            process_images_with_ocr(images, &config, &mut warnings),
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected the general thread budget (2), not the larger VLM max_concurrency (6), \
             to bound the number of image OCR tasks started concurrently"
        );
    }

    #[tokio::test]
    async fn custom_backend_cannot_bypass_embedded_image_element_policy() {
        crate::plugins::register_ocr_backend(Arc::new(PolicyIgnoringBackend))
            .expect("register policy-ignoring OCR backend");
        let config = crate::core::config::ExtractionConfig {
            ocr: Some(OcrConfig {
                backend: POLICY_BACKEND_NAME.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let images = vec![ExtractedImage {
            data: Bytes::from_static(b"image"),
            ..Default::default()
        }];
        let mut warnings = Vec::new();

        let images = process_images_with_ocr(images, &config, &mut warnings)
            .await
            .expect("embedded-image OCR must succeed");
        let nested = images[0].ocr_result.as_ref().expect("OCR result must be preserved");

        assert_eq!(nested.content, "embedded OCR");
        assert!(nested.ocr_elements.is_none());
        assert!(warnings.is_empty());
        crate::plugins::unregister_ocr_backend(POLICY_BACKEND_NAME).unwrap();
    }

    const DISABLED_OCR_BACKEND_NAME: &str = "disabled-image-ocr-test-backend";

    /// Counts every `process_image` call, so a test can assert that a caller who disabled OCR
    /// never reached the backend rather than trusting the returned images alone.
    struct CountingBackend {
        calls: Arc<AtomicUsize>,
    }

    impl Plugin for CountingBackend {
        fn name(&self) -> &str {
            DISABLED_OCR_BACKEND_NAME
        }

        fn version(&self) -> String {
            "1.0.0".to_string()
        }

        fn initialize(&self) -> crate::Result<()> {
            Ok(())
        }

        fn shutdown(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl OcrBackend for CountingBackend {
        async fn process_image(&self, _image_bytes: &[u8], _config: &OcrConfig) -> crate::Result<ExtractedDocument> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExtractedDocument::default())
        }

        fn supports_language(&self, _lang: &str) -> bool {
            true
        }

        fn backend_type(&self) -> OcrBackendType {
            OcrBackendType::Custom
        }
    }

    struct DisabledBackendGuard;

    impl Drop for DisabledBackendGuard {
        fn drop(&mut self) {
            let _ = crate::plugins::unregister_ocr_backend(DISABLED_OCR_BACKEND_NAME);
        }
    }

    /// `disable_ocr` and the `ocr.enabled = false` shorthand are documented as hard switches
    /// ("Disable OCR entirely, even for images"), but this helper used to synthesize a default
    /// (enabled) `OcrConfig` when none was configured and forward any caller config verbatim —
    /// so an explicit opt-out still OCR'd every image and loaded the default backend's model.
    /// Both variants must now short-circuit before the backend runs, while the images survive
    /// unprocessed.
    #[tokio::test]
    async fn disabled_ocr_never_reaches_the_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        crate::plugins::register_ocr_backend(Arc::new(CountingBackend {
            calls: Arc::clone(&calls),
        }))
        .expect("register counting OCR backend");
        let _guard = DisabledBackendGuard;

        let image = || ExtractedImage {
            data: Bytes::from_static(b"image"),
            ..Default::default()
        };
        let mut warnings = Vec::new();

        let config = crate::core::config::ExtractionConfig {
            ocr: Some(OcrConfig {
                backend: DISABLED_OCR_BACKEND_NAME.to_string(),
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let images = process_images_with_ocr(vec![image()], &config, &mut warnings)
            .await
            .expect("a disabled OCR config must still return the images");
        assert!(
            images[0].ocr_result.is_none(),
            "ocr.enabled = false must skip embedded-image OCR"
        );

        let config = crate::core::config::ExtractionConfig {
            disable_ocr: true,
            ..Default::default()
        };
        let images = process_images_with_ocr(vec![image()], &config, &mut warnings)
            .await
            .expect("a disabled OCR config must still return the images");
        assert!(
            images[0].ocr_result.is_none(),
            "disable_ocr must skip embedded-image OCR"
        );

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a caller who disabled OCR must never reach the OCR backend"
        );
        assert!(warnings.is_empty());
    }
}
