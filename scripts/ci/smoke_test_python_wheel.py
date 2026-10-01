#!/usr/bin/env python3
"""Prove an installed xberg wheel actually extracts documents, not just imports.

An import-only check (`python -c "import xberg"`) is not a sufficient smoke
test for this wheel: `xberg/api.py` eagerly imports the PyO3 extension module
at `import xberg` time, so a wheel with a completely missing or ABI-broken
native extension already fails on import. But native shared libraries linked
dynamically into the extension (ONNX Runtime, libheif, ...) can still resolve
fine at dlopen-time while being subtly wrong for the run: wrong musl/glibc
libc family, a shared lib silently dropped by the closure-vendoring step
(scripts/ci/vendor-native-closure.sh), or a panic in the extraction pipeline
itself. None of those show up until real extraction code runs. This script
therefore runs an actual extraction against a known fixture and asserts the
exact expected text comes back.

Usage: smoke_test_python_wheel.py <fixture-path> <expected-substring>
Exit code is non-zero, with a message on stderr, on any failure: extension
import failure, extraction raising, an empty result, or a content mismatch.
"""

from __future__ import annotations

import asyncio
import sys


def fail(message: str) -> None:
    print(f"SMOKE TEST FAILED: {message}", file=sys.stderr)
    sys.exit(1)


async def run(fixture_path: str, expected_substring: str) -> None:
    try:
        from xberg import (
            ExtractInput,
            ExtractionConfig,
            OcrConfig,
            OcrPipelineConfig,
            OcrPipelineStage,
            OcrQualityThresholds,
            PaddleOcrConfig,
            TesseractConfig,
            extract,
        )
    except Exception as exc:  # report and fail loud, not silently
        fail(f"could not import the xberg native extension: {exc!r}")
        return

    try:
        pipeline = OcrPipelineConfig(
            stages=[
                OcrPipelineStage(
                    backend="tesseract",
                    tesseract_config=TesseractConfig(psm=6, enable_table_detection=False),
                ),
                OcrPipelineStage(
                    backend="paddleocr",
                    priority=90,
                    paddle_ocr_settings=PaddleOcrConfig(
                        use_angle_cls=True,
                        enable_table_detection=True,
                        model_tier="server",
                    ),
                ),
            ],
            quality_thresholds=OcrQualityThresholds(),
        )
        config = ExtractionConfig(ocr=OcrConfig(enabled=False, pipeline=pipeline))
        output = await extract(ExtractInput(kind="uri", uri=fixture_path), config)
    except Exception as exc:  # report and fail loud, not silently
        fail(f"extract() raised for fixture {fixture_path!r}: {exc!r}")
        return

    if not output.results:
        fail(f"extract() returned zero results for fixture {fixture_path!r}")
        return

    content = output.results[0].content
    if expected_substring not in content:
        fail(
            f"expected substring {expected_substring!r} not found in extracted "
            f"content for fixture {fixture_path!r}; got {content!r}"
        )
        return

    print(f"SMOKE TEST PASSED: found {expected_substring!r} in extracted content")


def main() -> None:
    if len(sys.argv) != 3:
        fail(f"usage: {sys.argv[0]} <fixture-path> <expected-substring>")
        return
    asyncio.run(run(sys.argv[1], sys.argv[2]))


if __name__ == "__main__":
    main()
