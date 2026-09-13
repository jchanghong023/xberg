#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""fulltest.py — xberg 集成测试：把测试目录下所有文件转换为 Markdown 并评估转换质量。

用法:
    python fulltest.py [--cli PATH] [--src DIR] [--out DIR] [--timeout SECS] [--keep-going]

默认调用 target\\debug\\xberg.exe，输出到 D:\\测试转markdown转换效果\\测试文档_md_fulltest。
每个文件打印一份质量报告（结构启发式 + 源文件文本召回率 + xberg 元数据），最后打印汇总。
默认遇到第一个 FAIL 立即终止（--keep-going 继续跑完）。
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

# ---------------------------------------------------------------- 默认配置
REPO = Path(r"D:\code1111111111\xberg")
DEFAULT_CLI = REPO / "target" / "debug" / "xberg.exe"
# 打包目录只用来提供模型缓存（HF_HOME）和 DLL 搜索路径；测试永远只用本地编译的 CLI
PKG_DIR = REPO / "xberg-cli-x86_64-pc-windows-msvc"
DEFAULT_SRC = Path(r"D:\测试转markdown转换效果\测试文档")
DEFAULT_OUT = Path(r"D:\测试转markdown转换效果\测试文档_md_fulltest")

AV_EXTS = {"mp4", "wmv", "mov", "mkv", "m4a", "mp3", "wav", "webm", "flv", "avi"}
TRANSCRIPTION_CFG = {"enabled": True, "model": "tiny"}

# 召回率阈值
RECALL_GOOD = 0.85
RECALL_FAIL = 0.60

TICKER_INTERVAL = 5  # 秒

# 同时输出到终端和质量报告文件的行（main 结束时写入 <out>/_quality-report.md）
REPORT_LINES: list = []


def emit(line=""):
    print(line, flush=True)
    REPORT_LINES.append(line)

sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)


# ---------------------------------------------------------------- 源文本抽取（召回率基准）
def extract_source_text(path: Path):
    """用第三方库从源文件独立抽取文本；返回 (文本, 库名) 或 (None, 原因)。"""
    ext = path.suffix.lower().lstrip(".")
    try:
        if ext == "pdf":
            import pymupdf
            with pymupdf.open(path) as doc:
                return "\n".join(page.get_text() for page in doc), "pymupdf"
        if ext == "docx":
            import docx
            d = docx.Document(str(path))
            parts = [p.text for p in d.paragraphs]
            for t in d.tables:
                for row in t.rows:
                    parts.append(" ".join(c.text for c in row.cells))
            return "\n".join(parts), "python-docx"
        if ext == "pptx":
            from pptx import Presentation
            prs = Presentation(str(path))
            parts = []
            for slide in prs.slides:
                for shape in slide.shapes:
                    if shape.has_text_frame:
                        parts.append(shape.text_frame.text)
                    if getattr(shape, "has_table", False):
                        for row in shape.table.rows:
                            parts.append(" ".join(c.text for c in row.cells))
            return "\n".join(parts), "python-pptx"
        if ext in ("xlsx", "xls", "ods"):
            from openpyxl import load_workbook
            wb = load_workbook(str(path), read_only=True, data_only=True)
            parts = []
            for ws in wb.worksheets:
                for row in ws.iter_rows(values_only=True):
                    parts.append(" ".join(str(v) for v in row if v is not None))
            return "\n".join(parts), "openpyxl"
    except ImportError as e:
        return None, f"缺少依赖({e.name})"
    except Exception as e:
        return None, f"源文件解析失败: {e}"
    return None, None  # 该格式没有对应的基准抽取器


def bigram_recall(source: str, md: str) -> float:
    """源文本字符 bigram 在 markdown 中的覆盖率（0~1）。空源返回 -1 表示无法评估。"""
    def norm(s):
        return re.sub(r"\s+", "", s)
    src, out = norm(source), norm(md)
    if len(src) < 20:
        return -1.0
    grams = {src[i:i + 2] for i in range(len(src) - 1)}
    if not grams:
        return -1.0
    out_set = {out[i:i + 2] for i in range(len(out) - 1)}
    return len(grams & out_set) / len(grams)


