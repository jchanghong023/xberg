"""Opt-in Windows E2E: one real worker, independent models, overlapping requests.

Requires an already-built fork CLI, real screenshot/document OCR assets, a PNG
with known text, and a sufficiently long real OCR document. Does not build or
download anything. Execution requires the user's explicit test authorization.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time
import tempfile
import zipfile


ANSI = re.compile(r"\x1b\[[0-9;]*m")
REQUEST_EVENT = re.compile(
    r"worker request (started|finished).*?pid=(\d+).*?channel=\"?(document|snapshot)\"?.*?id=(\d+)"
)


class Worker:
    def __init__(self, cli, config, timeout):
        env = os.environ.copy()
        env["RUST_LOG"] = "info"
        env["NO_COLOR"] = "1"
        bundled_ort = cli.parent / "onnxruntime.dll"
        if bundled_ort.is_file():
            env["ORT_DYLIB_PATH"] = str(bundled_ort)
        if (cli.parent / "models").is_dir():
            env["HF_HUB_CACHE"] = str(cli.parent / "models")
        env["PATH"] = str(cli.parent) + os.pathsep + env.get("PATH", "")
        self.process = subprocess.Popen(
            [str(cli), "--log-level", "info", "worker", "--no-config-discovery",
             "--config-json", json.dumps(config, ensure_ascii=False)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            encoding="utf-8", errors="replace", env=env,
        )
        self.timeout = timeout
        self.condition = threading.Condition()
        self.responses = {}
        self.events = []
        self.loads = {"document": 0, "snapshot": 0, "media": 0}
        self.faults = []
        self.readers = [
            threading.Thread(target=self.read_stdout, daemon=True),
            threading.Thread(target=self.read_stderr, daemon=True),
        ]
        for reader in self.readers:
            reader.start()

    def read_stdout(self):
        for line in self.process.stdout:
            with self.condition:
                try:
                    response = json.loads(line)
                    request_id = response["id"]
                    if request_id in self.responses:
                        raise ValueError("duplicate response id")
                    self.responses[request_id] = response
                except (ValueError, KeyError, TypeError):
                    self.faults.append("stdout must contain one complete, unique JSON response per request")
                self.condition.notify_all()

    def read_stderr(self):
        for line in self.process.stderr:
            line = ANSI.sub("", line)
            with self.condition:
                event = REQUEST_EVENT.search(line)
                if event:
                    action, pid, channel, request_id = event.groups()
                    self.events.append({"action": action, "pid": int(pid), "channel": channel,
                                        "id": int(request_id)})
                if "worker snapshot models loaded" in line:
                    self.loads["snapshot"] += 1
                if "PaddleOCR engine initialized successfully" in line:
                    self.loads["document"] += 1
                if "SenseVoice and Silero VAD sessions initialized successfully" in line:
                    self.loads["media"] += 1
                self.condition.notify_all()

    def wait(self, predicate, description):
        deadline = time.monotonic() + self.timeout
        with self.condition:
            while not predicate():
                if self.faults:
                    raise AssertionError(self.faults[0])
                if self.process.poll() is not None:
                    raise AssertionError(f"worker exited before {description}: {self.process.returncode}")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimeoutError(description)
                self.condition.wait(min(left, 0.2))

    def send(self, request):
        self.process.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def response(self, request_id):
        self.wait(lambda: request_id in self.responses, f"response {request_id}")
        return self.responses[request_id]

    def has_event(self, request_id, action):
        return any(e["id"] == request_id and e["action"] == action for e in self.events)

    def close(self):
        self.process.stdin.close()
        code = self.process.wait(timeout=self.timeout)
        for reader in self.readers:
            reader.join(timeout=5)
        if code:
            raise AssertionError(f"worker EOF exit code: {code}")
        if self.faults:
            raise AssertionError(self.faults[0])

    def kill(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=10)


def require_text(response, field, expected):
    assert response.get("ok") is True, f"request {response['id']} failed: {response.get('error_kind', 'conversion error')}"
    value = response["document"]["content"] if field == "document" else response[field]
    assert expected in value, f"request {response['id']} omitted required {field} text"


def mode_fixture(path, png):
    """Native text plus one real embedded screenshot: detects fast-mode leakage."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as doc:
        doc.writestr("[Content_Types].xml", '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        doc.writestr("_rels/.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        doc.writestr("word/_rels/document.xml.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/screen.png"/></Relationships>')
        doc.writestr("word/document.xml", '''<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body><w:p><w:r><w:t>WORKER_NATIVE_TEXT_2987</w:t></w:r></w:p><w:p><w:r><w:drawing><wp:inline><wp:extent cx="6000000" cy="3400000"/><wp:docPr id="1" name="screenshot"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="1" name="screen.png"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="6000000" cy="3400000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p></w:body></w:document>''')
        doc.writestr("word/media/screen.png", png)


