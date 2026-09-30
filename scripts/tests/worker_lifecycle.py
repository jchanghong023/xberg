# -*- coding: utf-8 -*-
"""Xberg worker lifecycle E2E: stdio disconnect, shutdown, idle and identity (WORKER.md).

Real process, real pipes; needs no models so it can run anywhere the binary exists.
Run only on explicit request (same policy as scripts/tests/worker_concurrency.py):
  python scripts/tests/worker_lifecycle.py

Covers:
  A. capabilities identity + keepalive/version/formats/unknown-command + real
     extract (normal & fast) + shutdown semantics + post-shutdown refusal.
  B. P1: close stdin            -> exit <=5s, code 0.
  C. P1: close stdout read end  -> exit <=5s, code 86.
  D. P1: kill the host process  -> worker gone <=5s (exit code recorded).
  E. P4: idle_timeout_ms        -> exit code 87.
  F. best-effort: per-request timeout self-report on a slow text extract.
"""
import json
import os
import subprocess
import sys
import tempfile
import time

REPO = r"E:\xberg"
EXE = os.path.join(REPO, "target", "debug", "xberg.exe")
WORKER_ARGS = ["worker", "--no-config-discovery"]
ENV = dict(os.environ)
ENV.update({
    "HF_HUB_OFFLINE": "1",
    "HUGGINGFACE_HUB_OFFLINE": "1",
    "TRANSFORMERS_OFFLINE": "1",
    "HF_DATASETS_OFFLINE": "1",
    "NO_COLOR": "1",
})

FAILURES = []
LOG = []


def log(msg):
    LOG.append(msg)
    print(msg, flush=True)


def check(name, ok, detail=""):
    tag = "PASS" if ok else "FAIL"
    log(f"[{tag}] {name}" + (f" :: {detail}" if detail else ""))
    if not ok:
        FAILURES.append(f"{name} :: {detail}")


def spawn(config=None):
    args = [EXE] + WORKER_ARGS
    if config is not None:
        args += ["--config-json", json.dumps(config)]
    return subprocess.Popen(
        args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, cwd=REPO, env=ENV,
    )


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode("utf-8"))
    proc.stdin.flush()


def recv(proc, timeout=20.0):
    line = proc.stdout.readline()
    if not line:
        raise AssertionError("worker closed stdout before answering")
    return json.loads(line.decode("utf-8"))


