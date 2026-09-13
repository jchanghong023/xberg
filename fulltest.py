#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""fulltest.py — xberg 集成验收：把测试目录下所有文件转换为 Markdown 并做可机读质量判定。

目标：不靠人工读正文，也能判断「转换是否可接受」。

三层检查（每层都独立出码）：
  1. 进程/结构 —— CLI 退出码、空结果、乱码/控制字符/格式泄漏、Markdown 围栏与表格列、
     图片引用可解析性、落盘图片文件合法性（大小 + magic bytes）
  2. 源文对齐   —— 基准去页眉页脚后 bigram 召回 + 数字/标识边界匹配 + 正文字符量比值 +
     分页/分片内容覆盖（源页有字而 MD 几乎对不上 → FAIL）
  3. 交叉核对   —— 源页数 vs 引擎 counts vs `## Page N`；源文件媒体清单 vs 引擎图片数 vs
     磁盘落盘 vs MD 引用（四方对账）；音视频文件体积 vs 转写文本量

用法:
    python fulltest.py [--cli PATH] [--src DIR] [--out DIR] [--timeout SECS]
                       [--keep-going] [--strict]
                       [--recall-fail F] [--recall-good F] [--num-recall-fail F]

默认调用 target\\debug\\xberg.exe，输出到 D:\\测试转markdown转换效果\\测试文档_md_fulltest。
每个文件打印质量报告；结束时写 `_quality-report.md` 与 `_quality-report.json`。
默认遇 FAIL 立即终止；`--keep-going` 跑完；`--strict` 把 WARN 也算失败（适合 CI 验收门禁）。
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

# ---------------------------------------------------------------- 默认配置
REPO = Path(r"D:\code1111111111\xberg")
DEFAULT_CLI = REPO / "target" / "debug" / "xberg.exe"
# 打包目录只用来提供模型缓存（HF_HOME）和 DLL 搜索路径；测试永远只用本地编译的 CLI
PKG_DIR = REPO / "xberg-cli-x86_64-pc-windows-msvc"
DEFAULT_SRC = Path(r"D:\测试转markdown转换效果\测试文档")
DEFAULT_OUT = Path(r"D:\测试转markdown转换效果\测试文档_md_fulltest")

AV_EXTS = {"mp4", "wmv", "mov", "mkv", "m4a", "mp3", "wav", "webm", "flv", "avi"}
IMAGE_EXTS = {"png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"}
# 测试语料以中文为主；whisper-tiny 无提示时常见英文幻觉，验收固定 zh
TRANSCRIPTION_CFG = {"enabled": True, "model": "tiny", "language": "zh"}

# 召回率阈值（可用 CLI 覆盖）
RECALL_GOOD = 0.85
RECALL_FAIL = 0.60
NUM_RECALL_FAIL = 0.50

# 正文体量：norm(MD)/norm(源)。MD 允许略少（剥页眉/结构重排），但不能塌缩。
CHAR_RATIO_FAIL = 0.35
CHAR_RATIO_WARN = 0.50
CHAR_RATIO_MIN_SRC = 80          # 源归一后不足此长度不评估
# 源基准清洗：重复页眉/页脚（PDF 整页 get_text 会带进来）
HEADER_MIN_COUNT = 4             # 同一短行出现 >=N 次视为页眉/页脚
HEADER_LINE_MIN = 8
HEADER_LINE_MAX = 90
# 分页内容覆盖：该页归一后字符 bigram 被 MD 命中的比例
PAGE_MIN_NORM_CHARS = 30         # 页内有效字太少不抽查
PAGE_COVER_FAIL = 0.12           # 命中率低于此 → 整页疑似丢失
PAGE_COVER_WARN = 0.35

TICKER_INTERVAL = 5  # 秒

# 音视频：体积与转写量的下限（防空转写 / 半截转写）。
# 短科教片/演示片语音本来就少，密度只作 WARN；仅在几乎无文本时 FAIL。
AV_SPARSE_WARN_CHARS_PER_MB = 80   # < 该值 → WARN
AV_MIN_CHARS_HARD = 40             # 低于此值直接 FAIL

# 图片落盘校验
IMG_MIN_BYTES = 64                 # 小于该字节数视为损坏/占位

# 源页数 vs MD `## Page N` 允许的相对误差
PAGE_CROSS_TOLERANCE = 0.05

# Markdown 语法：连续表格块允许的 pipe 数偏差
TABLE_PIPE_SLACK = 1

# 同时输出到终端和质量报告文件的行（main 结束时写入 <out>/_quality-report.md）
REPORT_LINES: list = []
# 机器可读逐文件结果
JSON_RESULTS: list = []


def emit(line=""):
    print(line, flush=True)
    REPORT_LINES.append(line)

sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)


# ---------------------------------------------------------------- 问题码
# severity: FAIL 阻断验收；WARN 需关注（--strict 时也阻断）
ISSUE_META = {
    "TIMEOUT": "FAIL",
    "CLI_FAIL": "FAIL",
    "EMPTY": "FAIL",
    "TOO_SHORT": "FAIL",
    "ENCODING": "FAIL",
    "BINARY_LEAK": "FAIL",
    "XML_LEAK": "FAIL",
    "MD_FENCE": "FAIL",
    "IMG_MISSING": "FAIL",      # 引用指向不存在的文件
    "IMG_LOST": "FAIL",         # 引用数 > 落盘数（丢图）
    "IMG_CORRUPT": "FAIL",      # 零字节 / 过小 / magic 不匹配
    "IMG_META": "WARN",
    "IMG_SRC_EMPTY": "FAIL",    # 源文件有图，但 MD 无引用且磁盘无落盘
    "IMG_SRC_GAP": "WARN",      # 源媒体数明显多于导出/落盘
    "RECALL_LOW": "FAIL",
    "NUM_RECALL_LOW": "FAIL",
    "RECALL_WEAK": "WARN",
    "CHAR_THIN": "FAIL",        # 正文体量相对源文过低
    "CHAR_THIN_SOFT": "WARN",
    "PAGE_BODY_MISSING": "FAIL",  # 源页有正文，MD 几乎对不上
    "PAGE_BODY_WEAK": "WARN",
    "AV_SPARSE": "FAIL",
    "AV_THIN": "WARN",
    "PAGE_MISMATCH": "WARN",
    "TABLE_COLS": "WARN",
    "DUP_SPAM": "WARN",
    "EMPTY_FLOOD": "WARN",
    "ENGINE_WARN": "WARN",
    "ENGINE_FAILISH": "FAIL",
    "CMD_FAILED": "FAIL",
}


