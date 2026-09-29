//! Probe tests — third-pass review of struct-tree-scope `/ActualText`
//!
//! These pin behaviour that the previous two review passes either
//! deferred or only touched at a surface level. Each probe is either
//! a regression pin (passes today, locks in the contract) or a bug
//! reproducer (fails today, proves a defect).
//!
//! Organised by category from the third-pass review brief:
//!   - architectural refactor (consecutive-run dedup boundaries)
//!   - `suppress_only` semantics for multi-page scopes
//!   - nested BDC / current_mcid stack discipline
//!   - MC-scope once-per-scope across Tj boundaries
//!   - locked-decision verification (MarkInfo, OCG)
//!   - edge cases (null, indirect, line breaks)
//!   - cross-path consistency at the structured surface
//!   - mutability / per-call lifecycle

// ~keep: test/bench binaries print by design; org logging policy exempts tests
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![allow(clippy::useless_vec)]

use xberg_native_pdf::converters::ConversionOptions;
use xberg_native_pdf::document::PdfDocument;

// =============================================================
// Minimal tagged-PDF builder (copied from `test_struct_actualtext.rs`
// since `tests/` are compiled as separate crates and the builder is
// private to its own file). Identical structure; kept tight.
// ============================================================= ~keep

enum K {
    Mcid(u32, u32),
    Obj(u32),
}

struct Elem {
    obj_num: u32,
    s: &'static str,
    parent_obj: u32,
    page_obj: Option<u32>,
    actual_text: Option<String>,
    children: Vec<K>,
}

impl Elem {
    fn new(obj_num: u32, s: &'static str, parent_obj: u32) -> Self {
        Self {
            obj_num,
            s,
            parent_obj,
            page_obj: None,
            actual_text: None,
            children: Vec::new(),
        }
    }
    fn page(mut self, page_obj: u32) -> Self {
        self.page_obj = Some(page_obj);
        self
    }
    fn actual_text(mut self, t: &str) -> Self {
        self.actual_text = Some(t.to_string());
        self
    }
    fn k(mut self, child: K) -> Self {
        self.children.push(child);
        self
    }
}

struct PdfBuilder {
    page_contents: Vec<Vec<u8>>,
    elems: Vec<Elem>,
    parent_tree_entries: Vec<Vec<(u32, u32)>>,
    suspects: bool,
    marked: bool,
    /// If true, MarkInfo dict is omitted entirely.
    no_mark_info: bool,
    /// Optional raw catalog entries inserted into the catalog dict
    /// (e.g. extra metadata). Currently unused but reserved.
    _extra_catalog: String,
}

impl PdfBuilder {
    fn new() -> Self {
        Self {
            page_contents: Vec::new(),
            elems: Vec::new(),
            parent_tree_entries: Vec::new(),
            suspects: false,
            marked: true,
            no_mark_info: false,
            _extra_catalog: String::new(),
        }
    }
    #[allow(dead_code)]
    fn suspects(mut self) -> Self {
        self.suspects = true;
        self
    }
    fn no_mark_info(mut self) -> Self {
        self.no_mark_info = true;
        self
    }
    fn marked(mut self, marked: bool) -> Self {
        self.marked = marked;
        self
    }
    fn add_page_content(&mut self, content: Vec<u8>) -> usize {
        self.page_contents.push(content);
        self.parent_tree_entries.push(Vec::new());
        self.page_contents.len() - 1
    }
    fn add_elem(&mut self, e: Elem) -> u32 {
        let n = e.obj_num;
        self.elems.push(e);
        n
    }
    fn register_mcid(&mut self, page_idx: usize, mcid: u32, struct_elem_obj: u32) {
        self.parent_tree_entries[page_idx].push((mcid, struct_elem_obj));
    }
    fn pdf_string_utf16be(s: &str) -> String {
        let mut out = String::from("<FEFF");
        for u in s.encode_utf16() {
            out.push_str(&format!("{:04X}", u));
        }
        out.push('>');
        out
    }

    fn build_with_ocg(self, layer_name: &str) -> Vec<u8> {
        self.build_impl(Some(layer_name.to_string()))
    }
    fn build(self) -> Vec<u8> {
        self.build_impl(None)
    }

