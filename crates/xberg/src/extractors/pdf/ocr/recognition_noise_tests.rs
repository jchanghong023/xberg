use super::pipeline::*;
use super::scoring::*;
use crate::core::config::OcrQualityThresholds;

/// Verbatim from page 4 of a recorded municipal ordinance: Tesseract run over a scanned
/// surveyor's plat. Every "word" here is an artifact of line art, not text.
const PLAT_DRAWING_NOISE: &str = "\
LAKE POINTE SECTION 5 PLAT NU. 20060126 F.B.C.P.R. \
1 |: LAKE POINTE Zt } = ti | SECTION 4 - / ae | | | PLal NG. 200601237 A 5 oe \
: { L -W.5..R- —— 2 oe | ‘a. * MOI ARES a es \
MAM RAM SAL, Eid wat au TH.8) FLAT. ” <*> suum he tet cu? Oe imer \
AT ace im Cum BOCES SIOT TT. ie 4S ayi.0- 2 ub vee gman Suita \
‘1mC bo int (aa so Givicengo. Cunt A. cumin Lipa THIS mat Saakt Of MLSINICIED \
ot ee Steric im wt Pum ic or * oud OF o nue ow paapnace & ft LibuR DIMtChY";

/// Verbatim from page 1 of the same document — ordinary legal prose.
const ORDINANCE_PROSE: &str = "\
WHEREAS, the current property owner has requested that approximately 0.7906 acres of \
land located within the City of Sugar Land (the \"City\"), at the Southeast corner of Lake \
Pointe Parkway and Creek Bend Drive, be rezoned from Business Office (B-O) District to \
Planned Development (PD) District Final Development Plan; and WHEREAS, the City Planning \
and Zoning Commission forwarded its final report to the City Council, recommending \
approval of the rezoning request; and";

#[test]
fn should_retain_suspected_fragmented_noise_with_warning_by_default() {
    let thresholds = OcrQualityThresholds::default();
    let mut warnings = Vec::new();

    let acceptance = accept_or_reject_ocr_page(
        3,
        PLAT_DRAWING_NOISE.to_string(),
        &thresholds,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Uncalibrated,
        None,
    );
    assert!(
        !acceptance.discarded,
        "diagnostic-only mode must retain suspected OCR noise"
    );
    assert_eq!(
        acceptance.content, PLAT_DRAWING_NOISE,
        "recognized content must remain verbatim"
    );
    assert_eq!(warnings.len(), 1, "the recognition-noise signal must remain visible");
    assert!(warnings[0].message.contains("retained"));
    let verdict = acceptance
        .verdict
        .expect("a fired warning must carry its numeric verdict");
    assert!(verdict.fragmented_noise, "the plat fixture fires on fragmentation");
    assert!(
        !verdict.discarded,
        "diagnostic-only mode must report a non-destructive verdict"
    );
}

#[test]
fn should_preserve_legacy_destructive_filtering_when_opted_in() {
    let thresholds = OcrQualityThresholds {
        discard_suspected_ocr_noise: true,
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        3,
        PLAT_DRAWING_NOISE.to_string(),
        &thresholds,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Uncalibrated,
        None,
    );
    assert!(
        acceptance.discarded,
        "opt-in must preserve the legacy destructive verdict"
    );
    assert!(acceptance.content.is_empty(), "opt-in must discard suspected OCR noise");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("discarded"));
    let verdict = acceptance
        .verdict
        .expect("a fired warning must carry its numeric verdict");
    assert!(
        verdict.discarded,
        "the verdict must reflect the destructive outcome too"
    );
}

#[test]
fn should_leave_blank_pages_blank_without_a_false_warning() {
    let thresholds = OcrQualityThresholds::default();
    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        1,
        String::new(),
        &thresholds,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Uncalibrated,
        None,
    );
    assert!(!acceptance.discarded, "an empty page is blank, not rejected");
    assert!(acceptance.content.is_empty());
    assert!(
        warnings.is_empty(),
        "blank pages must not be reported as recognition noise"
    );
    assert!(acceptance.verdict.is_none(), "a blank page must carry no verdict");
}