# ---------------------------------------------------------------- 结构启发式
def structural_metrics(md_text: str, img_dir: Path):
    lines = md_text.splitlines()
    chars = len(md_text)
    headings = sum(1 for l in lines if re.match(r"^#{1,6}\s", l))
    table_rows = sum(1 for l in lines if l.lstrip().startswith("|"))
    img_refs = len(re.findall(r"!\[[^\]]*\]\([^)]+\)", md_text))
    fences = sum(1 for l in lines if l.lstrip().startswith("```"))
    nonempty = sum(1 for l in lines if l.strip())
    replacement = md_text.count("\ufffd")
    ctrl = sum(1 for c in md_text if ord(c) < 32 and c not in "\n\r\t")
    img_dir_files = [p for p in img_dir.iterdir() if p.is_file()] if img_dir.is_dir() else []
    img_names = {p.name for p in img_dir_files}
    # markdown 里引用的图片文件是否都真实存在
    broken_refs = [ref for ref in re.findall(r"!\[[^\]]*\]\(([^)]+)\)", md_text)
                   if Path(ref).name not in img_names]
    return {
        "chars": chars, "headings": headings, "table_rows": table_rows,
        "img_refs": img_refs, "fences": fences, "nonempty": nonempty,
        "replacement": replacement, "ctrl": ctrl, "imgs_on_disk": len(img_dir_files),
        "broken_refs": broken_refs,
        "fence_balanced": fences % 2 == 0,
    }


def judge_structure(m, issues):
    if m["chars"] == 0:
        issues.append("转换结果为空")
    elif m["chars"] < 20:
        issues.append(f"内容过短({m['chars']}字符)")
    if not m["fence_balanced"]:
        issues.append("代码围栏不成对")
    if m["replacement"] > 0:
        issues.append(f"含 {m['replacement']} 个替换字符(疑似乱码)")
    if m["ctrl"] > 10:
        issues.append(f"含 {m['ctrl']} 个控制字符")
    if m["img_refs"] != m["imgs_on_disk"]:
        issues.append(f"图片引用({m['img_refs']})与导出图片数({m['imgs_on_disk']})不一致")
    if m["broken_refs"]:
        issues.append(f"{len(m['broken_refs'])} 个图片引用指向不存在的文件: "
                      f"{', '.join(m['broken_refs'][:3])}")


def judge_meta_images(meta, m, issues):
    """xberg 元数据里的图片数与磁盘导出图片数交叉校验。"""
    imgs_meta = (meta.get("counts") or {}).get("images")
    if imgs_meta and imgs_meta != m["imgs_on_disk"]:
        issues.append(f"元数据图片数({imgs_meta})与磁盘导出数({m['imgs_on_disk']})不一致")


# ---------------------------------------------------------------- 子进程执行（带进度）
def run_with_ticker(label, cmd, env, timeout):
    """运行子进程，每 5 秒在同一行刷新已运行秒数；超时杀掉进程。"""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    t0 = time.time()
    while True:
        try:
            out, err = proc.communicate(timeout=TICKER_INTERVAL)
            break
        except subprocess.TimeoutExpired:
            print(f"\r  {label} 运行中... {int(time.time()-t0)}s", end="", flush=True)
            if timeout and time.time() - t0 > timeout:
                proc.kill()
                out, err = proc.communicate()
                print("\r" + " " * 100 + "\r", end="", flush=True)
                raise subprocess.TimeoutExpired(cmd, timeout)
    print("\r" + " " * 100 + "\r", end="", flush=True)
    return proc.returncode, out, err, time.time() - t0


# ---------------------------------------------------------------- 单文件转换
def build_cmd(cli: Path, src_file: Path, img_dir: Path, transcription: bool):
    cfg = {"images": {"output_format": {"type": "png"}}}
    if transcription:
        cfg["transcription"] = TRANSCRIPTION_CFG
    return [
        str(cli), "extract", str(src_file),
        "--no-config-discovery",
        "--content-format", "markdown",
        "--format", "json",
        "--extract-images", "true",
        "--output-dir", str(img_dir),
        "--config-json", json.dumps(cfg, ensure_ascii=False),
    ]


