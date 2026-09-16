#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""slowtest.py — 慢速全量验证：先打完整发布 zip 包，再对打包版 CLI 跑 fulltest.py。

流程:
  1. 运行 scripts/publish/cli/package-cli-windows.ps1 打完整包
     （release 编译 + 模型/DLL 打包 + 门禁校验，可能 30 分钟以上）
  2. 把 xberg-cli-x86_64-pc-windows-msvc.zip 解压到临时目录
  3. 用解压出的 xberg.exe 跑 fulltest.py（--keep-going 全量测完），
     模型目录（--pkg-dir）与输出目录都指向临时目录，结果保留供检查

用法:
    python slowtest.py [--skip-package] [--src DIR] [--timeout SECS]

与 fulltest.py 的分工：fulltest.py 测本地编译版（快，只编译不打包）；
本脚本测打包版（慢，包含完整打包）。两者都只在用户明确要求时运行。
"""

import argparse
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent
PKG_STEM = "xberg-cli-x86_64-pc-windows-msvc"
ZIP_PATH = REPO / f"{PKG_STEM}.zip"
PACKAGE_SCRIPT = REPO / "scripts" / "publish" / "cli" / "package-cli-windows.ps1"

sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)


def run_package():
    """运行打包脚本（实时透传输出）。返回 True 表示 zip 已生成。"""
    pwsh = shutil.which("pwsh")
    if not pwsh:
        # 打包脚本带 `#Requires -Version 7.4`（Start-Process -Environment 等依赖），
        # Windows PowerShell 5.1 会在加载期被直接拒绝，回退只会报误导性的语法错误。
        print("未找到 pwsh：打包需要 PowerShell >= 7.4，请先安装 PowerShell 7"
              "（https://aka.ms/powershell），不要用 Windows PowerShell 5.1 运行。")
        return False
    cmd = [pwsh, "-NoProfile", "-File", str(PACKAGE_SCRIPT)]
    proc = subprocess.run(cmd, cwd=str(REPO))
    if proc.returncode != 0:
        print(f"打包失败（退出码 {proc.returncode}）")
        return False
    if not ZIP_PATH.is_file():
        print(f"打包脚本执行完但未生成 zip: {ZIP_PATH}")
        return False
    print(f"打包完成: {ZIP_PATH}")
    return True


def main():
    ap = argparse.ArgumentParser(description="打包完整 zip 并对打包版 CLI 跑 fulltest.py（慢）")
    ap.add_argument("--skip-package", action="store_true", help="复用已有的 zip，不重新打包")
    ap.add_argument("--src", default=None, help="传给 fulltest.py 的测试目录")
    ap.add_argument("--timeout", type=int, default=None, help="传给 fulltest.py 的单文件超时(秒)")
    args = ap.parse_args()

    t0 = time.time()

    print("[1/3] 完整打包（release 编译 + 模型/DLL + 门禁校验）")
    if args.skip_package:
        if not ZIP_PATH.is_file():
            sys.exit(f"--skip-package 但找不到已有 zip: {ZIP_PATH}")
        print(f"  复用已有包: {ZIP_PATH}")
    else:
        if not PACKAGE_SCRIPT.is_file():
            sys.exit(f"找不到打包脚本: {PACKAGE_SCRIPT}")
        if not run_package():
            sys.exit(1)

    tmp = Path(tempfile.mkdtemp(prefix="xberg-slowtest-"))
    print(f"\n[2/3] 解压到临时目录: {tmp}")
    with zipfile.ZipFile(ZIP_PATH) as zf:
        zf.extractall(tmp)
    exes = list(tmp.rglob("xberg.exe"))
    if not exes:
        sys.exit(f"解压后找不到 xberg.exe: {tmp}")
    exe = exes[0]
    print(f"  使用打包版 CLI: {exe}")

    cmd = [sys.executable, str(REPO / "fulltest.py"),
           "--cli", str(exe),
           "--out", str(tmp / "fulltest-out"),
           "--pkg-dir", str(exe.parent),
           "--keep-going"]
    if args.src:
        cmd += ["--src", args.src]
    if args.timeout:
        cmd += ["--timeout", str(args.timeout)]

    print("\n[3/3] 对打包版 CLI 跑 fulltest.py（--keep-going 全量测完）")
    rc = subprocess.run(cmd).returncode
    report = tmp / "fulltest-out" / "_quality-report.md"
    if report.is_file():
        stamp = time.strftime("%Y%m%d-%H%M%S")
        stable = REPO / "target" / f"slowtest-report-{stamp}.md"
        stable.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(report, stable)
        print(f"质量报告副本: {stable}")
    print(f"\nslowtest 完成: 总耗时 {time.time()-t0:.0f}s | 临时目录(保留供检查): {tmp}")
    sys.exit(rc)


if __name__ == "__main__":
    main()