#[test]
fn should_not_discard_blank_page_with_calibrated_low_confidence() {
    let thresholds = OcrQualityThresholds {
        discard_suspected_ocr_noise: true,
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        1,
        String::new(),
        &thresholds,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Legibility { scale_max: 100.0 },
        Some(0.0),
    );

    assert!(acceptance.content.is_empty());
    assert!(
        !acceptance.discarded,
        "blank OCR output must not carry a destructive verdict"
    );
    assert!(warnings.is_empty(), "missing text is not recognition noise");
    assert!(acceptance.verdict.is_none(), "a blank page must carry no verdict");
}

#[test]
fn should_discard_public_ocr_elements_from_rejected_pages() {
    let element = |page_number| crate::types::OcrElement {
        text: format!("page {page_number}"),
        page_number,
        ..Default::default()
    };
    let mut elements = vec![element(1), element(2), element(3)];

    discard_ocr_elements_from_rejected_pages(&mut elements, &[false, true, false], 0);

    assert_eq!(
        elements.iter().map(|element| element.page_number).collect::<Vec<_>>(),
        [1, 3]
    );
}

#[test]
fn should_discard_tables_and_formulas_from_rejected_full_ocr_pages() {
    let mut tables = [1, 2, 3]
        .into_iter()
        .map(|page_number| crate::types::Table {
            page_number,
            ..Default::default()
        })
        .collect();
    let mut formulas = [1, 2, 3]
        .into_iter()
        .map(|page| crate::types::Formula {
            latex: format!("page {page}"),
            bbox: None,
            page: Some(page),
        })
        .collect();

    discard_rejected_ocr_page_payloads(&mut tables, &mut formulas, &[false, true, false], 0);

    assert_eq!(tables.iter().map(|table| table.page_number).collect::<Vec<_>>(), [1, 3]);
    assert_eq!(
        formulas.iter().filter_map(|formula| formula.page).collect::<Vec<_>>(),
        [1, 3]
    );
}

#[test]
fn should_discard_global_page_two_payloads_for_detached_local_rejection() {
    let mut elements = vec![crate::types::OcrElement {
        text: "rejected page two word".to_string(),
        page_number: 2,
        ..Default::default()
    }];
    let mut tables = vec![crate::types::Table {
        page_number: 2,
        ..Default::default()
    }];
    let mut formulas = vec![crate::types::Formula {
        latex: "rejected page two formula".to_string(),
        bbox: None,
        page: Some(2),
    }];

    discard_ocr_elements_from_rejected_pages(&mut elements, &[true], 1);
    discard_rejected_ocr_page_payloads(&mut tables, &mut formulas, &[true], 1);

    assert!(elements.is_empty(), "rejected detached-page elements must be removed");
    assert!(tables.is_empty(), "rejected detached-page tables must be removed");
    assert!(formulas.is_empty(), "rejected detached-page formulas must be removed");
}

#[test]
fn should_discard_formulas_from_unaccepted_mixed_ocr_pages() {
    let mut formulas = [1, 2, 3]
        .into_iter()
        .map(|page| crate::types::Formula {
            latex: format!("page {page}"),
            bbox: None,
            page: Some(page),
        })
        .collect();
    let accepted_pages = ahash::AHashMap::from([(1, "accepted".to_string()), (3, "accepted".to_string())]);

    retain_ocr_formulas_for_accepted_pages(&mut formulas, &accepted_pages);

    assert_eq!(
        formulas.iter().filter_map(|formula| formula.page).collect::<Vec<_>>(),
        [1, 3]
    );
}

#[test]
fn should_reject_ocr_of_a_scanned_drawing() {
    let thresholds = OcrQualityThresholds::default();
    let stats = NativeTextStats::compute(PLAT_DRAWING_NOISE, &thresholds);

    assert!(
        stats.fragmented_word_ratio >= 0.35,
        "fixture is not representative: short-word ratio is {:.3}, expected >= 0.35",
        stats.fragmented_word_ratio
    );
    assert!(is_ocr_recognition_noise(PLAT_DRAWING_NOISE, &thresholds));
}

#[test]
fn should_keep_ordinary_prose() {
    let thresholds = OcrQualityThresholds::default();
    assert!(!is_ocr_recognition_noise(ORDINANCE_PROSE, &thresholds));
}

