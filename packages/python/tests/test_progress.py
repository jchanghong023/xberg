import inspect

import xberg
from xberg.progress import ProgressEvent, _ProgressListener


def test_top_level_extract_functions_accept_optional_progress_callback() -> None:
    assert inspect.signature(xberg.extract).parameters["on_progress"].default is None
    assert inspect.signature(xberg.extract_batch).parameters["on_progress"].default is None


def test_progress_listener_builds_typed_ocr_page_event() -> None:
    events: list[ProgressEvent] = []
    listener = _ProgressListener(events.append)

    listener.on_progress("ocr_page", 7, 12, 2, "tesseract", 3)

    assert events == [ProgressEvent("ocr_page", 7, 12, 2, "tesseract", 3)]


def test_progress_listener_ignores_incomplete_non_page_event() -> None:
    events: list[ProgressEvent] = []
    listener = _ProgressListener(events.append)

    listener.on_progress("extract_complete", None, None, None, None, None)

    assert events == []