    fn build_impl(self, ocg_layer: Option<String>) -> Vec<u8> {
        use std::collections::BTreeMap;

        let n_pages = self.page_contents.len() as u32;
        let catalog = 1u32;
        let pages = 2u32;
        let font = 3u32;
        let first_page = 4u32;
        let first_content = first_page + n_pages;
        let parent_tree = first_content + n_pages;
        let struct_tree_root = parent_tree + 1;
        let ocg = if ocg_layer.is_some() {
            Some(struct_tree_root + 1)
        } else {
            None
        };
        let first_struct_elem = struct_tree_root + 1 + if ocg.is_some() { 1 } else { 0 };

        let mut used = std::collections::HashSet::new();
        for e in &self.elems {
            assert!(e.obj_num >= first_struct_elem, "elem obj_num too low");
            assert!(used.insert(e.obj_num), "duplicate elem obj_num {}", e.obj_num);
        }
        let root_elem = self.elems[0].obj_num;

        let mut objs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();

        let mark_info = if self.no_mark_info {
            String::new()
        } else if self.suspects {
            " /MarkInfo << /Marked true /Suspects true >>".to_string()
        } else if self.marked {
            " /MarkInfo << /Marked true >>".to_string()
        } else {
            " /MarkInfo << /Marked false >>".to_string()
        };
        let oc_props = match ocg {
            Some(o) => {
                format!(" /OCProperties << /OCGs [{} 0 R] /D << /Order [{} 0 R] >> >>", o, o)
            }
            None => String::new(),
        };
        objs.insert(
            catalog,
            format!(
                "<< /Type /Catalog /Pages {} 0 R{} /StructTreeRoot {} 0 R{} >>",
                pages, mark_info, struct_tree_root, oc_props
            )
            .into_bytes(),
        );
        if let (Some(ocg_num), Some(layer_name)) = (ocg, ocg_layer.clone()) {
            objs.insert(ocg_num, format!("<< /Type /OCG /Name ({}) >>", layer_name).into_bytes());
        }
        let kids: String = (0..n_pages)
            .map(|i| format!("{} 0 R", first_page + i))
            .collect::<Vec<_>>()
            .join(" ");
        objs.insert(
            pages,
            format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids, n_pages).into_bytes(),
        );
        objs.insert(font, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());

        for i in 0..n_pages {
            let content_obj = first_content + i;
            let content_bytes = &self.page_contents[i as usize];
            let stream_obj = format!("<< /Length {} >>\nstream\n", content_bytes.len());
            let mut stream = stream_obj.into_bytes();
            stream.extend_from_slice(content_bytes);
            stream.extend_from_slice(b"\nendstream");
            objs.insert(content_obj, stream);
            let props_entry = match ocg {
                Some(o) => format!(" /Properties << /L {} 0 R >>", o),
                None => String::new(),
            };
            objs.insert(
                first_page + i,
                format!(
                    "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 612 792] \
                     /Resources << /Font << /F1 {} 0 R >> /ProcSet [/PDF /Text]{} >> \
                     /Contents {} 0 R /StructParents {} >>",
                    pages, font, props_entry, content_obj, i
                )
                .into_bytes(),
            );
        }

        let mut nums = String::from("<< /Nums [");
        for (i, entries) in self.parent_tree_entries.iter().enumerate() {
            nums.push_str(&format!("{} [", i));
            let mut by_mcid: Vec<(u32, u32)> = entries.clone();
            by_mcid.sort_by_key(|(m, _)| *m);
            let max_mcid = by_mcid.iter().map(|(m, _)| *m).max().unwrap_or(0);
            for m in 0..=max_mcid {
                if let Some((_, e)) = by_mcid.iter().find(|(mm, _)| *mm == m) {
                    nums.push_str(&format!("{} 0 R ", e));
                } else {
                    nums.push_str("null ");
                }
            }
            nums.push(']');
            if i + 1 < self.parent_tree_entries.len() {
                nums.push(' ');
            }
        }
        nums.push_str("] >>");
        objs.insert(parent_tree, nums.into_bytes());