def run(args, report):
    config = json.loads(args.config.read_text(encoding="utf-8-sig"))
    assert "snapshot_ocr" in config, "config must explicitly specify the independent snapshot_ocr block"
    assert config.get("ocr", {}).get("backend", "paddle-ocr") == "paddle-ocr", "document model reuse probe requires PaddleOCR"
    assert not config.get("disable_ocr", False), "document OCR must be enabled"
    assert config.get("transcription", {}).get("enabled"), "transcription must be enabled"
    # Keep model caching, but prevent a cached document result from bypassing inference.
    config["use_cache"] = False
    encoded = base64.b64encode(args.snapshot.read_bytes()).decode("ascii")
    report["config_sha256"] = hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()
    report["fixture_sha256"] = {
        "document": hashlib.sha256(args.document.read_bytes()).hexdigest(),
        "snapshot": hashlib.sha256(args.snapshot.read_bytes()).hexdigest(),
        "media": hashlib.sha256(args.media.read_bytes()).hexdigest(),
    }
    report["cli_sha256"] = hashlib.sha256(args.cli.read_bytes()).hexdigest()
    scratch = Path(__file__).resolve().parents[2] / ".tmp"
    scratch.mkdir(exist_ok=True)
    temporary = tempfile.TemporaryDirectory(prefix="worker-e2e-", dir=scratch)
    mixed = Path(temporary.name) / "mode.docx"
    mode_fixture(mixed, args.snapshot.read_bytes())
    worker = Worker(args.cli.resolve(strict=True), config, args.timeout)
    report["pid"] = worker.process.pid
    try:
        # Warm both channels once; actual expected text is mandatory, not just exit 0.
        worker.send({"id": 1, "command": "extract", "path": str(args.document.resolve())})
        require_text(worker.response(1), "document", args.document_text)
        worker.send({"id": 2, "command": "ocr_snapshot", "image_base64": encoded})
        require_text(worker.response(2), "text", args.snapshot_text)
        worker.send({"id": 10, "command": "transcribe", "path": str(args.media.resolve())})
        require_text(worker.response(10), "markdown", args.media_text)
        worker.send({"id": 11, "command": "extract", "path": str(mixed)})
        require_text(worker.response(11), "document", args.snapshot_text)
        worker.wait(lambda: worker.loads["document"] > 0 and worker.loads["snapshot"] == 1,
                    "both independent model sets loaded")
        worker.wait(lambda: worker.loads["media"] == 1, "resident media models loaded")
        initial_loads = worker.loads.copy()

        for request_id, command in [(12, "capabilities"), (13, "formats"), (14, "model_state")]:
            worker.send({"id": request_id, "command": command})
        assert worker.response(12)["protocol_version"] == 2
        assert worker.response(12)["pid"] == worker.process.pid
        assert {"pdf", "docx", "png"} <= {row["extension"] for row in worker.response(13)["formats"]}
        initial_models = worker.response(14)["models"]
        assert initial_models["snapshot"]["state"] == "ready"
        assert initial_models["document"]["models"]["resident_sessions"]
        assert initial_models["transcription"]["state"] == "ready"

        worker.send({"id": 3, "command": "extract", "path": str(args.document.resolve())})
        worker.wait(lambda: worker.has_event(3, "started"), "real document request started")
        worker.send({"id": 4, "command": "ocr_snapshot", "image_base64": encoded})
        worker.send({"id": 5, "command": "snapshot_state"})
        require_text(worker.response(4), "text", args.snapshot_text)
        assert worker.response(5).get("state") == "ready", "resident screenshot model must remain ready"
        require_text(worker.response(3), "document", args.document_text)
        worker.wait(lambda: worker.has_event(3, "finished") and worker.has_event(4, "finished"),
                    "overlap evidence on stderr")
        positions = {(e["id"], e["action"]): i for i, e in enumerate(worker.events)}
        assert positions[(3, "started")] < positions[(4, "started")] < positions[(4, "finished")] < positions[(3, "finished")], (
            "no complete screenshot inference overlapped document conversion; use a longer real OCR document"
        )

        # Keep the SAME stdin and process alive for another batch and both failure paths.
        worker.send({"id": 6, "command": "ocr_snapshot", "image_base64": "not base64"})
        assert worker.response(6).get("error_kind") == "input_invalid"
        missing = args.document.parent / (args.document.name + ".worker-e2e-missing")
        assert not missing.exists(), "missing-file test path unexpectedly exists"
        worker.send({"id": 7, "command": "extract", "path": str(missing)})
        assert worker.response(7).get("ok") is False
        worker.send({"id": 8, "command": "extract", "path": str(args.document.resolve())})
        require_text(worker.response(8), "document", args.document_text)
        worker.send({"id": 9, "command": "ocr_snapshot", "image_base64": encoded})
        require_text(worker.response(9), "text", args.snapshot_text)

        # Fast changes this document only; the following normal request restores OCR.
        worker.send({"id": 15, "command": "extract", "path": str(mixed), "mode": "fast"})
        require_text(worker.response(15), "document", "WORKER_NATIVE_TEXT_2987")
        assert args.snapshot_text not in worker.response(15)["document"]["content"]
        assert worker.response(15)["document"].get("images"), "fast must retain extracted pictures"
        worker.send({"id": 16, "command": "ocr_snapshot", "image_base64": encoded})
        require_text(worker.response(16), "text", args.snapshot_text)
        worker.send({"id": 17, "command": "extract", "path": str(mixed), "mode": "normal"})
        require_text(worker.response(17), "document", args.snapshot_text)

        # Stop each real inference route, then reuse the SAME resident process.
        for task, cancel_id, command, payload in [
            (18, 19, "extract", {"path": str(args.document.resolve())}),
            (20, 21, "ocr_snapshot", {"image_base64": encoded}),
            (22, 23, "transcribe", {"path": str(args.media.resolve())}),
        ]:
            worker.send({"id": task, "command": command, **payload})
            worker.wait(lambda: worker.has_event(task, "started"), f"cancellable {command} started")
            worker.send({"id": cancel_id, "command": "cancel", "target_id": task})
            assert worker.response(cancel_id)["accepted"] is True, "fixture ended before cancellation"
            assert worker.response(task).get("error_kind") == "cancelled"
        for task, command, payload in [
            (24, "extract", {"path": str(args.document.resolve())}),
            (25, "ocr_snapshot", {"image_base64": encoded}),
            (26, "transcribe", {"path": str(args.media.resolve())}),
        ]:
            worker.send({"id": task, "command": command, "timeout_ms": 1, **payload})
            assert worker.response(task).get("error_kind") == "timeout"
        worker.send({"id": 27, "command": "extract", "path": str(mixed)})
        require_text(worker.response(27), "document", args.snapshot_text)
        worker.send({"id": 28, "command": "ocr_snapshot", "image_base64": encoded})
        require_text(worker.response(28), "text", args.snapshot_text)
        worker.send({"id": 29, "command": "transcribe", "path": str(args.media.resolve())})
        require_text(worker.response(29), "markdown", args.media_text)
        assert worker.response(29)["segments"] == worker.response(10)["segments"], "VAD reset must isolate streams"
        worker.send({"id": 30, "command": "model_state"})
        assert worker.response(30)["models"] == initial_models, "resident model identity changed"
        worker.close()
        assert worker.loads == initial_loads, "later batches reloaded a resident model"
        assert set(worker.responses) == set(range(1, 31)), "missing or unsolicited response"
        assert {e["pid"] for e in worker.events} == {worker.process.pid}, "both channels must run in the same worker PID"
        report.update(status="PASS", model_loads=worker.loads, events=worker.events,
                      checks=["real document and screenshot text", "same PID inference overlap",
                              "cross-batch model reuse", "independent failure paths", "JSON framing", "EOF exit",
                              "fast/normal isolation", "in-process queries", "cancel and timeout all three routes",
                              "post-cancel reuse", "media/VAD stream isolation"])
    finally:
        worker.kill()
        temporary.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cli", "config", "document", "snapshot", "media"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--document-text", required=True, help="required OCR text in the document result")
    parser.add_argument("--snapshot-text", required=True, help="required text in the screenshot result")
    parser.add_argument("--media-text", required=True, help="required recognized speech in the media result")
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--out", type=Path, default=Path(".tmp/worker-concurrency/report.json"))
    args = parser.parse_args()
    report = {"status": "FAIL"}
    try:
        if os.name != "nt":
            raise RuntimeError("this fork's E2E is Windows-only")
        if not args.document_text.strip() or not args.snapshot_text.strip() or not args.media_text.strip():
            raise ValueError("expected document and screenshot text must be nonempty")
        if args.timeout <= 0:
            raise ValueError("timeout must be positive")
        run(args, report)
    except Exception as error:
        report["error"] = str(error)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"worker concurrency E2E: {report['status']} — {args.out}")
    if report["status"] != "PASS":
        print(report.get("error", "verification failed"))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