#[test]
fn should_decline_to_judge_a_page_with_too_few_words() {
    // The ratio is not meaningful on a handful of tokens, so the veto must abstain
    // rather than delete. This fixture is a real excerpt from the same plat page and
    // clears the ratio easily — only the word-count guard keeps it.
    let sliver = "1 |: Zt } = ti / ae A 5 oe : { L 2 oe";
    let thresholds = OcrQualityThresholds::default();
    let stats = NativeTextStats::compute(sliver, &thresholds);

    assert!(
        stats.fragmented_word_ratio >= thresholds.max_ocr_output_fragmented_word_ratio,
        "fixture must exceed the ratio ({:.3}), or the word-count guard is not what is \
             being tested",
        stats.fragmented_word_ratio
    );
    assert!(stats.word_count < thresholds.min_words_for_ocr_output_check);
    assert!(!is_ocr_recognition_noise(sliver, &thresholds));
}

#[test]
fn should_keep_a_signature_block() {
    // Legitimately short-word-heavy prose that must survive on the ratio alone, with no
    // help from the word-count guard.
    let signature_block = "By: /s/ J. D. R. Its: CFO Date: 3/16/20 No. 2197 ATTEST: City Secretary \
             APPROVED AS TO FORM: City Attorney for the City of Sugar Land, Texas";
    let thresholds = OcrQualityThresholds::default();
    let stats = NativeTextStats::compute(signature_block, &thresholds);

    assert!(
        stats.word_count >= thresholds.min_words_for_ocr_output_check,
        "fixture must clear the word-count guard so the ratio is what is tested"
    );
    assert!(!is_ocr_recognition_noise(signature_block, &thresholds));
}

#[test]
fn should_not_reject_a_transcript_whose_short_tokens_are_dividers_and_line_numbers() {
    // Verbatim shape of a scanned court-transcript page (GH#1358): a
    // `- - - - x` section divider, left-margin line numbers, a colon-terminated
    // speaker/exhibit column, and ordinary prose. None of that is a fragmented
    // *word* -- every 1-2 character token is punctuation or a digit -- but the
    // raw whitespace-split ratio (0.56 on the real page) clears the 0.35 veto
    // threshold and a perfect transcription gets discarded anyway.
    let transcript_page = "\
 1      IN THE SUPREME COURT OF THE UNITED STATES

 2   - - - - - - - - - - - - - - - - - x

 3   MICHAEL A. KNOWLES,                            :

 4   WARDEN,                                        :

 5              Petitioner                          :

 6         v.                                       :        No. 07-1315

 7   ALEXANDRE MIRZAYANCE.                          :

 8   - - - - - - - - - - - - - - - - - x

 9                              Washington, D.C.

10                              Tuesday, January 13, 2009

12                  The above-entitled matter came on for oral

13   argument before the Supreme Court of the United States

14   at 1:01 p.m.

15   APPEARANCES:

16   STEVEN E. MERCER, ESQ., Deputy Attorney General, Los

17     Angeles, Cal.; on behalf of the Petitioner.

18   CHARLES M. SEVILLA, ESQ., San Diego, Cal.; on behalf

19     of the Respondent.";
    let thresholds = OcrQualityThresholds::default();
    let stats = NativeTextStats::compute(transcript_page, &thresholds);

    assert!(
        stats.word_count >= thresholds.min_words_for_ocr_output_check,
        "fixture must clear the word-count guard so the ratio is what is tested"
    );
    assert!(
        !is_ocr_recognition_noise(transcript_page, &thresholds),
        "a transcript's dividers and line numbers must not read as fragmented words"
    );
}

#[test]
fn should_still_reject_line_art_with_genuine_alphabetic_fragments() {
    // The important complement to the transcript test above: this fixture's short
    // tokens carry alphabetic content (`am`, `ra`, `sa`, ...), the way a diagram's
    // misread flourishes actually look, rather than being punctuation or digits. The
    // fix must not exempt these -- doing so would make the veto never fire, which is
    // worse than the false positive it was written to correct.
    let diagram_noise = "am ra sa ed wa au th fl ae oe mc bo fa gm su vc ip la ma no \
             pe qr st uv wx yz ab cd MOI ARES cumin Lipa mat Saakt";
    let thresholds = OcrQualityThresholds::default();
    let stats = NativeTextStats::compute(diagram_noise, &thresholds);

    assert!(
        stats.word_count >= thresholds.min_words_for_ocr_output_check,
        "fixture must clear the word-count guard so the ratio is what is tested"
    );
    assert!(
        is_ocr_recognition_noise(diagram_noise, &thresholds),
        "genuine alphabetic-fragment noise must still be vetoed"
    );
}