def make_issue(code: str, message: str) -> dict:
    sev = ISSUE_META.get(code, "WARN")
    return {"code": code, "severity": sev, "message": message}


def issues_to_verdict(issues: list) -> str:
    if any(i["severity"] == "FAIL" for i in issues):
        return "FAIL"
    if issues:
        return "WARN"
    return "PASS"


# ---------------------------------------------------------------- 源文本抽取（召回率基准）
def extract_source_text(path: Path):
    """用第三方库从源文件独立抽取文本；返回 (文本, 库名, extras) 或 (None, 原因, {})。

    extras 目前含 pages（PDF 页数 / PPTX 幻灯片数 / XLSX 工作表数），供交叉核对。
    """
    ext = path.suffix.lower().lstrip(".")
    extras = {}
    try:
        if ext == "pdf":
            import pymupdf
            with pymupdf.open(path) as doc:
                extras["pages"] = doc.page_count
                return "\n".join(page.get_text() for page in doc), "pymupdf", extras
        if ext == "docx":
            import docx
            d = docx.Document(str(path))
            parts = [p.text for p in d.paragraphs]
            for t in d.tables:
                for row in t.rows:
                    parts.append(" ".join(c.text for c in row.cells))
            return "\n".join(parts), "python-docx", extras
        if ext == "pptx":
            from pptx import Presentation
            prs = Presentation(str(path))
            extras["pages"] = len(prs.slides)
            parts = []
            for slide in prs.slides:
                for shape in slide.shapes:
                    if shape.has_text_frame:
                        parts.append(shape.text_frame.text)
                    if getattr(shape, "has_table", False):
                        for row in shape.table.rows:
                            parts.append(" ".join(c.text for c in row.cells))
            return "\n".join(parts), "python-pptx", extras
        if ext in ("xlsx", "xls", "ods"):
            from openpyxl import load_workbook
            wb = load_workbook(str(path), read_only=True, data_only=True)
            extras["pages"] = len(wb.sheetnames)
            parts = []
            for ws in wb.worksheets:
                for row in ws.iter_rows(values_only=True):
                    parts.append(" ".join(str(v) for v in row if v is not None))
            return "\n".join(parts), "openpyxl", extras
    except ImportError as e:
        return None, f"缺少依赖({e.name})", extras
    except Exception as e:
        return None, f"源文件解析失败: {e}", extras
    return None, None, extras  # 该格式没有对应的基准抽取器


def count_source_images(path: Path):
    """独立统计源文件内嵌图片数量（不依赖 xberg）。返回 (数量|None, 备注)。"""
    ext = path.suffix.lower().lstrip(".")
    try:
        if ext == "pdf":
            import pymupdf
            with pymupdf.open(path) as doc:
                n = 0
                for page in doc:
                    n += len(page.get_images(full=True))
                return n, "pymupdf.get_images"
        if ext in ("docx", "pptx", "xlsx", "ods"):
            import zipfile
            prefixes = {
                "docx": ("word/media/",),
                "pptx": ("ppt/media/",),
                "xlsx": ("xl/media/",),
                "ods": ("Pictures/", "ObjectReplacements/"),
            }[ext if ext != "ods" else "ods"]
            with zipfile.ZipFile(path) as z:
                n = sum(
                    1 for name in z.namelist()
                    if any(name.startswith(p) for p in prefixes)
                    and not name.endswith("/")
                    and not name.endswith(".bin")
                )
                return n, f"zip:{prefixes[0]}*"
    except Exception as e:
        return None, f"源图片清单失败: {e}"
    return None, None


def page_text_chunks(path: Path):
    """按页/按幻灯片拆出源文，供分页内容抽查。返回 list[str] 或 None。"""
    ext = path.suffix.lower().lstrip(".")
    try:
        if ext == "pdf":
            import pymupdf
            with pymupdf.open(path) as doc:
                return [page.get_text() or "" for page in doc]
        if ext == "pptx":
            from pptx import Presentation
            prs = Presentation(str(path))
            chunks = []
            for slide in prs.slides:
                parts = []
                for shape in slide.shapes:
                    if shape.has_text_frame:
                        parts.append(shape.text_frame.text or "")
                    if getattr(shape, "has_table", False):
                        for row in shape.table.rows:
                            parts.append(" ".join(c.text or "" for c in row.cells))
                chunks.append("\n".join(parts))
            return chunks
    except Exception:
        return None
    return None


def strip_pdf_page_numbers(text: str) -> str:
    """去掉「整行只有数字」的页脚/页码，避免把页码算进数字召回率基准。"""
    return "\n".join(
        line for line in (text or "").splitlines()
        if not re.fullmatch(r"\s*\d{1,4}\s*", line)
    )


# 公认无信息的重复行（分隔线/表格框）
DUP_IGNORE_RE = re.compile(r"^(?:[-=_*|:\s]+|#*\s*Page\s*\d+\s*|#*\s*\d+\s*)$")


def strip_repeated_short_lines(text: str) -> str:
    """去掉源文里反复出现的短行（PDF 页眉/页脚、版式装饰），再进入召回基准。

    只对非表格、长度适中的行做去重；xlsx 合法重复单元格通常更短或进表格路径。
    """
    if not text:
        return text
    lines = text.splitlines()
    counter: Counter = Counter()
    for line in lines:
        raw = line.strip()
        if not raw or raw.startswith("|"):
            continue
        if HEADER_LINE_MIN <= len(raw) <= HEADER_LINE_MAX and not DUP_IGNORE_RE.match(raw):
            # 页码/版本号归一，避免「…, 41」「…, 42」被算成两行
            key = re.sub(r"\d+", "#", raw)
            counter[key] += 1
    drop = {k for k, c in counter.items() if c >= HEADER_MIN_COUNT}
    if not drop:
        return text
    kept = []
    for line in lines:
        raw = line.strip()
        if raw and not raw.startswith("|") and HEADER_LINE_MIN <= len(raw) <= HEADER_LINE_MAX:
            key = re.sub(r"\d+", "#", raw)
            if key in drop:
                continue
        kept.append(line)
    return "\n".join(kept)


def _norm_ws(s: str) -> str:
    return re.sub(r"\s+", "", s or "")


