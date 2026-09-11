//! Regression test for GitHub #1387, generalized after the same defect recurred.
//!
//! On 1.0.9-1.0.14, `crates/xberg-ffi/Cargo.toml` dropped the baked `"full"` feature from
//! the `xberg` dependency (commit cf7fa0533d, "stop forcing heic/candle into no-heic
//! consumers") and replaced it with an explicit, hand-maintained feature list for the
//! `not(android/ios/windows/macos-x86_64)` target — the block that builds the native
//! libraries shipped for linux-x64, linux-aarch64, and macos-arm64 (osx-arm64) across every
//! `xberg-ffi` consumer (C, C#, Go, Java). That list omitted `"excel"`, so the sole extractor
//! for xlsx/xlsm/xlsb/xltx/ods (`crates/xberg/src/extractors/excel.rs`, gated by
//! `#[cfg(feature = "excel")]`, see `crates/xberg/Cargo.toml:55`) was compiled out of those
//! native libraries while the static format catalogue in `crates/xberg/src/core/mime.rs`
//! kept advertising xlsx/xlsm/xlsb/xltx/ods unconditionally — producing
//! `UnsupportedFormatException` for a format `ListSupportedFormats()` still reports as
//! supported.
//!
//! The original fix for that incident hardcoded a single check for `"excel"`, which made the
//! guard structurally blind to any other omission from the same hand-maintained list. The
//! defect recurred: eleven more features present in core's `full` feature (`analysis`,
//! `candle-vlm-ocr`, `formats`, `heic`, `redaction-ml`, `redaction-rehydrate`, `services`,
//! `static-embeddings`, `summarization`, `summarization-llm`, `translation`) were missing from
//! the same desktop-target list, silently dropping capabilities such as summarization from
//! every C-FFI-based binding (Java, Go, C#, Swift, Zig, C). This file now asserts the general
//! invariant instead of one hardcoded feature name: every feature named in core's `full`
//! feature must also be requested by the desktop-target `xberg` dependency, unless it is
//! written down in `DELIBERATE_FULL_FEATURE_EXCLUSIONS` with a reason. The original
//! excel-specific assertions are kept below as-is; they document the original incident.
//!
//! `crates/xberg/Cargo.toml` already carries a near-identical historical fix for
//! `windows-target` (see the comment above its own `"excel"` entry, referencing "the 1.0.4
//! Windows XLSX bug") — this test locks in the analogous fix for the desktop/server target
//! and prevents a future edit of the hand-maintained feature list from silently dropping a
//! `full` feature again.
//!
//! Both manifests are parsed with the real `toml` crate (already a non-optional dependency of
//! this crate, see `crates/xberg/Cargo.toml`) rather than hand-rolled string scraping. This
//! matters for two independent reasons found while writing this test:
//!
//! - An earlier version of the excel-only test assumed the `default = [...]` feature array was
//!   always formatted one entry per line, and silently produced a single bogus entry (the whole
//!   array, as one string with embedded quotes) once `alef`'s regen reformatted it onto a single
//!   line — turning the safeguard into a false positive that no longer proved anything.
//! - Core's `full = [...]` array has `#` comments between some of its entries, and at least one
//!   of those comments itself contains a quoted string that looks like a feature name (the
//!   `candle-vlm-ocr` entry's comment discusses the `ocr.backend = "candle-glm-ocr"` config
//!   value). A naive regex over the raw array text would pick that quoted word up as if it were
//!   a feature, corrupting the extracted set. A real TOML parse ignores comments entirely and
//!   returns exactly the feature strings.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test binaries print by design

use std::fs;
use std::path::Path;

use toml::Table;