        objs.insert(
            struct_tree_root,
            format!(
                "<< /Type /StructTreeRoot /K {} 0 R /ParentTree {} 0 R >>",
                root_elem, parent_tree
            )
            .into_bytes(),
        );

        for e in &self.elems {
            let mut body = format!("<< /Type /StructElem /S /{} /P {} 0 R", e.s, e.parent_obj);
            if let Some(pg) = e.page_obj {
                body.push_str(&format!(" /Pg {} 0 R", pg));
            }
            if let Some(ref at) = e.actual_text {
                body.push_str(&format!(" /ActualText {}", Self::pdf_string_utf16be(at)));
            }
            let mcr = |m: u32, pg_idx: u32| -> String {
                format!("<< /Type /MCR /Pg {} 0 R /MCID {} >>", first_page + pg_idx, m)
            };
            if e.children.len() == 1 {
                match &e.children[0] {
                    K::Mcid(m, p) => body.push_str(&format!(" /K {}", mcr(*m, *p))),
                    K::Obj(o) => body.push_str(&format!(" /K {} 0 R", o)),
                }
            } else if !e.children.is_empty() {
                body.push_str(" /K [");
                for (i, c) in e.children.iter().enumerate() {
                    if i > 0 {
                        body.push(' ');
                    }
                    match c {
                        K::Mcid(m, p) => body.push_str(&mcr(*m, *p)),
                        K::Obj(o) => body.push_str(&format!("{} 0 R", o)),
                    }
                }
                body.push(']');
            }
            body.push_str(" >>");
            objs.insert(e.obj_num, body.into_bytes());
        }

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n");
        let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
        for (num, body) in &objs {
            offsets.insert(*num, out.len());
            out.extend_from_slice(format!("{} 0 obj\n", num).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_offset = out.len();
        let max_num = *objs.keys().max().unwrap();
        let n_objs = max_num + 1;
        out.extend_from_slice(format!("xref\n0 {}\n", n_objs).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for n in 1..n_objs {
            if let Some(off) = offsets.get(&n) {
                out.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
            } else {
                out.extend_from_slice(b"0000000000 00000 f \n");
            }
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                n_objs, xref_offset
            )
            .as_bytes(),
        );
        out
    }
}

fn three_mcid_consecutive() -> Vec<u8> {
    let mut s = String::from("BT\n/F1 12 Tf\n50 700 Td\n");
    for (m, g) in [(0u32, "A"), (1, "B"), (2, "C")] {
        s.push_str(&format!("/Span << /MCID {} >> BDC\n({}) Tj\nEMC\n", m, g));
    }
    s.push_str("ET\n");
    s.into_bytes()
}

// =============================================================
// Probe 1 (extended): three consecutive same-replacement MCIDs
//                     emit the replacement EXACTLY once.
// ============================================================= ~keep

fn fixture_probe1_three_same_run() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_page_content(three_mcid_consecutive());
    let _span = b.add_elem(
        Elem::new(8, "Span", 7)
            .page(4)
            .actual_text("RUN")
            .k(K::Mcid(0, 0))
            .k(K::Mcid(1, 0))
            .k(K::Mcid(2, 0)),
    );
    b.register_mcid(0, 0, 8);
    b.register_mcid(0, 1, 8);
    b.register_mcid(0, 2, 8);
    b.build()
}

#[test]
fn probe1_three_consecutive_same_run_emits_once() {
    let pdf = fixture_probe1_three_same_run();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert_eq!(
        extracted.matches("RUN").count(),
        1,
        "three consecutive same-replacement MCIDs must collapse to ONE emission, got {:?}",
        extracted
    );
    assert!(!extracted.contains('A'), "raw 'A' must NOT appear, got {:?}", extracted);
    assert!(!extracted.contains('B'), "raw 'B' must NOT appear, got {:?}", extracted);
    assert!(!extracted.contains('C'), "raw 'C' must NOT appear, got {:?}", extracted);
}

// =============================================================
// Probe 2: mid-run inner override — outer O, inner I on MCID 1.
//          Expected: O, I, O (three emissions).
// ============================================================= ~keep

