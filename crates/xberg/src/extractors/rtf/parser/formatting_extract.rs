//! The formatting-metadata pass: [`extract_rtf_formatting`] and the
//! span-to-annotation conversion consumed by callers of the text-extraction
//! pass in [`super::text_extract`].

use super::{
    FldrsltCloseState, RtfFormattingData, RtfFormattingSpan, close_fldinst_group, close_fldrslt_group,
    consume_adjacent_hex_escape, parse_font_charset_table, parse_rtf_color_table, resolve_decode_codepage,
};
use crate::extractors::rtf::encoding::{decode_ansi_bytes, parse_hex_byte, parse_rtf_control_word};
use crate::types::TextAnnotation;
use crate::types::document_structure::AnnotationKind;

/// Extract formatting metadata from RTF content.
///
/// This performs a lightweight pass over the RTF to extract:
/// - Bold/italic/underline formatting state changes
/// - Color table and color references
/// - Header/footer text
/// - Hyperlink field instructions
pub(crate) fn extract_rtf_formatting(content: &str) -> RtfFormattingData {
    let color_table = parse_rtf_color_table(content);
    let font_charsets = parse_font_charset_table(content);
    let mut spans = Vec::new();
    let mut hyperlinks = Vec::new();
    let mut text_offset: usize = 0;
    let mut span_start: usize = 0;

    let mut in_header = false;
    let mut in_footer = false;
    let mut header_depth: i32 = 0;
    let mut footer_depth: i32 = 0;
    let mut header_buf = String::new();
    let mut footer_buf = String::new();

    let mut in_fldinst = false;
    let mut fldinst_depth: i32 = 0;
    let mut fldinst_content = String::new();
    let mut in_fldrslt = false;
    let mut fldrslt_depth: i32 = 0;
    let mut fldrslt_start: usize = 0;
    let mut pending_hyperlink_url: Option<String> = None;

    #[derive(Clone)]
    struct FmtState {
        bold: bool,
        italic: bool,
        underline: bool,
        strikethrough: bool,
        color_idx: u16,
    }

    // Closes the current formatting span (if the output advanced since
    // `span_start`) using `fmt`'s active formatting, then advances
    // `span_start` to `text_offset` unconditionally. Used on `}`, on
    // `\plain`, and by each `update_fmt_field!` invocation below -- all
    // three previously duplicated this exact push-then-advance pattern. ~keep
    fn push_span_if_open(
        spans: &mut Vec<RtfFormattingSpan>,
        span_start: &mut usize,
        text_offset: usize,
        fmt: &FmtState,
    ) {
        if text_offset > *span_start {
            spans.push(RtfFormattingSpan {
                start: *span_start,
                end: text_offset,
                bold: fmt.bold,
                italic: fmt.italic,
                underline: fmt.underline,
                strikethrough: fmt.strikethrough,
                color_index: fmt.color_idx,
            });
        }
        *span_start = text_offset;
    }

    let mut fmt = FmtState {
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        color_idx: 0,
    };
    let mut fmt_stack: Vec<FmtState> = Vec::new();

    let mut group_depth: i32 = 0;
    let mut skip_depth: i32 = 0;
    let mut chars = content.chars().peekable();
    let mut expect_destination = false;
    let mut ignorable_pending = false;

    // Mirrors the extraction pass's codepage tracking so both passes count the
    // same number of output bytes for `\'hh` escape runs.
    let mut ansi_codepage_stack: Vec<u32> = vec![1252];
    // Mirrors the extraction pass's active-font tracking (see `font_id_stack`
    // in `extract_text_from_rtf`) so `\'hh` escapes decode identically in both.
    let mut font_id_stack: Vec<Option<u16>> = vec![None];
    let mut default_font_id: Option<u16> = None;

    let skip_dests = [
        "fonttbl",
        "stylesheet",
        "info",
        "listtable",
        "listoverridetable",
        "generator",
        "filetbl",
        "revtbl",
        "rsidtbl",
        "xmlnstbl",
        "mmathPr",
        "themedata",
        "colorschememapping",
        "datastore",
        "latentstyles",
        "datafield",
        "objdata",
        "objclass",
        "panose",
        "bkmkstart",
        "bkmkend",
        "wgrffmtfilter",
        "fcharset",
        "pgdsctbl",
        "colortbl",
        "pict",
    ];

    let mut group_has_text: Vec<bool> = Vec::new();
    let mut pending_boundary_space = false;

    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                group_depth += 1;
                expect_destination = true;
                fmt_stack.push(fmt.clone());
                group_has_text.push(false);
                pending_boundary_space = false;
                let current_codepage = ansi_codepage_stack.last().copied().unwrap_or(1252);
                ansi_codepage_stack.push(current_codepage);
                let current_font = font_id_stack.last().copied().flatten();
                font_id_stack.push(current_font);
            }
            '}' => {
                group_depth -= 1;
                expect_destination = false;
                ignorable_pending = false;
                if ansi_codepage_stack.len() > 1 {
                    ansi_codepage_stack.pop();
                }
                if font_id_stack.len() > 1 {
                    font_id_stack.pop();
                }
                if let Some(parent) = fmt_stack.pop() {
                    let changed = fmt.bold != parent.bold
                        || fmt.italic != parent.italic
                        || fmt.underline != parent.underline
                        || fmt.strikethrough != parent.strikethrough
                        || fmt.color_idx != parent.color_idx;
                    if changed {
                        push_span_if_open(&mut spans, &mut span_start, text_offset, &fmt);
                        fmt = parent;
                    }
                }
                if skip_depth > 0 && group_depth < skip_depth {
                    skip_depth = 0;
                }
                if in_header && group_depth < header_depth {
                    in_header = false;
                }
                if in_footer && group_depth < footer_depth {
                    in_footer = false;
                }
                close_fldinst_group(
                    group_depth,
                    &mut in_fldinst,
                    fldinst_depth,
                    &mut fldinst_content,
                    &mut pending_hyperlink_url,
                );
                close_fldrslt_group(
                    group_depth,
                    text_offset,
                    FldrsltCloseState {
                        in_fldrslt: &mut in_fldrslt,
                        fldrslt_depth,
                        fldrslt_start,
                        pending_hyperlink_url: &mut pending_hyperlink_url,
                        hyperlinks: &mut hyperlinks,
                    },
                );
                let produced_text = group_has_text.pop().unwrap_or(false);
                if produced_text && skip_depth == 0 {
                    pending_boundary_space = true;
                }
            }
            '\\' => {
                if let Some(&next_ch) = chars.peek() {
                    match next_ch {
                        '\\' | '{' | '}' => {
                            chars.next();
                            expect_destination = false;
                            if in_fldinst {
                                fldinst_content.push(next_ch);
                            }
                            if skip_depth > 0 {
                                continue;
                            }
                            if pending_boundary_space && text_offset > 0 {
                                text_offset += 1;
                            }
                            pending_boundary_space = false;
                            text_offset += next_ch.len_utf8();
                            if let Some(flag) = group_has_text.last_mut() {
                                *flag = true;
                            }
                            if in_header {
                                header_buf.push(next_ch);
                            }
                            if in_footer {
                                footer_buf.push(next_ch);
                            }
                        }
                        '\'' => {
                            chars.next();
                            expect_destination = false;
                            let hex1 = chars.next();
                            let hex2 = chars.next();
                            let bytes = if let (Some(h1), Some(h2)) = (hex1, hex2)
                                && let Some(byte) = parse_hex_byte(h1 as u8, h2 as u8)
                            {
                                let mut bytes = vec![byte];
                                while let Some(next_byte) = consume_adjacent_hex_escape(&mut chars) {
                                    bytes.push(next_byte);
                                }
                                Some(bytes)
                            } else {
                                None
                            };
                            if skip_depth > 0 {
                                continue;
                            }
                            if let Some(bytes) = bytes.as_deref() {
                                let codepage = resolve_decode_codepage(
                                    &font_id_stack,
                                    default_font_id,
                                    &font_charsets,
                                    &ansi_codepage_stack,
                                );
                                let decoded = decode_ansi_bytes(bytes, codepage);
                                if pending_boundary_space && text_offset > 0 {
                                    text_offset += 1;
                                }
                                pending_boundary_space = false;
                                text_offset += decoded.len();
                                if let Some(flag) = group_has_text.last_mut() {
                                    *flag = true;
                                }
                            }
                        }
                        '*' => {
                            chars.next();
                            ignorable_pending = true;
                        }
                        _ => {
                            let (word, param) = parse_rtf_control_word(&mut chars);

                            if expect_destination || ignorable_pending {
                                expect_destination = false;

                                if ignorable_pending {
                                    ignorable_pending = false;
                                    if word == "fldinst" {
                                        in_fldinst = true;
                                        fldinst_depth = group_depth;
                                        if skip_depth == 0 {
                                            skip_depth = group_depth;
                                        }
                                        continue;
                                    }
                                    if skip_depth == 0 {
                                        skip_depth = group_depth;
                                    }
                                    continue;
                                }

                                match word.as_str() {
                                    "fldinst" => {
                                        in_fldinst = true;
                                        fldinst_depth = group_depth;
                                        if skip_depth == 0 {
                                            skip_depth = group_depth;
                                        }
                                        continue;
                                    }
                                    "fldrslt" => {
                                        in_fldrslt = true;
                                        fldrslt_depth = group_depth;
                                        fldrslt_start = text_offset;
                                        continue;
                                    }
                                    _ => {}
                                }

                                if skip_dests.contains(&word.as_str()) {
                                    if skip_depth == 0 {
                                        skip_depth = group_depth;
                                    }
                                    continue;
                                }
                            }

                            if in_fldinst {
                                fldinst_content.push_str(&word);
                            }
                            if word == "ansicpg"
                                && let Some(val) = param
                                && val > 0
                                && let Some(codepage) = ansi_codepage_stack.last_mut()
                            {
                                *codepage = val as u32;
                            }
                            if word == "f"
                                && let Some(val) = param
                                && let Some(font_id) = font_id_stack.last_mut()
                            {
                                *font_id = Some(val.max(0) as u16);
                            }
                            if word == "deff"
                                && let Some(val) = param
                            {
                                default_font_id = Some(val.max(0) as u16);
                            }
                            if skip_depth > 0 {
                                continue;
                            }

                            macro_rules! update_fmt_field {
                                ($field:ident, $new_val:expr) => {
                                    let new_val = $new_val;
                                    if new_val != fmt.$field {
                                        push_span_if_open(&mut spans, &mut span_start, text_offset, &fmt);
                                        fmt.$field = new_val;
                                    }
                                };
                            }

                            match word.as_str() {
                                "b" => {
                                    update_fmt_field!(bold, param.unwrap_or(1) != 0);
                                }
                                "i" => {
                                    update_fmt_field!(italic, param.unwrap_or(1) != 0);
                                }
                                "ul" => {
                                    update_fmt_field!(underline, param.unwrap_or(1) != 0);
                                }
                                "ulnone" => {
                                    update_fmt_field!(underline, false);
                                }
                                "strike" => {
                                    update_fmt_field!(strikethrough, param.unwrap_or(1) != 0);
                                }
                                "cf" => {
                                    update_fmt_field!(color_idx, param.unwrap_or(0) as u16);
                                }
                                "plain"
                                    if (fmt.bold
                                        || fmt.italic
                                        || fmt.underline
                                        || fmt.strikethrough
                                        || fmt.color_idx != 0) =>
                                {
                                    push_span_if_open(&mut spans, &mut span_start, text_offset, &fmt);
                                    fmt.bold = false;
                                    fmt.italic = false;
                                    fmt.underline = false;
                                    fmt.strikethrough = false;
                                    fmt.color_idx = 0;
                                }
                                "header" | "headerl" | "headerr" | "headerf" => {
                                    in_header = true;
                                    header_depth = group_depth;
                                }
                                "footer" | "footerl" | "footerr" | "footerf" => {
                                    in_footer = true;
                                    footer_depth = group_depth;
                                }
                                "par" | "line" => {
                                    text_offset += 1;
                                    if in_header {
                                        header_buf.push('\n');
                                    }
                                    if in_footer {
                                        footer_buf.push('\n');
                                    }
                                }
                                "tab" => {
                                    text_offset += 1;
                                }
                                "bullet" => {
                                    text_offset += '\u{2022}'.len_utf8();
                                }
                                "lquote" => {
                                    text_offset += '\u{2018}'.len_utf8();
                                }
                                "rquote" => {
                                    text_offset += '\u{2019}'.len_utf8();
                                }
                                "ldblquote" => {
                                    text_offset += '\u{201C}'.len_utf8();
                                }
                                "rdblquote" => {
                                    text_offset += '\u{201D}'.len_utf8();
                                }
                                "endash" => {
                                    text_offset += '\u{2013}'.len_utf8();
                                }
                                "emdash" => {
                                    text_offset += '\u{2014}'.len_utf8();
                                }
                                "u" => {
                                    if let Some(code_num) = param {
                                        let code_u = if code_num < 0 {
                                            (code_num + 65536) as u32
                                        } else {
                                            code_num as u32
                                        };
                                        if let Some(c) = char::from_u32(code_u) {
                                            text_offset += c.len_utf8();
                                            if in_header {
                                                header_buf.push(c);
                                            }
                                            if in_footer {
                                                footer_buf.push(c);
                                            }
                                        }
                                    }
                                    if let Some(&next) = chars.peek()
                                        && next != '\\'
                                        && next != '{'
                                        && next != '}'
                                    {
                                        chars.next();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            '\n' | '\r' => {}
            ' ' | '\t' => {
                if in_fldinst {
                    fldinst_content.push(' ');
                }
                if skip_depth > 0 {
                    continue;
                }
                if text_offset > 0 {
                    text_offset += 1;
                    if let Some(flag) = group_has_text.last_mut() {
                        *flag = true;
                    }
                }
            }
            _ => {
                if in_fldinst {
                    fldinst_content.push(ch);
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                if pending_boundary_space && text_offset > 0 {
                    text_offset += 1;
                }
                pending_boundary_space = false;
                text_offset += ch.len_utf8();
                if let Some(flag) = group_has_text.last_mut() {
                    *flag = true;
                }
                if in_header {
                    header_buf.push(ch);
                }
                if in_footer {
                    footer_buf.push(ch);
                }
            }
        }
    }

    if text_offset > span_start && (fmt.bold || fmt.italic || fmt.underline || fmt.strikethrough || fmt.color_idx != 0)
    {
        spans.push(RtfFormattingSpan {
            start: span_start,
            end: text_offset,
            bold: fmt.bold,
            italic: fmt.italic,
            underline: fmt.underline,
            strikethrough: fmt.strikethrough,
            color_index: fmt.color_idx,
        });
    }

    let header_trimmed = header_buf.trim().to_string();
    let footer_trimmed = footer_buf.trim().to_string();

    RtfFormattingData {
        spans,
        color_table,
        header_text: if header_trimmed.is_empty() {
            None
        } else {
            Some(header_trimmed)
        },
        footer_text: if footer_trimmed.is_empty() {
            None
        } else {
            Some(footer_trimmed)
        },
        hyperlinks,
    }
}

/// Convert RTF formatting spans into `TextAnnotation` vectors for a paragraph.
///
/// Given the byte range of a paragraph within the full extracted text,
/// produces annotations from the formatting spans that overlap.
pub(crate) fn spans_to_annotations(
    para_start: usize,
    para_end: usize,
    formatting: &RtfFormattingData,
) -> Vec<TextAnnotation> {
    let mut annotations = Vec::new();
    for span in &formatting.spans {
        if span.end <= para_start || span.start >= para_end {
            continue;
        }
        let ann_start = span.start.max(para_start) - para_start;
        let ann_end = span.end.min(para_end) - para_start;
        if ann_start >= ann_end {
            continue;
        }
        let s = ann_start as u32;
        let e = ann_end as u32;
        if span.bold {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Bold,
            });
        }
        if span.italic {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Italic,
            });
        }
        if span.underline {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Underline,
            });
        }
        if span.strikethrough {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Strikethrough,
            });
        }
        if span.color_index > 0
            && let Some(color) = formatting.color_table.get(span.color_index as usize)
            && !color.is_empty()
            && color != "#000000"
        {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Color { value: color.clone() },
            });
        }
    }

    for (link_start, link_end, url) in &formatting.hyperlinks {
        if *link_end <= para_start || *link_start >= para_end {
            continue;
        }
        let s = (link_start.max(&para_start) - para_start) as u32;
        let e = (link_end.min(&para_end) - para_start) as u32;
        if s < e {
            annotations.push(TextAnnotation {
                start: s,
                end: e,
                kind: AnnotationKind::Link {
                    url: url.clone(),
                    title: None,
                },
            });
        }
    }

    annotations
}