def parse_envelope(stdout_bytes):
    env = json.loads(stdout_bytes.decode("utf-8", errors="replace"))
    result = env.get("result", {})
    return result, {
        "extraction_method": result.get("extraction_method"),
        "counts": result.get("counts", {}) or {},
        "warnings": result.get("processing_warnings", []) or [],
        "languages": result.get("detected_languages", []) or [],
        "quality_score": result.get("quality_score"),
        "time_ms": env.get("extraction_time_ms"),
    }


def write_images_from_json(result, img_dir: Path):
    """JSON 模式不落盘图片（CLI 只在 text/toon 模式写），由 Python 把内联字节数组写成文件。"""
    n = 0
    for im in result.get("images") or []:
        data = im.get("data")
        if not data:
            continue
        fmt = (im.get("format") or "png").lstrip(".")
        (img_dir / f"image_{im.get('image_index', n)}.{fmt}").write_bytes(bytes(data))
        n += 1
    return n


def save_markdown(out_dir: Path, stem: str, md_text: str, img_dirname: str):
    """把 markdown 里的图片引用加上子目录前缀后再保存，保证打开时图片可见。"""
    saved = re.sub(r"(!\[[^\]]*\]\()([^)/]+)\)", rf"\g<1>{img_dirname}/\g<2>)", md_text)
    (out_dir / f"{stem}.md").write_text(saved, encoding="utf-8")


def convert_one(cli: Path, src_file: Path, out_dir: Path, timeout: int, env: dict,
                transcription: bool):
    """转换单个文件（只用本地编译的 CLI，不做任何回退）。返回 (md_text, meta, elapsed, returncode, used_cli)。"""
    stem = src_file.stem
    img_dir = out_dir / f"{stem}_images"
    img_dir.mkdir(parents=True, exist_ok=True)
    label = f"[{stem}]"
    notes = []

    cmd = build_cmd(cli, src_file, img_dir, transcription)
    rc, out, err, elapsed = run_with_ticker(label, cmd, env, timeout)
    (out_dir / f"{stem}.err.txt").write_bytes(err or b"")

    if rc != 0:
        err_text = (err or b"").decode("utf-8", errors="replace")
        if transcription and "unknown field `transcription`" in err_text:
            notes.append("当前 CLI 未启用 transcription feature，音视频必须用本地编译的全功能版测试："
                         "cargo build -p xberg-cli --features all")
        first_err = next((l for l in err_text.splitlines()
                          if re.search(r"ERROR|Error|error \[", l)), err_text.splitlines()[:1])
        return ("", {"warnings": [f"退出码 {rc}: {first_err}"], "counts": {},
                     "languages": [], "notes": notes},
                elapsed, rc, cli)

    result, meta = parse_envelope(out)
    md_text = result.get("content", "") or ""
    n_imgs = write_images_from_json(result, img_dir)
    meta["notes"] = notes + ([f"由 Python 从 JSON 内联数据落盘 {n_imgs} 张图片"] if n_imgs else [])
    save_markdown(out_dir, stem, md_text, img_dir.name)
    return md_text, meta, elapsed, rc, cli


# ---------------------------------------------------------------- 报告
def report_file(name, verdict, m, recall, recall_note, meta, elapsed, issues, used_cli):
    ok = "\u2705" if verdict == "PASS" else ("\u26a0\ufe0f " if verdict == "WARN" else "\u274c")
    emit(f"\n{'='*72}")
    emit(f"{ok} [{verdict}] {name}   ({elapsed:.1f}s)")
    emit(f"  内容: {m['chars']} 字符 | 非空行 {m['nonempty']} | 标题 {m['headings']} | "
         f"表格行 {m['table_rows']} | 图片引用 {m['img_refs']}(落盘 {m['imgs_on_disk']}) | "
         f"代码围栏 {m['fences']}")
    if recall is not None:
        tag = "良好" if recall >= RECALL_GOOD else ("偏低" if recall >= RECALL_FAIL else "差")
        emit(f"  文本召回率: {recall:.1%} ({tag})   [{recall_note}]")
    elif recall_note:
        emit(f"  文本召回率: 跳过   [{recall_note}]")
    method = meta.get("extraction_method")
    counts = {k: v for k, v in (meta.get("counts") or {}).items() if v}
    qs = meta.get("quality_score")
    emit(f"  引擎: 方法={method} | counts={counts} | 语言={meta.get('languages')} | "
         f"质量分={qs if qs is not None else '-'}")
    for note in meta.get("notes") or []:
        emit(f"  说明: {note}")
    warns = meta.get("warnings") or []
    if warns:
        emit(f"  警告({len(warns)}):")
        for w in warns[:5]:
            emit(f"    - {str(w)[:200]}")
    if issues:
        emit(f"  问题: {'; '.join(issues)}")