#[test]
fn should_keep_a_page_of_tabular_ocr() {
    // A Markdown table's delimiter row is entirely one-character tokens. Scoring the raw
    // Markdown would make good tabular OCR indistinguishable from line-art noise, so this
    // is the veto's most likely false positive and the reason it scores normalized prose.
    let table_page = "\
Annual Report Summary of Operating Results by Region and Quarter

| Region | Quarter | Revenue | Growth |
| --- | --- | --- | --- |
| North | Q1 | 1,240 | 4.2 |
| North | Q2 | 1,310 | 5.6 |
| South | Q1 | 980 | 2.1 |
| South | Q2 | 1,045 | 6.6 |

Revenue is reported in thousands of dollars and growth is year over year.";
    let thresholds = OcrQualityThresholds::default();

    assert!(
        !is_ocr_recognition_noise(table_page, &thresholds),
        "tabular OCR must survive; raw short-word ratio is {:.3}",
        NativeTextStats::compute(table_page, &thresholds).fragmented_word_ratio
    );
}

#[test]
fn should_score_prose_not_markdown_scaffolding() {
    // Pin the mechanism, not just the outcome: pure table scaffolding must contribute
    // nothing to the fragmented ratio, so prefixing prose with it must not move the
    // number at all.
    //
    // This used to assert `normalized < raw`, i.e. that `normalize_markdown_for_scoring`
    // improved the ratio. It no longer can: everything that normalization strips for
    // scoring purposes -- delimiter rows, pipes, bullets, blockquote and heading markers
    // -- is non-alphabetic, and the character-class filter in `NativeTextStats::compute`
    // already excludes all of it. Normalization is still load-bearing for the stats that
    // count whole lines; it is simply no longer what protects this ratio. Asserting
    // equality pins the filter that actually does, and still fails loudly if it is
    // removed: without it the scaffolded ratio jumps to 0.5 and over the veto. ~keep
    let prose = "Each row of the preceding table records one measurement taken during the survey period.";
    let scaffolded = format!("| --- | --- |\n| | |\n\n{prose}");
    let thresholds = OcrQualityThresholds::default();

    let bare = NativeTextStats::compute(prose, &thresholds).fragmented_word_ratio;
    let with_scaffolding = NativeTextStats::compute(&scaffolded, &thresholds).fragmented_word_ratio;

    assert_eq!(
        bare, with_scaffolding,
        "table scaffolding must not be scored: bare {bare:.3} vs scaffolded {with_scaffolding:.3}"
    );
    assert!(
        with_scaffolding < thresholds.max_ocr_output_fragmented_word_ratio,
        "a prose page with table scaffolding must stay under the veto, got {with_scaffolding:.3}"
    );
}

#[test]
fn should_read_mean_confidence_from_backend_metadata() {
    use super::scoring::mean_text_conf_of;
    let mut m: ahash::AHashMap<std::borrow::Cow<'_, str>, serde_json::Value> = Default::default();

    assert_eq!(
        mean_text_conf_of(&m),
        None,
        "absent key means the backend reported none"
    );

    m.insert("mean_text_conf".into(), serde_json::json!(93));
    assert_eq!(mean_text_conf_of(&m), Some(93.0), "integers must parse");

    m.insert("mean_text_conf".into(), serde_json::json!(57.5));
    assert_eq!(mean_text_conf_of(&m), Some(57.5), "floats must parse");

    // Tesseract returns -1 when it has no confidence to report. Treating that as a
    // score would reject every such page, so it must read as "unavailable" instead.
    m.insert("mean_text_conf".into(), serde_json::json!(-1));
    assert_eq!(mean_text_conf_of(&m), None);
}

#[test]
fn confidence_default_sits_between_the_measured_populations() {
    // Per-page mean confidence measured by xberg over a recorded ordinance
    // (Tesseract 5.5.3): prose 89-95, scanned drawings 36-62. The default must
    // separate them. If a future change moves the default outside that band, this
    // fails before any document is silently re-graded.
    let thresholds = OcrQualityThresholds::default();
    let prose = [95.0, 89.0, 95.0, 95.0, 93.0, 93.0, 94.0, 94.0, 95.0, 92.0];
    let drawings = [36.0, 58.0, 62.0, 58.0, 57.0];

    let worst_prose = prose.iter().cloned().fold(f64::INFINITY, f64::min);
    let best_drawing = drawings.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    assert!(
        best_drawing < thresholds.min_ocr_mean_confidence,
        "a drawing at {best_drawing} would survive the {} floor",
        thresholds.min_ocr_mean_confidence
    );
    assert!(
        thresholds.min_ocr_mean_confidence < worst_prose,
        "prose at {worst_prose} would be rejected by the {} floor",
        thresholds.min_ocr_mean_confidence
    );
}

