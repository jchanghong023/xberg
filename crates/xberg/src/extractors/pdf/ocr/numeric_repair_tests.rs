//! Unit tests for GH#1789 numeric-token repair (dropped thousands separators, a decimal point
//! misread for a grouping comma, and one number split into two tokens). Split out of
//! `recognition_noise_tests.rs` to keep that file under poly's file-length lint; a sibling
//! module in the same style, registered the same way in `mod.rs`.

use super::pipeline::*;
use super::scoring::*;

#[test]
fn should_add_thousands_separators_to_a_bare_integer() {
    assert_eq!(
        repair_ocr_numeric_tokens("Total: 1172 units").as_ref(),
        "Total: 1,172 units"
    );
}

#[test]
fn should_repair_nine_digit_integers_but_not_ten() {
    assert_eq!(
        repair_ocr_numeric_tokens("123456789 units").as_ref(),
        "123,456,789 units"
    );
    // Ten digits is outside the rule's 4-9 digit window and must be left alone.
    assert_eq!(
        repair_ocr_numeric_tokens("1234567890 units").as_ref(),
        "1234567890 units"
    );
}

#[test]
fn should_not_add_separators_to_a_three_digit_number() {
    // Below the 4-digit floor: too common as a quantity or short id.
    assert_eq!(repair_ocr_numeric_tokens("Qty: 172 units").as_ref(), "Qty: 172 units");
}

#[test]
fn should_not_touch_a_year_after_fy() {
    assert_eq!(repair_ocr_numeric_tokens("FY 2025 Budget").as_ref(), "FY 2025 Budget");
}

#[test]
fn should_not_touch_a_concatenated_fiscal_year_header() {
    // "FY2025-26": the leading 'Y' already blocks the left side, and the trailing '-26'
    // blocks the right side even before the FY-specific check is consulted.
    assert_eq!(repair_ocr_numeric_tokens("FY2025-26").as_ref(), "FY2025-26");
}

#[test]
fn should_not_touch_a_number_running_into_a_slash_or_hyphen() {
    assert_eq!(repair_ocr_numeric_tokens("2024-2025 term").as_ref(), "2024-2025 term");
    assert_eq!(repair_ocr_numeric_tokens("case 1234/5678").as_ref(), "case 1234/5678");
}

#[test]
fn should_not_touch_a_number_followed_by_percent() {
    assert_eq!(
        repair_ocr_numeric_tokens("growth of 1234% this year").as_ref(),
        "growth of 1234% this year"
    );
}

#[test]
fn should_not_touch_a_number_already_inside_a_larger_token() {
    for text in ["invoice-11723456", "part11234567", "id_1234567"] {
        assert_eq!(
            repair_ocr_numeric_tokens(text).as_ref(),
            text,
            "rewrote a compound token: {text}"
        );
    }
}

#[test]
fn should_preserve_parentheses_around_a_negative_amount() {
    assert_eq!(
        repair_ocr_numeric_tokens("Vacancy Savings (2100)").as_ref(),
        "Vacancy Savings (2,100)"
    );
}

#[test]
fn should_repair_a_decimal_point_misread_for_a_grouping_comma() {
    assert_eq!(
        repair_ocr_numeric_tokens("Utility Users Tax 7.812").as_ref(),
        "Utility Users Tax 7,812"
    );
    assert_eq!(
        repair_ocr_numeric_tokens("Total 812.812 units").as_ref(),
        "Total 812,812 units"
    );
}

#[test]
fn should_not_touch_a_genuine_two_digit_decimal() {
    // Exactly three fractional digits is the signature of a misread grouping comma; two is
    // an ordinary decimal amount and must be left alone.
    assert_eq!(
        repair_ocr_numeric_tokens("Rate: 7.81 percent").as_ref(),
        "Rate: 7.81 percent"
    );
}

#[test]
fn should_not_touch_a_percentage_shaped_like_the_period_rule() {
    assert_eq!(repair_ocr_numeric_tokens("Yield 7.812%").as_ref(), "Yield 7.812%");
}

#[test]
fn should_not_touch_a_four_digit_leading_group_before_the_point() {
    // \d{1,3} caps the leading group at 3 digits; 1234.567 is an ordinary decimal.
    assert_eq!(repair_ocr_numeric_tokens("1234.567 total").as_ref(), "1234.567 total");
}

#[test]
fn should_join_a_digit_split_from_its_grouped_remainder_by_a_gap() {
    assert_eq!(
        repair_ocr_numeric_tokens("Services 2 2,411 total").as_ref(),
        "Services 22,411 total"
    );
}

#[test]
fn should_not_join_across_more_than_one_space() {
    assert_eq!(
        repair_ocr_numeric_tokens("row 2  2,411 total").as_ref(),
        "row 2  2,411 total"
    );
}