def bigram_recall(source: str, md: str) -> float:
    """源文本字符 bigram 在 markdown 中的覆盖率（0~1）。空源返回 -1 表示无法评估。"""
    src, out = _norm_ws(source), _norm_ws(md)
    if len(src) < 20:
        return -1.0
    grams = {src[i:i + 2] for i in range(len(src) - 1)}
    if not grams:
        return -1.0
    out_set = {out[i:i + 2] for i in range(len(out) - 1)}
    return len(grams & out_set) / len(grams)


def char_volume_ratio(source: str, md: str) -> float | None:
    """norm(MD)/norm(源)；源过短返回 None。"""
    src_n = len(_norm_ws(source))
    if src_n < CHAR_RATIO_MIN_SRC:
        return None
    return len(_norm_ws(md)) / src_n


# 数字 / 短 ID：验收时最关键，漏掉一个关键参数就可能整份文档不可用
NUMBER_TOKEN_RE = re.compile(r"(?<![\w.])(?:\d+\.\d+|\d{2,})(?![\w.])")
# 技术标识（含下划线/连字符的英文短词，长度≥4，避免 a/the/of 这类）
IDENT_TOKEN_RE = re.compile(r"\b[A-Za-z][A-Za-z0-9]*(?:[_-][A-Za-z0-9]+)+\b|\b[A-Za-z]{6,}\b")


def extract_critical_tokens(text: str) -> tuple[set, set]:
    """从源文抽取「数字 token」与「标识 token」。"""
    if not text:
        return set(), set()
    nums = set(NUMBER_TOKEN_RE.findall(text))
    idents = {t for t in IDENT_TOKEN_RE.findall(text) if len(t) >= 4}
    return nums, idents


def _token_boundary_hit(token: str, md: str, out_tokens: set) -> bool:
    """token 必须整词命中，禁止「12」撞进「312」这类子串虚高。"""
    if token in out_tokens:
        return True
    if not md:
        return False
    # 非字母数字边界（含中文/标点/空白/围栏字符）视为整词边界
    pat = re.compile(rf"(?<![0-9A-Za-z_.]){re.escape(token)}(?![0-9A-Za-z_.])")
    return bool(pat.search(md))


def token_recall(source: str, md: str):
    """返回 (数字召回率, 标识召回率, 数字缺失样例, 标识缺失样例)。无源 token 时返回 -1。"""
    src_nums, src_idents = extract_critical_tokens(source)
    out_nums, out_idents = extract_critical_tokens(md)
    if src_nums:
        hit = {t for t in src_nums if _token_boundary_hit(t, md, out_nums)}
        num_r = len(hit) / len(src_nums)
        missing_nums = sorted(src_nums - hit, key=lambda x: (-len(x), x))[:8]
    else:
        num_r, missing_nums = -1.0, []
    if src_idents:
        hit_i = {t for t in src_idents if _token_boundary_hit(t, md, out_idents)}
        id_r = len(hit_i) / len(src_idents)
        missing_id = sorted(src_idents - hit_i, key=lambda x: (-len(x), x))[:8]
    else:
        id_r, missing_id = -1.0, []
    return num_r, id_r, missing_nums, missing_id


# ---------------------------------------------------------------- Markdown / 结构分析
MOJIBAKE_PATTERNS = (
    "\ufffd", "锟斤拷", "烫烫烫", "屯屯屯",
    "â€", "Ã¤", "Ã©", "Ã¼", "â€™", "â€œ",
)
XML_LEAK_RE = re.compile(
    r"(?:<\?xml\b|xmlns:|<w:[a-zA-Z]|<a:t>|<a:t\b|<p:sld\b|<Relationship\b)",
    re.I,
)
BINARY_LEAK_RE = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f]{3,}")
PAGE_TITLE_RE = re.compile(r"^#{1,3}\s*Page\s+(\d+)\s*$", re.I | re.M)


def structural_metrics(md_text: str, img_dir: Path):
    lines = md_text.splitlines()
    chars = len(md_text)
    headings = sum(1 for l in lines if re.match(r"^#{1,6}\s", l))
    table_rows = sum(1 for l in lines if l.lstrip().startswith("|"))
    img_refs = len(re.findall(r"!\[[^\]]*\]\([^)]+\)", md_text))
    fences = sum(1 for l in lines if l.lstrip().startswith("```"))
    nonempty = sum(1 for l in lines if l.strip())
    empty = len(lines) - nonempty
    replacement = md_text.count("\ufffd")
    ctrl = sum(1 for c in md_text if ord(c) < 32 and c not in "\n\r\t")
    img_dir_files = [p for p in img_dir.iterdir() if p.is_file()] if img_dir.is_dir() else []
    img_names = {p.name for p in img_dir_files}
    broken_refs = [ref for ref in re.findall(r"!\[[^\]]*\]\(([^)]+)\)", md_text)
                   if Path(ref).name not in img_names]
    # 重复非平凡行（页眉页脚刷屏）。表行/标题/围栏不算——xlsx 合法重复行很常见。
    line_counts = Counter()
    in_fence = False
    for l in lines:
        raw = l.strip()
        if raw.startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence or not raw or raw.startswith("|") or raw.startswith("#"):
            continue
        if len(raw) >= 12 and not DUP_IGNORE_RE.match(raw):
            # 长句重复多见于交叉引用正文；页眉/页脚通常较短
            if len(raw) <= 120:
                line_counts[raw] += 1
    top_dup = line_counts.most_common(3)
    # 最长连续空行
    max_blank_run = cur = 0
    for l in lines:
        if not l.strip():
            cur += 1
            max_blank_run = max(max_blank_run, cur)
        else:
            cur = 0
    page_titles = [int(x) for x in PAGE_TITLE_RE.findall(md_text)]
    return {
        "chars": chars, "headings": headings, "table_rows": table_rows,
        "img_refs": img_refs, "fences": fences, "nonempty": nonempty, "empty": empty,
        "replacement": replacement, "ctrl": ctrl, "imgs_on_disk": len(img_dir_files),
        "broken_refs": broken_refs,
        "fence_balanced": fences % 2 == 0,
        "max_blank_run": max_blank_run,
        "top_dup": top_dup,
        "page_titles": page_titles,
        "max_page": max(page_titles) if page_titles else 0,
        "img_dir_files": img_dir_files,
    }