/// Locates the `crates/xberg-ffi/Cargo.toml` manifest relative to this crate.
fn ffi_manifest_source() -> String {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ffi_manifest = manifest_dir
        .parent()
        .expect("crates/xberg has a parent crates/ directory")
        .join("xberg-ffi")
        .join("Cargo.toml");
    fs::read_to_string(&ffi_manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", ffi_manifest.display()))
}

/// Locates this crate's own `crates/xberg/Cargo.toml` manifest, the source of truth for the
/// `full` feature.
fn core_manifest_source() -> String {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let core_manifest = manifest_dir.join("Cargo.toml");
    fs::read_to_string(&core_manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", core_manifest.display()))
}

/// Parses `source` as a TOML document, panicking with the parse error on malformed input.
fn parse_manifest(source: &str) -> Table {
    source
        .parse::<Table>()
        .unwrap_or_else(|error| panic!("failed to parse manifest as TOML: {error}"))
}

/// Reads the string array at `path` (a sequence of table keys) from `manifest`, panicking
/// with a precise description of what was expected if any segment is missing or not the
/// expected shape.
fn string_array_at(manifest: &Table, path: &[&str]) -> Vec<String> {
    let mut current = manifest
        .get(path[0])
        .unwrap_or_else(|| panic!("expected top-level key `{}` in manifest", path[0]));
    for (depth, key) in path.iter().enumerate().skip(1) {
        current = current.get(key).unwrap_or_else(|| {
            let traversed = path[..depth].join(".");
            panic!("expected key `{key}` under `{traversed}` in manifest")
        });
    }
    current
        .as_array()
        .unwrap_or_else(|| panic!("expected `{}` to be a TOML array, got {current:?}", path.join(".")))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("expected `{}` entries to be strings, got {entry:?}", path.join(".")))
                .to_owned()
        })
        .collect()
}

/// The exact `cfg(...)` target key (without surrounding brackets/quotes) used for the
/// default native build target: linux-x64, linux-aarch64, and macos-arm64.
const DEFAULT_NATIVE_TARGET_CFG: &str = r#"cfg(not(any(target_os = "android", target_os = "ios", target_os = "windows", all(target_os = "macos", target_arch = "x86_64"))))"#;

/// Deliberate, written-down exclusions from the "every `full` feature must be requested by the
/// desktop-target `xberg` dependency" invariant checked below. Each entry is
/// `(feature, reason)`. Add an entry here only when a `full` feature is intentionally left out
/// of the desktop build — anything left out without an entry here fails
/// `ffi_default_target_dependency_requests_every_full_feature_or_has_a_written_exclusion`.
const DELIBERATE_FULL_FEATURE_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "heic",
        "libheif-sys's build.rs cannot cross-compile, which would break Swift's cross-built Linux \
         targets; `full-no-heic` is declared in alef.toml as the opt-in instead",
    ),
    (
        "formats",
        "aggregate that expands to include `heic`; its other members are already listed \
         individually, so enabling it would only add the excluded libheif dependency",
    ),
    (
        "candle-vlm-ocr",
        "native FFI artifacts deliberately ship without Candle model backends to stay lean and \
         portable; alef.toml declares `candle-ocr` as a declare-only opt-in for the same reason",
    ),
    (
        "services",
        "expands to include `otel`; installing a subscriber or OTLP exporter is a CLI/service \
         concern, and this crate is a library that must only emit spans",
    ),
];

/// Reads the `full` feature's member array from `crates/xberg/Cargo.toml`.
fn full_feature_members() -> Vec<String> {
    let source = core_manifest_source();
    let manifest = parse_manifest(&source);
    string_array_at(&manifest, &["features", "full"])
}

/// Reads the desktop-target (linux-x64/linux-arm64/macos-arm64) `xberg` dependency's
/// `features` array from `crates/xberg-ffi/Cargo.toml`.
fn ffi_default_target_features() -> Vec<String> {
    let source = ffi_manifest_source();
    let manifest = parse_manifest(&source);
    string_array_at(
        &manifest,
        &["target", DEFAULT_NATIVE_TARGET_CFG, "dependencies", "xberg", "features"],
    )
}