def print_summary(results, stopped_early, early_reason=""):
    n_pass = sum(1 for r in results if r[1] == "PASS")
    n_warn = sum(1 for r in results if r[1] == "WARN")
    n_fail = sum(1 for r in results if r[1] == "FAIL")
    emit(f"\n\n{'#'*72}")
    emit(f"# 汇总: 共 {len(results)} 个文件 | PASS {n_pass} | WARN {n_warn} | FAIL {n_fail}"
         + (f" | 提前终止: {early_reason}" if stopped_early else ""))
    emit("#" * 72)
    emit(f"{'文件':<42}{'判定':<6}{'耗时(s)':<9}{'召回率':<9}问题")
    for name, verdict, recall, issues, elapsed in results:
        rs = f"{recall:.0%}" if recall is not None and recall >= 0 else "-"
        emit(f"{name:<42}{verdict:<6}{elapsed:<9.1f}{rs:<9}{'; '.join(issues) if issues else ''}")
    bad = [r for r in results if r[1] != "PASS"]
    if bad:
        emit("\n需要关注的文件:")
        for name, verdict, _, issues, _ in bad:
            emit(f"  [{verdict}] {name}: {'; '.join(issues) if issues else '(见上方报告)'}")
    elif not stopped_early:
        emit("\n全部文件通过。")
    return n_fail


# ---------------------------------------------------------------- 主流程
def preflight(cli: Path, env: dict, files: list):
    if not cli.is_file():
        sys.exit(f"找不到 CLI: {cli}")
    try:
        v = subprocess.run([str(cli), "--version"], capture_output=True, env=env, timeout=60)
        if v.returncode != 0:
            sys.exit(f"CLI 无法运行 (--version 退出码 {v.returncode}): {cli}\n{v.stderr.decode('utf-8', 'replace')}")
        print(f"CLI 就绪: {v.stdout.decode('utf-8', 'replace').strip()}  ({cli})")
    except (subprocess.TimeoutExpired, OSError) as e:
        sys.exit(f"CLI 探测失败: {e}")

    # 音视频必须用本地编译版转写；CLI 缺 transcription feature 时在跑任何转换前就终止。
    # 探测用 max_bytes=1：feature 在位时文件立即被大小上限拒绝（快、无模型加载），缺 feature
    # 时配置合并直接报 unknown field。
    if any(f.suffix.lower().lstrip(".") in AV_EXTS for f in files):
        probe = subprocess.run(
            [str(cli), "extract", str(next(f for f in files if f.suffix.lower().lstrip(".") in AV_EXTS)),
             "--no-config-discovery", "--format", "json",
             "--config-json", json.dumps({"transcription": {**TRANSCRIPTION_CFG, "max_bytes": 1}})],
            capture_output=True, env=env, timeout=120)
        if "unknown field `transcription`" in probe.stderr.decode("utf-8", errors="replace"):
            sys.exit("当前 CLI 未启用 transcription feature，无法按约定用本地编译版测试音视频。\n"
                     f"请先执行: cargo build -p xberg-cli --features all\n  CLI: {cli}")
        print("transcription feature 就绪（音视频将用本地编译版转写）")