#[test]
fn should_not_join_when_a_third_digit_follows_the_grouped_remainder() {
    // The lookahead forbids a trailing digit, so "2 2,4111" is left as two tokens.
    assert_eq!(repair_ocr_numeric_tokens("2 2,4111").as_ref(), "2 2,4111");
}

#[test]
fn should_apply_join_before_separator_so_the_reassembled_number_is_not_re_split() {
    // The join produces "22,411", already correctly grouped; the separator rule must not
    // then treat the post-join text as a fresh 5-digit bare integer anywhere else in it.
    let text = "row 2 2,411 and another 55411 elsewhere";
    assert_eq!(
        repair_ocr_numeric_tokens(text).as_ref(),
        "row 22,411 and another 55,411 elsewhere"
    );
}

#[test]
fn should_leave_a_well_formed_table_alone_and_avoid_allocating() {
    let text = "Property Tax 48,210 49,850 51,344\nSales Tax 21,406 21,977 22,636";
    let out = repair_ocr_numeric_tokens(text);
    assert!(
        matches!(out, std::borrow::Cow::Borrowed(_)),
        "must not allocate when nothing is broken"
    );
    assert_eq!(out.as_ref(), text);
}

#[test]
fn should_not_touch_ordinary_prose_numbers() {
    for text in [
        "Section 3.5 of the ordinance",
        "approximately 0.7906 acres",
        "phone 555.123.4567",
        "lot area 2,842 sf",
    ] {
        assert_eq!(repair_ocr_numeric_tokens(text).as_ref(), text, "rewrote prose: {text}");
    }
}

/// A bare 4-digit year with no `FY ` prefix is indistinguishable from a genuine 4-digit
/// amount to the separator rule alone -- this is exactly the limitation the issue's own
/// "Suggested fix" names ("in prose, a four-digit number is often a year or an identifier")
/// and why the repair is opt-in rather than a safe default. This test documents the
/// behavior rather than asserting a false safety property: enabling `numeric_repair` on
/// prose-heavy documents can rewrite a year.
#[test]
fn should_group_a_bare_year_with_no_fy_prefix_documenting_the_known_limitation() {
    assert_eq!(
        repair_ocr_numeric_tokens("the year 2024 saw growth").as_ref(),
        "the year 2,024 saw growth"
    );
}

/// End-to-end reproduction of the issue's own `repair.py` example: the reference script
/// reports `REPAIRED spj {'s': 1, 'p': 3, 'j': 0}` over the synthetic shaded-rows page and
/// zero corruptions across 165 repaired outputs. This is the same three-rule composition
/// applied to a page-shaped block of already-correct financial values plus the three
/// documented failure shapes, asserting every already-correct value in the block is
/// byte-identical before and after (the zero-corruption property, checked directly rather
/// than by a downstream word-count proxy).
#[test]
fn should_repair_every_documented_failure_shape_without_touching_correct_values() {
    let before = "\
PROPERTY TAX
48,210 49,850 51,344 52,885 54,471 56,105
SALES TAX
21,406 21,977 22,636 23,315 24,015 24,735
UTILITY USERS TAX
7.812 7,968 8,127 8,290 8,456 8,625
FRANCHISE FEES
3104 3,197 3,293 3,391 3,493 3,598
SUBTOTAL TAXES
80,532 82,992 85,400 87,881 90,435 93,063
SERVICES
19,334 19,914 20,511 21,126 2 1,759 22,411
FY2025-26 FY2026-27
BOND REVENUE
1,500 1,545 1,591 1,639 1,688 1,739";
    let expected = "\
PROPERTY TAX
48,210 49,850 51,344 52,885 54,471 56,105
SALES TAX
21,406 21,977 22,636 23,315 24,015 24,735
UTILITY USERS TAX
7,812 7,968 8,127 8,290 8,456 8,625
FRANCHISE FEES
3,104 3,197 3,293 3,391 3,493 3,598
SUBTOTAL TAXES
80,532 82,992 85,400 87,881 90,435 93,063
SERVICES
19,334 19,914 20,511 21,126 21,759 22,411
FY2025-26 FY2026-27
BOND REVENUE
1,500 1,545 1,591 1,639 1,688 1,739";
    let repaired = repair_ocr_numeric_tokens(before);
    assert_eq!(repaired.as_ref(), expected);

    // Zero-corruption check: every already-well-formed value token present in `before` must
    // still be present, unchanged, in the repaired output -- this is the exact property the
    // issue measured as "no correct value was changed to a wrong one", checked directly
    // rather than inferred from a length-sensitive ratio (per this repo's own
    // measurement-discipline rule).
    let already_correct = [
        "48,210", "49,850", "51,344", "52,885", "54,471", "56,105", "21,406", "21,977", "22,636", "23,315", "24,015",
        "24,735", "7,968", "8,127", "8,290", "8,456", "8,625", "3,197", "3,293", "3,391", "3,493", "3,598", "80,532",
        "82,992", "85,400", "87,881", "90,435", "93,063", "19,334", "19,914", "20,511", "21,126", "1,500", "1,545",
        "1,591", "1,639", "1,688", "1,739",
    ];
    for value in already_correct {
        assert!(
            repaired.matches(value).count() >= before.matches(value).count(),
            "already-correct value {value} was lost or altered by the repair"
        );
    }
}