fn fixture_probe2_mid_run_inner_override() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_page_content(three_mcid_consecutive());
    // Outer covers MCIDs 0 and 2 (siblings flanking the inner subtree
    // that covers MCID 1).
    //   Outer Span /ActualText "O" /K [Mcid0, Inner, Mcid2]
    //     Inner Span /ActualText "I" /K [Mcid1] ~keep
    let outer = b.add_elem(
        Elem::new(8, "Span", 7)
            .page(4)
            .actual_text("O")
            .k(K::Mcid(0, 0))
            .k(K::Obj(9))
            .k(K::Mcid(2, 0)),
    );
    let _inner = b.add_elem(Elem::new(9, "Span", 8).page(4).actual_text("I").k(K::Mcid(1, 0)));
    b.register_mcid(0, 0, outer);
    b.register_mcid(0, 1, 9);
    b.register_mcid(0, 2, outer);
    b.build()
}

#[test]
fn probe2_mid_run_inner_override_yields_o_i_o() {
    let pdf = fixture_probe2_mid_run_inner_override();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    // O must appear twice (once for MCID 0, once for MCID 2 — the inner
    // breaks the run, so the outer fires before AND after). ~keep
    assert_eq!(
        extracted.matches('O').count(),
        2,
        "outer 'O' must emit twice (MCIDs 0 and 2 — run broken by inner), got {:?}",
        extracted
    );
    assert_eq!(
        extracted.matches('I').count(),
        1,
        "inner 'I' must emit exactly once at MCID 1, got {:?}",
        extracted
    );
    for raw in ['A', 'B', 'C'] {
        assert!(
            !extracted.contains(raw),
            "raw {:?} must NOT appear, got {:?}",
            raw,
            extracted
        );
    }
}

// =============================================================
// Probe 6: multi-page scope whose first descendant is on page 1,
//          NOT page 0. The implementation uses pre-order
//          "first descendant page", not numeric min. Page 0 must
//          NOT receive a suppress_only entry for this scope, and
//          page 1 must emit.
// ============================================================= ~keep

fn fixture_probe6_first_descendant_on_page1() -> Vec<u8> {
    // Two pages. The H1 scope covers MCID 0 on page 1 first (in pre-
    // order children) and MCID 0 on page 0 second. Per the index
    // builder, `first_page = 1`, and the page-0 MCID gets suppress_only. ~keep
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(A) Tj\nEMC\nET\n".to_vec());
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(B) Tj\nEMC\nET\n".to_vec());
    // Struct elem H1: /K [page-1 MCR, page-0 MCR] — pre-order picks
    // page 1 as first. ~keep
    let h1 = b.add_elem(
        Elem::new(10, "H1", 9)
            .actual_text("Heading X")
            .k(K::Mcid(0, 1))
            .k(K::Mcid(0, 0)),
    );
    b.register_mcid(0, 0, h1);
    b.register_mcid(1, 0, h1);
    b.build()
}

#[test]
fn probe6_first_descendant_on_page1_emits_on_page1_suppresses_page0() {
    let pdf = fixture_probe6_first_descendant_on_page1();
    let doc = PdfDocument::from_bytes(pdf).expect("open");

    let p0 = doc.extract_text(0).expect("extract_text 0");
    let p1 = doc.extract_text(1).expect("extract_text 1");

    // Per the implementation's pre-order rule, page 1 is the bearing
    // first page; page 0 must be suppress-only (no emission, no raw). ~keep
    assert!(
        p1.contains("Heading X"),
        "page 1 (first descendant in pre-order) must contain the replacement, got {:?}",
        p1
    );
    assert!(
        !p0.contains("Heading X"),
        "page 0 must NOT receive the replacement (different page), got {:?}",
        p0
    );
    assert!(
        !p0.contains('A'),
        "page 0 raw glyph 'A' must be suppressed (covered MCID), got {:?}",
        p0
    );
    assert!(
        !p1.contains('B'),
        "page 1 raw glyph 'B' must be suppressed (covered MCID), got {:?}",
        p1
    );
}

// =============================================================
// Probe 7: multi-page scope spanning pages 0 and 2 (skipping page
//          1). Page 1 carries an unrelated MCID with no struct-tree
//          ActualText. Verify page 1 stays untouched: no spurious
//          suppress, no spurious emission.
// ============================================================= ~keep