def main():
    ap = argparse.ArgumentParser(description="xberg Markdown 转换集成测试")
    ap.add_argument("--cli", default=str(DEFAULT_CLI))
    ap.add_argument("--src", default=str(DEFAULT_SRC))
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--pkg-dir", default=str(PKG_DIR),
                    help="提供 models/ 的目录（用于 HF_HOME 与 PATH），默认仓库内打包目录")
    ap.add_argument("--keep-going", action="store_true", help="遇到 FAIL 不终止，继续测完")
    args = ap.parse_args()

    cli = Path(args.cli)
    src = Path(args.src)
    out_dir = Path(args.out)
    if not src.is_dir():
        sys.exit(f"找不到测试目录: {src}")
    out_dir.mkdir(parents=True, exist_ok=True)

    env = os.environ.copy()
    pkg_dir = Path(args.pkg_dir)
    if pkg_dir.is_dir():
        env["PATH"] = str(pkg_dir) + os.pathsep + env.get("PATH", "")
        env["HF_HOME"] = str(pkg_dir / "models")

    files = sorted(p for p in src.iterdir() if p.is_file())
    if not files:
        sys.exit(f"测试目录为空: {src}")
    preflight(cli, env, files)
    print(f"集成测试: {len(files)} 个文件 | 源: {src} | 输出: {out_dir} | "
          f"超时: {args.timeout}s | 遇 FAIL {'继续' if args.keep_going else '立即终止'}")

    results = []  # (name, verdict, recall, issues, elapsed)
    stopped_early, early_reason = False, ""
    t_start = time.time()
    for i, f in enumerate(files, 1):
        print(f"\n[{i}/{len(files)}] {f.name} ({f.stat().st_size/1024:.0f} KB) ...")
        av = f.suffix.lower().lstrip(".") in AV_EXTS
        try:
            md_text, meta, elapsed, rc, used_cli = convert_one(cli, f, out_dir, args.timeout,
                                                               env, transcription=av)
        except subprocess.TimeoutExpired:
            meta = {"warnings": [f"超时(>{args.timeout}s)"], "counts": {}, "languages": [],
                    "notes": []}
            md_text, elapsed, rc = "", float(args.timeout), 124
            m = structural_metrics("", out_dir / f"{f.stem}_images")
            issues = [f"超时(>{args.timeout}s)"]
            results.append((f.name, "FAIL", None, issues, elapsed))
            report_file(f.name, "FAIL", m, None, None, meta, elapsed, issues, cli)
            if not args.keep_going:
                stopped_early, early_reason = True, f"{f.name} 超时"
                break
            continue

        m = structural_metrics(md_text, out_dir / f"{f.stem}_images")
        issues = []
        judge_structure(m, issues)
        judge_meta_images(meta, m, issues)
        if rc != 0:
            issues.append("转换命令失败")

        recall, recall_note = None, None
        if rc == 0:
            src_text, note = extract_source_text(f)
            if src_text is not None:
                recall = bigram_recall(src_text, md_text)
                recall_note = f"基准抽取: {note}"
                if recall >= 0 and recall < RECALL_FAIL:
                    issues.append(f"召回率过低({recall:.0%})")
            elif note:
                recall_note = note

        verdict = "FAIL" if ("转换命令失败" in issues or "转换结果为空" in issues
                             or any("召回率过低" in x for x in issues)) else ("WARN" if issues else "PASS")
        report_file(f.name, verdict, m, recall, recall_note, meta, elapsed, issues, cli)
        results.append((f.name, verdict, recall, issues, elapsed))

        if verdict == "FAIL" and not args.keep_going:
            stopped_early, early_reason = True, f.name
            break

    n_fail = print_summary(results, stopped_early, early_reason)
    emit(f"\n总耗时 {time.time()-t_start:.0f}s")
    header = (f"# xberg 转换质量报告\n\n- CLI: `{cli}`\n- 源目录: `{src}`\n"
              f"- 生成时间: {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
    report_path = out_dir / "_quality-report.md"
    report_path.write_text(header + "\n".join(REPORT_LINES) + "\n", encoding="utf-8")
    print(f"质量报告已保存: {report_path}")
    sys.exit(1 if (n_fail or stopped_early) else 0)


if __name__ == "__main__":
    main()
