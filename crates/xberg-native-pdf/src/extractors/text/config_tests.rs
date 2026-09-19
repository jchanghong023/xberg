//! Configuration and marked-content tests for [`super`].
//!
//! Split out of `extractors/text.rs` for file size: the parent was 16,288 lines
//! (673 KiB), over the repository's 500 KiB file-safety limit. A child module sees
//! the parent's private items exactly as the inline module did. ~keep

use super::*;

#[test]
fn test_space_threshold_default() {
    let config = TextExtractionConfig::new();
    assert_eq!(config.space_insertion_threshold, -120.0);

    let extractor = TextExtractor::new();
    assert_eq!(extractor.config.space_insertion_threshold, -120.0);
}

#[test]
fn test_space_threshold_custom() {
    let config = TextExtractionConfig::with_space_threshold(-80.0);
    assert_eq!(config.space_insertion_threshold, -80.0);

    let extractor = TextExtractor::with_config(config);
    assert_eq!(extractor.config.space_insertion_threshold, -80.0);
}

#[test]
fn test_space_threshold_disabled() {
    // Test that threshold can be disabled with NEG_INFINITY ~keep
    let config = TextExtractionConfig::with_space_threshold(f32::NEG_INFINITY);
    assert_eq!(config.space_insertion_threshold, f32::NEG_INFINITY);

    let extractor = TextExtractor::with_config(config);
    assert_eq!(extractor.config.space_insertion_threshold, f32::NEG_INFINITY);
}

#[test]
fn test_adaptive_enabled_by_default() {
    let config = SpanMergingConfig::default();
    assert!(
        config.use_adaptive_threshold,
        "Adaptive threshold should be enabled by default"
    );
}

#[test]
fn test_legacy_mode_disables_adaptive() {
    let legacy = SpanMergingConfig::legacy();
    assert!(
        !legacy.use_adaptive_threshold,
        "Legacy mode should disable adaptive threshold"
    );
    assert_eq!(legacy.conservative_threshold_pt, 0.1);
}

#[test]
fn test_adaptive_constructor_enables_adaptive() {
    let adaptive = SpanMergingConfig::adaptive();
    assert!(
        adaptive.use_adaptive_threshold,
        "Adaptive constructor should enable adaptive threshold"
    );
    assert!(
        adaptive.adaptive_config.is_some(),
        "Adaptive constructor should set adaptive_config"
    );
}

// ============================================================================
// Artifact Type Parsing Tests (PDF Spec Section 14.8.2.2)
// ============================================================================ ~keep

#[test]
fn test_parse_artifact_type_pagination_header() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Header".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Pagination(PaginationSubtype::Header)));
}

#[test]
fn test_parse_artifact_type_pagination_footer() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Footer".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Pagination(PaginationSubtype::Footer)));
}

#[test]
fn test_parse_artifact_type_pagination_watermark() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Pagination".to_string()));
    props.insert("Subtype".to_string(), Object::Name("Watermark".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Pagination(PaginationSubtype::Watermark)));
}

#[test]
fn test_parse_artifact_type_layout() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Layout".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Layout));
}

#[test]
fn test_parse_artifact_type_background() {
    let mut props = HashMap::new();
    props.insert("Type".to_string(), Object::Name("Background".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Background));
}

#[test]
fn test_parse_artifact_type_subtype_only() {
    // Some PDFs use /Subtype without /Type ~keep
    let mut props = HashMap::new();
    props.insert("Subtype".to_string(), Object::Name("Header".to_string()));

    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, Some(ArtifactType::Pagination(PaginationSubtype::Header)));
}

#[test]
fn test_parse_artifact_type_empty() {
    let props = HashMap::new();
    let result = TextExtractor::parse_artifact_type(&props);
    assert_eq!(result, None);
}

// ============================================================================
// ActualText Verification Tests (PDF Spec Section 14.9.4)
// ============================================================================
//
// ActualText provides replacement text for content that cannot be accurately
// represented by the content stream (ligatures, decorated glyphs, formulas).
// Per ISO 32000-1:2008 Section 14.9.4, ActualText takes precedence over
// character extraction. ~keep

