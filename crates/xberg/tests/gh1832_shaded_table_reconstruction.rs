//! Ground-truth measurement for the scanned-table reconstruction cluster: GH#1832 (a two-word
//! header splits into two columns), GH#1833 (several values glue into one cell across the
//! shading's underscore-like marks) and GH#1834 (the tail of a multi-word row label becomes a
//! row of its own).
//!
//! All three were reported against `shaded_table_scan.pdf` and all three are properties of the
//! same reconstruction, so they are measured together against one ground truth rather than
//! asserted one symptom at a time. The fixture is synthetic -- a generated six-year summary with
//! shaded subtotal rows -- so every cell's correct value is known exactly and the metric can be
//! an absolute count of correct values recovered, never a ratio over output whose length these
//! changes alter.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: org logging policy exempts tests
#![cfg(all(feature = "ocr", feature = "pdf"))]

mod helpers;
use helpers::extract_bytes_document_blocking;
use xberg::core::config::{ExtractionConfig, OcrConfig};
use xberg::types::TesseractConfig;

const SCANNED_TABLE: &[u8] = include_bytes!("fixtures/ocr/shaded_table_scan.pdf");

/// The fixture's 23 data rows, label first then Year 1 through Year 6, read off the rendered
/// page. Every value is printed in full on the page, so a miss is a reconstruction defect and
/// never an ambiguity in the source. ~keep
const GROUND_TRUTH: &[[&str; 7]] = &[
    ["APPLES", "48,210", "49,850", "51,344", "52,885", "54,471", "56,105"],
    ["PEARS", "21,406", "21,977", "22,636", "23,315", "24,015", "24,735"],
    ["PLUMS AND FIGS", "7,812", "7,968", "8,127", "8,290", "8,456", "8,625"],
    ["GRAPES", "3,104", "3,197", "3,293", "3,391", "3,493", "3,598"],
    [
        "SUBTOTAL FRUIT",
        "80,532",
        "82,992",
        "85,400",
        "87,881",
        "90,435",
        "93,063",
    ],
    [
        "MELONS AND BERRIES",
        "5,103",
        "5,256",
        "5,413",
        "5,576",
        "5,743",
        "5,915",
    ],
    ["TRANSFERS IN", "3,250", "3,250", "3,250", "3,250", "3,250", "3,250"],
    [
        "TOTAL PRODUCE",
        "88,885",
        "91,498",
        "94,063",
        "96,707",
        "99,428",
        "102,228",
    ],
    ["BREAD", "41,920", "43,177", "44,472", "45,806", "47,180", "48,595"],
    ["CHEESE", "18,402", "19,138", "19,903", "20,699", "21,527", "22,388"],
    [
        "SPOILAGE", "(2,100)", "(2,163)", "(2,227)", "(2,294)", "(2,363)", "(2,434)",
    ],
    [
        "SUBTOTAL DAIRY",
        "58,222",
        "60,152",
        "62,148",
        "64,211",
        "66,344",
        "68,549",
    ],
    ["SALT", "4,871", "5,017", "5,167", "5,322", "5,481", "5,645"],
    ["PEPPER", "19,334", "19,914", "20,511", "21,126", "21,759", "22,411"],
    ["TRANSFERS OUT", "6,102", "6,285", "6,473", "6,667", "6,867", "7,073"],
    [
        "SUBTOTAL OTHER GOODS",
        "30,307",
        "31,216",
        "32,151",
        "33,115",
        "34,107",
        "35,129",
    ],
    [
        "TOTAL GOODS",
        "88,529",
        "91,368",
        "94,299",
        "97,326",
        "100,451",
        "103,678",
    ],
    ["TIMBER", "1,500", "1,545", "1,591", "1,639", "1,688", "1,739"],
    [
        "TOTAL NON GOODS",
        "(1,200)",
        "(1,236)",
        "(1,273)",
        "(1,311)",
        "(1,350)",
        "(1,391)",
    ],
    [
        "OPENING STOCK BALANCE",
        "30,118",
        "30,474",
        "30,604",
        "30,368",
        "29,749",
        "28,726",
    ],
    [
        "NET CHANGE (DEFICIT)",
        "356",
        "130",
        "(236)",
        "(619)",
        "(1,023)",
        "(1,450)",
    ],
    ["EXPECTED SURPLUS", "1,012", "1,042", "1,073", "1,105", "1,138", "1,172"],
    [
        "CLOSING STOCK BALANCE",
        "30,474",
        "30,604",
        "30,368",
        "29,749",
        "28,726",
        "27,276",
    ],
];

