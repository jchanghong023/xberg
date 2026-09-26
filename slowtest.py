#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""slowtest.py — 慢速全量验证：先打完整发布 zip 包，再对打包版 CLI 跑 fulltest.py。

流程:
  1. 运行 scripts/publish/cli/package-cli-windows.ps1 打完整包
     （release 编译 + 模型/DLL 打包 + 门禁校验，可能 30 分钟以上）
  2. 把 xberg-cli-x86_64-pc-windows-msvc.zip 解压到 target/slowtest-tmp
  3. 用解压出的 xberg.exe 跑 fulltest.py（--keep-going 全量测完），
     模型目录（--pkg-dir）与输出目录都指向该目录

解压目录固定放在仓库 target/ 下（不再用系统 %TEMP%）：跑成功即删除（报告副本已存
target/slowtest-report-*.md），失败保留供检查。历史行为是每次运行都在 %TEMP% 留下
0.6~0.9 GB 且永不清除（2026-09-20 实测积压 6 个目录 ≈ 4.4 GB）。要无条件保留：

用法:
    python slowtest.py [--skip-package] [--src DIR] [--timeout SECS] [--keep-tmp]

与 fulltest.py 的分工：fulltest.py 测本地编译版（快，只编译不打包）；
本脚本测打包版（慢，包含完整打包）。两者都只在用户明确要求时运行。
"""

import argparse
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent
PKG_STEM = "xberg-cli-x86_64-pc-windows-msvc"
ZIP_PATH = REPO / f"{PKG_STEM}.zip"
PACKAGE_SCRIPT = REPO / "scripts" / "publish" / "cli" / "package-cli-windows.ps1"
# 解压目录：仓库 target/ 内（cargo clean 会一并回收；不再往系统 %TEMP% 里堆垃圾）
TMP_DIR = REPO / "target" / "slowtest-tmp"

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
    ap.add_argument("--keep-tmp", action="store_true",
                    help=f"跑完后保留 {TMP_DIR}（默认：成功即删除，失败保留）")
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

    # 固定目录：每轮先清空，避免上一轮的残留混进本轮（解压 + 输出目录都在里面，
    # 半截残留会让「解压后找不到 xberg.exe」这类判断读到旧文件）。
    shutil.rmtree(TMP_DIR, ignore_errors=True)
    TMP_DIR.mkdir(parents=True, exist_ok=True)
    tmp = TMP_DIR
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
           "--keep-going",
           # 打包版验收必须覆盖最重测试段（音视频转写）：fulltest.py 默认跳过它们，见 --deep
           "--deep"]
    if args.src:
        cmd += ["--src", args.src]
    if args.timeout:
        cmd += ["--timeout", str(args.timeout)]

    print("\n[3/3] 对打包版 CLI 跑 fulltest.py（--keep-going 全量测完）")
    rc = subprocess.run(cmd).returncode
    out_dir = tmp / "fulltest-out"
    # 机器可读报告也要外带一份：成功时临时目录会被删掉，只留 .md 的话回归对比
    # （_quality-report.json 的逐文件问题码/指标）就没了。
    stamp = time.strftime("%Y%m%d-%H%M%S")
    kept = []
    for name in ("_quality-report.md", "_quality-report.json"):
        src_report = out_dir / name
        if src_report.is_file():
            dest = REPO / "target" / f"slowtest-report-{stamp}{src_report.suffix}"
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src_report, dest)
            kept.append(dest)
    for dest in kept:
        print(f"报告副本: {dest}")
    if not args.keep_tmp and rc == 0:
        shutil.rmtree(tmp, ignore_errors=True)
        tmp_note = f"临时目录已删除（报告副本见 target/）：{tmp}"
    else:
        reason = "--keep-tmp" if args.keep_tmp else f"退出码 {rc}，保留供检查"
        tmp_note = f"临时目录保留（{reason}）：{tmp}"
    print(f"\nslowtest 完成: 总耗时 {time.time()-t0:.0f}s | {tmp_note}")
    sys.exit(rc)


if __name__ == "__main__":
    main()