#[test]
fn numeric_repair_enabled_defaults_to_disabled() {
    let config = crate::core::config::ExtractionConfig {
        ocr: Some(crate::core::config::OcrConfig::default()),
        ..Default::default()
    };
    assert!(!numeric_repair_enabled(&config));
    // No `ocr` config at all must also read as disabled, not panic or default-enable.
    assert!(!numeric_repair_enabled(
        &crate::core::config::ExtractionConfig::default()
    ));
}

#[test]
fn numeric_repair_enabled_reads_the_ocr_config_flag() {
    let config = crate::core::config::ExtractionConfig {
        ocr: Some(crate::core::config::OcrConfig {
            numeric_repair: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(numeric_repair_enabled(&config));
}

/// GH#1836: the separator rule rewrites any bare 4-9 digit integer whose neighbours are not word
/// characters, and an hOCR `bbox` is exactly that shape. Documents the corruption the
/// `ocr_content_is_repairable_prose` guard exists to prevent, on the renderer's real output shape
/// rather than on a contrived string -- at the default 300 dpi a Letter page is 2550x3300 px, so
/// four-digit coordinates are the norm. Not a gate on desired behaviour: the repair is *correct*
/// here in isolation, which is why the fix is to not run it on markup at all.
#[test]
fn the_separator_rule_would_rewrite_hocr_bbox_coordinates() {
    assert_eq!(
        repair_ocr_numeric_tokens("<span class='ocrx_word' title='bbox 1234 567 1456 612; x_wconf 96'>Total</span>")
            .as_ref(),
        "<span class='ocrx_word' title='bbox 1,234 567 1,456 612; x_wconf 96'>Total</span>",
        "the three-digit coordinates must stay bare and the four-digit ones must be grouped, \
         which is what makes this corrupting on real hOCR"
    );
}

/// GH#1836: the same shape in Tesseract's TSV word table, where the delimiter is a tab.
#[test]
fn the_separator_rule_would_rewrite_tsv_coordinate_columns() {
    assert_eq!(
        repair_ocr_numeric_tokens("5\t1\t1\t1\t1\t1234\t2048\t96\t21\t96.5\tTotal").as_ref(),
        "5\t1\t1\t1\t1\t1,234\t2,048\t96\t21\t96.5\tTotal"
    );
}

/// GH#1836: a markup renderer must switch the repair off entirely, because on these routes the
/// OCR "text" *is* the markup. An allowlist, so a renderer added later is excluded by default.
#[test]
fn only_prose_renderers_are_repairable() {
    let with_format = |output_format: &str| crate::core::config::OcrConfig {
        tesseract_config: Some(crate::types::TesseractConfig {
            output_format: output_format.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(ocr_content_is_repairable_prose(&with_format("text")));
    assert!(ocr_content_is_repairable_prose(&with_format("markdown")));
    assert!(!ocr_content_is_repairable_prose(&with_format("hocr")));
    assert!(!ocr_content_is_repairable_prose(&with_format("tsv")));
    // An unrecognized renderer defaults to "not prose" rather than being silently corrupted.
    assert!(!ocr_content_is_repairable_prose(&with_format("alto")));
    // No `tesseract_config` at all cannot have selected a markup renderer: the default is
    // `"markdown"` (`types::formats::TesseractConfig::default`).
    assert!(ocr_content_is_repairable_prose(
        &crate::core::config::OcrConfig::default()
    ));
}

/// GH#1836 regression gate: `numeric_repair = true` together with a markup renderer must read as
/// disabled. Fails on unfixed code, where `numeric_repair_enabled` consulted only the flag.
#[test]
fn numeric_repair_enabled_is_false_for_a_markup_renderer() {
    let with_format = |output_format: &str| crate::core::config::ExtractionConfig {
        ocr: Some(crate::core::config::OcrConfig {
            numeric_repair: true,
            tesseract_config: Some(crate::types::TesseractConfig {
                output_format: output_format.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(!numeric_repair_enabled(&with_format("hocr")));
    assert!(!numeric_repair_enabled(&with_format("tsv")));
    // The control: the same flag on a prose renderer is still honoured, so this is not just
    // turning the feature off.
    assert!(numeric_repair_enabled(&with_format("markdown")));
    assert!(numeric_repair_enabled(&with_format("text")));
}
