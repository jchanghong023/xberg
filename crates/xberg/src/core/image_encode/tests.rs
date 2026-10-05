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

#[cfg(feature = "ocr")]
#[test]
fn jbig2_to_png_preserves_pixels_and_output_format() {
    let source = include_bytes!("../../extraction/image/fixtures/synthetic-valid.jb2");
    let expected = image::load_from_memory(include_bytes!("../../extraction/image/fixtures/synthetic-valid.png"))
        .expect("independent MuPDF render")
        .to_luma8();
    for source_format in ["JBIG2", "jb2", "unknown"] {
        let mut image = make_image(Bytes::from_static(source), source_format);
        assert!(re_encode_default(&mut image, ImageOutputFormat::Png).expect("JBIG2 to PNG re-encode"));
        assert_eq!(image.format.as_ref(), "png");
        assert_eq!(image::guess_format(&image.data).expect("PNG magic"), ImageFormat::Png);
        let actual = image::load_from_memory(&image.data)
            .expect("previewable PNG")
            .to_luma8();
        assert_eq!(actual.dimensions(), (320, 96));
        assert_eq!(
            actual, expected,
            "re-encoding must retain every independently rendered pixel"
        );
    }
}

#[cfg(feature = "ocr")]
#[test]
fn jbig2_to_png_budget_failure_preserves_source() {
    let source = Bytes::from_static(include_bytes!("../../extraction/image/fixtures/synthetic-valid.jb2"));
    let mut image = make_image(source.clone(), "jb2");
    let limits = SecurityLimits {
        max_content_size: 320 * 96,
        ..Default::default()
    };
    let result = re_encode(
        &mut image,
        ImageOutputFormat::Png,
        &limits,
        &crate::core::config::extraction::ImageExtractionConfig::default(),
        #[cfg(feature = "svg")]
        &SvgOptions::default(),
    );
    assert!(
        matches!(result, Err(EncodeWarning::DecodeFailed { ref message, .. })
            if message.contains("security_limits.max_content_size")),
        "the request's live-byte budget must stop decoding: {result:?}"
    );
    assert_eq!(image.data, source);
    assert_eq!(image.format.as_ref(), "jb2");
}

#[cfg(feature = "ocr")]
#[test]
fn truncated_jbig2_to_png_preserves_decoder_error_and_source() {
    let source = include_bytes!("../../extraction/image/fixtures/synthetic-valid.jb2");
    let truncated = Bytes::copy_from_slice(&source[..source.len() - 1]);
    let mut image = make_image(truncated.clone(), "jb2");
    let result = re_encode_default(&mut image, ImageOutputFormat::Png);
    assert!(
        matches!(result, Err(EncodeWarning::DecodeFailed { ref message, .. })
            if message.contains("JBIG2 header parse failed")),
        "truncation must retain the real parser error, not become Undecodable: {result:?}"
    );
    assert_eq!(image.data, truncated);
    assert_eq!(image.format.as_ref(), "jb2");
}

#[cfg(feature = "ocr")]
const LOSSLESS_RGB_JP2: &[u8] = include_bytes!("fixtures/lossless-rgb.jp2");
#[cfg(feature = "ocr")]
const LOSSLESS_RGB_J2K: &[u8] = include_bytes!("fixtures/lossless-rgb.j2k");

#[cfg(feature = "ocr")]
#[test]
fn jpeg2000_images_to_png_preserve_independent_pixels_and_metadata_without_warnings() {
    // Both fixtures were encoded losslessly by Pillow/OpenJPEG from this exact
    // 4x3 RGB grid. Expected pixels do not come from Xberg's JPEG 2000 decoder.
    let expected = image::RgbImage::from_raw(
        4,
        3,
        vec![
            255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 17, 83, 149, 201, 37, 109, 255, 128, 0, 0, 128,
            255, 64, 32, 16, 127, 127, 127, 240, 224, 208,
        ],
    )
    .expect("independent 4x3 RGB grid");
    for source in [LOSSLESS_RGB_JP2, LOSSLESS_RGB_J2K] {
        for source_format in ["JPEG2000", "jp2", "j2k", "jpg2", "jpc", "unknown"] {
            let mut original = make_image(Bytes::from_static(source), source_format);
            original.image_index = 7;
            original.width = Some(4);
            original.height = Some(3);
            let (images, renames, warnings) = re_encode_images(
                vec![original],
                ImageOutputFormat::Png,
                &SecurityLimits::default(),
                &crate::core::config::extraction::ImageExtractionConfig::default(),
            );
            assert!(warnings.is_empty(), "{source_format}: {warnings:?}");
            assert_eq!(renames, vec![(7, source_format.to_string(), "png".to_string())]);
            assert_eq!(images.len(), 1);
            let converted = &images[0];
            assert_eq!(converted.format.as_ref(), "png");
            assert_eq!(converted.image_index, 7);
            assert_eq!((converted.width, converted.height), (Some(4), Some(3)));
            assert_eq!(
                image::guess_format(&converted.data).expect("PNG magic"),
                ImageFormat::Png
            );
            let actual = image::load_from_memory(&converted.data)
                .expect("consumer-previewable PNG")
                .to_rgb8();
            assert_eq!(actual, expected, "{source_format}: every pixel must survive");
        }
    }
}

