#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""testgate.py — 三级测试门：fastcheck / fulltest / slowtest。

层级语义（详见 AGENTS.md「三级测试门」）：

  fastcheck  AI 可自主执行的快速反馈，60 秒墙钟硬超时（超时杀进程树、判失败，
             绝不因超时报成功）。只是快速反馈，不代表完整验证。
  fulltest   当前平台（Windows）完整本地验证：fmt + 编译 + fulltest.py E2E
             + 单元测试 + clippy。仅限用户对本次运行明确授权后执行。
  slowtest   fulltest 门全部阶段 + slowtest.py（完整打包 + 打包版 E2E）；
             远程发布流水线（build-windows-cli.yml，会创建带 tag 的公开
             GitHub Release，属真实发布副作用）仅在用户明确授权发布目标、
             且显式传 --with-release-ci 时触发并等待最终状态。
  WSL / 跨平台：不适用（本 fork 仅支持 Windows，见 AGENTS.md），报告为
  SKIPPED_NOT_APPLICABLE。

用法：
    python testgate.py fastcheck                        # agent 可自主
    python testgate.py fulltest                         # 需用户明确授权
    python testgate.py slowtest [--with-release-ci]     # 需用户明确授权

与 fulltest.py / slowtest.py 的分工：那两个脚本仍是各自的验收入口（用户点名
「跑 fulltest.py / slowtest.py」时只跑脚本本身）；本文件的 fulltest / slowtest
子命令是在它们之上叠加 Rust 侧检查的门（用户点名「门」或「完整本地验证」时用）。
"""

import argparse
import ast
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent

# fork feature 集，与 AGENTS.md / scripts/publish/cli/package-cli-windows.ps1 保持一致。
FEATURES_FORK = "formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api"
# xberg lib 侧没有 core-cli（那是 xberg-cli 的聚合 feature）；lib default
# （tokio-runtime、simd-utf8）保持开启，等价于 CLI 构建转发到 lib 的能力面。
FEATURES_LIB = "formats-no-heic,analysis,ocr,paddle-ocr,transcription,layout-detection,api"

FASTCHECK_BUDGET_SECONDS = 60.0

# 2026-09-19 已执行 cargo fmt -p xberg 清掉 155 文件量级的既有漂移（用户授权的
# 批量重排版），三 crate 全部纳入 fastcheck，防漂移回潮。
FMT_FASTCHECK_CRATES = ["xberg", "xberg-cli", "xberg-windows-metafile"]

PS1_PARSE_TARGETS = [
    "scripts/publish/cli/package-cli-windows.ps1",
    "scripts/publish/cli/offline-smoke.ps1",
    "scripts/ci/verify-windows-dll-closure.ps1",
]

RELEASE_WORKFLOW = "build-windows-cli.yml"
RELEASE_CI_WAIT_SECONDS = 130 * 60  # workflow 自身 timeout-minutes: 120，留裕量

sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)

PASS = "PASS"
FAIL = "FAIL"
TIMEOUT = "TIMEOUT"
SKIPPED_PRIOR_FAIL = "SKIPPED_PRIOR_FAIL"
SKIPPED_NOT_AUTHORIZED = "SKIPPED_NOT_AUTHORIZED"
SKIPPED_NOT_APPLICABLE = "SKIPPED_NOT_APPLICABLE"
UNVERIFIED = "UNVERIFIED"


# ---------------------------------------------------------------- 命令执行

def _kill_tree(proc):
    """终止整个进程树（fastcheck 超时用；不留孤儿子进程）。

    taskkill 可能失败（进程已退出但句柄仍被持有、权限不足等）。失败后回退
    `proc.kill()`，且等待有界 —— 杀进程路径一旦变成无界等待，超时的 stage 会
    挂住整门而不是按 TIMEOUT 收场，与 60 秒硬超时的承诺不符。
    """
    if os.name == "nt":
        killed = subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                                capture_output=True).returncode == 0
    else:  # pragma: no cover — 本 fork 仅 Windows，保底分支
        killed = True
        try:
            proc.kill()
        except OSError:
            killed = False
    if not killed:
        # taskkill 失败时的最后手段：只杀直接子进程（孤儿子进程尽力而为）。
        try:
            proc.kill()
        except OSError:
            pass
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        # kill 后 10 秒仍不退出：放弃等待，交由调用方按 TIMEOUT 判失败。
        pass


def run_cmd(cmd, deadline=None, env=None):
    """运行命令，输出直接透传。返回 (returncode, timed_out)。"""
    shown = " ".join(str(c) for c in cmd)
    if len(shown) > 160:  # pwsh -Command 内联脚本这类超长参数只回显开头
        shown = shown[:160] + " …(截断)"
    print(f"  $ {shown}", flush=True)
    try:
        proc = subprocess.Popen([str(c) for c in cmd], cwd=str(REPO), env=env)
    except OSError as exc:
        # 缺 pwsh/python 之类：报清晰的一行而不是整门 traceback。
        print(f"  ! 无法启动 {cmd[0]}：{exc}", flush=True)
        return 1, False
    if deadline is None:
        return proc.wait(), False
    while True:
        try:
            return proc.wait(timeout=0.2), False
        except subprocess.TimeoutExpired:
            if time.monotonic() > deadline:
                _kill_tree(proc)
                return None, True


def check_py_syntax(files):
    bad = []
    for rel in files:
        path = REPO / rel
        try:
            ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        except (OSError, SyntaxError) as exc:
            bad.append(f"{rel}: {exc}")
    return bad


def ps1_parse_command():
    """单次 pwsh 调用解析全部 PS1 目标（省掉重复的 pwsh 启动开销）。"""
    checks = []
    for rel in PS1_PARSE_TARGETS:
        full = str(REPO / rel).replace("'", "''")
        checks.append(
            f"$t=$null;$e=$null;"
            f"[System.Management.Automation.Language.Parser]::ParseFile('{full}',[ref]$t,[ref]$e)|Out-Null;"
            f"if($e -and $e.Count){{ $e|ForEach-Object{{ Write-Output ('{full}:'+$_.Extent.StartLineNumber+': '+$_.Message) }}; $bad=1 }}"
        )
    script = ("$bad=0;" + "".join(checks)
              + f"if($bad){{ exit 1 }} else {{ Write-Output 'PS1 parse OK ({len(PS1_PARSE_TARGETS)} files)' }}")
    return ["pwsh", "-NoProfile", "-Command", script]


# ---------------------------------------------------------------- fastcheck

def gate_fastcheck():
    t0 = time.monotonic()
    deadline = t0 + FASTCHECK_BUDGET_SECONDS
    results = []  # (stage, status, detail)

    def stage(name, status, detail="", seconds=None):
        sec = f", {seconds:.1f}s" if seconds is not None else ""
        print(f"[fastcheck] {name}: {status}{sec}" + (f" — {detail}" if detail else ""), flush=True)
        results.append((name, status))

    # 1) 仓库自有测试脚本语法（含本文件自身）
    bad = check_py_syntax(["fulltest.py", "slowtest.py", "testgate.py"])
    stage("py-syntax", FAIL if bad else PASS, "; ".join(bad))

    # 2) fulltest.py 判定器自测（AGENTS.md 允许 agent 主动运行）
    if deadline - time.monotonic() <= 0:
        stage("fulltest-selftest", TIMEOUT, "60 秒预算耗尽")
    else:
        ts = time.monotonic()
        rc, timed_out = run_cmd([sys.executable, REPO / "fulltest.py", "--selftest"],
                                deadline=deadline)
        if timed_out:
            stage("fulltest-selftest", TIMEOUT)
        else:
            stage("fulltest-selftest", FAIL if rc != 0 else PASS, "" if rc == 0 else f"退出码 {rc}",
                  time.monotonic() - ts)

    # 3) 打包 / CI 关键 PS1 解析
    if deadline - time.monotonic() <= 0:
        stage("ps1-parse", TIMEOUT, "60 秒预算耗尽")
    else:
        ts = time.monotonic()
        rc, timed_out = run_cmd(ps1_parse_command(), deadline=deadline)
        if timed_out:
            stage("ps1-parse", TIMEOUT)
        else:
            stage("ps1-parse", FAIL if rc != 0 else PASS, "" if rc == 0 else f"退出码 {rc}",
                  time.monotonic() - ts)

    # 4) cargo fmt --check（仅当前干净的 crate；见 FMT_FASTCHECK_CRATES 注释）
    for crate in FMT_FASTCHECK_CRATES:
        if deadline - time.monotonic() <= 0:
            stage(f"fmt-{crate}", TIMEOUT, "60 秒预算耗尽")
            continue
        ts = time.monotonic()
        rc, timed_out = run_cmd(["cargo", "fmt", "--check", "-p", crate], deadline=deadline)
        if timed_out:
            stage(f"fmt-{crate}", TIMEOUT)
        else:
            stage(f"fmt-{crate}", FAIL if rc != 0 else PASS, "" if rc == 0 else f"退出码 {rc}",
                  time.monotonic() - ts)

    elapsed = time.monotonic() - t0
    failed = [s for s, st in results if st != PASS]
    print(f"\nfastcheck: {'FAIL' if failed else 'PASS'}, {elapsed:.1f}s "
          f"(预算 {FASTCHECK_BUDGET_SECONDS:.0f}s)", flush=True)
    return 1 if failed else 0


# ---------------------------------------------------------------- fulltest

# (阶段名, 命令, 依赖的阶段名列表)
FULLTEST_STAGES = [
    ("fmt-xberg", ["cargo", "fmt", "--check", "-p", "xberg"], []),
    ("fmt-xberg-cli", ["cargo", "fmt", "--check", "-p", "xberg-cli"], []),
    ("fmt-xberg-windows-metafile", ["cargo", "fmt", "--check", "-p", "xberg-windows-metafile"], []),
    ("build-cli", ["cargo", "build", "-p", "xberg-cli", "--no-default-features",
                   "--features", FEATURES_FORK], []),
    ("e2e-fulltest.py", [sys.executable, REPO / "fulltest.py", "--keep-going"], ["build-cli"]),
    ("test-documents-corpus", None, []),  # 内置检查：见 gate_fulltest 特殊分支
    ("test-xberg", ["cargo", "test", "-p", "xberg", "--features", FEATURES_LIB],
     ["test-documents-corpus"]),
    ("test-xberg-cli", ["cargo", "test", "-p", "xberg-cli", "--no-default-features",
                        "--features", FEATURES_FORK], ["test-documents-corpus"]),
    ("test-xberg-windows-metafile", ["cargo", "test", "-p", "xberg-windows-metafile"], []),
    ("clippy-xberg", ["cargo", "clippy", "-p", "xberg", "--features", FEATURES_LIB,
                      "--", "-D", "warnings"], []),
    ("clippy-xberg-cli", ["cargo", "clippy", "-p", "xberg-cli", "--no-default-features",
                          "--features", FEATURES_FORK, "--", "-D", "warnings"], []),
    ("clippy-xberg-windows-metafile", ["cargo", "clippy", "-p", "xberg-windows-metafile",
                                       "--", "-D", "warnings"], []),
]

# test_documents 子模块的二进制语料不进 git（corpus.lock.json 锁 693 个对象，从公开
# bucket 拉取，见 test_documents/scripts/fetch_corpus.py）。缺语料时 50+ 个测试只报
# "fixture not found"，极难归因——test 阶段前先对清单做存在性预检（只查路径，不验哈
# 希，秒级；哈希由 fetch 脚本自身保证）。首次拉取约 618 MB：
#   cd test_documents && python scripts/fetch_corpus.py
CORPUS_MANIFEST = REPO / "test_documents" / "corpus.lock.json"


def _corpus_missing_paths():
    import json
    try:
        manifest = json.loads(CORPUS_MANIFEST.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return [f"无法读取 {CORPUS_MANIFEST}：{exc}"]
    root = CORPUS_MANIFEST.parent
    return [rel for rel in manifest.get("objects", {})
            if not (root / rel).is_file()]

# test 阶段首次编译时按 .cargo/config.toml 的 jobs=28 并行会在本机耗尽页面文件
# （os error 1455，2026-09-19 实测；CARGO_BUILD_JOBS=8 重跑成功）。只压编译并行度，
# 测试线程数不动——覆盖语义不变。.cargo/config.toml 由 alef 生成（DO NOT EDIT），
# 故用环境变量在门内覆盖，而不是改全局配置。
TEST_BUILD_JOBS = "8"
TEST_STAGE_PREFIX = "test-"


def _stage_env(name):
    if name.startswith(TEST_STAGE_PREFIX):
        merged = dict(os.environ)
        merged["CARGO_BUILD_JOBS"] = TEST_BUILD_JOBS
        return merged
    return None


def gate_fulltest():
    print("fulltest 门：当前平台（Windows）完整本地验证 —— 仅限用户明确授权后运行。", flush=True)
    statuses = {}
    for name, cmd, needs in FULLTEST_STAGES:
        if any(statuses.get(n) != PASS for n in needs):
            statuses[name] = SKIPPED_PRIOR_FAIL
            print(f"[fulltest] {name}: {SKIPPED_PRIOR_FAIL}", flush=True)
            continue
        ts = time.monotonic()
        if name == "test-documents-corpus":
            missing = _corpus_missing_paths()
            statuses[name] = FAIL if missing else PASS
            detail = ""
            if missing:
                shown = ", ".join(missing[:5])
                detail = (f"缺 {len(missing)} 个语料对象（如 {shown}）；"
                          f"修复：cd test_documents && python scripts/fetch_corpus.py")
            print(f"[fulltest] {name}: {statuses[name]}, {time.monotonic()-ts:.1f}s"
                  + (f" — {detail}" if detail else ""), flush=True)
            continue
        rc, _ = run_cmd(cmd, env=_stage_env(name))
        statuses[name] = PASS if rc == 0 else FAIL
        print(f"[fulltest] {name}: {statuses[name]}, {time.monotonic()-ts:.0f}s", flush=True)

    failed = [n for n, st in statuses.items() if st != PASS]
    print(f"\nfulltest 门: {'FAIL' if failed else 'PASS'}"
          + (f"（失败/跳过: {', '.join(failed)}）" if failed else ""), flush=True)
    return 1 if failed else 0


# ---------------------------------------------------------------- slowtest

def _release_ci_gate():
    """触发 build-windows-cli.yml 并等待最终状态。

    该 workflow 会创建带时间戳 tag 的公开 GitHub Release（真实发布副作用），
    因此只在用户明确授权发布目标、并显式传 --with-release-ci 时才会走到这里。
    它在 CI 里只调用 package-cli-windows.ps1，不会回调 testgate（无递归风险）。
    """
    if shutil.which("gh") is None:
        return UNVERIFIED, "gh CLI 不可用"
    repo = _fork_repo()
    if repo is None:
        return UNVERIFIED, "无法从 origin remote 解析 fork 仓库（gh 调用必须显式 -R，见 _fork_repo）"
    rc, out = _git(["status", "--porcelain"])
    if out.strip():
        return FAIL, "工作区不干净（远程 CI 测的不是本地改动），先提交并推送"
    rc, branch = _git(["rev-parse", "--abbrev-ref", "HEAD"])
    rc, head = _git(["rev-parse", "HEAD"])
    rc, origin = _git(["rev-parse", f"origin/{branch.strip()}"])
    if head.strip() != origin.strip():
        return FAIL, f"HEAD 未推送到 origin/{branch.strip()}，远程测不到当前提交"
    if subprocess.run(["gh", "auth", "status"], capture_output=True).returncode != 0:
        return UNVERIFIED, "gh 未认证"

    print(f"  $ gh workflow run {RELEASE_WORKFLOW} -R {repo} --ref {branch.strip()}", flush=True)
    _, before = _gh_json(["run", "list", "-R", repo, "--workflow", RELEASE_WORKFLOW,
                          "--branch", branch.strip(), "--limit", "1",
                          "--json", "databaseId"])
    run_before = before[0]["databaseId"] if before else None
    run_p = subprocess.run(["gh", "workflow", "run", "-R", repo, RELEASE_WORKFLOW,
                            "--ref", branch.strip()], capture_output=True, text=True,
                           encoding="utf-8", errors="replace")
    if run_p.returncode != 0:
        lines = [ln for ln in (run_p.stderr or run_p.stdout or "").splitlines() if ln.strip()]
        return FAIL, (f"gh workflow run 退出码 {run_p.returncode}"
                      + (f"：{lines[-1].strip()}" if lines else ""))

    t0 = time.monotonic()
    run_id = None
    while time.monotonic() - t0 < RELEASE_CI_WAIT_SECONDS:
        time.sleep(30)
        _, runs = _gh_json(["run", "list", "-R", repo, "--workflow", RELEASE_WORKFLOW,
                            "--branch", branch.strip(), "--limit", "5",
                            "--json", "databaseId,status,conclusion"])
        fresh = [r for r in (runs or []) if run_before is None or r["databaseId"] > run_before]
        if fresh:
            run_id = fresh[0]["databaseId"]
            _, cur = _gh_json(["run", "view", "-R", repo, str(run_id),
                               "--json", "status,conclusion"])
            if not isinstance(cur, dict):
                continue  # 单次查询失败：等下一轮轮询
            status = cur.get("status")
            print(f"  [release-ci] run {run_id}: {status} / {cur.get('conclusion')}", flush=True)
            if status == "completed":
                conclusion = cur.get("conclusion")
                return (PASS if conclusion == "success" else FAIL), f"run {run_id}: {conclusion}"
    if run_id is None:
        return UNVERIFIED, f"{RELEASE_CI_WAIT_SECONDS//60} 分钟内未出现新 run"
    return UNVERIFIED, f"等待超时（run {run_id} 未完成）"


def _git(args):
    p = subprocess.run(["git"] + args, cwd=str(REPO), capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    return p.returncode, p.stdout


def _fork_repo():
    """origin remote 对应的 OWNER/REPO（gh 调用的显式 -R 目标）。

    本检出同时有 origin（fork）与 upstream；gh 在本目录解析到哪个仓库取决于
    remote 解析顺序（本机实测解析到 upstream 的 xberg-io/xberg），而
    build-windows-cli.yml 只存在于 fork —— 不显式 -R 时 run list 直接 404，
    `gh workflow run` 也必失败。
    """
    rc, url = _git(["remote", "get-url", "origin"])
    url = url.strip()
    if rc != 0 or not url:
        return None
    if "/github.com/" in url:  # https://github.com/<owner>/<repo>(.git)
        tail = url.rsplit("/github.com/", 1)[-1]
    else:  # git@github.com:<owner>/<repo>(.git)
        tail = url.split(":")[-1]
    tail = tail.removesuffix("/").removesuffix(".git")
    return tail or None


def _gh_json(args):
    """运行 gh 并把 `--json` 输出解析为 Python 对象（对象或数组，按 gh 返回原样）。

    rc != 0 或输出不是合法 JSON（漏带 --json 时 gh 打印人读文本）都返回
    (rc, None)，由调用方决定如何呈现；绝不把半截文本当数据用。
    """
    p = subprocess.run(["gh"] + args, cwd=str(REPO), capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    if p.returncode != 0:
        return p.returncode, None
    import json
    try:
        return 0, json.loads(p.stdout)
    except json.JSONDecodeError:
        return 1, None


def gate_slowtest(with_release_ci):
    print("slowtest 门：fulltest 门 + 打包验证（+ 可选远程发布流水线）—— 仅限用户明确授权后运行。",
          flush=True)
    statuses = {}

    ts = time.monotonic()
    rc = subprocess.run([sys.executable, str(REPO / "testgate.py"), "fulltest"]).returncode
    statuses["fulltest-gate"] = PASS if rc == 0 else FAIL
    print(f"[slowtest] fulltest-gate: {statuses['fulltest-gate']}, {time.monotonic()-ts:.0f}s", flush=True)

    if statuses["fulltest-gate"] == PASS:
        ts = time.monotonic()
        rc, _ = run_cmd([sys.executable, REPO / "slowtest.py"])
        statuses["slowtest.py"] = PASS if rc == 0 else FAIL
        print(f"[slowtest] slowtest.py: {statuses['slowtest.py']}, {time.monotonic()-ts:.0f}s", flush=True)
    else:
        # 前置本地验证已失败：默认停止后续昂贵阶段，不在已知失败状态上打包/发布。
        statuses["slowtest.py"] = SKIPPED_PRIOR_FAIL
        print(f"[slowtest] slowtest.py: {SKIPPED_PRIOR_FAIL}", flush=True)

    # WSL / 跨平台：本 fork 仅支持 Windows（AGENTS.md），不适用。
    statuses["wsl-cross-platform"] = SKIPPED_NOT_APPLICABLE
    print(f"[slowtest] wsl-cross-platform: {SKIPPED_NOT_APPLICABLE}（本 fork 仅支持 Windows）", flush=True)

    if with_release_ci:
        local_failed = [n for n, st in statuses.items() if st != PASS and st != SKIPPED_NOT_APPLICABLE]
        if local_failed:
            # 本地验证已失败时不得触发真实发布流水线（远程阶段浪费资源且会在
            # 已知失败状态上制造公开 Release）。修复后重跑本门再触发。
            statuses["release-ci"] = SKIPPED_PRIOR_FAIL
            print(f"[slowtest] release-ci: {SKIPPED_PRIOR_FAIL}"
                  f" — 本地阶段未全绿（{', '.join(local_failed)}），不触发发布流水线", flush=True)
        else:
            ts = time.monotonic()
            status, detail = _release_ci_gate()
            statuses["release-ci"] = status
            print(f"[slowtest] release-ci: {status}, {time.monotonic()-ts:.0f}s"
                  + (f" — {detail}" if detail else ""), flush=True)
    else:
        statuses["release-ci"] = SKIPPED_NOT_AUTHORIZED
        print(f"[slowtest] release-ci: {SKIPPED_NOT_AUTHORIZED}"
              f" — {RELEASE_WORKFLOW} 会创建公开 Release，需用户明确授权发布目标并加 --with-release-ci",
              flush=True)

    hard_failed = [n for n, st in statuses.items()
                   if st in (FAIL, TIMEOUT, SKIPPED_PRIOR_FAIL, UNVERIFIED)]
    print(f"\nslowtest 门: {'FAIL' if hard_failed else 'PASS'}"
          + (f"（失败/未验证/被跳过: {', '.join(hard_failed)}）" if hard_failed else ""), flush=True)
    if statuses["release-ci"] != PASS:
        print("注意：远程发布流水线未通过（未授权/未运行/失败），本次不能宣称『完整 slowtest 已通过』。",
              flush=True)
    return 1 if hard_failed else 0


# ---------------------------------------------------------------- 入口

def main():
    ap = argparse.ArgumentParser(description="三级测试门：fastcheck / fulltest / slowtest（见 AGENTS.md）")
    sub = ap.add_subparsers(dest="gate", required=True)
    sub.add_parser("fastcheck", help="快速反馈（agent 可自主，60 秒硬超时）")
    sub.add_parser("fulltest", help="当前平台完整本地验证（需用户明确授权）")
    sp_slow = sub.add_parser("slowtest", help="fulltest 门 + 打包验证（需用户明确授权）")
    sp_slow.add_argument("--with-release-ci", action="store_true",
                         help="额外触发 build-windows-cli.yml（创建公开 Release，需用户明确授权发布目标）")
    args = ap.parse_args()

    if args.gate == "fastcheck":
        sys.exit(gate_fastcheck())
    if args.gate == "fulltest":
        sys.exit(gate_fulltest())
    sys.exit(gate_slowtest(args.with_release_ci))


if __name__ == "__main__":
    main()