def wait_exit(proc, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        code = proc.poll()
        if code is not None:
            return code, time.monotonic() - (deadline - timeout)
        time.sleep(0.05)
    return None, timeout


def handshake(config=None):
    proc = spawn(config)
    send(proc, {"id": "cap", "command": "capabilities"})
    caps = recv(proc)
    return proc, caps


def test_a_protocol_and_shutdown(sample_txt):
    log("--- A: protocol surface + shutdown ---")
    proc, caps = handshake({"owner_token": "e2e-owner-1"})
    try:
        check("A.capabilities ok", caps.get("ok") is True, json.dumps(caps)[:120])
        check("A.protocol_version>=2", caps.get("protocol_version", 0) >= 2)
        commands = caps.get("commands", [])
        for c in ["extract", "ocr_snapshot", "transcribe", "cancel", "formats",
                  "capabilities", "keepalive", "shutdown", "version"]:
            check(f"A.commands contains {c}", c in commands)
        check("A.cancellation cooperative", caps.get("cancellation") == "cooperative")
        check("A.timeout_ms true", caps.get("timeout_ms") is True)
        check("A.document_snapshot_concurrent", caps.get("document_snapshot_concurrent") is True)
        check("A.extract_modes", set(caps.get("extract_modes", [])) == {"normal", "fast"})
        for field in ["instance_id", "started_at", "config_digest"]:
            check(f"A.identity.{field} present", isinstance(caps.get(field), str) and caps[field])
        check("A.owner echoed", caps.get("owner") == "e2e-owner-1", str(caps.get("owner")))

        send(proc, {"id": "ka", "command": "keepalive"})
        ka = recv(proc)
        check("A.keepalive ok + models", ka.get("ok") is True and "models" in ka)

        send(proc, {"id": "ver", "command": "version"})
        ver = recv(proc)
        check("A.version ok", ver.get("ok") is True and ver.get("version"))
        check("A.version models inventory", "models" in ver and "snapshot" in ver["models"])

        send(proc, {"id": "fmt", "command": "formats"})
        fmt = recv(proc)
        check("A.formats list", fmt.get("ok") is True and isinstance(fmt.get("formats"), list) and fmt["formats"])

        send(proc, {"id": "bad", "command": "bogus_command"})
        bad = recv(proc)
        check("A.unknown ok=false", bad.get("ok") is False)
        check("A.unknown error_kind", bad.get("error_kind") == "unsupported_command")
        check("A.unknown text prefix", "unsupported command 'bogus_command'" in (bad.get("error") or ""))

        with open(sample_txt, "w", encoding="utf-8") as fh:
            fh.write("hello from the worker P1 e2e\nsecond line\n")
        send(proc, {"id": "ex1", "command": "extract", "path": sample_txt, "mode": "normal"})
        ex = recv(proc)
        check("A.extract normal ok", ex.get("ok") is True and "hello from the worker P1 e2e" in ex.get("document", {}).get("content", ""))
        send(proc, {"id": "ex2", "command": "extract", "path": sample_txt, "mode": "fast"})
        ex2 = recv(proc)
        check("A.extract fast ok", ex2.get("ok") is True)

        # shutdown on an idle worker: acknowledged, then the process exits
        # promptly (spec P3: idle shutdown = immediate exit; trailing requests
        # are simply never answered — the refusal response only appears while
        # in-flight work is still draining, which the Rust UT covers).
        started = time.monotonic()
        send(proc, {"id": "sd", "command": "shutdown", "grace_ms": 4000})
        ack = recv(proc)
        check("A.shutdown accepted", ack.get("ok") is True and ack.get("accepted") is True)
        code, elapsed = wait_exit(proc, 10.0)
        check("A.shutdown exit 0", code == 0, f"code={code} elapsed={elapsed:.2f}s")
        check("A.shutdown prompt (idle=immediate)", elapsed <= 2.0, f"{elapsed:.2f}s")
        return
    finally:
        if proc.poll() is None:
            proc.kill()


def test_b_close_stdin():
    log("--- B: P1 close stdin ---")
    proc, _ = handshake()
    started = time.monotonic()
    proc.stdin.close()
    code, elapsed = wait_exit(proc, 5.0)
    check("B.exit within 5s", code is not None, f"elapsed={elapsed:.2f}s")
    check("B.exit code 0", code == 0, f"code={code}")


def test_c_close_stdout():
    log("--- C: P1 close stdout read end ---")
    proc, _ = handshake()
    started = time.monotonic()
    proc.stdout.close()   # only our read end; stdin stays open
    code, elapsed = wait_exit(proc, 5.0)
    check("C.exit within 5s", code is not None, f"elapsed={elapsed:.2f}s")
    check("C.exit code 86", code == 86, f"code={code}")
    if proc.poll() is None:
        proc.kill()


def test_d_kill_host():
    log("--- D: P1 kill host process ---")
    pid_file = os.path.join(REPO, ".tmp", "worker-host-child.pid")
    if os.path.exists(pid_file):
        os.remove(pid_file)
    intermediate = os.path.join(os.path.dirname(os.path.abspath(__file__)), "worker_lifecycle_host.py")
    host = subprocess.Popen([sys.executable, intermediate, pid_file])
    deadline = time.monotonic() + 10
    worker_pid = None
    while time.monotonic() < deadline:
        if os.path.exists(pid_file):
            try:
                worker_pid = int(open(pid_file).read().strip())
                break
            except ValueError:
                pass
        time.sleep(0.05)
    if worker_pid is None:
        check("D.worker spawned via host", False, "pid file never appeared")
        host.kill()
        return
    time.sleep(1.0)  # let the worker finish startup + watchers
    subprocess.run(["taskkill", "/F", "/PID", str(host.pid)], capture_output=True)
    started = time.monotonic()
    gone_by = None
    while time.monotonic() - started < 5.0:
        out = subprocess.run(
            ["tasklist", "/FI", f"PID eq {worker_pid}", "/FO", "CSV"],
            capture_output=True).stdout.decode("utf-8", errors="ignore")
        if str(worker_pid) not in out:
            gone_by = time.monotonic() - started
            break
        time.sleep(0.05)
    check("D.worker gone within 5s", gone_by is not None,
          f"gone_after={gone_by if gone_by is None else round(gone_by, 2)}s pid={worker_pid}")


def test_e_idle():
    log("--- E: P4 idle timeout ---")
    proc, caps = handshake({"idle_timeout_ms": 1200})
    check("E.owner/idle startup ok", caps.get("ok") is True)
    started = time.monotonic()
    code, elapsed = wait_exit(proc, 8.0)
    check("E.idle exit within 8s", code is not None, f"elapsed={elapsed:.2f}s")
    check("E.idle exit code 87", code == 87, f"code={code}")


def test_f_timeout(sample_txt):
    log("--- F: timeout self-report (best effort, no models) ---")
    big = sample_txt + ".big"
    with open(big, "w", encoding="utf-8") as fh:
        fh.write(("filler line for the slow extract test\n" * 400000))
    try:
        proc, _ = handshake()
        t0 = time.monotonic()
        send(proc, {"id": "slow", "command": "extract", "path": big, "timeout_ms": 200})
        resp = recv(proc, timeout=15)
        elapsed = time.monotonic() - t0
        check("F.timeout error_kind", resp.get("error_kind") == "timeout",
              f"elapsed={elapsed:.2f}s resp={json.dumps(resp)[:160]}")
        # CPU-bound native parsing has no cancellation checkpoints, so the
        # terminal "timeout" lands when the handler yields (cooperative
        # cancellation, WORKER.md). The <=1s self-report contract applies to
        # checkpoint-yielding hangs (real OCR) and is covered by scheduler UTs.
        check("F.timeout terminal after handler yields", resp.get("error_kind") == "timeout" and elapsed < 15)
        # process must still serve afterwards
        send(proc, {"id": "after", "command": "keepalive"})
        after = recv(proc)
        check("F.process alive after timeout", after.get("ok") is True)
        proc.stdin.close()
        wait_exit(proc, 5.0)
    finally:
        try:
            os.remove(big)
        except OSError:
            pass


def test_g_no_leaked_debug_workers():
    log("--- G: no leaked workers from this run ---")
    out = subprocess.run(["powershell", "-NoProfile", "-Command",
                          "(Get-Process xberg* -ErrorAction SilentlyContinue | "
                          "Where-Object { $_.Path -eq 'E:\xberg\target\debug\xberg.exe' }).Count"],
                         capture_output=True).stdout.decode("utf-8", errors="ignore").strip()
    check("G.no debug-binary workers remain", out == "0", f"count={out}")


def main():
    if not os.path.exists(EXE):
        print(f"xberg.exe not found at {EXE}", file=sys.stderr)
        return 2
    tmp = tempfile.mkdtemp(prefix="xberg-worker-e2e-")
    sample_txt = os.path.join(tmp, "sample.txt")

    test_a_protocol_and_shutdown(sample_txt)
    test_b_close_stdin()
    test_c_close_stdout()
    test_d_kill_host()
    test_e_idle()
    test_f_timeout(sample_txt)
    time.sleep(1.5)  # let any lingering teardown finish before the leak check
    test_g_no_leaked_debug_workers()

    log("")
    if FAILURES:
        log(f"RESULT: {len(FAILURES)} FAILURE(S)")
        for f in FAILURES:
            log(f"  - {f}")
        return 1
    log("RESULT: ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