fn fixture_probe7_skip_middle_page() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(P0) Tj\nEMC\nET\n".to_vec());
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(MIDDLE) Tj\nEMC\nET\n".to_vec());
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(P2) Tj\nEMC\nET\n".to_vec());
    // For 3 pages: catalog=1, pages=2, font=3, page0=4, page1=5,
    // page2=6, content0=7, content1=8, content2=9, parent_tree=10,
    // struct_tree_root=11, first elem obj = 12. ~keep
    let _doc = b.add_elem(Elem::new(12, "Document", 11).k(K::Obj(13)).k(K::Obj(14)));
    let _h1 = b.add_elem(
        Elem::new(13, "H1", 12)
            .actual_text("Span across")
            .k(K::Mcid(0, 0))
            .k(K::Mcid(0, 2)),
    );
    let _p = b.add_elem(Elem::new(14, "P", 12).k(K::Mcid(0, 1)));
    b.register_mcid(0, 0, 13);
    b.register_mcid(1, 0, 14);
    b.register_mcid(2, 0, 13);
    b.build()
}

#[test]
fn probe7_skip_middle_page_keeps_middle_untouched() {
    let pdf = fixture_probe7_skip_middle_page();
    let doc = PdfDocument::from_bytes(pdf).expect("open");

    let p0 = doc.extract_text(0).expect("extract_text 0");
    let p1 = doc.extract_text(1).expect("extract_text 1");
    let p2 = doc.extract_text(2).expect("extract_text 2");

    assert!(p0.contains("Span across"), "page 0 must emit replacement, got {:?}", p0);
    assert!(!p0.contains("P0"), "page 0 raw glyphs must be suppressed, got {:?}", p0);

    assert!(
        p1.contains("MIDDLE"),
        "page 1 raw text must survive untouched, got {:?}",
        p1
    );
    assert!(
        !p1.contains("Span across"),
        "page 1 must NOT receive the unrelated H1 replacement, got {:?}",
        p1
    );

    // Page 2 (later page of the H1 scope): raw suppressed, NO second
    // replacement. ~keep
    assert!(
        !p2.contains("Span across"),
        "page 2 must NOT emit the replacement a second time, got {:?}",
        p2
    );
    assert!(!p2.contains("P2"), "page 2 raw glyphs must be suppressed, got {:?}", p2);
}

// =============================================================
// Probe 9: three-level BDC nesting — Tj after all three EMCs
//          restores to outer (no MCID). Pin behaviour.
// ============================================================= ~keep

