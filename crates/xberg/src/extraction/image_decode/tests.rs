use image::ImageEncoder;

use super::*;

fn grayscale_png(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![127_u8; usize::try_from(u64::from(width) * u64::from(height)).unwrap()];
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&pixels, width, height, image::ExtendedColorType::L8)
        .expect("encode grayscale PNG");
    bytes
}

fn decode_for_encode_under_test(
    bytes: &[u8],
    limits: &SecurityLimits,
    additional_bytes_per_pixel: u64,
) -> Result<image::DynamicImage> {
    let image = decode_standard_image_with_security_limits(bytes, limits)?;
    validate_dynamic_image_additional_live_bytes(&image, limits, additional_bytes_per_pixel, 0)?;
    Ok(image)
}

#[test]
fn encode_decode_rejects_peak_when_output_buffer_exceeds_budget() {
    let bytes = grayscale_png(10, 10);
    let limits = SecurityLimits {
        max_content_size: 399,
        ..Default::default()
    };

    let error = decode_for_encode_under_test(&bytes, &limits, 3)
        .expect_err("100-byte luma plus 300-byte encoded-output budget must exceed 399 bytes");

    assert!(matches!(error, XbergError::Validation { .. }));
}

#[test]
fn rgb_decode_rejects_peak_when_source_alone_fits() {
    let bytes = grayscale_png(10, 10);
    let limits = SecurityLimits {
        max_content_size: 399,
        ..Default::default()
    };

    let error = decode_standard_rgb8_with_security_limits(&bytes, &limits)
        .expect_err("100-byte luma plus 300-byte RGB conversion must exceed 399 bytes");

    assert!(matches!(error, XbergError::Validation { .. }));
    assert!(error.to_string().contains("live image-processing bytes"));
}

#[test]
fn rgb_decode_accepts_peak_at_exact_budget() {
    let bytes = grayscale_png(10, 10);
    let exact_peak = bytes.len() + 400;
    let limits = SecurityLimits {
        max_content_size: exact_peak,
        ..Default::default()
    };

    let rgb =
        decode_standard_rgb8_with_security_limits(&bytes, &limits).expect("the exact source-plus-RGB peak must fit");

    assert_eq!(rgb.dimensions(), (10, 10));
    assert_eq!(rgb.as_raw().len(), 300);
}

#[test]
fn rgb_decode_counts_still_live_encoded_input() {
    let bytes = grayscale_png(10, 10);
    let limits = SecurityLimits {
        max_content_size: bytes.len() + 399,
        ..Default::default()
    };

    let error = decode_standard_rgb8_with_security_limits(&bytes, &limits)
        .expect_err("encoded input must remain in the live peak during pixel conversion");

    assert!(matches!(error, XbergError::Validation { .. }));
}

#[test]
fn luma_decode_rejects_peak_when_rgba_source_alone_fits() {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[0_u8; 400], 10, 10, image::ExtendedColorType::Rgba8)
        .expect("encode RGBA PNG");
    let limits = SecurityLimits {
        max_content_size: 499,
        ..Default::default()
    };

    let error = decode_standard_luma8_with_security_limits(&bytes, &limits)
        .expect_err("400-byte RGBA plus 100-byte luma conversion must exceed 499 bytes");

    assert!(matches!(error, XbergError::Validation { .. }));
    assert!(error.to_string().contains("live image-processing bytes"));
}

fn budget_of(max_content_size: usize) -> ImageDecodeBudget {
    ImageDecodeBudget::from_security_limits(&SecurityLimits {
        max_content_size,
        ..Default::default()
    })
}

#[test]
fn budget_admits_a_pixel_count_over_budget_when_the_byte_count_fits() {
    // GH#1761 crossing point: the budget is a number of BYTES, so a
    // 10_000-pixel image whose decoded form is 500 bytes is within a
    // 1_000-byte budget. Comparing the pixel count against it rejected
    // this. Every caller today also passes a byte count >= the pixel
    // count, so only a test at the crossing point can see the unit
    // mismatch. ~keep
    budget_of(1_000)
        .validate(100, 100, 500)
        .expect("10_000 pixels decoding to 500 bytes fits a 1_000-byte budget");
}

