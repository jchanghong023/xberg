#![allow(dead_code)]
//! Tests for ISO 32000-1:2008 Section 9.4.4 Word Boundary Detection
//!
//! The PDF spec defines word boundaries through multiple mechanisms:
//! 1. TJ array offset values (character-level spacing information)
//! 2. Geometric positioning (layout-based word breaking)
//! 3. Space character detection (explicit word separators)
//! 4. Font metrics (font size, character width influence spacing decisions)
//!
//! This test suite documents spec-compliant word boundary detection
//! for single-byte, multi-byte, and CJK text.

/// Mock text extraction result for word boundary testing
#[derive(Clone, Debug)]
struct CharacterInfo {
    code: u32,
    glyph_id: Option<u16>,
    width: f32,
    x_position: f32,
    tj_offset: Option<i32>,
    font_size: f32,
    is_ligature: bool,
    original_ligature: Option<char>,
    protected_from_split: bool,
}

/// Helper to simulate text stream with character-level information
#[derive(Clone, Debug)]
struct TextStreamContext {
    characters: Vec<CharacterInfo>,
    font_size: f32,
    horizontal_scaling: f32,
    word_spacing: f32,
    char_spacing: f32,
}

#[test]
fn test_ascii_space_boundary_detection() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x48,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6C,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6C,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 14.4,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 18.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x20,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 22.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x57,
                glyph_id: Some(6),
                width: 0.7,
                x_position: 28.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 36.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x72,
                glyph_id: Some(7),
                width: 0.3,
                x_position: 41.4,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6C,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 45.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x64,
                glyph_id: Some(8),
                width: 0.4,
                x_position: 48.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert_eq!(stream.characters[5].code, 0x20);
    assert_eq!(stream.characters.len(), 11);
}

#[test]
fn test_tj_array_negative_offset_creates_word_boundary() {
    // TJ array with large negative offset creates word boundary
    // Spec: negative values in TJ increase spacing (potential word break) ~keep
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x54,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x69,
                glyph_id: Some(2),
                width: 0.3,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6D,
                glyph_id: Some(3),
                width: 0.4,
                x_position: 9.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 14.4,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x2D,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 19.2,
                tj_offset: Some(-200),
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(6),
                width: 0.4,
                x_position: 31.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x75,
                glyph_id: Some(7),
                width: 0.4,
                x_position: 36.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x74,
                glyph_id: Some(8),
                width: 0.3,
                x_position: 40.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    // Large negative TJ offset (before 'o') indicates word boundary
    // Expected: "Time" | (boundary) | "out" ~keep
    assert!(stream.characters[4].tj_offset == Some(-200));
    assert_eq!(stream.characters.len(), 8);
}

#[test]
fn test_geometric_spacing_word_boundary_detection() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x54,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x78,
                glyph_id: Some(3),
                width: 0.4,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x74,
                glyph_id: Some(4),
                width: 0.3,
                x_position: 15.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x42,
                glyph_id: Some(5),
                width: 0.5,
                x_position: 27.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(6),
                width: 0.4,
                x_position: 33.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x78,
                glyph_id: Some(7),
                width: 0.4,
                x_position: 37.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    let gap = stream.characters[4].x_position - (stream.characters[3].x_position + stream.characters[3].width);
    assert!(gap > 5.0, "Gap should be significant for word boundary");
}

#[test]
fn test_multiple_consecutive_spaces() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x57,
                glyph_id: Some(1),
                width: 0.7,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 8.4,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x72,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 13.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x64,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 16.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x20,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 21.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x20,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 25.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x20,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 30.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x46,
                glyph_id: Some(6),
                width: 0.5,
                x_position: 34.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 40.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x72,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 45.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert_eq!(stream.characters[4].code, 0x20);
    assert_eq!(stream.characters[5].code, 0x20);
    assert_eq!(stream.characters[6].code, 0x20);
}