/// GH#1387 recurred because the original fix only ever checked for `"excel"`: eleven more
/// `full` features (`analysis`, `candle-vlm-ocr`, `formats`, `heic`, `redaction-ml`,
/// `redaction-rehydrate`, `services`, `static-embeddings`, `summarization`,
/// `summarization-llm`, `translation`) were separately missing from the same hand-maintained
/// desktop-target list, silently dropping capabilities such as summarization from every
/// C-FFI-based binding. Every feature in core's `full` must now be requested by the default
/// (linux-x64/linux-arm64/macos-arm64) `xberg` dependency, unless it is written down in
/// `DELIBERATE_FULL_FEATURE_EXCLUSIONS` with a reason.
#[test]
fn ffi_default_target_dependency_requests_every_full_feature_or_has_a_written_exclusion() {
    let full_features = full_feature_members();
    let ffi_features = ffi_default_target_features();
    let excluded: Vec<&str> = DELIBERATE_FULL_FEATURE_EXCLUSIONS
        .iter()
        .map(|&(feature, _)| feature)
        .collect();

    let missing: Vec<String> = full_features
        .into_iter()
        .filter(|feature| !ffi_features.contains(feature) && !excluded.contains(&feature.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "the default (linux-x64/linux-arm64/macos-arm64) xberg-ffi target dependency is missing \
         these features present in core's `full` feature: {missing:?}. Either add them to the \
         desktop target's `features` list in crates/xberg-ffi/Cargo.toml (this is the GH#1387 \
         defect recurring), or add a justified `(feature, reason)` entry to \
         DELIBERATE_FULL_FEATURE_EXCLUSIONS in this file."
    );
}

/// A written-down exclusion that is stale — the feature it claims is excluded is actually
/// present in the desktop-target list — is misleading documentation, not a safeguard.
#[test]
fn deliberate_full_feature_exclusions_are_not_stale() {
    let ffi_features = ffi_default_target_features();

    for &(feature, reason) in DELIBERATE_FULL_FEATURE_EXCLUSIONS {
        assert!(
            !reason.is_empty(),
            "DELIBERATE_FULL_FEATURE_EXCLUSIONS entry for `{feature}` must carry a non-empty reason"
        );
        assert!(
            !ffi_features.iter().any(|enabled| enabled == feature),
            "DELIBERATE_FULL_FEATURE_EXCLUSIONS claims `{feature}` is excluded from the desktop \
             target's features list, but it is already present there; remove the stale exclusion"
        );
    }
}

/// The default target — used for linux-x64/linux-arm64/macos-arm64 native builds (Go, C,
/// Java, and C# runtime packs) — must carry `"excel"`, or xlsx/xlsm/xlsb/xltx/ods extraction
/// silently regresses to `UnsupportedFormat` on those platforms while `list_supported_formats`
/// keeps advertising the formats (GH#1387).
#[test]
fn ffi_default_target_dependency_must_enable_excel_feature() {
    let features = ffi_default_target_features();

    assert!(
        features.iter().any(|feature| feature == "excel"),
        "the default (linux-x64/linux-arm64/macos-arm64) xberg-ffi target dependency must \
         request the \"excel\" feature so xlsx/xlsm/xlsb/xltx/ods extraction is compiled into \
         the shipped native library (GH#1387); got features = {features:?}"
    );
}

/// `xberg-ffi` must expose its own `excel` feature (mapping to `xberg/excel`) so binding
/// authors and CI can request it explicitly, and must enable it in its `default` feature set
/// so a plain `cargo build --release -p xberg-ffi` restores spreadsheet extraction.
#[test]
fn ffi_crate_declares_and_defaults_to_excel_feature() {
    let source = ffi_manifest_source();
    let manifest = parse_manifest(&source);

    let excel_feature = string_array_at(&manifest, &["features", "excel"]);
    assert_eq!(
        excel_feature,
        vec!["xberg/excel".to_owned()],
        "xberg-ffi must declare `excel = [\"xberg/excel\"]` in its [features] table; got {excel_feature:?}"
    );

    let default_features = string_array_at(&manifest, &["features", "default"]);
    assert!(
        default_features.iter().any(|feature| feature == "excel"),
        "xberg-ffi's default feature set must include \"excel\"; got {default_features:?}"
    );
}