fn config_with_preprocessing(psm: i32, shaded: bool, downstream_otsu: bool) -> ExtractionConfig {
    let preprocessing = xberg::types::ImagePreprocessingConfig {
        normalize_shaded_rows: shaded,
        deskew: downstream_otsu,
        binarization_method: if downstream_otsu { "otsu" } else { "none" }.to_string(),
        ..Default::default()
    };
    ExtractionConfig {
        force_ocr: true,
        use_cache: false,
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            tesseract_config: Some(TesseractConfig {
                psm: Some(psm),
                use_cache: false,
                enable_table_detection: true,
                preprocessing: Some(preprocessing),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// How many of the 138 ground-truth values land in the correct row, in the correct column.
/// Position matters: a value recovered into the wrong column is a reconstruction failure, and a
/// metric that only asked "does this string appear somewhere" would score GH#1833's glued cell
/// as a success.
fn correct_values_in_place(table: &xberg::types::Table) -> (usize, Vec<String>) {
    let mut correct = 0;
    let mut misses = Vec::new();
    for truth in GROUND_TRUTH {
        let label = truth[0];
        let Some(row) = table
            .cells
            .iter()
            .find(|row| row.first().is_some_and(|cell| cell.trim() == label))
        else {
            misses.push(format!("{label}: row absent"));
            continue;
        };
        for (offset, expected) in truth[1..].iter().enumerate() {
            match row.get(offset + 1) {
                Some(actual) if actual.trim() == *expected => correct += 1,
                Some(actual) => misses.push(format!("{label} col{}: want {expected:?} got {actual:?}", offset + 1)),
                None => misses.push(format!("{label} col{}: want {expected:?} got <short row>", offset + 1)),
            }
        }
    }
    (correct, misses)
}

/// The eight shaded rows of `shaded_table_scan.pdf`, by fill kind. Determined from the fixture's
/// own pixels (the fixture has no generator in this repo -- vendored from closed PR #1824): the
/// page's single JPX image decoded to grayscale, each band's row-median taken over the central
/// 6/8 of the width, mirroring `ocr::shaded_rows::find_shaded_bands`. Measured medians, of 255 --
/// DARK 74/76/76, MID 148/149, LIGHT 165/165/167. Only the DARK rows fall below
/// `DARK_ROW_GRAY_THRESHOLD` (110) and so only they reach `invert_dark_bands`; that is what makes
/// them a separate population for GH#1837's measurement. ~keep
const DARK_FILL_ROW_LABELS: &[&str] = &["TOTAL PRODUCE", "TOTAL GOODS", "CLOSING STOCK BALANCE"];
/// White text on a mid-grey fill (row-median 148-149). Main's `normalize_shaded_rows` doc comment
/// records these as regressing 12 -> 6 under the option; 2 rows x 6 year columns = 12. GH#1837. ~keep
const MID_FILL_ROW_LABELS: &[&str] = &["TOTAL NON GOODS", "NET CHANGE (DEFICIT)"];
/// Dark text on a light-grey fill (row-median 165-167); the three labels
/// `gh1785_shaded_row_normalization.rs` counts on its own, different fixture. GH#1837. ~keep
const LIGHT_FILL_ROW_LABELS: &[&str] = &["SUBTOTAL FRUIT", "SUBTOTAL DAIRY", "SUBTOTAL OTHER GOODS"];

/// One row-kind's positional scoring result: correct values, values possible (rows * 6), and the
/// labels whose row is entirely absent from the reconstructed table.
struct KindScore {
    kind: &'static str,
    correct: usize,
    of: usize,
    rows_absent: Vec<&'static str>,
}

/// [`correct_values_in_place`], partitioned by row-fill kind (DARK / MID / LIGHT / PLAIN) so a
/// page-wide total cannot hide a trade between kinds -- exactly the blindness that let the
/// published 98/98/102/102 pooled totals show zero page-wide change while GH#1837 and main's own
/// doc comment each claim a kind-specific loss. PLAIN is every ground-truth label in none of the
/// three shaded-kind lists above.
fn correct_values_by_kind(table: &xberg::types::Table) -> [KindScore; 4] {
    let mut scores = [
        KindScore {
            kind: "DARK",
            correct: 0,
            of: 0,
            rows_absent: Vec::new(),
        },
        KindScore {
            kind: "MID",
            correct: 0,
            of: 0,
            rows_absent: Vec::new(),
        },
        KindScore {
            kind: "LIGHT",
            correct: 0,
            of: 0,
            rows_absent: Vec::new(),
        },
        KindScore {
            kind: "PLAIN",
            correct: 0,
            of: 0,
            rows_absent: Vec::new(),
        },
    ];
    for truth in GROUND_TRUTH {
        let label = truth[0];
        let index = if DARK_FILL_ROW_LABELS.contains(&label) {
            0
        } else if MID_FILL_ROW_LABELS.contains(&label) {
            1
        } else if LIGHT_FILL_ROW_LABELS.contains(&label) {
            2
        } else {
            3
        };
        let score = &mut scores[index];
        score.of += 6;
        let Some(row) = table
            .cells
            .iter()
            .find(|row| row.first().is_some_and(|cell| cell.trim() == label))
        else {
            score.rows_absent.push(label);
            continue;
        };
        for (offset, expected) in truth[1..].iter().enumerate() {
            if row.get(offset + 1).is_some_and(|actual| actual.trim() == *expected) {
                score.correct += 1;
            }
        }
    }
    scores
}

/// Extract once and return the first reconstructed table, or `None` if the page produced none.
fn first_table(psm: i32, shaded: bool) -> Option<xberg::types::Table> {
    first_table_with_preprocessing(psm, shaded, true)
}

fn first_table_with_preprocessing(psm: i32, shaded: bool, downstream_otsu: bool) -> Option<xberg::types::Table> {
    let config = config_with_preprocessing(psm, shaded, downstream_otsu);
    let document = extract_bytes_document_blocking(SCANNED_TABLE, "application/pdf", &config)
        .expect("forced OCR of the scanned table must succeed");
    document.tables.into_iter().next()
}

/// The page must come back with a table at all.
///
/// Before the header-less sparse-column fold in `pdf::table_reconstruct`, it did not -- at any
/// segmentation mode, with or without shaded-row normalisation. The grid was reconstructed
/// correctly (23 rows) and then discarded wholesale by the `column_sparsity` gate, because a
/// misread shaded row contributed three stray glyphs that minted a phantom header-less column
/// which was 19/22 empty. Three stray cells cost the entire table (xberg-io/xberg#1797).
#[test]
fn the_scanned_table_is_not_discarded_over_a_phantom_column() {
    for psm in [3, 11] {
        let table = first_table(psm, false)
            .unwrap_or_else(|| panic!("PSM {psm} produced no table at all; the sparsity gate discarded it"));
        assert!(
            table.cells.len() >= 20,
            "PSM {psm}: the 23-row grid must survive, got {} rows",
            table.cells.len()
        );
    }
}

/// An absolute count of ground-truth values recovered into the correct row and column, which is
/// the metric that decides whether a reconstruction change helped. Never a ratio: these changes
/// alter the grid's width, and a ratio over a narrower grid rises when values are lost.
///
/// The floor is deliberately well under the measured value so ordinary OCR jitter does not fail
/// the build; what it pins is the order of magnitude, against the 0 this fixture produced when
/// no table was built at all. Measured: 102 at PSM 11 and 98 at PSM 3, from 36 and 18 before the
/// split-header column was folded away (GH#1832). Every one of the 36 remaining misses is a row
/// whose *label* OCR mangled (`[TOTAL GOODS`, `EE NET CHANGE (DEFICIT)`), so the harness cannot
/// match the row at all -- no correctly-labelled row has a value in the wrong cell.
#[test]
fn enough_ground_truth_values_land_in_the_right_cell() {
    let table = first_table(11, false).expect("PSM 11 must produce a table");
    let (correct, misses) = correct_values_in_place(&table);
    assert!(
        correct >= 80,
        "only {correct} of {} ground-truth values landed in the right cell (floor 80);          first misses: {:?}",
        GROUND_TRUTH.len() * 6,
        misses.iter().take(8).collect::<Vec<_>>()
    );
}

/// GH#1832 specifically: each "Year N" header must be one column, not two.
///
/// This is the defect the value count above is dominated by rather than a separate symptom. OCR
/// splits the header across two x-tracks, each mints a column, and the right-hand one holds a
/// header fragment and no data -- so every value in the table sits one or more places left of the
/// column it belongs to while the OCR itself read it correctly.
#[test]
fn each_year_header_occupies_exactly_one_column() {
    for psm in [3, 11] {
        let table = first_table(psm, false).unwrap_or_else(|| panic!("PSM {psm} must produce a table"));
        let header = table.cells.first().expect("the table must have a header row");
        let years: Vec<&str> = header.iter().skip(1).take(6).map(String::as_str).collect();
        assert_eq!(
            years,
            ["Year 1", "Year 2", "Year 3", "Year 4", "Year 5", "Year 6"],
            "PSM {psm}: the six year headers must each occupy one column; whole header: {header:?}"
        );
    }
}

/// The edge of a shaded row reads as runs of underscores fused onto the values beside it, and
/// such a fused word's box used to close the gap between two columns and join two values into
/// one cell (GH#1833). At PSM 3 two value pairs were glued that way: each value must now fill a
/// cell of its own. OCR reads some commas as periods, so the check folds them.
#[test]
fn values_glued_by_the_shading_underscore_marks_fill_cells_of_their_own() {
    let table = first_table(3, false).expect("PSM 3 must produce a table");
    let cells: Vec<String> = table
        .cells
        .iter()
        .flatten()
        .map(|cell| cell.trim().replace('.', ","))
        .collect();
    for value in ["(2,100)", "(2,163)", "6,867"] {
        assert!(
            cells.iter().any(|cell| cell == value),
            "PSM 3: {value} must fill a cell of its own: {:?}",
            table.cells
        );
    }
    assert!(
        cells
            .iter()
            .any(|cell| cell.ends_with("7,073") && !cell.contains("6,867")),
        "PSM 3: 7,073 must sit in a cell apart from 6,867: {:?}",
        table.cells
    );
}

/// The edge of a shaded row also reads as a tall `=` or a thin dash of its own, at a low
/// confidence, in the gap between two values, and the cell merge then joined both values into one
/// cell (GH#1858). At PSM 3 one pair was glued that way: each value must now fill a cell of its own.
#[test]
fn values_glued_by_a_shading_mark_word_fill_cells_of_their_own() {
    let table = first_table(3, false).expect("PSM 3 must produce a table");
    let cells: Vec<String> = table
        .cells
        .iter()
        .flatten()
        .map(|cell| cell.trim().replace('.', ","))
        .collect();
    for value in ["22,636", "23,315"] {
        assert!(
            cells.iter().any(|cell| cell == value),
            "PSM 3: {value} must fill a cell of its own: {:?}",
            table.cells
        );
    }
}

/// No cell that holds a value keeps an underscore mark. The positive twin: a value the page
/// prints in every column still reads whole.
#[test]
fn no_value_cell_keeps_the_shading_underscore_marks() {
    for psm in [3, 11] {
        let table = first_table(psm, false).unwrap_or_else(|| panic!("PSM {psm} must produce a table"));
        let marked: Vec<&String> = table
            .cells
            .iter()
            .flatten()
            .filter(|cell| cell.contains('_') && cell.chars().any(|ch| ch.is_ascii_digit()))
            .collect();
        assert!(
            marked.is_empty(),
            "PSM {psm}: value cells keep underscore marks: {marked:?}"
        );
        assert!(
            table.cells.iter().flatten().any(|cell| cell.trim() == "3,250"),
            "PSM {psm}: a plainly printed value must still read whole"
        );
    }
}

/// Measurement harness for the rest of the cluster -- GH#1833 (values glue across the shading's
/// underscore marks) and GH#1834 (a label's tail becomes its own row); GH#1832's split header is
/// fixed and gated above. This prints the full grid and the per-cell misses at four
/// configurations so a change to the remaining two can be scored against ground truth.
/// Ignored because it is a report, not a gate.
#[test]
#[ignore = "measurement report for GH#1832/1833/1834; run with --ignored --nocapture"]
fn measure_shaded_table_reconstruction() {
    for psm in [3, 11] {
        for shaded in [false, true] {
            let Some(table) = first_table(psm, shaded) else {
                println!("=== PSM {psm} shaded={shaded}: NO TABLE ===");
                continue;
            };
            let (correct, misses) = correct_values_in_place(&table);
            println!(
                "=== PSM {psm} shaded={shaded}: {correct} of {} values in place, {} rows x {} cols ===",
                GROUND_TRUTH.len() * 6,
                table.cells.len(),
                table.cells.first().map_or(0, Vec::len)
            );
            for score in correct_values_by_kind(&table) {
                println!(
                    "  KIND {} psm={psm} shaded={shaded}: {} of {} (rows absent: {:?})",
                    score.kind, score.correct, score.of, score.rows_absent
                );
            }
            for row in &table.cells {
                println!("  {row:?}");
            }
            for miss in misses.iter().take(25) {
                println!("  MISS {miss}");
            }
        }
    }
}

/// Per-fill-kind absolute counts for GH#1837, in a form a shell harness can grep and compare:
/// one line per (psm, shaded, kind) carrying `labels <found>/<expected>` and
/// `values <correct>/<possible>`.
///
/// Labels are reported separately from values because the loss GH#1837 measures is a *label*
/// loss: when a shaded row's label no longer matches, `correct_values_by_kind` cannot find the
/// row at all and its six values score 0 for a reason that has nothing to do with the values.
/// A pooled total is deliberately not printed -- the published 98/98 and 103/106 pooled figures
/// are flat because the dark/mid loss is offset by a light/plain gain, which is exactly how this
/// defect stayed hidden.
///
/// Ignored because it is a report, not a gate. Run with `--ignored --nocapture`.
#[test]
#[ignore = "GH#1837 per-fill-kind measurement; run with --ignored --nocapture"]
fn measure_1837_labels_by_fill_kind() {
    for psm in [3, 11] {
        for shaded in [false, true] {
            let Some(table) = first_table(psm, shaded) else {
                println!("GH1837 psm={psm} shaded={shaded} NOTABLE");
                continue;
            };
            println!(
                "GH1837 psm={psm} shaded={shaded} GRID rows={} cols={}",
                table.cells.len(),
                table.cells.first().map_or(0, Vec::len)
            );
            for score in correct_values_by_kind(&table) {
                let expected_labels = score.of / 6;
                let found_labels = expected_labels - score.rows_absent.len();
                println!(
                    "GH1837 psm={psm} shaded={shaded} {} labels {found_labels}/{expected_labels} \
                     values {}/{} absent {:?}",
                    score.kind, score.correct, score.of, score.rows_absent
                );
            }
        }
    }
}

/// GH#1837 four-arm diagnosis: cross shaded-row normalization off/on with the downstream
/// whole-page Otsu pass off/on and report absolute correct values for every fill kind. Each arm
/// asserts that the table survived before scoring so a missing table cannot masquerade as zero
/// recognized values. This also tests GH#1897's claimed symptom on the same ground truth without
/// introducing the disproven region re-pass. ~keep
#[test]
#[ignore = "GH#1837 four-arm Otsu measurement; run with --ignored --nocapture"]
fn measure_1837_normalization_crossed_with_downstream_otsu() {
    for psm in [3, 11] {
        for shaded in [false, true] {
            for downstream_otsu in [false, true] {
                let table = first_table_with_preprocessing(psm, shaded, downstream_otsu)
                    .unwrap_or_else(|| panic!("PSM {psm} shaded={shaded} otsu={downstream_otsu} must produce a table"));
                assert!(
                    table.cells.len() >= 20,
                    "PSM {psm} shaded={shaded} otsu={downstream_otsu}: table has only {} rows",
                    table.cells.len()
                );
                for score in correct_values_by_kind(&table) {
                    println!(
                        "GH1837 psm={psm} shaded={shaded} otsu={downstream_otsu} {} values {}/{} absent {:?}",
                        score.kind, score.correct, score.of, score.rows_absent
                    );
                }
            }
        }
    }
}

/// GH#1837 diagnosis: print the raw TSV records for the shaded-row labels beside the rows that
/// survive table reconstruction. This distinguishes recognition artifacts such as a leading `[` or
/// a standalone `|` from later cell assignment. ~keep
#[test]
#[ignore = "GH#1837 raw recognition versus reconstructed grid; run with --ignored --nocapture"]
fn measure_1837_raw_tsv_against_reconstructed_rows() {
    let mut config = config_with_preprocessing(11, true, true);
    config
        .ocr
        .as_mut()
        .and_then(|ocr| ocr.tesseract_config.as_mut())
        .expect("the measurement config must carry an explicit Tesseract config")
        .output_format = "tsv".to_string();
    let document = extract_bytes_document_blocking(SCANNED_TABLE, "application/pdf", &config)
        .expect("forced OCR of the scanned table must succeed");

    for line in document.content.lines().filter(|line| {
        let fields: Vec<&str> = line.split('\t').collect();
        let near_page_bottom = fields
            .get(7)
            .and_then(|top| top.parse::<u32>().ok())
            .is_some_and(|top| top >= 1300);
        ["TOTAL", "CHANGE", "STOCK", "SUBTOTAL"]
            .iter()
            .any(|needle| line.contains(needle))
            || line.ends_with("\t|")
            || line.contains("\t[")
            || near_page_bottom
    }) {
        println!("RAW {line}");
    }
    let table = document
        .tables
        .first()
        .expect("PSM 11 normalized OCR must produce a table");
    assert!(table.cells.len() >= 20, "the scored table must survive reconstruction");
    for row in &table.cells {
        println!("GRID {row:?}");
    }
}

/// The absolute count of DARK-fill ground-truth values landing in the correct cell, at PSM 11,
/// with `normalize_shaded_rows` off vs on. Requires the table to survive with its full row count
/// first (mirrors `the_scanned_table_is_not_discarded_over_a_phantom_column`): a `0 of 18` from a
/// table discarded wholesale (the GH#1797 shape) renders identically to a genuine dark-row loss,
/// so a short table panics here instead of silently scoring 0.
fn dark_fill_correct_values(psm: i32, shaded: bool) -> usize {
    let table = first_table(psm, shaded).unwrap_or_else(|| panic!("PSM {psm} shaded={shaded} must produce a table"));
    assert!(
        table.cells.len() >= 20,
        "PSM {psm} shaded={shaded}: table has only {} rows, short of the 23-row grid -- \
         a discarded table would score 0 of 18 here for the wrong reason",
        table.cells.len()
    );
    correct_values_by_kind(&table)
        .into_iter()
        .find(|score| score.kind == "DARK")
        .expect("DARK is always present in correct_values_by_kind's fixed-size result")
        .correct
}

/// GH#1837: the claim that `normalize_shaded_rows` lowers dark-fill row values. Nothing in this
/// repo asserted a dark-row value before this test: `gh1785_shaded_row_normalization.rs` counts
/// only the three LIGHT-fill labels on a different fixture, and `shaded_rows.rs`'s own unit tests
/// assert pixel polarity on a synthetic page, not values read.
///
/// The original four-arm `invert_dark_bands` ablation was measured against the fixture's embedded
/// 196-dpi raster even though production normalizes at 300 dpi, so its causal conclusion was not
/// valid. The production-path four-arm report above instead crosses normalization with downstream
/// Xberg Otsu. After the fix, removing Otsu recovers no DARK or MID value: at PSM 3 DARK drops
/// from 18 to 17 and MID stays at 6, while at PSM 11 DARK stays at 18 and MID at 11; LIGHT also
/// drops from 18 to 17 at PSM 11. Downstream Otsu is therefore not the loss's cause on this
/// fixture. See xberg-io/xberg#1837. ~keep
///
/// FLOOR_OFF and FLOOR_ON are the measured PSM-11 values on this fixture. Update them only
/// alongside a re-run of the four-arm measurement, quoting the new absolute counts.
#[test]
fn dark_fill_rows_do_not_lose_more_values_than_measured_when_shaded_normalisation_is_enabled() {
    const PSM: i32 = 11;
    /// Measured 18 of 18 at PSM 11 with normalize_shaded_rows = false, 2026-09-27. ~keep
    const FLOOR_OFF: usize = 18;
    /// Measured 18 of 18 at PSM 11 with normalize_shaded_rows = true after GH#1837. ~keep
    const FLOOR_ON: usize = 18;

    let off = dark_fill_correct_values(PSM, false);
    let on = dark_fill_correct_values(PSM, true);

    assert!(
        off >= FLOOR_OFF,
        "dark-fill rows, option off: {off} of 18 (floor {FLOOR_OFF})"
    );
    assert_eq!(
        on, FLOOR_ON,
        "dark-fill rows, option on: {on} of 18 (expected the measured {FLOOR_ON})"
    );
}

/// GH#1837: normalized band edges that OCR reads as `[` / `|` must not hide rows whose
/// labels and values were otherwise recognized. The exact per-kind counts pin the recovery to
/// DARK and MID rows while proving the LIGHT and PLAIN gains remain intact. ~keep
#[test]
fn normalized_band_edge_glyphs_do_not_hide_recognized_shaded_rows() {
    let table = first_table(11, true).expect("PSM 11 normalized OCR must produce a table");
    assert!(table.cells.len() >= 20, "the scored table must survive reconstruction");

    let scores = correct_values_by_kind(&table);
    let actual: Vec<(&str, usize)> = scores.iter().map(|score| (score.kind, score.correct)).collect();
    assert_eq!(actual, [("DARK", 18), ("MID", 11), ("LIGHT", 18), ("PLAIN", 90)]);
}

/// The UNCONFIGURED path: OCR on, no `tesseract_config` at all.
///
/// Every other measurement in this file builds a `TesseractConfig`, which means none of them can
/// see a change whose whole point is what happens when the caller supplies none — the default
/// segmentation mode (GH#1786), the default preprocessing decision (GH#1894), the table-region
/// re-pass (GH#1897). A change gated on "the caller chose nothing" is invisible to a harness that
/// always chooses, and a suite that stays green tells you nothing about it either way.
///
/// Measured on this build: **103** of 138, identical at `3f77bd2e90` and with GH#1894 applied --
/// this fixture is not dark enough to fail the pixel test GH#1894 bypasses, so it does not
/// exercise that change. Recording the number anyway, because the next change to this path needs
/// a baseline that was actually run rather than assumed.
///
/// Run with `--ignored --nocapture`; it prints an absolute count, never a ratio, per the method
/// this file documents above. ~keep
#[test]
#[ignore = "measurement for the unconfigured OCR path; run with --ignored --nocapture"]
fn measure_unconfigured_ocr_path() {
    let config = ExtractionConfig {
        force_ocr: true,
        use_cache: false,
        ocr: Some(OcrConfig {
            backend: "tesseract".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let document = extract_bytes_document_blocking(SCANNED_TABLE, "application/pdf", &config)
        .expect("forced OCR of the scanned table must succeed");
    let Some(table) = document.tables.into_iter().next() else {
        println!("UNCONFIGURED: no table at all, 0 of {} values", GROUND_TRUTH.len() * 6);
        return;
    };
    let (correct, misses) = correct_values_in_place(&table);
    println!(
        "UNCONFIGURED: {correct} of {} values in place, {} rows x {} cols",
        GROUND_TRUTH.len() * 6,
        table.cells.len(),
        table.cells.first().map_or(0, Vec::len)
    );
    for miss in misses.iter().take(12) {
        println!("  MISS {miss}");
    }
}