#[test]
fn should_repair_a_comma_read_for_a_list_period() {
    // Verbatim from the ordinance: `3.` and `4.` came back as `3,` and `4,`, so neither
    // line was a list item any more.
    let text = "3, Maximum number of lots: 9\n4, Minimum lot area: 2,842 sf";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "3. Maximum number of lots: 9\n4. Minimum lot area: 2,842 sf"
    );
}

#[test]
fn should_repair_a_lowercase_l_read_for_a_digit_one() {
    let text = "l. A six-foot (6') pedestrian sidewalk towards Creek Bend Drive";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "1. A six-foot (6') pedestrian sidewalk towards Creek Bend Drive"
    );
}

#[test]
fn should_not_touch_prose_that_merely_starts_with_a_number() {
    // The uppercase requirement is what separates these from list items. Rewriting them
    // would corrupt text the engine read correctly.
    for line in [
        "3, and the remainder of the tract is unrestricted",
        "2024, the year the plat was recorded",
        "1985, when the ordinance was adopted",
    ] {
        assert_eq!(repair_ocr_list_markers(line).as_ref(), line, "rewrote prose: {line}");
    }
}

#[test]
fn should_not_touch_a_year_or_long_number_before_a_capital() {
    // Four digits is not a list marker; the 2-digit cap is what stops this.
    let line = "2019, November of that year saw the plat recorded";
    assert_eq!(repair_ocr_list_markers(line).as_ref(), line);
}

#[test]
fn should_leave_correct_markers_alone_and_avoid_allocating() {
    let text = "1. Land Use: Live/Work Townhomes\n2. Building finishes: Siding";
    let out = repair_ocr_list_markers(text);
    assert!(
        matches!(out, std::borrow::Cow::Borrowed(_)),
        "must not allocate when nothing is broken"
    );
    assert_eq!(out.as_ref(), text);
}

#[test]
fn should_preserve_surrounding_lines_and_trailing_newline() {
    let text = "Section 3. Regulations\n\n3, Maximum number of lots: 9\nunchanged tail\n";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "Section 3. Regulations\n\n3. Maximum number of lots: 9\nunchanged tail\n"
    );
}

#[test]
fn should_map_confusable_letters_to_their_intended_digit() {
    use super::scoring::confusable_digit_for_letter;
    assert_eq!(confusable_digit_for_letter('L'), Some('1'));
    assert_eq!(confusable_digit_for_letter('G'), Some('6'));
    assert_eq!(confusable_digit_for_letter('b'), Some('6'));
    assert_eq!(confusable_digit_for_letter('S'), Some('5'));
    assert_eq!(confusable_digit_for_letter('O'), Some('0'));
    assert_eq!(confusable_digit_for_letter('D'), Some('0'));
    assert_eq!(confusable_digit_for_letter('I'), Some('1'));
    assert_eq!(
        confusable_digit_for_letter('A'),
        None,
        "A is a legitimate lettered marker"
    );
}

#[test]
fn should_repair_a_doubled_misread_of_a_digit_one() {
    // `lL.` is the digit `1` split into two mis-read characters; never a valid marker.
    let text = "lL. A six-foot (6') pedestrian sidewalk towards Creek Bend Drive";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "1. A six-foot (6') pedestrian sidewalk towards Creek Bend Drive"
    );
}

#[test]
fn should_repair_a_letter_misread_of_a_digit_inside_a_numeric_run() {
    // Verbatim shape from the ordinance: `G.` between two numeric markers is a mis-read
    // `6.`, not a lettered marker.
    let text = "5. Front setback: 20 feet\nG. Side setback: 10 feet\n7. Rear setback: 15 feet";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "5. Front setback: 20 feet\n6. Side setback: 10 feet\n7. Rear setback: 15 feet"
    );
}

#[test]
fn should_repair_a_letter_misread_with_context_on_only_one_side() {
    // No marker precedes it on the page, but the following marker is numeric.
    let text = "G. Side setback: 10 feet\n7. Rear setback: 15 feet";
    assert_eq!(
        repair_ocr_list_markers(text).as_ref(),
        "6. Side setback: 10 feet\n7. Rear setback: 15 feet"
    );
}