def judge_structure(m, issues):
    if m["chars"] == 0:
        issues.append(make_issue("EMPTY", "转换结果为空"))
        return
    if m["chars"] < 20:
        issues.append(make_issue("TOO_SHORT", f"内容过短({m['chars']}字符)"))
    if not m["fence_balanced"]:
        issues.append(make_issue("MD_FENCE", "代码围栏不成对"))
    # 乱码：替换字符密集才算问题。转写/OCR 偶发 1 个 U+FFFD 不足以判 FAIL。
    if m["replacement"] >= 5 or (m["chars"] > 0 and m["replacement"] / m["chars"] > 0.002):
        issues.append(make_issue("ENCODING", f"含 {m['replacement']} 个替换字符(疑似乱码)"))
    if m["ctrl"] > 10:
        issues.append(make_issue("ENCODING", f"含 {m['ctrl']} 个控制字符"))
    # 丢图：引用多于落盘。落盘多于引用不算问题（PPTX 剥重复装饰图引用，数据仍保留）。
    if m["img_refs"] > m["imgs_on_disk"]:
        issues.append(make_issue(
            "IMG_LOST",
            f"图片引用({m['img_refs']})多于导出图片数({m['imgs_on_disk']})"))
    if m["broken_refs"]:
        issues.append(make_issue(
            "IMG_MISSING",
            f"{len(m['broken_refs'])} 个图片引用指向不存在的文件: "
            f"{', '.join(m['broken_refs'][:3])}"))
    # 空行洪水（一页全是空白行 / 分页残留）
    if m["nonempty"] >= 10 and m["empty"] > m["nonempty"] * 3:
        issues.append(make_issue(
            "EMPTY_FLOOD",
            f"空行过多({m['empty']}空/{m['nonempty']}非空)"))
    # 页眉页脚刷屏：同一长行重复很多次
    for text, cnt in m.get("top_dup") or []:
        if cnt >= 8:
            issues.append(make_issue(
                "DUP_SPAM",
                f"行重复 {cnt} 次(疑似页眉/页脚残留): {text[:40]}…"))
            break


def judge_md_integrity(md_text: str, issues):
    """泄漏与编码：直接扫原文。"""
    if not md_text:
        return
    xml_hits = len(XML_LEAK_RE.findall(md_text))
    if xml_hits >= 3:
        issues.append(make_issue(
            "XML_LEAK",
            f"疑似 Office/XML 原始标记泄漏 {xml_hits} 处（如 xmlns/w:t/oleObject）"))
    bin_hits = BINARY_LEAK_RE.findall(md_text)
    if bin_hits:
        issues.append(make_issue(
            "BINARY_LEAK",
            f"含 {len(bin_hits)} 段不可打印二进制/控制序列"))
    moji = sum(md_text.count(p) for p in ("锟斤拷", "烫烫烫", "â€", "Ã¤", "â€™"))
    if moji >= 3:
        issues.append(make_issue("ENCODING", f"含 {moji} 处典型乱码序列"))


def judge_tables(md_text: str, issues):
    """连续 | 开头的块内，pipe 数量应大致一致（允许对齐差 1）。"""
    lines = md_text.splitlines()
    i = 0
    bad_blocks = 0
    while i < len(lines):
        if not lines[i].lstrip().startswith("|"):
            i += 1
            continue
        block = []
        while i < len(lines) and lines[i].lstrip().startswith("|"):
            block.append(lines[i])
            i += 1
        counts = [
            len(re.findall(r"(?<!\\)\|", row)) for row in block
            if not re.match(r"^\s*\|[\s:|-]+\|?\s*$", row)
        ]
        if len(counts) >= 3 and counts:
            lo, hi = min(counts), max(counts)
            if hi - lo > TABLE_PIPE_SLACK:
                bad_blocks += 1
    if bad_blocks:
        issues.append(make_issue(
            "TABLE_COLS",
            f"{bad_blocks} 个表格块列数（| 计数）不一致"))


_MAGIC = {
    "png": [b"\x89PNG\r\n\x1a\n"],
    "jpg": [b"\xff\xd8\xff"],
    "jpeg": [b"\xff\xd8\xff"],
    "gif": [b"GIF87a", b"GIF89a"],
    "bmp": [b"BM"],
}


def judge_image_files(m, issues):
    """落盘图片：过小 / 空文件 / 扩展名与 magic 不符。"""
    corrupt = []
    for p in m.get("img_dir_files") or []:
        try:
            sz = p.stat().st_size
        except OSError:
            corrupt.append(f"{p.name}(stat失败)")
            continue
        if sz == 0:
            corrupt.append(f"{p.name}(0字节)")
            continue
        if sz < IMG_MIN_BYTES:
            corrupt.append(f"{p.name}({sz}B过小)")
            continue
        ext = p.suffix.lower().lstrip(".")
        magics = _MAGIC.get(ext)
        if magics:
            head = p.read_bytes()[:8]
            if not any(head.startswith(mg) for mg in magics):
                corrupt.append(f"{p.name}(magic不符{ext})")
        elif ext == "webp":
            head = p.read_bytes()[:12]
            if len(head) < 12 or head[:4] != b"RIFF" or head[8:12] != b"WEBP":
                corrupt.append(f"{p.name}(magic不符webp)")
    if corrupt:
        issues.append(make_issue(
            "IMG_CORRUPT",
            f"{len(corrupt)} 个落盘图片异常: {', '.join(corrupt[:4])}"))


def judge_meta_images(meta, m, issues):
    """xberg 元数据里的图片数与磁盘导出图片数交叉校验。"""
    imgs_meta = (meta.get("counts") or {}).get("images")
    if imgs_meta and imgs_meta != m["imgs_on_disk"]:
        issues.append(make_issue(
            "IMG_META",
            f"元数据图片数({imgs_meta})与磁盘导出数({m['imgs_on_disk']})不一致"))


def judge_source_images(src_img_count, meta, m, issues):
    """源文件媒体清单 vs 引擎图片数 vs 磁盘 vs MD 引用（四方对账）。

    堵住「引擎丢图且不写引用 → 引用0/落盘0 → 旧检查全过」的漏洞。
    """
    if src_img_count is None or src_img_count <= 0:
        return
    disk = m["imgs_on_disk"]
    refs = m["img_refs"]
    imgs_meta = (meta.get("counts") or {}).get("images") or 0

    # 源里有图，但既无引用也无落盘 → 整类丢失
    if refs == 0 and disk == 0:
        issues.append(make_issue(
            "IMG_SRC_EMPTY",
            f"源文件内嵌图片约 {src_img_count} 个，但 MD 无图片引用且磁盘无落盘"))
        return

    # 源媒体数明显多于导出（去重/装饰图会略少，留 2 张或 30% 容差）
    recovered = max(disk, imgs_meta, refs)
    slack = max(2, int(src_img_count * 0.3))
    if src_img_count - recovered > slack:
        issues.append(make_issue(
            "IMG_SRC_GAP",
            f"源媒体约 {src_img_count} 个，恢复 {recovered} 个"
            f"(磁盘{disk}/元数据{imgs_meta}/引用{refs})，缺口 {src_img_count - recovered}"))


