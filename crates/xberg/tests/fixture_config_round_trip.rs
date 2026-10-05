//! Assert every e2e fixture's `config` block actually takes effect.
//!
//! Unknown nested keys must be rejected instead of silently doing nothing. The
//! round-trip audit below provides a second line of defence for generated fixtures.
//!
//! The fixture audit parses each config into the real `ExtractionConfig`, serializes it
//! back, and asserts every requested leaf survives the round trip. A key that serde
//! dropped is a key that did nothing.
//!
//! It also catches the subtler variant that bit a fixture-authoring pass: using the Rust
//! field name where serde declares a different wire name. `ChunkingConfig::max_characters`
//! is `#[serde(rename = "max_chars", alias = "max_characters")]` and `overlap` is
//! `rename = "max_overlap"`, so a fixture written from the struct definition rather than
//! the wire contract can be wrong in a way that greps clean.
//!
//! Fixtures whose config names a feature this build does not enable are skipped with a
//! message rather than failed, so the test stays honest under any feature set.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)] // ~keep: test/bench binaries print by design; org logging policy exempts tests

use std::path::{Path, PathBuf};

use xberg::core::config::ExtractionConfig;

/// Repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/xberg has a grandparent")
        .to_path_buf()
}

/// Every `fixtures/**/*.json` path, sorted for deterministic reporting.
fn fixture_paths(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("fixtures")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "json") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Collect every leaf path in `value` as (dotted_path, leaf), skipping nulls.
///
/// Nulls are skipped because an explicitly-null optional is indistinguishable from an
/// absent one after a round trip through `skip_serializing_if = "Option::is_none"`.
fn leaves(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, serde_json::Value)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                leaves(child, &path, out);
            }
        }
        serde_json::Value::Null => {}
        leaf => out.push((prefix.to_string(), leaf.clone())),
    }
}

/// Look a dotted path up in a JSON object.
fn lookup<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn canonical_wire_path(path: &str) -> &str {
    match path {
        "chunking.max_characters" => "chunking.max_chars",
        "chunking.overlap" => "chunking.max_overlap",
        _ => path,
    }
}

/// Whether every entry the fixture requests survives in `got`, which may carry
/// more entries.
///
/// Nested config blocks with `#[serde(default)]` materialize omitted fields on
/// the way out (a staged `ocr.pipeline.stages[0].tesseract_config.psm` comes
/// back as the full tesseract config), so the audit's contract is that
/// *requested* keys survive, not that the wire form is verbatim. Arrays must
/// keep their length: silently growing or shrinking a staged list changes
/// behavior. Unknown-key typos stay a hard deserialization error
/// (`nested_config_typos_are_rejected`), independent of this check.
fn requested_survives(wanted: &serde_json::Value, got: &serde_json::Value) -> bool {
    match (wanted, got) {
        (serde_json::Value::Object(want_map), serde_json::Value::Object(got_map)) => {
            want_map.iter().all(|(key, child)| {
                got_map
                    .get(key)
                    .is_some_and(|present| requested_survives(child, present))
            })
        }
        (serde_json::Value::Array(want_items), serde_json::Value::Array(got_items)) => {
            want_items.len() == got_items.len()
                && want_items
                    .iter()
                    .zip(got_items)
                    .all(|(child, present)| requested_survives(child, present))
        }
        _ => wanted == got,
    }
}

#[test]
fn nested_config_typos_are_rejected() {
    let cases = [
        ("chunking", serde_json::json!({"chunking": {"maxChars": 500}})),
        (
            "security_limits",
            serde_json::json!({"security_limits": {"max_archive_bytes": 500}}),
        ),
        ("ocr", serde_json::json!({"ocr": {"backnd": "tesseract"}})),
        (
            "tesseract",
            serde_json::json!({"ocr": {"tesseract_config": {"psn": 6}}}),
        ),
        (
            "preprocessing",
            serde_json::json!({"ocr": {"tesseract_config": {"preprocessing": {"deskeww": true}}}}),
        ),
        (
            "pdf_options",
            serde_json::json!({"pdf_options": {"pdf_backend": "pdfium"}}),
        ),
    ];

    for (name, value) in cases {
        let error = serde_json::from_value::<ExtractionConfig>(value)
            .expect_err("a nested config typo must fail deserialization");
        assert!(
            error.to_string().contains("unknown field"),
            "{name} typo returned the wrong error: {error}"
        );
    }
}

#[test]
fn every_fixture_config_key_survives_a_round_trip() {
    let root = repo_root();
    let paths = fixture_paths(&root);
    assert!(
        !paths.is_empty(),
        "no fixtures found under {}/fixtures — has the corpus moved?",
        root.display()
    );

    let mut checked = 0usize;
    let mut skipped = Vec::new();
    let mut failures = Vec::new();

    for path in &paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(fixture) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(config_json) = fixture.get("config") else {
            continue;
        };
        if config_json.as_object().is_none_or(serde_json::Map::is_empty) {
            continue;
        }

        let parsed: ExtractionConfig = match serde_json::from_value(config_json.clone()) {
            Ok(config) => config,
            Err(error) => {
                // An unknown top-level key here means the feature is compiled out of this
                // build, not that the fixture is wrong — `ExtractionConfig` does deny
                // unknown fields, so a genuine typo is already a hard error elsewhere.
                skipped.push(format!("{}: {error}", path.display()));
                continue;
            }
        };

        let round_tripped = serde_json::to_value(&parsed).expect("ExtractionConfig serializes");

        let mut requested = Vec::new();
        leaves(config_json, "", &mut requested);

        for (leaf_path, wanted) in requested {
            match lookup(&round_tripped, canonical_wire_path(&leaf_path)) {
                Some(got) if requested_survives(&wanted, got) => {}
                Some(got) => failures.push(format!(
                    "{}: `{leaf_path}` round-tripped to {got} instead of {wanted}",
                    path.display()
                )),
                None => failures.push(format!(
                    "{}: `{leaf_path}` was DROPPED — the key does nothing. Check it against the \
                     serde wire name (e.g. ChunkingConfig uses `max_chars`, not `max_characters`).",
                    path.display()
                )),
            }
        }
        checked += 1;
    }

    for message in &skipped {
        eprintln!("SKIP (feature not enabled in this build): {message}");
    }

    assert!(
        failures.is_empty(),
        "{} fixture config key(s) do not survive a round trip through ExtractionConfig, so they \
         silently do nothing:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );

    assert!(
        checked > 0,
        "no fixture carried a non-empty config that this build could parse — the assertion above \
         proved nothing. Re-run with more features enabled."
    );
    eprintln!(
        "checked {checked} fixture config(s) across {} fixture file(s)",
        paths.len()
    );
}