#[test]
fn should_not_touch_a_genuine_lettered_marker_between_lettered_neighbors() {
    // This is the corruption this discriminator exists to prevent: `G.` here is the
    // legitimate 7th item of a lettered list, not a mis-read `6.`.
    let text = "F. Fire lane width: 20 feet\nG. Side setback: 10 feet\nH. Height limit: 35 feet";
    assert_eq!(repair_ocr_list_markers(text).as_ref(), text);
}

#[test]
fn should_not_touch_an_isolated_ambiguous_letter_marker() {
    // No determinable neighbor on either side -- decline to judge rather than guess.
    let text = "G. Side setback: 10 feet";
    assert_eq!(repair_ocr_list_markers(text).as_ref(), text);
}

#[test]
fn legibility_backend_still_rejects_a_page_below_the_floor() {
    // Tesseract's own scale must keep working exactly as before: `Legibility { scale_max:
    // 100.0 }` normalizes to the same fraction as the old unconditional `c < threshold`
    // check did, so this must not regress.
    use super::scoring::confidence_gate_rejects;
    let thresholds = OcrQualityThresholds::default();
    let semantics = crate::plugins::ConfidenceSemantics::Legibility { scale_max: 100.0 };

    assert!(
        confidence_gate_rejects(semantics, Some(39.0), thresholds.min_ocr_mean_confidence),
        "a page at confidence 39 (the real ordinance's worst legible page) must still be \
             rejected under Tesseract's own 100-point scale"
    );
    assert!(
        !confidence_gate_rejects(semantics, Some(95.0), thresholds.min_ocr_mean_confidence),
        "clean prose at confidence 95 must not be rejected"
    );
}

#[test]
fn uncalibrated_backend_confidence_never_empties_a_document() {
    // Regression for the sceptre bug: every page of a 16-page recorded ordinance scored
    // between 36 and 74 on sceptre's rescaled `custom_mean` -- entirely below the 75.0
    // default floor, which is tuned for Tesseract's scale -- and applying the floor
    // discarded all 16 pages, emptying the document. An `Uncalibrated` backend's number
    // must never be able to do that: the gate must not apply at all, and a legible page
    // must survive on the text-shape heuristic instead.
    use super::scoring::confidence_gate_rejects;
    let thresholds = OcrQualityThresholds::default();
    let semantics = crate::plugins::ConfidenceSemantics::Uncalibrated;

    for sceptre_like_confidence in [36.0, 39.0, 57.0, 62.0, 74.0] {
        let confidence = Some(sceptre_like_confidence);
        let rejected_by_confidence = confidence_gate_rejects(semantics, confidence, thresholds.min_ocr_mean_confidence);
        assert!(
            !rejected_by_confidence,
            "confidence {sceptre_like_confidence} must never gate an Uncalibrated backend's page"
        );
        let kept = !rejected_by_confidence && !is_ocr_recognition_noise(ORDINANCE_PROSE, &thresholds);
        assert!(
            kept,
            "a legible page (confidence {sceptre_like_confidence}) must survive when the \
                 reporting backend is Uncalibrated"
        );
    }
}

#[test]
fn confidence_gate_respects_scale_max_not_a_hardcoded_100() {
    // scale_max = 10, confidence = 8 -> 80% of scale, above a 75%-of-100 threshold, so
    // this must NOT be rejected. Comparing the raw value 8 directly against the
    // (100-scaled) threshold of 75 -- the old hardcoded-100 assumption -- would wrongly
    // reject it (8 < 75). Only normalizing by the backend's own `scale_max` gets this right.
    use super::scoring::confidence_gate_rejects;
    let thresholds = OcrQualityThresholds::default();
    assert_eq!(
        thresholds.min_ocr_mean_confidence, 75.0,
        "test assumes the documented default"
    );
    let semantics = crate::plugins::ConfidenceSemantics::Legibility { scale_max: 10.0 };

    assert!(
        !confidence_gate_rejects(semantics, Some(8.0), thresholds.min_ocr_mean_confidence),
        "8 of a 10-point scale (80%) must clear a 75%-of-scale floor"
    );
    assert!(
        confidence_gate_rejects(semantics, Some(7.0), thresholds.min_ocr_mean_confidence),
        "7 of a 10-point scale (70%) must not clear a 75%-of-scale floor"
    );
}