#[cfg(feature = "ocr")]
#[test]
fn jpeg2000_images_native_preserve_source_without_warnings() {
    for source in [LOSSLESS_RGB_JP2, LOSSLESS_RGB_J2K] {
        let (images, renames, warnings) = re_encode_images(
            vec![make_image(Bytes::from_static(source), "JPEG2000")],
            ImageOutputFormat::Native,
            &SecurityLimits::default(),
            &crate::core::config::extraction::ImageExtractionConfig::default(),
        );
        assert!(warnings.is_empty());
        assert!(renames.is_empty());
        assert_eq!(images[0].data.as_ref(), source);
        assert_eq!(images[0].format.as_ref(), "JPEG2000");
    }
}

#[cfg(feature = "ocr")]
#[test]
fn malformed_jpeg2000_images_to_png_warn_and_preserve_source() {
    for source in [&LOSSLESS_RGB_JP2[..12], &LOSSLESS_RGB_J2K[..4]] {
        for source_format in ["JPEG2000", "jpg2", "jpc", "unknown"] {
            let (images, renames, warnings) = re_encode_images(
                vec![make_image(Bytes::from_static(source), source_format)],
                ImageOutputFormat::Png,
                &SecurityLimits::default(),
                &crate::core::config::extraction::ImageExtractionConfig::default(),
            );
            assert!(renames.is_empty());
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].source.as_ref(), "image_encoder");
            assert!(
                warnings[0].message.contains("failed to decode"),
                "retain the real decode failure: {:?}",
                warnings[0]
            );
            assert_eq!(images[0].data.as_ref(), source);
            assert_eq!(images[0].format.as_ref(), source_format);
        }
    }
}

#[cfg(feature = "ocr")]
#[test]
fn jpeg2000_images_to_png_security_budget_failures_warn_and_preserve_source() {
    for source in [LOSSLESS_RGB_JP2, LOSSLESS_RGB_J2K] {
        // Reject encoded input, then encoded + decoded RGB pixels, then encoding
        // peak memory. A successful header probe must not bypass any budget.
        for (max_content_size, failure) in [
            (source.len() - 1, "failed to decode"),
            (source.len() + 4 * 3 * 3 - 1, "failed to decode"),
            (source.len() + 4 * 3 * 3, "failed to encode"),
        ] {
            let limits = SecurityLimits {
                max_content_size,
                ..Default::default()
            };
            let (images, renames, warnings) = re_encode_images(
                vec![make_image(Bytes::from_static(source), "JPEG2000")],
                ImageOutputFormat::Png,
                &limits,
                &crate::core::config::extraction::ImageExtractionConfig::default(),
            );
            assert!(renames.is_empty());
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].source.as_ref(), "image_encoder");
            assert!(warnings[0].message.contains(failure), "{:?}", warnings[0]);
            assert!(
                warnings[0].message.contains("security_limits.max_content_size"),
                "{:?}",
                warnings[0]
            );
            assert_eq!(images[0].data.as_ref(), source);
            assert_eq!(images[0].format.as_ref(), "JPEG2000");
        }
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

/// Rasterizing a text-bearing SVG must render the glyphs, not a blank canvas.
///
/// An SVG member's `<text>` is the only carrier of its content, so a raster that
/// drops text (usvg without the `text` feature silently discards text nodes)
/// would make SVG→OCR extraction structurally impossible. A white canvas with
/// black text must therefore produce a non-trivial pixel histogram: this is the
/// unit-level guard for the embedded-SVG OCR path.
#[cfg(feature = "svg")]
#[test]
fn rasterize_svg_renders_text_glyphs() {
    let svg = concat!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40" viewBox="0 0 120 40">"##,
        r##"<rect width="120" height="40" fill="#ffffff"/>"##,
        r##"<text x="6" y="28" font-family="Arial" font-size="20" fill="#000000">SVGTextAlpha</text>"##,
        "</svg>",
    );
    let options = crate::core::config::extraction::SvgOptions::default();
    let (png, format) = super::rasterize_svg(svg.as_bytes(), ImageOutputFormat::Png, &options)
        .expect("rasterizing a clean SVG must succeed");

    assert_eq!(format, "png");
    let decoded = image::load_from_memory(&png).expect("rasterized SVG must decode as PNG");
    let rgba = decoded.to_rgba8();
    let non_white = rgba.pixels().filter(|p| p[0] < 240 || p[1] < 240 || p[2] < 240).count();
    assert!(
        non_white > 50,
        "text glyphs must appear in the raster ({non_white} non-white pixels); \
         a blank canvas means usvg dropped the <text> node",
    );
}

/// External references stay disabled when rasterizing for OCR: an `<image>` href
/// pointing at a network URL resolves to nothing and must not abort the render.
#[cfg(feature = "svg")]
#[test]
fn rasterize_svg_with_external_href_does_not_fetch_and_still_renders() {
    let svg = concat!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="40" height="40">"##,
        r##"<rect width="40" height="40" fill="#ffffff"/>"##,
        r##"<image x="0" y="0" width="40" height="40" xlink:href="http://127.0.0.1:9/x.png"/>"##,
        r##"<rect x="4" y="4" width="8" height="8" fill="#000000"/>"##,
        "</svg>",
    );
    let options = crate::core::config::extraction::SvgOptions::default();
    let (png, _) = super::rasterize_svg(svg.as_bytes(), ImageOutputFormat::Png, &options)
        .expect("external href must be ignored, not fetched");
    let decoded = image::load_from_memory(&png).expect("rasterized SVG must decode as PNG");
    let rgba = decoded.to_rgba8();
    let black = rgba.pixels().filter(|p| p[0] < 16 && p[1] < 16 && p[2] < 16).count();
    assert!(black >= 64, "local shapes must still render ({black} black pixels)");
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
