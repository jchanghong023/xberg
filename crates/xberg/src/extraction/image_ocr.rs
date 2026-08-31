//! Centralized image OCR processing.
//!
//! Extracted images are processed with bounded concurrency. Windows metafiles
//! are rasterized in memory immediately before OCR; the original image remains
//! unchanged in the returned document.

use std::borrow::Cow;

use crate::types::{ExtractedDocument, ExtractedImage};

#[derive(Debug)]
struct ImageOcrPreprocessError {
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

fn bounded_metafile_dimensions(
    image: &ExtractedImage,
    image_config: &crate::core::config::ImageExtractionConfig,
    security_limits: &crate::extractors::security::SecurityLimits,
) -> Result<(u32, u32), ImageOcrPreprocessError> {
    let width = image
        .width
        .filter(|value| *value > 0)
        .ok_or_else(|| ImageOcrPreprocessError::new("rasterize_decode", "shape width is unavailable"))?;
    let height = image
        .height
        .filter(|value| *value > 0)
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

fn push_image_warning(
    warnings: &mut Vec<crate::types::ProcessingWarning>,
    image: Option<&ExtractedImage>,
    index: Option<usize>,
    stage: &str,
    reason: &str,
) {
    let image_index = index
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let page = image
        .and_then(|value| value.page_number)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let format = image
        .map(|value| value.format.as_ref())
        .filter(|format| {
            matches!(
                *format,
                "jpeg" | "png" | "gif" | "bmp" | "svg" | "tiff" | "webp" | "emf" | "wmf" | "unknown"
            )
        })
        .unwrap_or("unknown");
    warnings.push(crate::types::ProcessingWarning {
        source: Cow::Borrowed("image_ocr"),
        message: Cow::Owned(format!(
            "image_index={image_index} page={page} format={format} stage={stage} reason={reason}"
        )),
    });
}

/// Process extracted images with OCR if configured.
#[cfg(all(feature = "ocr", feature = "tokio-runtime"))]
pub(crate) async fn process_images_with_ocr(
    mut images: Vec<ExtractedImage>,
    config: &crate::core::config::ExtractionConfig,
    warnings: &mut Vec<crate::types::ProcessingWarning>,
) -> crate::Result<Vec<ExtractedImage>> {
    if images.is_empty() || config.ocr.is_none() {
        return Ok(images);
    }

    use std::collections::VecDeque;
    use tokio::task::JoinSet;

    let ocr_config = config.ocr.as_ref().unwrap();
    let output_format = config.output_format.clone();
    let acceleration = ocr_config.acceleration.clone();
    let image_config = config.images.clone().unwrap_or_default();
    let security_limits = config.security_limits.clone().unwrap_or_default();
    let max_tasks = crate::core::config::concurrency::resolve_thread_budget(config.concurrency.as_ref());

    type TaskFailure = (&'static str, String);
    type OcrTaskResult = (usize, Result<ExtractedDocument, TaskFailure>);
    type PendingOcrTask = (
        usize,
        ExtractedImage,
        crate::core::config::OcrConfig,
        crate::core::config::ImageExtractionConfig,
        crate::extractors::security::SecurityLimits,
    );
    let mut join_set: JoinSet<OcrTaskResult> = JoinSet::new();
    let mut pending: VecDeque<PendingOcrTask> = VecDeque::with_capacity(images.len());

    for (index, image) in images.iter().cloned().enumerate() {
        let mut task_ocr_config = ocr_config.clone();
        task_ocr_config.output_format = Some(output_format.clone());
        task_ocr_config.acceleration = acceleration.clone();
        pending.push_back((
            index,
            image,
            task_ocr_config,
            image_config.clone(),
            security_limits.clone(),
        ));
    }

    let spawn_task = |join_set: &mut JoinSet<OcrTaskResult>, task: PendingOcrTask| {
        join_set.spawn(async move {
            let (index, image, task_ocr_config, image_config, security_limits) = task;
            let result: Result<ExtractedDocument, TaskFailure> = async {
                let detected = crate::extraction::image_format::detect_image_format(&image.data);
                let prepared =
                    if matches!(detected.as_ref(), "emf" | "wmf") || matches!(image.format.as_ref(), "emf" | "wmf") {
                        tokio::task::spawn_blocking(move || {
                            prepare_image_for_ocr(&image, &image_config, &security_limits)
                                .map(Cow::into_owned)
                                .map(bytes::Bytes::from)
                        })
                        .await
                        .map_err(|_error| ("rasterize_decode", "preprocessing task failed".to_string()))?
                        .map_err(|error| (error.stage, error.reason))?
                    } else {
                        image.data.clone()
                    };

                let backend = {
                    let registry = crate::plugins::registry::get_ocr_backend_registry();
                    let registry = registry.read();
                    registry
                        .get(&task_ocr_config.backend)
                        .map(Clone::clone)
                        .map_err(|_error| ("ocr_backend", "backend unavailable".to_string()))?
                };
                backend
                    .process_image(&prepared, &task_ocr_config)
                    .await
                    .map_err(|_error| ("ocr_backend", "backend processing failed".to_string()))
            }
            .await;
            (index, result)
        });
    };

    while join_set.len() < max_tasks {
        let Some(task) = pending.pop_front() else {
            break;
        };
        spawn_task(&mut join_set, task);
    }

    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((index, Ok(mut ocr_document))) => {
                ocr_document.images = None;
                ocr_config.apply_public_element_policy(&mut ocr_document);
                if ocr_document.content.trim().is_empty() {
                    push_image_warning(
                        warnings,
                        images.get(index),
                        Some(index),
                        "ocr_empty",
                        "backend returned empty text",
                    );
                }
                images[index].ocr_result = Some(Box::new(ocr_document));
            }
            Ok((index, Err((stage, reason)))) => {
                push_image_warning(warnings, images.get(index), Some(index), stage, &reason);
                images[index].ocr_result = None;
            }
            Err(_error) => {
                push_image_warning(warnings, None, None, "ocr_backend", "bounded image task failed");
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
}