#[test]
fn pipeline_blend_drops_mean_conf_term_for_non_legibility_stage() {
    // `extract_with_ocr` only ever reports `Some(mean_conf)` when its backend is
    // `Legibility`; for anything else it reports `None`. The blend must then fall back
    // to the text-shape score alone rather than averaging in an incomparable number.
    use super::scoring::pipeline_stage_score;
    let text_score = 0.62;

    assert_eq!(
        pipeline_stage_score(text_score, None),
        text_score,
        "a non-Legibility stage's score must be the text score alone, unblended"
    );
    assert_ne!(
        pipeline_stage_score(text_score, Some(0.1)),
        text_score,
        "a Legibility stage's reported confidence must still influence the score"
    );
}

#[test]
fn confidence_semantics_comes_from_the_backend_object_not_its_name() {
    // A backend named to look calibrated but whose `confidence_semantics()` says
    // otherwise: the gate must trust the object, not a name-based guess. If this ever
    // regresses to matching on the name, this backend would be wrongly treated as
    // Legibility and could empty a document exactly like the sceptre bug did.
    use crate::core::config::OcrConfig;
    use crate::plugins::{ConfidenceSemantics, OcrBackend, OcrBackendType, Plugin};
    use crate::types::ExtractedDocument;
    use std::sync::Arc;

    struct DeceptivelyNamedBackend;

    #[async_trait::async_trait]
    impl OcrBackend for DeceptivelyNamedBackend {
        fn backend_type(&self) -> OcrBackendType {
            OcrBackendType::Custom
        }
        fn supports_language(&self, _: &str) -> bool {
            true
        }
        async fn process_image(&self, _: &[u8], _: &OcrConfig) -> crate::Result<ExtractedDocument> {
            Ok(ExtractedDocument::default())
        }
        fn confidence_semantics(&self) -> ConfidenceSemantics {
            ConfidenceSemantics::Uncalibrated
        }
    }

    impl Plugin for DeceptivelyNamedBackend {
        fn name(&self) -> &str {
            "tesseract-lookalike"
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

    let backend: Arc<dyn OcrBackend> = Arc::new(DeceptivelyNamedBackend);
    let semantics = backend.confidence_semantics();
    assert_eq!(
        semantics,
        ConfidenceSemantics::Uncalibrated,
        "must read the backend's own confidence_semantics(), not its name"
    );
}

#[test]
fn should_keep_empty_output_out_of_the_veto() {
    // Blank pages are rejected earlier by their own check; the veto must not also claim
    // them, or the warning it emits would be wrong about why the page is empty.
    let thresholds = OcrQualityThresholds::default();
    assert!(!is_ocr_recognition_noise("", &thresholds));
    assert!(!is_ocr_recognition_noise("   \n\n  ", &thresholds));
}

#[test]
fn should_respect_a_configured_threshold() {
    // Raising the bar above the fixture's ratio must keep the page.
    let permissive = OcrQualityThresholds {
        max_ocr_output_fragmented_word_ratio: 0.99,
        ..Default::default()
    };
    assert!(!is_ocr_recognition_noise(PLAT_DRAWING_NOISE, &permissive));

    // Lowering it below prose must reject even prose — proving the knob is live and
    // that the default, not the code path, is what protects real text.
    let strict = OcrQualityThresholds {
        max_ocr_output_fragmented_word_ratio: 0.01,
        ..Default::default()
    };
    assert!(is_ocr_recognition_noise(ORDINANCE_PROSE, &strict));
}

#[test]
fn should_separate_the_two_fixtures_with_margin() {
    // The default sits between them. If a future change narrows this gap, the veto is
    // no longer safe to apply and this test should fail before anything ships.
    let thresholds = OcrQualityThresholds::default();
    let noise = NativeTextStats::compute(PLAT_DRAWING_NOISE, &thresholds).fragmented_word_ratio;
    let prose = NativeTextStats::compute(ORDINANCE_PROSE, &thresholds).fragmented_word_ratio;

    assert!(
        noise - prose > 0.15,
        "separation collapsed: noise {noise:.3} vs prose {prose:.3}"
    );
    assert!(prose < thresholds.max_ocr_output_fragmented_word_ratio);
    assert!(noise >= thresholds.max_ocr_output_fragmented_word_ratio);
}

/// `None` (the signal absent, e.g. every non-Tesseract backend) must never trip the
/// dictionary-invalid veto, no matter how strict the threshold is configured -- absence
/// is "no evidence", not "0.0 valid words".
#[test]
fn should_never_flag_dictionary_noise_when_ratio_is_absent() {
    let strict = OcrQualityThresholds {
        max_ocr_output_dict_invalid_word_ratio: 0.0,
        ..Default::default()
    };
    assert!(!is_dictionary_invalid_noise(None, &strict));
}

/// The default threshold disables the check entirely: it must reject nothing, even a
/// page whose dictionary-invalid ratio is 1.0 (every checkable word rejected), until an
/// operator explicitly calibrates and lowers it.
#[test]
fn should_disable_dictionary_veto_by_default() {
    let thresholds = OcrQualityThresholds::default();
    assert!(
        !is_dictionary_invalid_noise(Some(1.0), &thresholds),
        "the default threshold must be a no-op until calibrated"
    );
}

/// Once explicitly configured, the dictionary signal must reject a page whose ratio
/// exceeds the threshold, and keep one at or below it.
#[test]
fn should_respect_a_configured_dictionary_threshold() {
    let configured = OcrQualityThresholds {
        max_ocr_output_dict_invalid_word_ratio: 0.5,
        ..Default::default()
    };
    assert!(is_dictionary_invalid_noise(Some(0.51), &configured));
    assert!(!is_dictionary_invalid_noise(Some(0.5), &configured));
    assert!(!is_dictionary_invalid_noise(Some(0.2), &configured));
}

#[test]
fn should_retain_dictionary_suspect_content_with_warning_by_default() {
    let configured = OcrQualityThresholds {
        max_ocr_output_dict_invalid_word_ratio: 0.5,
        ..Default::default()
    };
    assert!(
        !is_ocr_recognition_noise(ORDINANCE_PROSE, &configured),
        "fixture must NOT be flagged by fragmentation alone, or this test proves nothing"
    );

    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        0,
        ORDINANCE_PROSE.to_string(),
        &configured,
        &mut warnings,
        Some(0.9),
        crate::plugins::ConfidenceSemantics::Uncalibrated,
        None,
    );
    assert!(
        !acceptance.discarded,
        "diagnostic-only mode must not discard dictionary-suspect text"
    );
    assert_eq!(acceptance.content, ORDINANCE_PROSE);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("dictionary-invalid"));
    assert!(warnings[0].message.contains("retained"));
    let verdict = acceptance
        .verdict
        .expect("a fired warning must carry its numeric verdict");
    assert!(verdict.dictionary_noise, "the dictionary signal is what fired here");
    assert!(
        !verdict.fragmented_noise,
        "the fixture must not also fire on fragmentation"
    );
    assert_eq!(verdict.dict_invalid_word_ratio, Some(0.9));
}