#[test]
fn probe9_three_level_bdc_nest_then_outer_tj_attributes_to_outer() {
    // BDC[10] (A) BDC[11] (B) BDC[12] (C) EMC EMC EMC (D)
    //
    // After all three EMCs, current_mcid should be None and (D)
    // should land in a span with mcid=None. We can verify that
    // through the public `extract_page_text` API by checking that
    // the "D" glyph's span carries no MCID. ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 10 >> BDC\n(A) Tj\n\
                    /Span << /MCID 11 >> BDC\n(B) Tj\n\
                    /Span << /MCID 12 >> BDC\n(C) Tj\n\
                    EMC\nEMC\nEMC\n\
                    100 0 Td\n(D) Tj\n\
                    ET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(
        Elem::new(8, "Document", 7)
            .k(K::Mcid(10, 0))
            .k(K::Mcid(11, 0))
            .k(K::Mcid(12, 0)),
    );
    b.register_mcid(0, 10, 8);
    b.register_mcid(0, 11, 8);
    b.register_mcid(0, 12, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");
    let d_span = page
        .spans
        .iter()
        .find(|s| s.text.contains('D'))
        .unwrap_or_else(|| panic!("no D span; got {:?}", page.spans));
    assert_eq!(
        d_span.mcid, None,
        "after closing three nested BDCs, the next Tj must attribute to NO MCID (mcid=None), \
         got mcid={:?} text={:?}",
        d_span.mcid, d_span.text
    );

    for (g, expect_mcid) in [('A', 10u32), ('B', 11), ('C', 12)] {
        let s = page
            .spans
            .iter()
            .find(|s| s.text.contains(g))
            .unwrap_or_else(|| panic!("no {} span", g));
        assert_eq!(
            s.mcid,
            Some(expect_mcid),
            "glyph {} must carry MCID {} (nesting/restore failure), got {:?}",
            g,
            expect_mcid,
            s.mcid
        );
    }
}

// =============================================================
// Probe 10: BDC without /MCID (e.g. `/Span << /ActualText (X) >>` with
//          no /MCID key). MC-scope ActualText still emits once, and
//          the enclosing scope's MCID stays in effect for any text
//          AFTER the inner EMC.
// ============================================================= ~keep

#[test]
fn probe10_bdc_without_mcid_keeps_outer_mcid_after_emc() {
    // Outer BDC[5] (A) inner BDC[no-MCID, /ActualText "fi"] (X) EMC
    // (B) EMC
    //
    // Expected on the spans:
    //   - "A" span has mcid=5
    //   - "X" gets suppressed; replaced by "fi"; mcid is the outer 5
    //   - "B" span has mcid=5 (restored to outer) ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 5 >> BDC\n(A) Tj\n\
                    /Span << /ActualText <FEFF00660069> >> BDC\n(X) Tj\nEMC\n\
                    (B) Tj\n\
                    EMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(5, 0)));
    b.register_mcid(0, 5, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");
    // B span must carry mcid=5 (outer restored). ~keep
    let b_span = page
        .spans
        .iter()
        .find(|s| s.text.contains('B'))
        .unwrap_or_else(|| panic!("no B span; got {:?}", page.spans));
    assert_eq!(
        b_span.mcid,
        Some(5),
        "after inner BDC-without-MCID closes, 'B' must attribute to outer MCID 5, got {:?}",
        b_span.mcid
    );

    // The extract_text path: "fi" appears once (MC-scope replacement),
    // raw "X" suppressed. ~keep
    let extracted = doc.extract_text(0).expect("extract_text");
    assert!(
        extracted.contains("fi"),
        "MC-scope /ActualText 'fi' must appear, got {:?}",
        extracted
    );
    assert!(!extracted.contains('X'), "raw 'X' must NOT appear, got {:?}", extracted);
}

// =============================================================
// Probe 12: EMC with empty stack (malformed PDF). Must not panic;
//          subsequent extraction must still complete.
// ============================================================= ~keep

#[test]
fn probe12_unmatched_emc_does_not_panic() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    EMC\n\
                    /Span << /MCID 0 >> BDC\n(OK) Tj\nEMC\n\
                    ET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert!(
        extracted.contains("OK"),
        "post-stray-EMC content must extract, got {:?}",
        extracted
    );
}

// =============================================================
// Probe 13: two Tj inside one MC-scope ActualText with an
//          intervening Td. Replacement emitted ONCE, not twice.
// ============================================================= ~keep

#[test]
fn probe13_mc_scope_two_tj_with_td_emits_once() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 0 /ActualText <FEFF006D0063> >> BDC\n\
                    (X) Tj\n100 0 Td\n(Y) Tj\n\
                    EMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert_eq!(
        extracted.matches("mc").count(),
        1,
        "MC-scope /ActualText must emit ONCE across multiple Tj+Td, got {:?}",
        extracted
    );
    for raw in ['X', 'Y'] {
        assert!(
            !extracted.contains(raw),
            "raw {:?} must NOT appear, got {:?}",
            raw,
            extracted
        );
    }
}

// =============================================================
// Probe 14: two MC-scope ActualText sequences in a row — each
//          emits its own replacement, no leakage.
// ============================================================= ~keep