#[test]
fn budget_still_rejects_a_byte_count_over_budget() {
    let error = budget_of(1_000)
        .validate(10, 10, 1_001)
        .expect_err("1_001 bytes must not fit a 1_000-byte budget");

    assert!(matches!(error, XbergError::Validation { .. }));
    assert!(error.to_string().contains("live image-processing bytes"));
}

#[test]
fn budget_accepts_a_byte_count_exactly_at_the_budget() {
    budget_of(1_000)
        .validate(10, 10, 1_000)
        .expect("a byte count exactly at the budget must fit");
}

#[test]
fn budget_rejects_a_zero_dimension() {
    // `validate` doubles as the zero-dimension guard; a zero extent is
    // malformed input regardless of how few bytes it claims. ~keep
    assert!(budget_of(1_000).validate(0, 10, 1).is_err());
    assert!(budget_of(1_000).validate(10, 0, 1).is_err());
    assert!(budget_of(1_000).validate(0, 0, 0).is_err());
}

#[test]
fn budget_handles_the_largest_representable_dimension_product() {
    // u32::MAX squared is 18_446_744_065_119_617_025, just under
    // u64::MAX, so widening both extents to u64 makes the product
    // unrepresentable-by-construction impossible. The `checked_mul` in
    // `validate` is therefore a backstop for a future widening of the
    // dimension types, not a branch reachable from u32 input — this test
    // pins the arithmetic that makes it unreachable, and shows such an
    // image is still refused on its byte count. ~keep
    assert!(u64::from(u32::MAX).checked_mul(u64::from(u32::MAX)).is_some());

    let error = budget_of(1_000)
        .validate(u32::MAX, u32::MAX, u64::MAX)
        .expect_err("a maximal image's byte count must exceed the budget");

    assert!(matches!(error, XbergError::Validation { .. }));
}

/// The PNM family's six representations (P1–P3 ASCII, P4–P6 binary) all decode,
/// and a raster shorter than the header declares fails with a parsing error
/// instead of a panic.
///
/// The truncated-P6 case pins the observed boundary of the acceptance corpus:
/// `P6 3 3 255` needs 27 raster bytes, so 9 bytes are truncated input, and the
/// documented contract for damaged input is a graceful, diagnosable failure.
#[test]
fn pnm_family_decodes_all_six_representations_and_truncated_p6_fails_gracefully() {
    let limits = SecurityLimits::default();
    let cases: [(&str, Vec<u8>); 6] = [
        ("P1", b"P1\n3 3\n0 1 0\n1 0 1\n0 1 0\n".to_vec()),
        ("P2", b"P2\n3 3\n255\n0 127 0\n127 0 127\n0 127 0\n".to_vec()),
        (
            "P3",
            b"P3\n3 3\n255\n0 0 0 0 255 0 0 0 255\n255 0 0 0 255 0 0 0 0\n0 0 255 0 255 0 255 0 0\n".to_vec(),
        ),
        ("P4", b"P4\n3 3\n\x40\xa0\x40".to_vec()),
        ("P5", b"P5\n3 3\n255\n\x00\x7f\x00\x7f\x00\x7f\x00\x7f\x00".to_vec()),
        ("P6", {
            let mut p6 = b"P6\n3 3\n255\n".to_vec();
            p6.extend(vec![0u8, 128, 64].repeat(9));
            p6
        }),
    ];

    for (magic, bytes) in cases {
        let decoded = decode_standard_image_with_security_limits(&bytes, &limits);
        let image = decoded.unwrap_or_else(|error| panic!("{magic} fixture must decode: {error}"));
        assert_eq!((image.width(), image.height()), (3, 3), "{magic} dimensions");
    }

    // 9 raster bytes for a 3×3 RGB raster (needs 27): truncated input.
    let mut truncated = b"P6\n3 3\n255\n".to_vec();
    truncated.extend(vec![0u8, 128, 64].repeat(3));
    let result = decode_standard_image_with_security_limits(&truncated, &limits);
    assert!(result.is_err(), "truncated P6 raster must fail, not silently succeed");
    let message = result.err().map(|error| error.to_string()).unwrap_or_default();
    assert!(!message.is_empty(), "truncated P6 must carry a diagnostic message");
}