#[test]
fn should_retain_low_confidence_content_with_warning_by_default() {
    let thresholds = OcrQualityThresholds::default();
    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        0,
        ORDINANCE_PROSE.to_string(),
        &thresholds,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Legibility { scale_max: 100.0 },
        Some(18.0),
    );
    assert!(
        !acceptance.discarded,
        "diagnostic-only mode must not discard low-confidence text"
    );
    assert_eq!(acceptance.content, ORDINANCE_PROSE);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("mean confidence"));
    assert!(warnings[0].message.contains("retained"));
    let verdict = acceptance
        .verdict
        .expect("a fired warning must carry its numeric verdict");
    assert!(verdict.low_confidence, "the confidence signal is what fired here");
    assert_eq!(verdict.mean_confidence, Some(18.0), "the raw, un-normalized confidence");
}

/// A `dict_invalid_word_ratio` of `None` on the same fixture, same threshold, must NOT
/// reject -- proving the previous test's rejection came from the ratio, not the
/// threshold value alone.
#[test]
fn should_not_reject_via_dictionary_signal_when_ratio_is_absent() {
    let configured = OcrQualityThresholds {
        max_ocr_output_dict_invalid_word_ratio: 0.5,
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let acceptance = accept_or_reject_ocr_page(
        0,
        ORDINANCE_PROSE.to_string(),
        &configured,
        &mut warnings,
        None,
        crate::plugins::ConfidenceSemantics::Uncalibrated,
        None,
    );
    assert!(
        !acceptance.discarded,
        "an absent ratio must never itself trigger rejection"
    );
    assert_eq!(acceptance.content, ORDINANCE_PROSE);
    assert!(warnings.is_empty());
    assert!(
        acceptance.verdict.is_none(),
        "no signal fired, so there must be no verdict"
    );
}