def judge_char_volume(ratio, issues):
    """正文体量：norm(MD)/norm(源) 过低说明可能整段丢失。"""
    if ratio is None:
        return
    if ratio < CHAR_RATIO_FAIL:
        issues.append(make_issue(
            "CHAR_THIN",
            f"正文体量比过低(MD/源={ratio:.2f} < {CHAR_RATIO_FAIL:.2f})"))
    elif ratio < CHAR_RATIO_WARN:
        issues.append(make_issue(
            "CHAR_THIN_SOFT",
            f"正文体量比偏低(MD/源={ratio:.2f} < {CHAR_RATIO_WARN:.2f})"))


def judge_page_body_coverage(chunks, md_text, issues):
    """源页/幻灯片有正文，但 MD 中几乎找不到 → 整页疑似丢失。"""
    if not chunks or len(chunks) < 2:
        return
    missing, weak = [], []
    for i, chunk in enumerate(chunks, 1):
        body = strip_repeated_short_lines(strip_pdf_page_numbers(chunk))
        src_n = len(_norm_ws(body))
        if src_n < PAGE_MIN_NORM_CHARS:
            continue
        cover = bigram_recall(body, md_text)
        if cover < 0:
            continue
        if cover < PAGE_COVER_FAIL:
            missing.append(i)
        elif cover < PAGE_COVER_WARN:
            weak.append(i)
    if missing:
        preview = ",".join(str(x) for x in missing[:8])
        issues.append(make_issue(
            "PAGE_BODY_MISSING",
            f"{len(missing)} 个源页正文在 MD 中几乎无覆盖(页码 {preview}"
            f"{'…' if len(missing) > 8 else ''})"))
    if weak and not missing:
        preview = ",".join(str(x) for x in weak[:8])
        issues.append(make_issue(
            "PAGE_BODY_WEAK",
            f"{len(weak)} 个源页正文覆盖偏低(页码 {preview}"
            f"{'…' if len(weak) > 8 else ''})"))


def judge_page_cross(meta, source_pages, m, issues):
    """源页数 / 引擎 counts.pages / MD `## Page N` 三方核对（都带容差，容忍空白末页等）。"""
    pages_meta = (meta.get("counts") or {}).get("pages")
    md_pages = m.get("max_page") or 0
    pairs = []

    def tol(n):
        return max(2, int(n * PAGE_CROSS_TOLERANCE))

    if source_pages and pages_meta and abs(source_pages - pages_meta) > tol(max(source_pages, pages_meta)):
        pairs.append(f"源{source_pages}页 vs 引擎{pages_meta}")
    if pages_meta and md_pages and abs(pages_meta - md_pages) > tol(pages_meta):
        pairs.append(f"引擎{pages_meta}页 vs MD标题{md_pages}页")
    if source_pages and md_pages and not pages_meta:
        if abs(source_pages - md_pages) > tol(source_pages):
            pairs.append(f"源{source_pages}页 vs MD标题{md_pages}页")
    if pairs:
        issues.append(make_issue("PAGE_MISMATCH", "页数交叉不一致: " + "; ".join(pairs)))


def judge_engine_warnings(meta, issues):
    """把引擎 processing_warnings 归类进 issues（部分只展示、部分升格）。"""
    FAILISH = ("panic", "internal error", "corrupt file", "fatal")
    for w in meta.get("warnings") or []:
        s = str(w).lower()
        if any(k in s for k in FAILISH):
            issues.append(make_issue("ENGINE_FAILISH", f"引擎严重警告: {str(w)[:160]}"))
        # OLE 跳过 / OCR 词典过滤等 → 统一记一条 WARN（去重靠调用方最多一条）
    ole = [w for w in (meta.get("warnings") or [])
           if "ole" in str(w).lower() or "not supported" in str(w).lower()]
    if ole:
        issues.append(make_issue(
            "ENGINE_WARN",
            f"引擎跳过 {len(ole)} 个不支持的嵌入对象/子资源"))


def judge_av_sparsity(src_file: Path, m, issues):
    """音视频：几乎无转写文本才 FAIL。

    视频体积主要来自画面码率，用 chars/MB 衡量语音量对 mp4/mov 等不公平；
    仅对纯音频（体积≈语音）做密度 WARN，视频只看绝对下限。
    """
    ext = src_file.suffix.lower().lstrip(".")
    if ext not in AV_EXTS:
        return
    try:
        mb = src_file.stat().st_size / (1024 * 1024)
    except OSError:
        return
    chars = m["chars"]
    if chars < AV_MIN_CHARS_HARD:
        issues.append(make_issue("AV_SPARSE", f"音视频转写过少({chars}字符, {mb:.1f}MB)"))
        return
    audio_only = ext in {"mp3", "wav", "m4a", "flac", "ogg", "wma"}
    if audio_only:
        density = chars / max(mb, 0.01)
        if density < AV_SPARSE_WARN_CHARS_PER_MB:
            issues.append(make_issue(
                "AV_THIN",
                f"音频转写偏少({chars}字符/{mb:.1f}MB ≈ {density:.1f} 字符/MB)"))


# ---------------------------------------------------------------- 子进程执行（带进度）
def run_with_ticker(label, cmd, env, timeout):
    """运行子进程：每 5s 刷新；每 15s 打一行真实换行进度，兼容不显示 \\r 的执行环境。"""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    t0 = time.time()
    tick = 0
    while True:
        try:
            out, err = proc.communicate(timeout=TICKER_INTERVAL)
            break
        except subprocess.TimeoutExpired:
            tick += 1
            elapsed = int(time.time() - t0)
            if tick % 3 == 0:
                # 每 ~15s 落一行，后台/管道环境也能看到
                print(f"  {label} 仍在转换... 已 {elapsed}s", flush=True)
            else:
                print(f"\r  {label} 运行中... {elapsed}s", end="", flush=True)
            if timeout and time.time() - t0 > timeout:
                proc.kill()
                out, err = proc.communicate()
                print(f"\n  {label} 超时(>{timeout}s)，已终止", flush=True)
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
                         "cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,transcription")
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
def _fmt_issues(issues):
    return "; ".join(f"[{i['code']}] {i['message']}" for i in issues)


