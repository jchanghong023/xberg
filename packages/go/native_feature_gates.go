package xberg

// This file is hand-written (not alef-generated) and exists to close a gap left by
// commits 99dddd895b ("fix(ffi): emit the C header's feature guards") and 58b06e0334
// ("fix(go): sync the vendored C header's feature guards"): those commits correctly
// fixed a cbindgen.toml escaping bug that had silently suppressed every
// `#if defined(XBERG_FEATURE_*)` guard in xberg.h, but nothing was added on the
// consumer side to define the matching preprocessor macros. Once the guards started
// emitting for real, cgo stopped seeing ~60 declarations that this package calls,
// because none of `XBERG_FEATURE_*` was ever passed via CGO_CFLAGS.
//
// The prebuilt native library this module links against (see native_setup.go /
// cmd/setup) is always built from crates/xberg-ffi with its Cargo.toml `default`
// feature set — every symbol gated by the macros below is therefore always present
// in the linked library. Defining them here makes the declarations visible to cgo
// again, matching what is actually shipped.
//
// Defining these macros is necessary but NOT sufficient to make this package compile.
// Measured with go1.26.6 at the commit that added this file:
//
//   without this file: 63 errors, all "could not determine what C.<symbol> refers to"
//   with this file:     0 of those, and 46 errors of a second, previously MASKED kind:
//                       "invalid operation: cX == nil (mismatched types
//                        _Ctype_XBERGAlefHandle and untyped nil)"
//
// That second break is an independent alef Go-codegen bug: XBERGAlefHandle is
// `typedef uint64_t` (xberg.h:2653), an opaque integer handle, but the generator emits
// pointer-style `== nil` / `!= nil` null checks against it; the correct check is `== 0`.
// It was invisible until now because cgo failed at name resolution and never reached
// type-checking. With both breaks fixed the package has no remaining error class
// (verified with `go build -gcflags=-e ./...`, which lifts the 10-error cap).
//
// This is a stopgap: the correct long-term fix is for the alef generator to emit
// these defines into the generated `#cgo CFLAGS` line in binding.go/trait_bridges.go
// (mirroring crates/xberg-ffi/Cargo.toml's `default` feature list through
// cbindgen.toml's `[defines]` table), the same way it already threads a `features`
// list for other bindings (e.g. `[crates.java].features` in alef.toml). Once alef
// does that, this file should be deleted.
//
// Keep this macro list in sync with the `#if defined(XBERG_FEATURE_*)` guards that
// actually appear in packages/go/include/xberg.h — not with the full feature list in
// crates/xberg-ffi/Cargo.toml, since not every Cargo feature maps to a cbindgen
// `[defines]` entry that actually gates an emitted declaration.

// #cgo CFLAGS: -DXBERG_FEATURE_API -DXBERG_FEATURE_API_TYPES -DXBERG_FEATURE_AUTO_ROTATE_TYPES -DXBERG_FEATURE_CLASSIFICATION -DXBERG_FEATURE_DIFF -DXBERG_FEATURE_HEURISTICS -DXBERG_FEATURE_HTML -DXBERG_FEATURE_KEYWORDS_RAKE -DXBERG_FEATURE_KEYWORDS_YAKE -DXBERG_FEATURE_LATE_INTERACTION -DXBERG_FEATURE_LATE_INTERACTION_PRESETS -DXBERG_FEATURE_LAYOUT_TYPES -DXBERG_FEATURE_MARKDOWN_FOOTNOTES -DXBERG_FEATURE_OFFICE -DXBERG_FEATURE_PDF -DXBERG_FEATURE_PRESETS -DXBERG_FEATURE_QUALITY -DXBERG_FEATURE_REDACTION -DXBERG_FEATURE_RERANKER -DXBERG_FEATURE_RERANKER_PRESETS -DXBERG_FEATURE_SPARSE_EMBEDDING_PRESETS -DXBERG_FEATURE_SPARSE_EMBEDDINGS -DXBERG_FEATURE_SVG -DXBERG_FEATURE_TOKIO_RUNTIME -DXBERG_FEATURE_TRANSCRIPTION -DXBERG_FEATURE_TRANSCRIPTION_TYPES -DXBERG_FEATURE_TREE_SITTER -DXBERG_FEATURE_URL_INGESTION
import "C"