#[test]
fn test_marked_content_context_with_actual_text() {
    let ctx = MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("fi".to_string()), // Ligature expansion ~keep
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert_eq!(ctx.actual_text, Some("fi".to_string()));
    assert!(!ctx.is_artifact);
}

#[test]
fn test_marked_content_context_with_expansion() {
    let ctx = MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: Some("Portable Document Format".to_string()),
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert_eq!(ctx.expansion, Some("Portable Document Format".to_string()));
}

#[test]
fn test_marked_content_context_artifact_with_actual_text() {
    // Verify artifacts can have ActualText (though typically they don't) ~keep
    let ctx = MarkedContentContext {
        tag: "Artifact".to_string(),
        is_artifact: true,
        artifact_type: Some(ArtifactType::Pagination(PaginationSubtype::Header)),
        actual_text: Some("Header text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    };

    assert!(ctx.is_artifact);
    assert_eq!(ctx.actual_text, Some("Header text".to_string()));
}

#[test]
fn test_get_current_actual_text_finds_first() {
    let mut extractor = TextExtractor::new();

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("outer text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("inner text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    // Should return innermost (most recent) ActualText ~keep
    let result = extractor.get_current_actual_text();
    assert_eq!(result, Some("inner text".to_string()));
}

#[test]
fn test_get_current_actual_text_skips_none() {
    let mut extractor = TextExtractor::new();

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: Some("replacement text".to_string()),
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    extractor.marked_content_stack.push(MarkedContentContext {
        artifact_type: None,
        tag: "Span".to_string(),
        is_artifact: false,
        actual_text: None,
        expansion: None,
        is_excluded_layer: false,
        is_placed_pdf: false,
        actual_text_emitted: false,
        own_mcid: None,
    });

    // Should find the ActualText from outer context ~keep
    let result = extractor.get_current_actual_text();
    assert_eq!(result, Some("replacement text".to_string()));
}

#[test]
fn test_get_current_actual_text_returns_none_when_empty() {
    let extractor = TextExtractor::new();

    let result = extractor.get_current_actual_text();
    assert_eq!(result, None);
}

#[cfg(test)]
mod profile_based_space_tests {
    use super::*;

    /// Test that ACADEMIC profile uses aggressive thresholds
    ///
    /// Academic papers have tight spacing (especially around punctuation).
    /// The profile should:
    /// - Use lower TJ offset threshold (-90 instead of -120)
    /// - Use lower word margin ratio (0.12 instead of 0.1)
    /// - Enable adaptive threshold for dynamic adjustment
    #[test]
    fn test_academic_profile_thresholds() {
        let profile = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);

        assert!(
            profile.tj_offset_threshold < -100.0,
            "Academic should use lower TJ threshold for more spaces"
        );

        assert!(
            profile.word_margin_ratio <= 0.15,
            "Academic should use conservative word margin"
        );

        let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
        assert_eq!(config.space_insertion_threshold, profile.tj_offset_threshold);
    }

    /// Test that POLICY profile uses conservative thresholds
    ///
    /// Policy documents (like GDPR) have justified text with precise spacing.
    /// The profile should:
    /// - Use higher TJ offset threshold (-110 to preserve structure)
    /// - Use higher word margin ratio (0.18-0.2 for justified text)
    /// - Preserve column boundaries and table structure
    #[test]
    fn test_policy_profile_thresholds() {
        let profile = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);

        assert!(
            profile.tj_offset_threshold > -120.0,
            "Policy should use higher TJ threshold to avoid over-spacing"
        );

        assert!(
            profile.word_margin_ratio >= 0.15,
            "Policy should use higher word margin for justified text"
        );

        let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
        assert_eq!(config.space_insertion_threshold, profile.tj_offset_threshold);
    }

    /// Test that FORM profile preserves field boundaries
    ///
    /// Forms have checkboxes, fields, and precise layout.
    /// The profile should:
    /// - Use conservative thresholds to avoid merging fields
    /// - High column boundary threshold to preserve structure
    /// - Enable adaptive threshold for form field detection
    #[test]
    fn test_form_profile_thresholds() {
        let profile = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Form);

        assert!(
            profile.tj_offset_threshold >= -120.0,
            "Form profile should be conservative with space insertion"
        );

        let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);
        assert_eq!(config.space_insertion_threshold, profile.tj_offset_threshold);
    }

    /// Test that profile selection works correctly for document types
    #[test]
    fn test_profile_selection_for_document_types() {
        let academic = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);
        let policy = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);
        let form = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Form);
        let mixed = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Mixed);

        let thresholds = [
            academic.tj_offset_threshold,
            policy.tj_offset_threshold,
            form.tj_offset_threshold,
            mixed.tj_offset_threshold,
        ];

        let unique_count = thresholds
            .iter()
            .filter(|t| !thresholds.iter().skip(1).any(|other| other == *t))
            .count();

        assert!(
            unique_count > 0,
            "Profiles should have different thresholds for different document types"
        );
    }

    /// Test that TextExtractionConfig can accept a profile
    #[test]
    fn test_config_with_profile() {
        let profile = crate::config::ExtractionProfile::ACADEMIC;

        let config = TextExtractionConfig::with_space_threshold(profile.tj_offset_threshold);

        assert_eq!(config.space_insertion_threshold, profile.tj_offset_threshold);
    }

    /// Test that profiles have reasonable threshold ranges
    #[test]
    fn test_profile_thresholds_in_reasonable_range() {
        let profiles = vec![
            crate::config::ExtractionProfile::CONSERVATIVE,
            crate::config::ExtractionProfile::ACADEMIC,
            crate::config::ExtractionProfile::POLICY,
            crate::config::ExtractionProfile::FORM,
        ];

        for profile in profiles {
            // TJ offsets should be negative (per PDF spec) ~keep
            assert!(
                profile.tj_offset_threshold < 0.0,
                "TJ threshold must be negative ({})",
                profile.name
            );

            // Should be in reasonable range (-150 to -50) ~keep
            assert!(
                profile.tj_offset_threshold >= -150.0 && profile.tj_offset_threshold <= -50.0,
                "TJ threshold out of range for {} ({})",
                profile.name,
                profile.tj_offset_threshold
            );

            // Word margin ratios should be positive and reasonable (0.05 to 0.25) ~keep
            assert!(
                profile.word_margin_ratio > 0.0 && profile.word_margin_ratio < 1.0,
                "Word margin ratio must be between 0 and 1 for {}",
                profile.name
            );

            assert!(
                profile.space_threshold_em_ratio > 0.0,
                "Space threshold EM ratio must be positive for {}",
                profile.name
            );
        }
    }

    /// Test that multiple profiles can coexist
    #[test]
    fn test_multiple_profiles_independent() {
        let academic = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Academic);
        let policy = crate::config::ExtractionProfile::for_document_type(crate::config::DocumentType::Policy);

        let academic_config = TextExtractionConfig::with_space_threshold(academic.tj_offset_threshold);
        let policy_config = TextExtractionConfig::with_space_threshold(policy.tj_offset_threshold);

        assert_ne!(
            academic_config.space_insertion_threshold, policy_config.space_insertion_threshold,
            "Academic and policy configs should have different thresholds"
        );
    }

    /// Test that default config is backward-compatible
    #[test]
    fn test_default_config_backward_compatible() {
        let default_config = TextExtractionConfig::default();
        let conservative_profile = crate::config::ExtractionProfile::CONSERVATIVE;

        assert_eq!(
            default_config.space_insertion_threshold, conservative_profile.tj_offset_threshold,
            "Default config should use conservative threshold for backward compatibility"
        );
    }

    /// Test that adjacent table cell values get spaces inserted between them.
    ///
    /// Simulates a form where two "$0.00" values are in adjacent cells with
    /// a small positive gap (1pt). The merge logic should insert a space because
    /// the spans are clearly separate tokens (ending/starting with digits/currency).
    #[test]
    fn test_adjacent_table_cell_values_not_concatenated() {
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        // "$0.00" at 10pt font is about 30pt wide (5 chars * ~6pt average width)
        // Second value starts at x=131, creating a 1pt gap (100 + 30 = 130, gap = 1pt) ~keep
        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "$0.00".to_string(),
                bbox: Rect::new(100.0, 700.0, 30.0, 10.0),
                font_name: "F1".to_string(),
                font_size: 10.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "$0.00".to_string(),
                bbox: Rect::new(131.0, 700.0, 30.0, 10.0), // 1pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 10.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
        assert_eq!(
            extractor.spans[0].text, "$0.00 $0.00",
            "Adjacent table cell values should have space between them, got: '{}'",
            extractor.spans[0].text
        );
    }

    /// Test that adjacent numeric values with small gaps get spaces.
    /// Covers cases like "100200" that should be "100 200" in table contexts.
    #[test]
    fn test_adjacent_numeric_values_not_concatenated() {
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "100".to_string(),
                bbox: Rect::new(200.0, 500.0, 18.0, 10.0),
                font_name: "F1".to_string(),
                font_size: 10.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "200".to_string(),
                bbox: Rect::new(219.5, 500.0, 18.0, 10.0), // 1.5pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 10.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
        assert_eq!(
            extractor.spans[0].text, "100 200",
            "Adjacent numeric values should have space between them, got: '{}'",
            extractor.spans[0].text
        );
    }

    /// Ensure that true word fragments (zero gap) still merge without space.
    /// E.g., "Hel" + "lo" with gap=0 should become "Hello" not "Hel lo".
    #[test]
    fn test_word_fragments_zero_gap_no_space() {
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "Hel".to_string(),
                bbox: Rect::new(100.0, 700.0, 18.0, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "lo".to_string(),
                bbox: Rect::new(118.0, 700.0, 12.0, 12.0), // 0pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(extractor.spans.len(), 1, "Adjacent spans should merge");
        assert_eq!(
            extractor.spans[0].text, "Hello",
            "Zero-gap word fragments should merge without space, got: '{}'",
            extractor.spans[0].text
        );
    }

    #[test]
    fn test_merge_decimal_dollar_value_split_boxes() {
        // Some forms have integer and decimal parts in separate fixed-width boxes.
        // e.g., "123456" at x=382.3 width=39.6, "72" at x=432.7 width=13.2
        // gap = 432.7 - (382.3 + 39.6) = 10.8pt
        // These should be merged as "123456.72" ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "123456".to_string(),
                bbox: Rect::new(382.3, 700.0, 39.6, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "72".to_string(),
                bbox: Rect::new(432.7, 700.0, 13.2, 12.0), // 10.8pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(
            extractor.spans.len(),
            1,
            "Decimal dollar value spans should merge into one"
        );
        assert_eq!(
            extractor.spans[0].text, "123456.72",
            "Integer and decimal parts should be joined with '.'"
        );
    }

    #[test]
    fn test_merge_decimal_value_small_integer_part() {
        // Smaller dollar amount: "50" + "00" -> "50.00" ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "50".to_string(),
                bbox: Rect::new(382.3, 700.0, 15.0, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "00".to_string(),
                bbox: Rect::new(407.0, 700.0, 13.2, 12.0), // 9.7pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(extractor.spans.len(), 1);
        assert_eq!(extractor.spans[0].text, "50.00");
    }

    #[test]
    fn test_no_decimal_merge_for_non_digit_spans() {
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "Hello".to_string(),
                bbox: Rect::new(382.3, 700.0, 39.6, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "72".to_string(),
                bbox: Rect::new(432.7, 700.0, 13.2, 12.0), // 10.8pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        // Should NOT merge because first span is not all digits ~keep
        assert_eq!(
            extractor.spans.len(),
            2,
            "Non-digit spans should not be merged as decimal values"
        );
    }

    #[test]
    fn test_no_decimal_merge_for_long_decimal_part() {
        // Should NOT merge "123456" + "723" (3-digit decimal part is not a cents pattern) ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "123456".to_string(),
                bbox: Rect::new(382.3, 700.0, 39.6, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "723".to_string(),
                bbox: Rect::new(432.7, 700.0, 18.0, 12.0), // 10.8pt gap ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        // Should NOT merge because decimal part has 3 digits (not a cents pattern) ~keep
        assert_eq!(
            extractor.spans.len(),
            2,
            "3-digit decimal part should not trigger decimal merge"
        );
    }

    /// Compact TextSpan builder for the intervening-ink decimal tests.
    fn digit_test_span(text: &str, bbox: Rect, font_size: f32) -> TextSpan {
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: text.to_string(),
            bbox,
            font_name: "F1".to_string(),
            font_size,
            font_weight: FontWeight::Normal,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            split_boundary_before: false,
            offset_semantic: false,
            is_italic: false,
            is_monospace: false,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            primary_detected: false,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: 0.0,
            wmode: 0,
            rtl_draw_logical: false,
            mirrored: false,
            page_rotation_applied: 0,
        }
    }

    #[test]
    fn test_no_decimal_merge_with_intervening_comma_glyph() {
        // Subscript index pairs (`P_{1,0}`, `i_2, i_4`) place two small
        // digit runs a split-box-sized gap apart WITH the separating comma
        // drawn between them — often later in the content stream, so the
        // digit spans are sequence-adjacent. Ink inside the gap proves the
        // digits are separate tokens: a genuine split-box amount has empty
        // space between its boxes. Gap here: 110.0 - 103.5 = 6.5pt at 7pt
        // font = 0.93x — squarely inside the genuine split-box band, so a
        // gap ceiling alone cannot reject it. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            digit_test_span("1", Rect::new(100.0, 700.0, 3.5, 7.0), 7.0),
            digit_test_span("0", Rect::new(110.0, 700.0, 3.5, 7.0), 7.0),
            // The comma, drawn after both digits in the content stream but
            // sitting geometrically inside the gap. ~keep
            digit_test_span(",", Rect::new(104.8, 699.0, 1.8, 3.0), 7.0),
        ];

        extractor.merge_adjacent_spans();
        assert!(
            !extractor.spans.iter().any(|s| s.text.contains("1.0")),
            "digits separated by a drawn comma must not merge into a decimal, got {:?}",
            extractor.spans.iter().map(|s| s.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_decimal_merge_across_font_sizes() {
        // Scientific notation in table rows: the exponent digit of one
        // value ("10^-4" drawn as "10" + superscript "4") and the mantissa
        // digit of the NEXT value ("3 ...") are both pure-digit runs a
        // split-box-sized gap apart, and were fused into a fabricated
        // decimal ("4 . 10-4 3 . 10-4" -> "4 . 10-4.3 . 10-4"). A genuine
        // split-box amount prints both halves at the SAME size; an
        // exponent is markedly smaller than the neighbouring mantissa, so
        // a size mismatch disqualifies the pair. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            // Exponent "4" of the previous value: 7pt. ~keep
            digit_test_span("4", Rect::new(200.0, 700.0, 4.0, 7.0), 7.0),
            // Mantissa "3" of the next value: 12pt, 8pt away (0.67-1.14x
            // either font size -- inside the merge band for both). ~keep
            digit_test_span("3", Rect::new(212.0, 700.0, 6.5, 12.0), 12.0),
        ];

        extractor.merge_adjacent_spans();
        assert!(
            !extractor.spans.iter().any(|s| s.text.contains('.')),
            "digit runs at mismatched font sizes must not merge into a decimal, got {:?}",
            extractor.spans.iter().map(|s| s.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_decimal_merge_with_ink_elsewhere_on_line_still_joins() {
        // Positive control for the intervening-ink test: ink elsewhere on
        // the same line (a comma before the amount) must not block a
        // genuine split-box merge — only ink INSIDE the gap counts. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        extractor.spans = vec![
            digit_test_span("123456", Rect::new(382.3, 700.0, 39.6, 12.0), 12.0),
            digit_test_span("72", Rect::new(432.7, 700.0, 13.2, 12.0), 12.0), // 10.8pt gap ~keep
            digit_test_span(",", Rect::new(300.0, 700.0, 2.5, 4.0), 12.0),
            // ~keep
        ];

        extractor.merge_adjacent_spans();
        assert!(
            extractor.spans.iter().any(|s| s.text == "123456.72"),
            "split-box amount must still merge when the line's other ink is outside the gap, got {:?}",
            extractor.spans.iter().map(|s| s.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_decimal_merge_for_wide_subscript_digits() {
        // Subscript index pairs (e.g. `P_{1,0}` in scientific PDFs) draw the
        // two subscript digits in a smaller font (~7pt) spaced far apart
        // (~1.5-1.7x the font size). The decimal-merge rule was joining them
        // into an invented decimal ("1" + "0" -> "1.0"). A real split-box
        // dollar amount clusters near ~0.8-1.0x the font size, so a wide gap is
        // not an integer/cents amount and must stay separate. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        // 7pt subscript digits: "1" at x=100.0 (w=3.5), "0" at x=114.5 (w=3.5).
        // gap = 114.5 - (100.0 + 3.5) = 11.0pt -> 11.0 / 7.0 = 1.57x font size. ~keep
        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "1".to_string(),
                bbox: Rect::new(100.0, 700.0, 3.5, 7.0),
                font_name: "F1".to_string(),
                font_size: 7.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "0".to_string(),
                bbox: Rect::new(114.5, 700.0, 3.5, 7.0), // 11.0pt gap = 1.57x font ~keep
                font_name: "F1".to_string(),
                font_size: 7.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(
            extractor.spans.len(),
            2,
            "Widely-spaced subscript digits should not merge into a decimal value"
        );
        assert!(
            !extractor.spans.iter().any(|s| s.text.contains('.')),
            "No invented decimal point should appear between subscript digits"
        );
    }

    #[test]
    fn test_decimal_merge_just_under_ceiling_still_joins() {
        // The ceiling that stops subscripts must not be so tight that it drops
        // genuine split-box amounts. Real amounts cluster near ~0.8-1.0x the
        // font size; this locks the ceiling by proving an amount at ~1.2x the
        // font size (just under the 1.3x cap) still merges. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::legacy();

        // 12pt digits: "1234" at x=200.0 (w=24.0), "56" at x=238.4 (w=12.0).
        // gap = 238.4 - (200.0 + 24.0) = 14.4pt -> 14.4 / 12.0 = 1.2x font size. ~keep
        extractor.spans = vec![
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "1234".to_string(),
                bbox: Rect::new(200.0, 700.0, 24.0, 12.0),
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
            TextSpan {
                provenance: None,
                text_rise: 0.0,
                artifact_type: None,
                text: "56".to_string(),
                bbox: Rect::new(238.4, 700.0, 12.0, 12.0), // 14.4pt gap = 1.2x font ~keep
                font_name: "F1".to_string(),
                font_size: 12.0,
                font_weight: FontWeight::Normal,
                color: Color::black(),
                mcid: None,
                mcid_scope: None,
                sequence: 1,
                split_boundary_before: false,
                offset_semantic: false,
                is_italic: false,
                is_monospace: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                rtl_draw_logical: false,
                mirrored: false,
                page_rotation_applied: 0,
            },
        ];

        extractor.merge_adjacent_spans();
        assert_eq!(
            extractor.spans.len(),
            1,
            "Amount just under the ceiling should still merge"
        );
        assert_eq!(extractor.spans[0].text, "1234.56");
    }

    #[test]
    fn test_cross_font_word_glue_single_letter_prefix() {
        // A single-letter span in one font, tight-kerned against a
        // multi-letter span in another font, is the drop-cap pattern.
        // These must merge into one word with the longer run's font
        // metadata — emitting per-letter emphasis runs corrupts proper
        // nouns. ~keep
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::default();

        extractor.spans = vec![
            TextSpan {
                text: "S".to_string(),
                bbox: Rect::new(72.0, 700.0, 10.0, 12.0),
                font_name: "Helvetica-Bold".to_string(),
                font_weight: FontWeight::Bold,
                font_size: 12.0,
                ..TextSpan::default()
            },
            TextSpan {
                text: "ales".to_string(),
                bbox: Rect::new(82.0, 700.0, 30.0, 12.0),
                font_name: "Helvetica".to_string(),
                font_weight: FontWeight::Normal,
                font_size: 12.0,
                ..TextSpan::default()
            },
        ];

        extractor.merge_adjacent_spans();

        assert_eq!(
            extractor.spans.len(),
            1,
            "cross_font_word_glue should merge 'S' + 'ales' into 'Sales'"
        );
        assert_eq!(extractor.spans[0].text, "Sales");
        // Dominant-font swap: the longer run (regular weight) should win. ~keep
        assert_eq!(extractor.spans[0].font_weight, FontWeight::Normal);
    }

    /// The advance-fold folds a sub-threshold TJ offset into the run's stored
    /// advance using the exact ISO 32000-1 §9.4.4 displacement
    /// (`-Tj/1000 * Tfs * Th`), keeping `char_widths.last` and
    /// `accumulated_width` in lockstep so the reconstructed geometry equals the
    /// text-matrix position. An empty buffer is a no-op (the next glyph
    /// re-anchors to the matrix).
    #[test]
    fn test_fold_offset_into_buffer_matches_spec_displacement() {
        let mut extractor = TextExtractor::new();
        {
            let st = extractor.state_stack.current_mut();
            st.font_size = 10.0;
            st.horizontal_scaling = 100.0; // Th = 1.0 ~keep
        }
        let mut buffer = TjBuffer::new(extractor.state_stack.current(), None, None);
        buffer.char_widths.push(5.0);
        buffer.accumulated_width = 5.0;

        // -120 TJ units => -(-120)/1000 * 10 * (100/100) = 1.2 (text space). ~keep
        extractor.fold_offset_into_buffer(&mut buffer, -120.0);
        let expected = 1.2_f32;
        assert!((buffer.char_widths.last().unwrap() - (5.0 + expected)).abs() < 1e-4);
        assert!((buffer.accumulated_width - (5.0 + expected)).abs() < 1e-4);
        // Invariant: sum(char_widths) == accumulated_width by construction. ~keep
        let sum: f32 = buffer.char_widths.iter().sum();
        assert!((sum - buffer.accumulated_width).abs() < 1e-4);

        // Empty buffer: nothing to fold into, must not panic or fabricate width. ~keep
        let mut empty = TjBuffer::new(extractor.state_stack.current(), None, None);
        extractor.fold_offset_into_buffer(&mut empty, -120.0);
        assert!(empty.char_widths.is_empty());
        assert_eq!(empty.accumulated_width, 0.0);
    }

    /// The cross-font glue ceiling (0.12em) must NOT glue a real word followed
    /// by a single-letter variable set in a different font run across a
    /// word-space gap (roman `solution` -> math-italic `U`, gap ~0.24em). This
    /// is the mirror of the drop-cap case above (gap ~0): a word space is a
    /// genuine boundary poppler/PDFium keep, so the two spans stay separate.
    #[test]
    fn test_cross_font_word_variable_not_glued() {
        let mut extractor = TextExtractor::new();
        extractor.merging_config = SpanMergingConfig::default();
        // fs 10; "solution" ends at x=112, "U" starts at x=114.4 => gap 2.4pt = 0.24em. ~keep
        extractor.spans = vec![
            TextSpan {
                text: "solution".to_string(),
                bbox: Rect::new(72.0, 700.0, 40.0, 10.0),
                font_name: "NimbusRomNo9L-Regu".to_string(),
                font_size: 10.0,
                ..TextSpan::default()
            },
            TextSpan {
                text: "U".to_string(),
                bbox: Rect::new(114.4, 700.0, 7.0, 10.0),
                font_name: "NimbusRomNo9L-Ital".to_string(),
                font_size: 10.0,
                ..TextSpan::default()
            },
        ];
        extractor.merge_adjacent_spans();
        assert_eq!(
            extractor.spans.len(),
            2,
            "a 0.24em word-space gap across a font change must NOT glue (drop-cap glue is for ~0 gaps)"
        );
    }
}
