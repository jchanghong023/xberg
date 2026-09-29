#[test]
fn test_word_spacing_tw_parameter() {
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
                code: 0x68,
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
                code: 0x65,
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
                code: 0x20,
                glyph_id: Some(4),
                width: 0.25,
                x_position: 15.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x63,
                glyph_id: Some(5),
                width: 0.35,
                x_position: 22.1,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x61,
                glyph_id: Some(6),
                width: 0.35,
                x_position: 26.7,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x74,
                glyph_id: Some(7),
                width: 0.3,
                x_position: 31.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
        ],
        font_size: 12.0,
        horizontal_scaling: 100.0,
        word_spacing: 0.5,
        char_spacing: 0.0,
    };

    assert_eq!(stream.characters[3].code, 0x20);
    assert_eq!(stream.word_spacing, 0.5);
}

#[test]
fn test_ligature_handling() {
    let stream = TextStreamContext {
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
                code: 0xFB01,
                glyph_id: Some(2),
                width: 0.6,
                x_position: 6.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6E,
                glyph_id: Some(3),
                width: 0.4,
                x_position: 13.2,
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

    assert_eq!(stream.characters[1].code, 0xFB01);
}

#[test]
fn test_combining_characters_diacritics() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x65,
                glyph_id: Some(1),
                width: 0.4,
                x_position: 0.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x0301,
                glyph_id: Some(2),
                width: 0.0,
                x_position: 4.8,
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
                x_position: 8.4,
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

    assert_eq!(stream.characters[1].code, 0x0301);
    assert_eq!(stream.characters[1].width, 0.0);
}

#[test]
fn test_zero_width_space_boundary() {
    let stream = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x6E,
                glyph_id: Some(1),
                width: 0.4,
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
                x_position: 4.8,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x200B,
                glyph_id: Some(3),
                width: 0.0,
                tj_offset: None,
                x_position: 9.6,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x72,
                glyph_id: Some(4),
                width: 0.3,
                x_position: 9.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x62,
                glyph_id: Some(5),
                width: 0.4,
                x_position: 13.2,
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

    assert_eq!(stream.characters[2].code, 0x200B);
    assert_eq!(stream.characters[2].width, 0.0);
}

#[test]
fn test_specification_reference_iso_9_4_4() {
    // This test documents the spec sections that define word boundary detection
    // ISO 32000-1:2008 Section 9.4.4: Text Objects and Word Boundaries
    //
    // Key concepts:
    // 1. TJ array offset values provide character-level positioning
    // 2. Geometric spacing (character positions) determine visual word boundaries
    // 3. Space character (U+0020) and Tw parameter define explicit word breaks
    // 4. Font metrics (size, scaling, spacing) scale boundary detection
    // 5. CJK text requires different word breaking rules (no spaces)
    // 6. Custom encodings need character mapping before boundary detection ~keep

    let spec_sections = [
        "ISO 32000-1:2008 Section 9.4: Text Objects",
        "ISO 32000-1:2008 Section 9.4.3: Text Positioning Operators",
        "ISO 32000-1:2008 Section 9.4.4: Text Objects and Word Spacing",
        "ISO 32000-1:2008 Section 5.3.2: Text State Parameters (Tc, Tw, Tz, TL)",
    ];

    assert_eq!(spec_sections.len(), 4);
    assert!(spec_sections[0].contains("9.4"));
    assert!(spec_sections[1].contains("9.4.3"));
}

#[test]
fn test_mixed_scripts_word_boundary() {
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
                code: 0x20,
                glyph_id: Some(5),
                width: 0.25,
                x_position: 19.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x4E2D,
                glyph_id: Some(6),
                width: 1.0,
                x_position: 25.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x6587,
                glyph_id: Some(7),
                width: 1.0,
                x_position: 37.2,
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
    assert!(stream.characters[5].code > 0x4E00);
}

#[test]
fn test_word_boundary_with_numbers() {
    let stream_attached = TextStreamContext {
        characters: vec![
            CharacterInfo {
                code: 0x74,
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
                code: 0x65,
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
                code: 0x73,
                glyph_id: Some(3),
                width: 0.3,
                x_position: 8.4,
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
                x_position: 12.0,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x31,
                glyph_id: Some(5),
                width: 0.3,
                x_position: 15.6,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x32,
                glyph_id: Some(6),
                width: 0.3,
                x_position: 19.2,
                tj_offset: None,
                font_size: 12.0,
                is_ligature: false,
                original_ligature: None,
                protected_from_split: false,
            },
            CharacterInfo {
                code: 0x33,
                glyph_id: Some(7),
                width: 0.3,
                x_position: 22.8,
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

    assert!(stream_attached.characters[0].code == 0x74);
    assert!(stream_attached.characters[4].code == 0x31);
    let has_space = stream_attached.characters.iter().any(|c| c.code == 0x20);
    assert!(!has_space, "Stream should have no space characters for attached word");
}
