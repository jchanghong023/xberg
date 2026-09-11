//! Unit tests for [`super`].
//!
//! Split out of `document.rs` purely for file size: the parent was 28,713 lines
//! (1.2 MiB) and tripped the repository's 500 KiB file-safety limit. A child
//! module sees the parent's private items exactly as an inline `mod tests` did. ~keep

use super::*;
use std::collections::HashMap;

fn name(s: &str) -> Object {
    Object::Name(s.to_string())
}

fn separation_cs(ink: &str) -> Object {
    Object::Array(vec![name("Separation"), name(ink), name("DeviceCMYK"), Object::Null])
}

fn device_n_cs(inks: &[&str]) -> Object {
    Object::Array(vec![
        name("DeviceN"),
        Object::Array(inks.iter().map(|s| name(s)).collect()),
        name("DeviceCMYK"),
        Object::Null,
    ])
}

#[test]
fn extracts_separation_ink_name() {
    let mut cs_dict = HashMap::new();
    cs_dict.insert("CS0".to_string(), separation_cs("Pantone-185"));
    let mut out = Vec::new();
    extract_inks_from_color_space_dict(&cs_dict, None, &mut out);
    assert_eq!(out, vec!["Pantone-185".to_string()]);
}

#[test]
fn extracts_devicen_ink_names_in_declared_order() {
    let mut cs_dict = HashMap::new();
    cs_dict.insert("CS0".to_string(), device_n_cs(&["Cyan", "Magenta", "SpotGold"]));
    let mut out = Vec::new();
    extract_inks_from_color_space_dict(&cs_dict, None, &mut out);
    assert_eq!(
        out,
        vec!["Cyan".to_string(), "Magenta".to_string(), "SpotGold".to_string()]
    );
}

#[test]
fn skips_all_and_none_colorants() {
    // §8.6.6.4: /All and /None are reserved; never plate names. ~keep
    let mut cs_dict = HashMap::new();
    cs_dict.insert("CS0".to_string(), separation_cs("All"));
    cs_dict.insert("CS1".to_string(), separation_cs("None"));
    cs_dict.insert("CS2".to_string(), device_n_cs(&["All", "Spot1", "None"]));
    let mut out = Vec::new();
    extract_inks_from_color_space_dict(&cs_dict, None, &mut out);
    assert_eq!(out, vec!["Spot1".to_string()]);
}

#[test]
fn ignores_non_separation_color_spaces() {
    let mut cs_dict = HashMap::new();
    cs_dict.insert("CS0".to_string(), Object::Array(vec![name("ICCBased"), Object::Null]));
    cs_dict.insert("CS1".to_string(), name("DeviceCMYK"));
    let mut out = Vec::new();
    extract_inks_from_color_space_dict(&cs_dict, None, &mut out);
    assert!(out.is_empty());
}

/// Build a minimal PDF that embeds a single colour-space object with a
/// self-referential Pattern array `5 0 obj [/Pattern 5 0 R]`. Used by the
/// cycle-handling regression below — the array as stored on disk is the
/// minimal shape that triggers unbounded recursion in the inks walker
/// before the depth/visited-set guard was added.
fn build_pdf_with_self_referential_pattern_cs() -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();

    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let off2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let off3 = pdf.len();
    pdf.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /ColorSpace << /CS0 5 0 R >> >> >>\nendobj\n",
        );

    let off4 = pdf.len();
    pdf.extend_from_slice(b"4 0 obj\n<< /Length 0 >>\nstream\n\nendstream\nendobj\n");

    // Object 5: a Pattern colour-space array whose underlying space is a
    // reference back to itself — the cycle the regression guards against. ~keep
    let off5 = pdf.len();
    pdf.extend_from_slice(b"5 0 obj\n[/Pattern 5 0 R]\nendobj\n");

    let xref_off = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n");
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    pdf.extend_from_slice(format!("{:010} 00000 n \n", off1).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", off2).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", off3).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", off4).as_bytes());
    pdf.extend_from_slice(format!("{:010} 00000 n \n", off5).as_bytes());
    pdf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    pdf
}

#[test]
fn self_referential_pattern_cs_does_not_stack_overflow() {
    // Regression: prior to the depth-bound + visited-set guard, a
    // self-referential Pattern colour space (§8.7.3.1) recursed through
    // `collect_inks_from_color_space` without termination and aborted
    // the process with a stack overflow. The fix records each indirect
    // underlying ref in a visited set and caps total walk depth at
    // `MAX_RECURSION_DEPTH`, mirroring `walk_form_xobject_tree_for_inks`. ~keep
    //
    // The call must return without panicking; the inks vector is left
    // empty because no concrete colorant ever surfaces on a self-cycle. ~keep
    let pdf = build_pdf_with_self_referential_pattern_cs();
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF should parse");

    // Resolve `5 0 R` to the on-disk Pattern array; this matches how the
    // page-level walker enters the helper after dereferencing a /ColorSpace
    // resource entry. ~keep
    let cs_def = doc
        .load_object(ObjectRef { id: 5, generation: 0 })
        .expect("object 5 should load");

    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<ObjectRef> = std::collections::HashSet::new();
    // The bug is a stack overflow, so the assertion is simply that this
    // call returns. The visited-set must dedupe the self-reference on
    // first encounter; without the guard, the recursion is unbounded. ~keep
    super::collect_inks_from_color_space(&cs_def, Some(&doc), &mut out, &mut visited, 0);
    assert!(
        out.is_empty(),
        "self-referential Pattern colour space surfaces no concrete colorants"
    );
}

#[test]
fn get_page_inks_handles_self_referential_pattern_cs() {
    // End-to-end shape of the same regression: the public
    // `get_page_inks` entry point walks the resource dictionary and
    // hits the cycle through the same helper. Must not stack-overflow. ~keep
    let pdf = build_pdf_with_self_referential_pattern_cs();
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF should parse");
    let inks = doc.get_page_inks(0).expect("page-inks walk must not panic");
    assert!(inks.is_empty(), "self-cycle yields no plates");
}