def report_file(name, verdict, m, recall_info, meta, elapsed, issues, used_cli):
    ok = "✅" if verdict == "PASS" else ("⚠️ " if verdict == "WARN" else "❌")
    emit(f"\n{'='*72}")
    emit(f"{ok} [{verdict}] {name}   ({elapsed:.1f}s)")
    emit(f"  内容: {m['chars']} 字符 | 非空行 {m['nonempty']} | 标题 {m['headings']} | "
         f"表格行 {m['table_rows']} | 图片引用 {m['img_refs']}(落盘 {m['imgs_on_disk']}) | "
         f"代码围栏 {m['fences']}")
    if m.get("max_page"):
        emit(f"  MD页标题: {len(m.get('page_titles') or [])} 个 | 最大 Page {m['max_page']}")
    ri = recall_info or {}
    bigram = ri.get("bigram")
    if bigram is not None and bigram >= 0:
        tag = "良好" if bigram >= ri.get("recall_good", RECALL_GOOD) else (
            "偏低" if bigram >= ri.get("recall_fail", RECALL_FAIL) else "差")
        emit(f"  文本召回率: {bigram:.1%} ({tag})   [{ri.get('note') or ''}]")
    elif ri.get("note"):
        emit(f"  文本召回率: 跳过   [{ri.get('note')}]")
    num_r, id_r = ri.get("num_recall"), ri.get("ident_recall")
    if num_r is not None and num_r >= 0:
        tag = "良好" if num_r >= 0.85 else ("偏低" if num_r >= NUM_RECALL_FAIL else "差")
        emit(f"  数字召回率: {num_r:.1%} ({tag})"
             + (f" | 缺失样例: {', '.join(ri.get('missing_nums') or [])}"
                if ri.get("missing_nums") and num_r < 0.85 else ""))
    if id_r is not None and id_r >= 0:
        emit(f"  标识召回率: {id_r:.1%}")
    cr = ri.get("char_ratio")
    if cr is not None:
        emit(f"  正文体量比(MD/源): {cr:.2f}")
    if ri.get("src_images") is not None:
        emit(f"  源内嵌图片数(独立统计): {ri['src_images']}")
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
        emit(f"  问题: {_fmt_issues(issues)}")


def print_summary(results, stopped_early, early_reason="", strict=False):
    n_pass = sum(1 for r in results if r["verdict"] == "PASS")
    n_warn = sum(1 for r in results if r["verdict"] == "WARN")
    n_fail = sum(1 for r in results if r["verdict"] == "FAIL")
    emit(f"\n\n{'#'*72}")
    emit(f"# 汇总: 共 {len(results)} 个文件 | PASS {n_pass} | WARN {n_warn} | FAIL {n_fail}"
         + (" | STRICT(WARN=FAIL)" if strict else "")
         + (f" | 提前终止: {early_reason}" if stopped_early else ""))
    emit("#" * 72)
    emit(f"{'文件':<42}{'判定':<6}{'耗时(s)':<9}{'召回':<8}{'数字召回':<9}问题")
    for r in results:
        bigram = r.get("recall")
        num_r = r.get("num_recall")
        bs = f"{bigram:.0%}" if bigram is not None and bigram >= 0 else "-"
        ns = f"{num_r:.0%}" if num_r is not None and num_r >= 0 else "-"
        emit(f"{r['name']:<42}{r['verdict']:<6}{r['elapsed']:<9.1f}{bs:<8}{ns:<9}"
             f"{_fmt_issues(r['issues']) if r['issues'] else ''}")
    bad = [r for r in results if r["verdict"] != "PASS"]
    if bad:
        emit("\n需要关注的文件:")
        for r in bad:
            emit(f"  [{r['verdict']}] {r['name']}: "
                 f"{_fmt_issues(r['issues']) if r['issues'] else '(见上方报告)'}")
        # 按问题码聚类，方便一次看清验收阻断点
        code_counts = Counter(i["code"] for r in results for i in r["issues"])
        if code_counts:
            emit("\n问题码分布:")
            for code, n in code_counts.most_common():
                sev = ISSUE_META.get(code, "WARN")
                emit(f"  {sev:<4} {code:<16} ×{n}")
    elif not stopped_early:
        emit("\n全部文件通过。")
    # 验收结论（严格模式下 WARN 也挡）
    blocking = n_fail + (n_warn if strict else 0)
    emit(f"\n验收结论: {'不通过' if (blocking or stopped_early) else '通过'}"
         f"（FAIL={n_fail} WARN={n_warn}{'，strict 下 WARN 计失败' if strict and n_warn else ''}）")
    return n_fail, n_warn


# ---------------------------------------------------------------- 主流程
def preflight(cli: Path, env: dict, files: list):
    if not cli.is_file():
        sys.exit(f"找不到 CLI: {cli}")
    print(f"[preflight] 探测 CLI 版本: {cli} ...", flush=True)
    try:
        v = subprocess.run([str(cli), "--version"], capture_output=True, env=env, timeout=60)
        if v.returncode != 0:
            sys.exit(f"CLI 无法运行 (--version 退出码 {v.returncode}): {cli}\n{v.stderr.decode('utf-8', 'replace')}")
        print(f"[preflight] CLI 就绪: {v.stdout.decode('utf-8', 'replace').strip()}", flush=True)
    except (subprocess.TimeoutExpired, OSError) as e:
        sys.exit(f"CLI 探测失败: {e}")

    # 音视频必须用本地编译版转写；CLI 缺 transcription feature 时在跑任何转换前就终止。
    # 探测用 max_bytes=1：feature 在位时文件立即被大小上限拒绝（快、无模型加载），缺 feature
    # 时配置合并直接报 unknown field。
    av_files = [f for f in files if f.suffix.lower().lstrip(".") in AV_EXTS]
    if av_files:
        print(f"[preflight] 检测到 {len(av_files)} 个音视频文件，探测 transcription feature "
              f"({av_files[0].name}) ...", flush=True)
        probe = subprocess.run(
            [str(cli), "extract", str(av_files[0]),
             "--no-config-discovery", "--format", "json",
             "--config-json", json.dumps({"transcription": {**TRANSCRIPTION_CFG, "max_bytes": 1}})],
            capture_output=True, env=env, timeout=120)
        if "unknown field `transcription`" in probe.stderr.decode("utf-8", errors="replace"):
            sys.exit("当前 CLI 未启用 transcription feature，无法按约定用本地编译版测试音视频。\n"
                     "请先执行: cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,transcription\n"
                     f"  CLI: {cli}")
        print("[preflight] transcription feature 就绪（音视频将用本地编译版转写）", flush=True)
    else:
        print("[preflight] 无音视频文件，跳过 transcription 探测", flush=True)