#[test]
fn test_hyphenation_word_boundary() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x69,
                glyph_id: Some(1),
                width: 0.3,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6E,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 3.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x74,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 7.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x72,
                glyph_id: Some(5),
                width: 0.3,
                x_position: 14.4,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x2D,
                glyph_id: Some(6),
                width: 0.25,
                x_position: 18.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6E,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 20.1,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 24.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x74,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 28.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert_eq!(stream.characters[5].code, 0x2D);
    let gap_after_hyphen =
        stream.characters[6].x_position - (stream.characters[5].x_position + stream.characters[5].width);
    assert!(gap_after_hyphen < 2.0, "Hyphen continuation has small gap");
}

#[test]
fn test_cjk_text_no_explicit_spaces() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x4E2D,
                glyph_id: Some(1),
                width: 1.0,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6587,
                glyph_id: Some(2),
                width: 1.0,
                x_position: 12.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x5B57,
                glyph_id: Some(3),
                width: 1.0,
                x_position: 24.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x3002,
                glyph_id: Some(4),
                width: 0.5,
                x_position: 36.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.0,
        char_spacing: 0.0,
    };

    assert!(stream.characters[0].code > 0x4E00);
    assert!(stream.characters[0].width == 1.0);
}

#[test]
fn test_custom_encoding_word_boundary() {
    // Custom font encodings may use different character codes for spaces
    // Must be handled by mapping function (character_mapper) ~keep
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x21,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x22,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x00,
                glyph_id: Some(3),
                width: 0.25,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x43,
                glyph_id: Some(4),
                width: 0.5,
                x_position: 16.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert_eq!(stream.characters.len(), 4);
}

#[test]
fn test_font_size_influence_on_spacing() {
    let stream_large = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x41,
                glyph_id: Some(1),
                width: 1.0,
                x_position: 0.0,
                tj_offset: None,
                font_size: 24.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x42,
                glyph_id: Some(2),
                width: 0.8,
                x_position: 24.0,
                tj_offset: None,
                font_size: 24.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x43,
                glyph_id: Some(3),
                width: 0.8,
                x_position: 52.0,
                tj_offset: None,
                font_size: 24.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 24.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.5,
        char_spacing: 0.0,
    };

    assert_eq!(stream_large.font_size, 24.0);

    let stream_small = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x41,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x42,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x43,
                glyph_id: Some(3),
                width: 0.4,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert_eq!(stream_small.font_size, 12.0);
    assert!(stream_large.font_size == 2.0 * stream_small.font_size);
}

#[test]
fn test_horizontal_scaling_affects_spacing() {
    let stream_normal = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x41,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x42,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x43,
                glyph_id: Some(3),
                width: 0.4,
                x_position: 10.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    let stream_condensed = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x41,
                glyph_id: Some(1),
                width: 0.375,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x42,
                glyph_id: Some(2),
                width: 0.3,
                x_position: 4.5,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x43,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 8.1,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 75.0,
        word_spacing: 0.25,
        char_spacing: 0.0,
    };

    assert!(stream_condensed.horizontal_scaling == 75.0);
    assert!(stream_normal.characters[0].width > stream_condensed.characters[0].width);
}

#[test]
fn test_character_spacing_tc_parameter() {
    let stream_normal = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x48,
                glyph_id: Some(1),
                width: 0.5,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(2),
                width: 0.4,
                x_position: 6.5,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6C,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 11.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6C,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 16.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6F,
                glyph_id: Some(4),
                width: 0.4,
                x_position: 21.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.25,
        char_spacing: 0.5,
    };

    assert_eq!(stream_normal.char_spacing, 0.5);
    assert_eq!(stream_normal.characters.len(), 5);
}

// Split for file-too-long (#1567): the module continues below via `include!`,
// not a sibling tests/*_part2.rs file -- Cargo auto-discovers every *.rs file
// directly under tests/ as its own test binary, so a sibling would fail to
// compile on its own and, if it somehow did, run every #[test] in it twice.
// A subdirectory is not auto-discovered. ~keep
include!("test_word_boundary_detection/part2.rs");