#[test]
fn probe14_two_mc_scope_actualtext_sequences_each_emit() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 0 /ActualText <FEFF0061> >> BDC\n(X) Tj\nEMC\n\
                    /Span << /MCID 1 /ActualText <FEFF0062> >> BDC\n(Y) Tj\nEMC\n\
                    ET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(0, 0)).k(K::Mcid(1, 0)));
    b.register_mcid(0, 0, 8);
    b.register_mcid(0, 1, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert_eq!(
        extracted.matches('a').count(),
        1,
        "first MC's 'a' must appear once, got {:?}",
        extracted
    );
    assert_eq!(
        extracted.matches('b').count(),
        1,
        "second MC's 'b' must appear once, got {:?}",
        extracted
    );
    assert!(!extracted.contains('X'), "raw 'X' must NOT appear, got {:?}", extracted);
    assert!(!extracted.contains('Y'), "raw 'Y' must NOT appear, got {:?}", extracted);
}

// =============================================================
// Probe 15: nested MC-scope ActualText — outer has /ActualText
//          "outer", inner has /ActualText "inner", inner wins
//          (innermost is the most-specific).
// ============================================================= ~keep

#[test]
fn probe15_nested_mc_scope_actualtext_inner_wins() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /ActualText <FEFF006F0075> >> BDC\n\
                    /Span << /MCID 0 /ActualText <FEFF0069006E> >> BDC\n\
                    (X) Tj\n\
                    EMC\nEMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert!(
        extracted.contains("in"),
        "inner MC-scope replacement 'in' must appear, got {:?}",
        extracted
    );
    assert!(
        !extracted.contains("ou"),
        "outer MC-scope replacement 'ou' must NOT appear (inner wins), got {:?}",
        extracted
    );
}

// =============================================================
// Probe 16: MC-scope /ActualText with EMPTY string. Pin behaviour:
//          today the empty replacement should suppress the raw
//          glyph entirely (no emission, no raw). This matches the
//          struct-tree-scope empty handling.
//
// If the implementation actually emits a raw glyph here, this is a
// MINOR inconsistency between MC-scope and struct-scope handling
// of empty /ActualText.
// ============================================================= ~keep

#[test]
fn probe16_mc_scope_empty_actualtext_pin_behaviour() {
    // /ActualText <FEFF> is an empty UTF-16BE string. ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 0 /ActualText <FEFF> >> BDC\n\
                    (X) Tj\n\
                    EMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7).k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    // Pin current behaviour: the MC-scope `actual_text` is `Some("")`,
    // so `peek_current_actual_text` returns Some("") and the emission
    // path runs — outputting an empty string and suppressing the raw
    // glyph. So neither the raw glyph nor any replacement text
    // appears. ~keep
    assert!(
        !extracted.contains('X'),
        "MC-scope empty /ActualText must still suppress the raw glyph 'X', got {:?}",
        extracted
    );
}

// =============================================================
// Probe 17: MarkInfo permutations.
//   17a — Suspects=true with NO MarkInfo dict at all
//         (parser sees /MarkInfo absent → struct_tree_marked uses
//         /StructTreeRoot existence). ActualText still resolves.
//   17b — Suspects=true with explicit MarkInfo (decoupled — already
//         covered by `actualtext_emits_when_suspects_true`; we add
//         the negative twin here for clarity).
//   17c — Marked=false WITHOUT a /StructTreeRoot... actually if
//         /StructTreeRoot is in the catalog, struct_tree_marked
//         still returns the tree. Pin that.
// ============================================================= ~keep

#[test]
fn probe17a_no_mark_info_still_resolves_actualtext_via_struct_tree_root() {
    let mut b = PdfBuilder::new().no_mark_info();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("replaced").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    // /StructTreeRoot is present in the catalog → struct_tree_marked
    // serves the tree even without /MarkInfo. ActualText fires. ~keep
    assert!(
        extracted.contains("replaced"),
        "no /MarkInfo + present /StructTreeRoot must still apply /ActualText, got {:?}",
        extracted
    );
    assert!(!extracted.contains('X'), "raw 'X' must NOT appear, got {:?}", extracted);
}

// Split for file-too-long (#1567): the module continues below via `include!`,
// not a sibling tests/*_part2.rs file -- Cargo auto-discovers every *.rs file
// directly under tests/ as its own test binary, so a sibling would fail to
// compile on its own and, if it somehow did, run every #[test] in it twice.
// A subdirectory is not auto-discovered. ~keep
include!("test_struct_actualtext_probes/part2.rs");
include!("test_struct_actualtext_probes/part3.rs");