def main():
    global RECALL_GOOD, RECALL_FAIL, NUM_RECALL_FAIL

    ap = argparse.ArgumentParser(description="xberg Markdown 转换集成验收")
    ap.add_argument("--cli", default=str(DEFAULT_CLI))
    ap.add_argument("--src", default=str(DEFAULT_SRC))
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--pkg-dir", default=str(PKG_DIR),
                    help="提供 models/ 的目录（用于 HF_HOME 与 PATH），默认仓库内打包目录")
    ap.add_argument("--keep-going", action="store_true", help="遇到 FAIL 不终止，继续测完")
    ap.add_argument("--strict", action="store_true",
                    help="WARN 也视为验收失败（CI 门禁用）")
    ap.add_argument("--recall-fail", type=float, default=RECALL_FAIL,
                    help=f"bigram 召回率 FAIL 阈值（默认 {RECALL_FAIL}）")
    ap.add_argument("--recall-good", type=float, default=RECALL_GOOD,
                    help=f"bigram 召回率「良好」阈值（默认 {RECALL_GOOD}）")
    ap.add_argument("--num-recall-fail", type=float, default=NUM_RECALL_FAIL,
                    help=f"数字 token 召回率 FAIL 阈值（默认 {NUM_RECALL_FAIL}）")
    args = ap.parse_args()
    RECALL_FAIL = args.recall_fail
    RECALL_GOOD = args.recall_good
    NUM_RECALL_FAIL = args.num_recall_fail

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
    n_av = sum(1 for f in files if f.suffix.lower().lstrip(".") in AV_EXTS)
    total_kb = sum(f.stat().st_size for f in files) / 1024
    print(f"[preflight] 队列: {len(files)} 个文件（音视频 {n_av}），合计 {total_kb/1024:.1f} MB", flush=True)
    for j, f in enumerate(files[:5], 1):
        tag = " [AV]" if f.suffix.lower().lstrip(".") in AV_EXTS else ""
        print(f"  {j}. {f.name} ({f.stat().st_size/1024:.0f} KB){tag}", flush=True)
    if len(files) > 5:
        print(f"  ... 其余 {len(files) - 5} 个略", flush=True)

    preflight(cli, env, files)
    print(f"集成测试: {len(files)} 个文件 | 源: {src} | 输出: {out_dir} | "
          f"超时: {args.timeout}s | 遇 FAIL {'继续' if args.keep_going else '立即终止'} | "
          f"strict={args.strict}", flush=True)

    results = []
    stopped_early, early_reason = False, ""
    t_start = time.time()
    n = len(files)

    def progress_line(i, verdict, elapsed, t_start):
        done = time.time() - t_start
        avg = done / i
        remain = avg * (n - i)
        mark = {"PASS": "ok", "WARN": "warn", "FAIL": "FAIL", "TIMEOUT": "TIMEOUT"}.get(verdict, verdict)
        print(f"→ 进度 {i}/{n} | {mark} | 本文件 {elapsed:.1f}s | "
              f"累计 {done:.0f}s | 均速 {avg:.1f}s/文件 | 预计剩余 ~{remain/60:.1f} 分钟",
              flush=True)

    for i, f in enumerate(files, 1):
        av = f.suffix.lower().lstrip(".") in AV_EXTS
        print(f"\n[{i}/{n}] {f.name} ({f.stat().st_size/1024:.0f} KB){' [音视频转写]' if av else ''} ...",
              flush=True)
        img_dir = out_dir / f"{f.stem}_images"
        try:
            md_text, meta, elapsed, rc, used_cli = convert_one(cli, f, out_dir, args.timeout,
                                                               env, transcription=av)
        except subprocess.TimeoutExpired:
            meta = {"warnings": [f"超时(>{args.timeout}s)"], "counts": {}, "languages": [],
                    "notes": []}
            md_text, elapsed, rc = "", float(args.timeout), 124
            m = structural_metrics("", img_dir)
            issues = [make_issue("TIMEOUT", f"超时(>{args.timeout}s)")]
            verdict = "FAIL"
            report_file(f.name, verdict, m, None, meta, elapsed, issues, cli)
            rec = {
                "name": f.name, "verdict": verdict, "elapsed": elapsed,
                "recall": None, "num_recall": None, "issues": issues,
                "chars": 0, "method": None, "counts": {},
            }
            results.append(rec)
            JSON_RESULTS.append({
                "name": f.name, "verdict": verdict, "elapsed_s": round(elapsed, 2),
                "recall": None, "num_recall": None, "ident_recall": None,
                "missing_numbers": [], "missing_idents": [],
                "metrics": {"chars": 0}, "issues": issues,
                "warnings": meta.get("warnings") or [], "notes": meta.get("notes") or [],
            })
            progress_line(i, "TIMEOUT", elapsed, t_start)
            if not args.keep_going:
                stopped_early, early_reason = True, f"{f.name} 超时"
                break
            continue

        m = structural_metrics(md_text, img_dir)
        issues = []
        if rc != 0:
            issues.append(make_issue("CMD_FAILED", "转换命令失败"))
            for w in (meta.get("warnings") or [])[:2]:
                issues.append(make_issue("CLI_FAIL", str(w)[:160]))
        else:
            judge_structure(m, issues)
            judge_md_integrity(md_text, issues)
            judge_tables(md_text, issues)
            judge_image_files(m, issues)
            judge_meta_images(meta, m, issues)
            judge_engine_warnings(meta, issues)
            judge_av_sparsity(f, m, issues)

        recall_info = {"note": None, "bigram": None, "num_recall": None,
                       "ident_recall": None, "missing_nums": [], "missing_id": [],
                       "char_ratio": None, "src_images": None,
                       "recall_good": RECALL_GOOD, "recall_fail": RECALL_FAIL}
        source_pages = None
        if rc == 0:
            print("  抽取源文本基准并计算召回率 ...", flush=True)
            src_text, note, extras = extract_source_text(f)
            source_pages = extras.get("pages") if extras else None
            src_img_n, src_img_note = count_source_images(f)
            recall_info["src_images"] = src_img_n
            if src_img_note and src_img_n is None:
                # 探测失败只记录，不阻断
                pass
            if src_text is not None:
                # 去掉整页 PDF 里反复出现的页眉/页脚，再进入召回与体量评估
                src_clean = strip_repeated_short_lines(src_text)
                src_for_tokens = strip_pdf_page_numbers(src_clean)
                bigram = bigram_recall(src_clean, md_text)
                num_r, id_r, miss_n, miss_i = token_recall(src_for_tokens, md_text)
                char_ratio = char_volume_ratio(src_clean, md_text)
                recall_info.update({
                    "bigram": bigram, "num_recall": num_r, "ident_recall": id_r,
                    "missing_nums": miss_n, "missing_id": miss_i,
                    "char_ratio": char_ratio,
                    "note": f"基准抽取: {note}" + (f"；已剥重复短行" if src_clean != src_text else ""),
                })
                if bigram >= 0 and bigram < RECALL_FAIL:
                    issues.append(make_issue(
                        "RECALL_LOW", f"召回率过低({bigram:.0%} < {RECALL_FAIL:.0%})"))
                elif bigram >= 0 and bigram < RECALL_GOOD:
                    issues.append(make_issue(
                        "RECALL_WEAK", f"召回率偏低({bigram:.0%} < {RECALL_GOOD:.0%})"))
                if num_r >= 0 and num_r < NUM_RECALL_FAIL:
                    sample = f" | 缺失: {', '.join(miss_n[:5])}" if miss_n else ""
                    issues.append(make_issue(
                        "NUM_RECALL_LOW",
                        f"数字召回率过低({num_r:.0%} < {NUM_RECALL_FAIL:.0%}){sample}"))
                judge_char_volume(char_ratio, issues)
            elif note:
                recall_info["note"] = note

            judge_source_images(src_img_n, meta, m, issues)

            # PDF/PPTX：按页/按幻灯片抽查「源页有字、MD 对不上」
            if f.suffix.lower() in (".pdf", ".pptx"):
                chunks = page_text_chunks(f)
                if chunks:
                    judge_page_body_coverage(chunks, md_text, issues)

            if source_pages:
                judge_page_cross(meta, source_pages, m, issues)

        verdict = issues_to_verdict(issues)
        report_file(f.name, verdict, m, recall_info, meta, elapsed, issues, used_cli)
        results.append({
            "name": f.name, "verdict": verdict, "elapsed": elapsed,
            "recall": recall_info.get("bigram"),
            "num_recall": recall_info.get("num_recall"),
            "issues": issues,
            "chars": m["chars"],
            "method": meta.get("extraction_method"),
            "counts": meta.get("counts") or {},
        })
        JSON_RESULTS.append({
            "name": f.name, "verdict": verdict, "elapsed_s": round(elapsed, 2),
            "recall": recall_info.get("bigram"),
            "num_recall": recall_info.get("num_recall"),
            "ident_recall": recall_info.get("ident_recall"),
            "missing_numbers": recall_info.get("missing_nums") or [],
            "missing_idents": recall_info.get("missing_id") or [],
            "char_ratio": recall_info.get("char_ratio"),
            "src_images": recall_info.get("src_images"),
            "source_pages": source_pages,
            "metrics": {
                "chars": m["chars"], "nonempty": m["nonempty"],
                "headings": m["headings"], "table_rows": m["table_rows"],
                "img_refs": m["img_refs"], "imgs_on_disk": m["imgs_on_disk"],
                "page_titles": len(m.get("page_titles") or []),
                "max_page": m.get("max_page") or 0,
            },
            "counts": meta.get("counts") or {},
            "extraction_method": meta.get("extraction_method"),
            "issues": issues,
            "warnings": meta.get("warnings") or [],
            "notes": meta.get("notes") or [],
        })
        progress_line(i, verdict, elapsed, t_start)

        blocking = verdict == "FAIL" or (args.strict and verdict == "WARN")
        if blocking and not args.keep_going:
            stopped_early, early_reason = True, f.name
            break

    n_fail, n_warn = print_summary(results, stopped_early, early_reason, strict=args.strict)
    emit(f"\n总耗时 {time.time()-t_start:.0f}s")
    header = (f"# xberg 转换质量报告\n\n- CLI: `{cli}`\n- 源目录: `{src}`\n"
              f"- 生成时间: {time.strftime('%Y-%m-%d %H:%M:%S')}\n"
              f"- 阈值: recall_fail={RECALL_FAIL} recall_good={RECALL_GOOD} "
              f"num_recall_fail={NUM_RECALL_FAIL} char_ratio={CHAR_RATIO_FAIL}/{CHAR_RATIO_WARN} "
              f"page_cover={PAGE_COVER_FAIL}/{PAGE_COVER_WARN} strict={args.strict}\n")
    report_path = out_dir / "_quality-report.md"
    report_path.write_text(header + "\n".join(REPORT_LINES) + "\n", encoding="utf-8")
    json_path = out_dir / "_quality-report.json"
    json_path.write_text(json.dumps({
        "cli": str(cli),
        "src": str(src),
        "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "thresholds": {
            "recall_fail": RECALL_FAIL, "recall_good": RECALL_GOOD,
            "num_recall_fail": NUM_RECALL_FAIL, "strict": args.strict,
            "char_ratio_fail": CHAR_RATIO_FAIL, "char_ratio_warn": CHAR_RATIO_WARN,
            "page_cover_fail": PAGE_COVER_FAIL, "page_cover_warn": PAGE_COVER_WARN,
        },
        "totals": {
            "files": len(results),
            "pass": sum(1 for r in results if r["verdict"] == "PASS"),
            "warn": n_warn,
            "fail": n_fail,
            "stopped_early": stopped_early,
            "early_reason": early_reason,
        },
        "files": JSON_RESULTS,
    }, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"质量报告已保存: {report_path}")
    print(f"JSON 报告已保存: {json_path}")

    blocking = n_fail + (n_warn if args.strict else 0)
    sys.exit(1 if (blocking or stopped_early) else 0)


if __name__ == "__main__":
    main()
