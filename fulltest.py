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
import shutil
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

# ---------------------------------------------------------------- 默认配置
REPO = Path(r"D:\code1111111111\xberg")
DEFAULT_CLI = REPO / "target" / "debug" / "xberg.exe"
# 打包目录只用来提供模型缓存（HF_HUB_CACHE）和 DLL 搜索路径；测试永远只用本地编译的 CLI
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
    # 深检（Markdown 语义 / 源文保真 / 金标准）
    "PSEUDO_HEADING": "FAIL",        # 标题行是内嵌对象文件名（## oleObject1.bin / ## Microsoft_Visio___.vsdx …）
    "GIANT_LINE": "FAIL",            # 单行 > GIANT_LINE_FAIL（内嵌对象被压成一行）
    "GIANT_LINE_SOFT": "WARN",       # 单行 > GIANT_LINE_WARN
    "ESCAPE_NOISE": "WARN",          # \# \. \< 与纯 "\-" 行等转义/实体噪声
    "FENCED_TABLE": "WARN",          # 完整表格被包进代码围栏
    "TABLE_CELL_MERGED": "WARN",     # 真值表值被合并进单个单元格
    "TABLE_HEADER_LONG": "WARN",     # 表头单元格是长备注/横幅
    "TABLE_COUNT_GAP": "WARN",       # MD 表格块数明显少于源表格数
    "GLYPH_SUBST": "FAIL",           # '#' 被解码成 'fi'（#include → fiinclude）
    "IDENT_FRAGMENTED": "WARN",      # 标识符被空格拆断（Implements → Impl ements）
    "OCR_CJK_LOW": "FAIL",           # 中文图片的 OCR 输出没有中文
    "OCR_BACKEND_MISMATCH": "FAIL",  # 生效的 OCR 后端/语言不支持中文
    "GOLDEN_TOKEN_MISSING": "FAIL",  # 期望文本未出现在 MD（源自期望文件）
    "GOLDEN_FORBIDDEN": "FAIL",      # 期望禁止的形态出现（源自期望文件）
    "EMBED_TEXT_LOSS": "WARN",       # 内嵌子文档文本召回不足
    "EMBED_MEDIA_LOST": "WARN",      # 内嵌子文档的图片未落盘
    "NOTES_MISSING": "WARN",         # PPTX 演讲者备注未输出
    "XLSX_SHAPE_TEXT_MISSING": "FAIL",  # xlsx 浮动图形文本丢失
    "PPTX_TITLE_LOST": "WARN",       # 幻灯片标题占位符文本不是标题
    "PAGE_FOOTER_BODY": "WARN",      # 幻灯片页码占位符进入正文
    "LIST_NUMBERING": "WARN",        # 有序列表编号断档（1-2-1）
    "DUP_CONTENT": "WARN",           # 同一内容以围栏块 + 正文两份出现
    "CAPTION_IN_TABLE": "WARN",      # 图表题注被粘进表格行
    "TOC_HEADING_GAP": "WARN",       # PDF 书签条目未以标题形式出现
    "GOLDEN_METRIC": "WARN",         # 逐文件指标越界（标题数/表数/字符数/…）
    "GOLDEN_ORDER": "WARN",          # 期望顺序被违反
    "CODE_AS_TABLE": "WARN",         # Verilog/代码被识别成 Markdown 表格
    "TOC_LEVEL_MISMATCH": "WARN",    # 标题层级与 PDF 书签层级不符
    "EMPHASIS_ODD": "WARN",          # ** 不成对（字面残留）
    "NONSTD_HIGHLIGHT": "WARN",      # 非标准 ==高亮== 标记
    "TABLE_CELL_FOLDED": "WARN",     # xlsx 单元格内部换行被折叠
    "RUNNING_HEAD": "WARN",          # 书眉/运行标题残留为纯加粗短行
    "BULLET_LEVEL_FLAT": "WARN",     # 源有层级项目符号但 MD 无缩进列表
}


def make_issue(code: str, message: str, severity: str | None = None) -> dict:
    sev = severity or ISSUE_META.get(code, "WARN")
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

# ---------------------------------------------------------------- 深检阈值（Markdown 语义 / 源文保真 / 金标准）
# 逐文件金标准与回归基线都在仓库外（内部文档派生，禁止入库）
EXPECTATIONS_DEFAULT = Path(r"D:\测试转markdown转换效果\_expectations.json")
BASELINE_DEFAULT = Path(r"D:\测试转markdown转换效果\_quality-baseline.json")

GIANT_LINE_FAIL = 10000          # 单行字符数 > 此值 → FAIL（音视频文件跳过本项：转写天然是单行）
GIANT_LINE_WARN = 2500           # > 此值 → WARN
ESCAPE_NOISE_MIN = 5             # 非围栏行里 \# \. \< 合计 + 纯 "\-" 行数 + \| 数 ≥ 此值 → WARN
EMPHASIS_ODD_MIN = 3             # 非围栏行中 ** 计数为奇数且无成对粗体的行数 ≥ 此值 → WARN
HIGHLIGHT_MIN = 5                # 非围栏 `==…==` 对数 ≥ 此值 → WARN
CODE_TABLE_MIN_ROWS = 3          # 表格块里含 Verilog/代码特征的行 ≥ 此值 → WARN
CODE_TABLE_RATIO = 0.5           # 且占该表数据行比例 ≥ 此值
TOC_LEVEL_MISMATCH_RATIO = 0.3   # PDF 书签标题的 # 层数与书签层级不符比例 ≥ 此值 → WARN
TABLE_HEADER_MAX_CHARS = 60      # 表头单元格字符数 > 此值 → WARN（备注横幅被当表头）
FENCED_TABLE_MIN_ROWS = 3        # 围栏内以 | 开头的行 ≥ 此值 → 该围栏被判为「表格被包进代码块」
MERGED_CELL_RE = re.compile(r"^[01XZxzlhLH]{3,}$")   # 真值表值被合并的形态（001 / 11X1）
TRUTH_TABLE_SYMBOL_RATIO = 0.15  # 表块里单符号格(0/1/X/Z/L/H)占比下限：低于此不算真值表式表格，
                                 # 避免把普通数字格（如 area_count=1000）当成合并值
DUP_BLOCK_MIN_CHARS = 200        # 参与重复检测的围栏块最小字符数
DUP_BLOCK_CONTAINMENT = 0.6      # 围栏块 8-gram 在正文（围栏外）的命中率 ≥ 此值 → 重复
FRAGMENTED_IDENT_MIN = 1         # 仅以「被空格拆断」形态出现的源标识符 ≥ 此值 → WARN（精确字符序列匹配）
XLSX_SHAPE_LOST_RATIO = 0.5      # 浮动图形文本丢失比例 ≥ 此值 → FAIL（否则 WARN）
TOC_HEADING_MIN_RECALL = 0.9     # PDF 书签（层级 ≤2、有页码）标题需 ≥ 此比例以标题形式出现
LIST_NUMBERING_MAX_GAP = 1       # 有序列表编号允许的最大断档
EMBED_LOSS_PARA_MIN = 3          # 内嵌子文档缺失段落数 ≥ 此值
EMBED_LOSS_RATIO = 0.05          # 且占其段落数比例 ≥ 此值（或 bigram < 0.95）→ EMBED_TEXT_LOSS
OCR_CJK_BACKENDS = {"paddle-ocr", "paddleocr", "sceptre"}   # 支持中文的 OCR 后端
# 判定输入取自金标准的码（逐条核对过出码分支）：未加载金标准时这些码不产生、
# 或换用基准阈值/默认值，与「加载了金标准」的基线不可比，回归对比必须两侧剔除。
# 维护约定：新增「是否出码依赖 exp」的检查时，必须把它的码登记到这里。
EXP_GATED_CODES = {
    "GOLDEN_TOKEN_MISSING", "GOLDEN_FORBIDDEN", "GOLDEN_METRIC", "GOLDEN_ORDER",
    "OCR_CJK_LOW", "OCR_BACKEND_MISMATCH", "BULLET_LEVEL_FLAT", "TOC_HEADING_GAP",
    "PSEUDO_HEADING", "TABLE_CELL_MERGED", "PAGE_FOOTER_BODY", "CODE_AS_TABLE",
    "NONSTD_HIGHLIGHT", "RUNNING_HEAD", "TABLE_CELL_FOLDED", "ESCAPE_NOISE",
}

# 深检正则
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")   # 引擎日志带 ANSI 颜色码，正则前必须剥掉
PSEUDO_HEADING_RE = re.compile(
    r"^#{1,6}\s+\S*\.(?:bin|docx?|xlsx?|pptx?|vsdx?|vsd|eddx|emf|wmf|msg|rtf)\s*$", re.I)
PAGE_FOOTER_FULL_RE = re.compile(r"\*{0,2}Page\s+\d+\*{0,2}", re.I)
BOLD_SHORT_RE = re.compile(r"^\*\*[^*]{6,60}\*\*$")
BOLD_PAIR_RE = re.compile(r"\*\*[^*\n]+\*\*")
ESCAPE_TOKEN_RE = re.compile(r"\\[#.]")   # \# \. 才计噪声；\< 在表格/正文里是正常转义写法
DASH_ONLY_RE = re.compile(r"^(?:\\-)+$")
ESCAPED_PIPE_RE = re.compile(r"\\\|")
ESCAPED_FENCE_RE = re.compile(r"\\`{3}")   # \`\`\` = 被压平的内嵌子文档把自身围栏当字面量带出
HIGHLIGHT_PAIR_RE = re.compile(r"==[^=\n]{1,60}==")
# 伪表格里的代码特征：Verilog/UDP/模型定义（实测手册的代码被切成两列表格）
CODE_TABLE_RE = re.compile(
    r"endmodule|always\s*@|\bassign\b|\bmodule\b|\bwire\b|\breg\b|\bparameter\b|"
    r"\bdefparam\b|model_source|verilog|(?:^|\|)\s*(?:input|output)\b")
CAPTION_IN_TABLE_RE = re.compile(r"(?:Table|Figure)\s+\d+[-–]\d+\.")
ORDERED_ITEM_RE = re.compile(r"^(\s*)(\d+)[.、．](?!\d)\s*\S")
TABLE_SEP_RE = re.compile(r"^\s*\|[\s:|-]+\|?\s*$")

# 全局：逐文件金标准与覆盖配置（main 按 CLI 参数加载；默认空 = 行为不变）
EXPECTATIONS: dict = {}
OCR_CONFIG: dict = {}
# None = 不给 CLI 传 layout；{} = 按引擎默认注入 layout（需 layout-detection 构建）
LAYOUT_CONFIG = None


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
    # 深检度量：表格块 / 超长行 / 中文量 / 伪标题 / 页码残留 / 纯加粗短行
    outside_lines, fenced_blocks = split_fenced(md_text)
    tables = sum(1 for _start, rows in _table_blocks(lines)
                 if any(TABLE_SEP_RE.match(x) for x in rows))
    max_line_len = max((len(l) for l in lines), default=0)
    cjk = len(re.findall(r"[\u4e00-\u9fff]", md_text))
    fenced_cjk = sum(len(re.findall(r"[\u4e00-\u9fff]", "\n".join(b)))
                     for _, b in fenced_blocks)
    pseudo_headings = sum(1 for l in outside_lines if PSEUDO_HEADING_RE.match(l.strip()))
    page_footer_body = sum(1 for l in outside_lines
                           if PAGE_FOOTER_FULL_RE.fullmatch(l.strip()))
    bold_short_lines = sum(1 for l in outside_lines if BOLD_SHORT_RE.match(l.strip()))
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
        "tables": tables,
        "max_line_len": max_line_len,
        "cjk": cjk,
        "fenced_cjk": fenced_cjk,
        "pseudo_headings": pseudo_headings,
        "page_footer_body": page_footer_body,
        "bold_short_lines": bold_short_lines,
    }


def json_metrics(m) -> dict:
    """`_quality-report.json` 逐文件结构指标。

    正常记录与超时记录共用同一组键，按统一 schema 读取的消费方不必区分两者。
    """
    return {
        "chars": m["chars"], "nonempty": m["nonempty"],
        "headings": m["headings"], "table_rows": m["table_rows"],
        "img_refs": m["img_refs"], "imgs_on_disk": m["imgs_on_disk"],
        "page_titles": len(m.get("page_titles") or []),
        "max_page": m.get("max_page") or 0,
        "tables": m.get("tables") or 0,
        "max_line_len": m.get("max_line_len") or 0,
        "cjk": m.get("cjk") or 0,
        "fenced_cjk": m.get("fenced_cjk") or 0,
        "pseudo_headings": m.get("pseudo_headings") or 0,
        "page_footer_body": m.get("page_footer_body") or 0,
        "bold_short_lines": m.get("bold_short_lines") or 0,
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
    """连续 | 开头的块内，pipe 数量应大致一致（允许对齐差 1）。围栏内的块不算表格。"""
    bad_blocks = 0
    for _start, block in _table_blocks(md_text.splitlines()):
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


# ---------------------------------------------------------------- 深检（Markdown 语义 / 源文保真 / 金标准）
def split_fenced(md_text: str):
    """返回 (围栏外的行, [(info_string, 行列表), …])；围栏成对性仍由 judge_structure 负责。"""
    outside, blocks = [], []
    cur, cur_info = None, ""
    for line in (md_text or "").splitlines():
        if line.lstrip().startswith("```"):
            if cur is None:
                cur, cur_info = [], line.lstrip()[3:].strip()
            else:
                blocks.append((cur_info, cur))
                cur, cur_info = None, ""
            continue
        if cur is None:
            outside.append(line)
        else:
            cur.append(line)
    if cur is not None:          # 未闭合围栏：剩余行仍算围栏内容
        blocks.append((cur_info, cur))
    return outside, blocks


def _table_blocks(lines):
    """连续以 | 开头的行构成表块；返回 [(起始行号, [行…]), …]。

    围栏内的行不算 Markdown 表格（那是代码/OCR 字面量，由 FENCED_TABLE 管），
    否则 ```text 里恰好以 | 开头的 OCR 行会被当成表格。
    """
    blocks, cur_start, cur, in_fence = [], None, [], False

    def flush():
        nonlocal cur_start, cur
        if cur:
            blocks.append((cur_start, cur))
        cur_start, cur = None, []

    for idx, line in enumerate(lines, 1):
        if line.lstrip().startswith("```"):
            flush()
            in_fence = not in_fence
            continue
        if in_fence or not line.lstrip().startswith("|"):
            flush()
            continue
        if cur_start is None:
            cur_start = idx
        cur.append(line)
    flush()
    return blocks


def _table_cells(row: str):
    return [c.strip() for c in re.split(r"(?<!\\)\|", row.strip().strip("|"))]


def _md_headings(md_text: str):
    """返回 [(归一化标题文本, # 个数, 原行), …]。

    归一化会去掉 `**`/`_`/`` ` ``/`~` 等强调与代码标记：标题常被渲染成
    `## **数字逻辑主要的** **Fault** **类型 **`，不去标记会把「已是标题」判成标题丢失。
    """
    out = []
    for line in md_text.splitlines():
        if re.match(r"^#{1,6}\s", line):
            n = len(line) - len(line.lstrip("#"))
            text = re.sub(r"[*_`~]", "", line.lstrip("#").strip())
            out.append((_norm_ws(text), n, line))
    return out


def _exp_int(exp, key):
    v = (exp or {}).get(key)
    return v if isinstance(v, int) and not isinstance(v, bool) else None


def _exp_msg(exp, key, n):
    """期望上限存在时把消息补成「（期望 ≤ N，实际 M）」。"""
    mx = _exp_int(exp, key)
    return f"（期望 ≤ {mx}，实际 {n}）" if mx is not None else ""


def judge_markdown_semantics(md_text: str, m, issues, exp, src_file: Path | None = None):
    """语义噪声：伪标题 / 超长行 / 转义噪声 / 围栏表格 / 合并单元格 / 表头横幅 /
    页码残留 / 题注入表 / 有序列表断档。"""
    exp = exp or {}
    outside, fenced = split_fenced(md_text)
    lines = md_text.splitlines()
    av = src_file is not None and src_file.suffix.lower().lstrip(".") in AV_EXTS

    # 伪标题：内嵌对象文件名被当成章节标题
    n_pseudo = m.get("pseudo_headings")
    if n_pseudo is None:
        n_pseudo = sum(1 for l in outside if PSEUDO_HEADING_RE.match(l.strip()))
    if n_pseudo > (_exp_int(exp, "max_pseudo_headings") or 0):
        first = next((l.strip() for l in outside if PSEUDO_HEADING_RE.match(l.strip())), "")
        issues.append(make_issue(
            "PSEUDO_HEADING",
            f"{n_pseudo} 个标题是内嵌对象文件名: {first[:60]}"
            + _exp_msg(exp, "max_pseudo_headings", n_pseudo)))

    # 超长行（内嵌对象被压成一行）；音视频转写天然单行，跳过
    if not av:
        maxlen = m.get("max_line_len") or 0
        if maxlen > GIANT_LINE_WARN:
            for idx, l in enumerate(lines, 1):
                if len(l) == maxlen:
                    preview = l.strip()[:40]
                    if maxlen > GIANT_LINE_FAIL:
                        issues.append(make_issue(
                            "GIANT_LINE",
                            f"第 {idx} 行单行 {maxlen} 字符(疑似内嵌对象被压平): {preview}"))
                    else:
                        issues.append(make_issue(
                            "GIANT_LINE_SOFT", f"第 {idx} 行单行 {maxlen} 字符: {preview}"))
                    break

    # 转义/实体噪声：\# \. \< + 纯 "\-" 行（围栏外）+ 全文 \|
    esc = len(ESCAPE_TOKEN_RE.findall("\n".join(outside)))
    dash_lines = sum(1 for l in outside if DASH_ONLY_RE.match(l.strip()))
    esc_pipes = len(ESCAPED_PIPE_RE.findall(md_text))
    fence_leaks = sum(1 for l in outside if ESCAPED_FENCE_RE.search(l))
    n_esc = esc + dash_lines + esc_pipes + fence_leaks
    m["escapes"], m["dash_lines"] = n_esc, dash_lines
    if n_esc >= ESCAPE_NOISE_MIN:
        issues.append(make_issue(
            "ESCAPE_NOISE",
            f"转义/实体噪声 {n_esc} 处(\\# \\. {esc} | 纯 \\- 行 {dash_lines} | "
            f"\\| {esc_pipes} | 转义围栏泄漏 {fence_leaks})"
            + _exp_msg(exp, "max_escapes", n_esc)))
    if dash_lines > (_exp_int(exp, "max_dash_lines") or 0):
        issues.append(make_issue(
            "ESCAPE_NOISE",
            f"{dash_lines} 行纯 \\- 残留(空段落转义)"
            + _exp_msg(exp, "max_dash_lines", dash_lines)))

    # 完整表格被包进代码围栏（要求多列形态；单列 OCR 树/列表不算）
    n_fenced_tables = 0
    for _info, block in fenced:
        rows = [l for l in block if l.lstrip().startswith("|")]
        if len(rows) < FENCED_TABLE_MIN_ROWS:
            continue
        multi = sum(1 for l in rows if sum(1 for c in _table_cells(l) if c) >= 2)
        if multi >= FENCED_TABLE_MIN_ROWS:
            n_fenced_tables += 1
    if n_fenced_tables:
        issues.append(make_issue(
            "FENCED_TABLE",
            f"{n_fenced_tables} 个代码围栏内是管道网格（源表格或图内表格被当字面量输出，"
            f"不会渲染成 Markdown 表格）"))

    # 真值表值被合并进单个单元格（只在真值表式表块里判定，避免普通数字格误报）
    merged, samples = 0, []
    for _start, rows in _table_blocks(lines):
        cells = [c for row in rows if not TABLE_SEP_RE.match(row)
                 for c in _table_cells(row) if c]
        if not cells:
            continue
        sym = sum(1 for c in cells if len(c) == 1 and c in "01XZxzlhLH")
        if sym / len(cells) < TRUTH_TABLE_SYMBOL_RATIO:
            continue
        for row in rows:
            if TABLE_SEP_RE.match(row):
                continue
            rcells = _table_cells(row)
            if sum(1 for c in rcells if not c) * 2 < len(rcells):
                continue                      # 合并值的形态是「该行多为空格 + 一个多值格」
            for cell in rcells:
                if MERGED_CELL_RE.match(cell) and len(set(cell)) >= 2:
                    merged += 1
                    if len(samples) < 3 and cell not in samples:
                        samples.append(cell)
    m["merged_cells"] = merged
    if merged > (_exp_int(exp, "max_merged_cells") or 0):
        issues.append(make_issue(
            "TABLE_CELL_MERGED",
            f"{merged} 个表格单元格是合并的真值表值(如 {', '.join(samples)})"
            + _exp_msg(exp, "max_merged_cells", merged)))

    # 表头单元格是长备注横幅
    long_headers = []
    for start, rows in _table_blocks(lines):
        if not rows:
            continue
        for cell in _table_cells(rows[0]):
            if len(cell) > TABLE_HEADER_MAX_CHARS:
                long_headers.append((start, cell))
                break
    if long_headers:
        start, cell = long_headers[0]
        issues.append(make_issue(
            "TABLE_HEADER_LONG",
            f"{len(long_headers)} 个表头单元格超过 {TABLE_HEADER_MAX_CHARS} 字符"
            f"(疑似备注横幅): 第 {start} 行 {len(cell)} 字符 {cell[:40]}"))

    # 页码占位符进入正文
    n_footer = m.get("page_footer_body")
    if n_footer is None:
        n_footer = sum(1 for l in outside if PAGE_FOOTER_FULL_RE.fullmatch(l.strip()))
    if n_footer > (_exp_int(exp, "max_page_footer_body") or 0):
        issues.append(make_issue(
            "PAGE_FOOTER_BODY", f"{n_footer} 处「Page N」占位符残留为正文"
            + _exp_msg(exp, "max_page_footer_body", n_footer)))

    # 图表题注粘进表格行
    n_caption = sum(1 for _s, rows in _table_blocks(lines) for row in rows
                    if CAPTION_IN_TABLE_RE.search(row))
    if n_caption:
        issues.append(make_issue(
            "CAPTION_IN_TABLE", f"{n_caption} 行表格内混入图表题注(Table/Figure N-M.)"))

    # 有序列表编号断档（空行不断块；缩进更浅视为回到外层；「重新从 1 起」切成新列表）
    bad_lists, detail = 0, ""
    i = 0
    while i < len(outside):
        if not outside[i].strip():
            i += 1
            continue
        mo = ORDERED_ITEM_RE.match(outside[i])
        if not mo:
            i += 1
            continue
        seq = []
        while i < len(outside):
            if not outside[i].strip():
                i += 1
                continue
            mm = ORDERED_ITEM_RE.match(outside[i])
            if not mm:
                break
            seq.append((len(mm.group(1)), int(mm.group(2))))
            i += 1
        if len(seq) < 2:
            continue
        groups = []
        for ind, n in seq:
            if groups and n == 1 and ind <= groups[-1][-1][0]:
                groups.append([(ind, n)])          # 同/更浅缩进处重新从 1 起 → 新列表
            elif groups:
                groups[-1].append((ind, n))
            else:
                groups.append([(ind, n)])
        broken = False
        for g in groups:
            if g[0][1] != 1:
                broken = True
                break
            prev_ind, prev_n = g[0]
            for ind, n in g[1:]:
                if ind < prev_ind:                # 回到外层列表：编号由外层决定，不比较
                    prev_ind, prev_n = ind, n
                elif n == prev_n + 1 or (n > prev_n + 1
                                         and n - prev_n - 1 <= LIST_NUMBERING_MAX_GAP):
                    prev_ind, prev_n = ind, n
                elif n == 1 and ind > prev_ind:    # 嵌套子列表重新从 1 起
                    prev_ind, prev_n = ind, n
                else:
                    broken = True
                    break
            if broken:
                break
        if not broken and len(groups) > 1 and len(groups[-1]) < 2:
            broken = True                          # 尾部只剩一个「重新从 1 起」的孤立项
        if broken:
            bad_lists += 1
            detail = detail or "-".join(str(n) for _i, n in seq[:8])
    if bad_lists:
        issues.append(make_issue(
            "LIST_NUMBERING", f"{bad_lists} 个有序列表编号断档(如 {detail})"))


def judge_duplication(md_text: str, issues):
    """同一内容以「围栏块 + 正文」两份出现（内嵌对象预览图 OCR 与解析文本重复等）。"""
    outside, fenced = split_fenced(md_text)
    body = _norm_ws("\n".join(outside))
    if len(body) < DUP_BLOCK_MIN_CHARS:
        return
    body_grams = {body[i:i + 8] for i in range(len(body) - 7)}
    hits, best, dups = 0, 0.0, 0
    seen = set()
    for _info, block in fenced:
        text = _norm_ws("\n".join(block))
        if len(text) < DUP_BLOCK_MIN_CHARS:
            continue
        if text in seen:
            dups += 1
        seen.add(text)
        grams = {text[i:i + 8] for i in range(len(text) - 7)}
        if not grams:
            continue
        contain = len(grams & body_grams) / len(grams)
        if contain >= DUP_BLOCK_CONTAINMENT:
            hits += 1
            best = max(best, contain)
    if hits:
        issues.append(make_issue(
            "DUP_CONTENT", f"{hits} 个围栏块内容与正文重复(最大命中率 {best:.0%})"))
    elif dups:
        issues.append(make_issue("DUP_CONTENT", f"{dups} 个围栏块与其他围栏块内容完全相同"))


def judge_ocr_channel(src_file: Path, md_text: str, err_path: Path, issues, exp,
                      ocr_requested, m=None):
    """从引擎日志读出生效的 OCR 后端/语言，按期望断言中文 OCR 通道。返回该文件通道信息。

    `xberg.ocr.language` 只在 OCR 未命中结果缓存（含 tessdata 素材化）时随 `xberg.ocr{}` span
    落进 stderr，热缓存运行观测不到 —— 观测不可靠，故观测不到时退回「请求配置」
    （run.ocr_config）判定；实际识别量仍由 OCR_CJK_LOW 把守。
    """
    exp = exp or {}
    backend = language = None
    cjk_backends_seen: list[str] = []
    if err_path.is_file():
        text = ANSI_RE.sub("", err_path.read_text("utf-8", errors="replace"))
        # 后端名带连字符（paddle-ocr），\w+ 会截成 "paddle"，任何 paddle 构建都被判成未知后端。
        mb = re.findall(r"OCR backend registered backend=([\w-]+)", text)
        ml = re.findall(r'xberg\.ocr\.language="([^"]*)"', text)
        backend = mb[-1] if mb else None
        cjk_backends_seen = [name for name in mb if name.lower() in OCR_CJK_BACKENDS]
        language = ml[-1] if ml else None
    req = ocr_requested if isinstance(ocr_requested, dict) else {}
    req_backend = str(req.get("backend") or "")
    req_lang = req.get("language")
    req_langs = [req_lang] if isinstance(req_lang, str) else [str(x) for x in (req_lang or [])]
    info = {"backend": backend, "language": language, "requested": ocr_requested,
            "expect_chinese": bool(exp.get("require_chinese_ocr"))}
    if exp.get("require_chinese_ocr"):
        # 观测值优先；观测不到 language（热缓存）时用请求语言
        lang_for_check = language if language is not None else " ".join(req_langs)
        observed = backend or req_backend
        # 显式请求了后端就按请求判定；否则看注册行里有没有任一 CJK 后端（同一次运行可能注册
        # 多个后端，最后一行未必是生效的那个），一行都没有时按未知处理。
        ok_backend = (
            req_backend.lower() in OCR_CJK_BACKENDS if req_backend else bool(cjk_backends_seen)
        )
        ok_lang = bool(re.search(r"chi_sim|chi_tra|\bch\b|\bzh\b", lang_for_check or "", re.I))
        if not (ok_backend or ok_lang):
            issues.append(make_issue(
                "OCR_BACKEND_MISMATCH",
                f"OCR 后端/语言 = {observed or '未观测到(err.txt 无 registry 行)'}/"
                f"{language if language is not None else ('+'.join(req_langs) or '默认 eng（未配置语言，按引擎默认推断）')}"
                f"，无法识别中文（期望 paddle-ocr(pp-ocrv6) 或 chi_sim）"))
    cjk = m.get("cjk") if m else None
    if cjk is None:
        cjk = len(re.findall(r"[\u4e00-\u9fff]", md_text))
    if _exp_int(exp, "min_cjk") is not None and cjk < exp["min_cjk"]:
        issues.append(make_issue(
            "OCR_CJK_LOW", f"OCR 输出汉字 {cjk} 个 < 期望 {exp['min_cjk']}"))
    fenced_cjk = m.get("fenced_cjk") if m else None
    if fenced_cjk is None:
        fenced_cjk = sum(len(re.findall(r"[\u4e00-\u9fff]", "\n".join(b)))
                         for _i, b in split_fenced(md_text)[1])
    if _exp_int(exp, "min_fenced_cjk") is not None and fenced_cjk < exp["min_fenced_cjk"]:
        issues.append(make_issue(
            "OCR_CJK_LOW",
            f"围栏内 OCR 汉字 {fenced_cjk} 个 < 期望 {exp['min_fenced_cjk']}"))
    return info


def judge_hash_glyph(src_text: str, md_text: str, issues):
    """源里的 #include/#faults 等被解码成 fiinclude/fifaults（'#' → 'fi'）。"""
    if not src_text or not md_text:
        return
    bad = []
    for token in sorted(set(re.findall(r"#([A-Za-z_][A-Za-z0-9_]{2,})", src_text))):
        if ("#" + token) in md_text:
            continue
        n_fi = md_text.count("fi" + token)
        if n_fi:
            bad.append((token, n_fi))
    if not bad:
        return
    total = sum(n for _t, n in bad)
    sample = ", ".join(f"#{t}→fi{t}" for t, _n in bad[:3])
    issues.append(make_issue(
        "GLYPH_SUBST", f"{total} 处 '#' 被解码成 'fi'（如 {sample}）"))


def _split_form_hits(token: str, md_text: str) -> int:
    """token 的字符在 MD 里被空白拆开的出现次数（精确字符序列 + 词边界）。"""
    rx = re.compile(r"(?<![0-9A-Za-z_.])" + r"\s*".join(re.escape(c) for c in token)
                    + r"(?![0-9A-Za-z_.])")
    return len(rx.findall(md_text))


def judge_ident_fragmentation(src_text: str, md_text: str, issues):
    """源标识符在 MD 里只以「被空格拆断」的形态出现（Implements → Impl ements）。"""
    if not src_text or not md_text:
        return
    _, idents = extract_critical_tokens(src_text)
    if not idents:
        return
    out_idents = extract_critical_tokens(md_text)[1]
    frag = []
    for t in sorted(idents):
        if _token_boundary_hit(t, md_text, out_idents):
            continue
        if _split_form_hits(t, md_text):
            frag.append(t)
    if len(frag) >= FRAGMENTED_IDENT_MIN:
        issues.append(make_issue(
            "IDENT_FRAGMENTED",
            f"{len(frag)} 个源标识符仅以被空格拆断的形态出现: {', '.join(frag[:3])}"))


def _embedded_text(data: bytes, ext: str) -> str:
    """用与 extract_source_text 同款库解析内嵌子文档字节。"""
    import io
    try:
        if ext == "docx":
            import docx
            d = docx.Document(io.BytesIO(data))
            parts = [p.text for p in d.paragraphs]
            for t in d.tables:
                for row in t.rows:
                    parts.append(" ".join(c.text for c in row.cells))
            return "\n".join(parts)
        if ext == "xlsx":
            from openpyxl import load_workbook
            wb = load_workbook(io.BytesIO(data), read_only=True, data_only=True)
            parts = []
            for ws in wb.worksheets:
                for row in ws.iter_rows(values_only=True):
                    parts.append(" ".join(str(v) for v in row if v is not None))
            return "\n".join(parts)
        if ext == "pptx":
            from pptx import Presentation
            prs = Presentation(io.BytesIO(data))
            parts = []
            for slide in prs.slides:
                for shape in slide.shapes:
                    if shape.has_text_frame:
                        parts.append(shape.text_frame.text or "")
            return "\n".join(parts)
    except Exception:
        return ""
    return ""


def _embedded_texts(path: Path):
    """内嵌子文档（*/embeddings/*.docx|xlsx|pptx）的文本清单。"""
    import zipfile
    out = []
    try:
        with zipfile.ZipFile(path) as z:
            for name in z.namelist():
                low = name.lower()
                if "/embeddings/" not in low or not low.endswith((".docx", ".xlsx", ".pptx")):
                    continue
                text = _embedded_text(z.read(name), low.rsplit(".", 1)[-1])
                if text:
                    out.append((name.rsplit("/", 1)[-1], text))
    except Exception:
        return out
    return out


def _embedded_media_md5(path: Path):
    """内嵌子文档 / embeddings 下直接内嵌的图片：返回 [(文件名, md5, 扩展名), …]。"""
    import hashlib, io, zipfile
    exts = (".png", ".jpg", ".jpeg", ".emf", ".wmf", ".bmp")
    out = []
    try:
        with zipfile.ZipFile(path) as z:
            for name in z.namelist():
                low = name.lower()
                if "/embeddings/" not in low:
                    continue
                if low.endswith(exts):
                    out.append((name.rsplit("/", 1)[-1],
                                hashlib.md5(z.read(name)).hexdigest(),
                                low.rsplit(".", 1)[-1]))
                elif low.endswith((".docx", ".xlsx", ".pptx")):
                    try:
                        with zipfile.ZipFile(io.BytesIO(z.read(name))) as z2:
                            for n2 in z2.namelist():
                                if "/media/" in n2.lower() and n2.lower().endswith(exts):
                                    out.append((n2.rsplit("/", 1)[-1],
                                                hashlib.md5(z2.read(n2)).hexdigest(),
                                                n2.lower().rsplit(".", 1)[-1]))
                    except Exception:
                        continue
    except Exception:
        return out
    return out


def _pixel_hash(data: bytes):
    """解码后的像素 md5（引擎会按 output_format 重编码，字节 md5 对不上时用它）。"""
    import hashlib, io
    try:
        from PIL import Image
        with Image.open(io.BytesIO(data)) as im:
            return hashlib.md5(im.convert("RGBA").tobytes()).hexdigest()
    except Exception:
        return None


def _embedded_media_lost(path: Path, m):
    """内嵌图片是否落盘。PNG 先比字节 md5，其余/重编码过的一律比解码像素。

    返回 (丢失文件名, 总数)；EMF/WMF 等矢量格式无法按像素比对，不计入分母。
    """
    import hashlib
    media = [x for x in _embedded_media_md5(path) if x[2] in ("png", "jpg", "jpeg", "bmp")]
    if not media:
        return [], 0
    # 取回原始字节用于像素比对
    import io, zipfile
    raw = {}
    try:
        with zipfile.ZipFile(path) as z:
            for name in z.namelist():
                low = name.lower()
                if "/embeddings/" not in low:
                    continue
                if low.endswith((".png", ".jpg", ".jpeg", ".bmp")):
                    raw[name.rsplit("/", 1)[-1]] = z.read(name)
                elif low.endswith((".docx", ".xlsx", ".pptx")):
                    try:
                        with zipfile.ZipFile(io.BytesIO(z.read(name))) as z2:
                            for n2 in z2.namelist():
                                if "/media/" in n2.lower() and n2.lower().endswith(
                                        (".png", ".jpg", ".jpeg", ".bmp")):
                                    raw[n2.rsplit("/", 1)[-1]] = z2.read(n2)
                    except Exception:
                        continue
    except Exception:
        pass
    disk_md5, disk_px = set(), set()
    for p in m.get("img_dir_files") or []:
        try:
            data = p.read_bytes()
        except OSError:
            continue
        disk_md5.add(hashlib.md5(data).hexdigest())
        px = _pixel_hash(data)
        if px:
            disk_px.add(px)
    lost = []
    for name, md5, _ext in media:
        if md5 in disk_md5:
            continue
        px = _pixel_hash(raw.get(name, b"")) if name in raw else None
        if px and px in disk_px:
            continue
        lost.append(name)
    return lost, len(media)


def _judge_pptx_render(src_file: Path, md_text: str, issues):
    """幻灯片标题占位符 / 演讲者备注是否落到 MD。"""
    try:
        from pptx import Presentation
        prs = Presentation(str(src_file))
    except Exception:
        return
    heads = [h for h, _n, _raw in _md_headings(md_text)]
    md_nos = _norm_ws(md_text)
    miss_t, miss_n = [], []
    for idx, slide in enumerate(prs.slides, 1):
        try:
            title = ((slide.shapes.title.text or "").strip()
                     if slide.shapes.title is not None else "")
        except Exception:
            title = ""
        if title:
            key = _norm_ws(title)[:20]
            if key and not any(key in h for h in heads):
                miss_t.append((idx, title))
        if slide.has_notes_slide:
            tf = slide.notes_slide.notes_text_frame
            notes = ((tf.text if tf is not None else "") or "").strip()
            if notes:
                key = _norm_ws(notes)[:20]
                if key and key not in md_nos:
                    miss_n.append((idx, notes))
    if miss_t:
        idx, t = miss_t[0]
        issues.append(make_issue(
            "PPTX_TITLE_LOST",
            f"{len(miss_t)} 张幻灯片标题未以标题形式输出(如 slide {idx}: {t[:40]})"))
    if miss_n:
        idx, t = miss_n[0]
        issues.append(make_issue(
            "NOTES_MISSING",
            f"{len(miss_n)} 张幻灯片演讲者备注丢失(如 slide {idx}: {_norm_ws(t)[:20]})"))


def _judge_xlsx_shapes(src_file: Path, md_text: str, issues):
    """xlsx 浮动图形（drawing xml）里的文本是否进入 MD。"""
    import zipfile
    texts = []
    try:
        with zipfile.ZipFile(src_file) as z:
            for name in z.namelist():
                if re.match(r"xl/drawings/.*\.xml$", name.replace("\\", "/")):
                    xml = z.read(name).decode("utf-8", errors="replace")
                    texts += [t.strip() for t in re.findall(r"<a:t>([^<]*)</a:t>", xml)]
    except Exception:
        return
    cand = [t for t in texts if len(t) >= 2]
    if not cand:
        return
    md_nos = _norm_ws(md_text)
    lost = [t for t in cand if _norm_ws(t) not in md_nos]
    if not lost:
        return
    ratio = len(lost) / len(cand)
    issues.append(make_issue(
        "XLSX_SHAPE_TEXT_MISSING",
        f"xlsx 浮动图形文本丢失 {len(lost)}/{len(cand)}({ratio:.0%}): "
        f"{', '.join(lost[:3])}",
        severity=None if ratio >= XLSX_SHAPE_LOST_RATIO else "WARN"))


def _judge_pdf_structure(src_file: Path, md_text: str, m, issues, exp):
    """PDF：书签标题召回 + 源表格数 vs MD 表格块数。"""
    try:
        import pymupdf
        doc = pymupdf.open(src_file)
    except Exception:
        return
    try:
        entries = [e for e in (doc.get_toc() or [])
                   if len(e) >= 3 and e[0] <= 2 and (e[2] or 0) > 0]
        if len(entries) >= 20:
            heads = [h for h, _n, _raw in _md_headings(md_text)]
            miss = [str(t) for _l, t, _p in entries
                    if _norm_ws(str(t)) and not any(_norm_ws(str(t)) in h for h in heads)]
            recall = 1 - len(miss) / len(entries)
            min_recall = exp.get("toc_heading_min_recall")
            if not isinstance(min_recall, (int, float)):
                min_recall = TOC_HEADING_MIN_RECALL
            if recall < min_recall:
                issues.append(make_issue(
                    "TOC_HEADING_GAP",
                    f"PDF 书签标题仅 {recall:.0%} 以标题形式出现"
                    f"(< {min_recall:.0%}，缺 {len(miss)}/{len(entries)} 条，"
                    f"如 {miss[0][:40] if miss else '-'})"))
        src_tables, src_kind = 0, "结构"
        try:
            # 权威口径：文档自带 `Table N-M.` 题注（List of Tables 与正文一致）；
            # 结构口径只作兜底——find_tables 会被页眉框与跨页切分污染。
            captions = set()
            for page in doc:
                for m_cap in re.finditer(r"\bTable\s+([A-Z]?\d+[-–]\d+)\.", page.get_text() or ""):
                    captions.add(m_cap.group(1))
            if len(captions) >= 10:
                src_tables, src_kind = len(captions), "题注"
            else:
                signatures = Counter()
                candidates = []
                for page in doc:
                    for t in page.find_tables().tables:
                        try:
                            data = t.extract()
                        except Exception:
                            continue
                        if len(data) < 2 or max((len(r) for r in data), default=0) < 2:
                            continue                  # 1 行/1 列碎片不算表
                        filled = [str(c).strip() for r in data
                                  for c in r if c and str(c).strip()]
                        if len(filled) < 8:
                            continue                  # 内容太少的框不算表
                        sig = _norm_ws(" ".join(sorted(set(filled))))[:120]
                        signatures[sig] += 1
                        candidates.append(sig)
                pages = max(doc.page_count, 1)
                furniture = {s for s, n in signatures.items()
                             if n >= max(10, pages * 0.2)}
                for sig in candidates:
                    if sig not in furniture:          # 每页重复的框是页眉/页脚家具
                        src_tables += 1
        except Exception:
            src_tables, src_kind = None, None
        md_tables = m.get("tables") or 0
        if src_tables and src_tables - md_tables >= 5 and md_tables < src_tables * 0.6:
            issues.append(make_issue(
                "TABLE_COUNT_GAP",
                f"源表格约 {src_tables} 个（{src_kind}口径），MD 表格块仅 {md_tables} 个"))
    finally:
        try:
            doc.close()
        except Exception:
            pass


def _judge_expectations(md_text: str, m, issues, exp):
    """按逐文件金标准断言：必需文本 / 禁止形态 / 顺序 / 数量指标。"""
    if not exp:
        return
    md_nos = _norm_ws(md_text)
    req = exp.get("required_tokens") or []
    missing = [str(t) for t in req if _norm_ws(str(t)) not in md_nos]
    if missing:
        issues.append(make_issue(
            "GOLDEN_TOKEN_MISSING",
            f"{len(missing)}/{len(req)} 个期望文本缺失: "
            f"{', '.join(x[:30] for x in missing[:8])}"))
    hits, first_ln = 0, 0
    for pat in exp.get("forbidden_patterns") or []:
        try:
            rx = re.compile(str(pat))
        except re.error:
            continue
        for ln, line in enumerate(md_text.splitlines(), 1):
            if rx.search(line):
                hits += 1
                first_ln = first_ln or ln
    if hits:
        issues.append(make_issue(
            "GOLDEN_FORBIDDEN", f"出现 {hits} 处期望禁止的形态(首个在第 {first_ln} 行)"))
    bad_order = []
    for pair in exp.get("order") or []:
        if not isinstance(pair, (list, tuple)) or len(pair) != 2:
            continue
        a, b = str(pair[0]), str(pair[1])
        ia, ib = md_text.find(a), md_text.find(b)
        if ia < 0 or ib < 0 or ia >= ib:
            bad_order.append(f"{a[:20]} 应在 {b[:20]} 之前")
    if bad_order:
        issues.append(make_issue("GOLDEN_ORDER", "; ".join(bad_order)))
    metric_specs = (
        ("min_headings", "headings", "min"), ("max_headings", "headings", "max"),
        ("min_tables", "tables", "min"), ("max_tables", "tables", "max"),
        ("min_chars", "chars", "min"), ("max_chars", "chars", "max"),
        ("min_images", "img_refs", "min"), ("max_images", "img_refs", "max"),
        ("max_line_len", "max_line_len", "max"), ("max_escapes", "escapes", "max"),
    )
    violated = []
    for key, metric, kind in metric_specs:
        lim = _exp_int(exp, key)
        if lim is None:
            continue
        actual = m.get(metric) or 0
        if (kind == "min" and actual < lim) or (kind == "max" and actual > lim):
            violated.append(
                f"{key}: {metric}={actual}(期望 {'≥' if kind == 'min' else '≤'} {lim})")
    if violated:
        issues.append(make_issue("GOLDEN_METRIC", "; ".join(violated)))


def judge_source_fidelity(src_file: Path, md_text: str, src_text, m, issues, exp,
                          src_img_n=None):
    """源文保真：内嵌子文档文本/图片、PPTX 标题与备注、xlsx 图形文本、PDF 书签；
    末尾统一套用逐文件金标准（required_tokens/forbidden_patterns/order/min-*/max-*）。"""
    ext = src_file.suffix.lower().lstrip(".")
    if ext in ("docx", "pptx", "xlsx", "ods"):
        # 去掉表格分隔/转义/markdown 标记后再比对：源是「空格连接的行内单元格」，
        # MD 是「| 分隔 + \# 转义 + ** 加粗」，不归一化会把这些差异算成"文字丢失"。
        cmp_text = _norm_ws(re.sub(r"[|\\*_`#>~]", "", md_text))
        cmp_grams = ({cmp_text[i:i + 12] for i in range(len(cmp_text) - 11)}
                     if len(cmp_text) <= 600_000 else None)
        for name, text in _embedded_texts(src_file):
            paras = [p for p in text.splitlines() if len(_norm_ws(p)) >= 12]
            miss = []
            for p in paras:
                pn = _norm_ws(re.sub(r"[|\\*_`#>~]", "", p))
                if pn[:20] in cmp_text:
                    continue
                if cmp_grams is not None and pn:
                    g = {pn[i:i + 12] for i in range(max(len(pn) - 11, 1))}
                    if g and len(g & cmp_grams) / len(g) >= 0.8:
                        continue          # 12-gram 八成命中 → 该段在，只是被标记/换行切开
                miss.append(p)
            bg = bigram_recall(text, cmp_text)
            ratio = len(miss) / len(paras) if paras else 0.0
            if (bg >= 0 and bg < 0.95) or (len(miss) >= EMBED_LOSS_PARA_MIN
                                           and ratio >= EMBED_LOSS_RATIO):
                bg_s = f"{bg:.0%}" if bg >= 0 else "-"
                issues.append(make_issue(
                    "EMBED_TEXT_LOSS",
                    f"内嵌子文档 {name} 正文缺失 {len(miss)}/{len(paras)} 段"
                    f"(bigram {bg_s}，缺段 {ratio:.0%})"))
        lost, total = _embedded_media_lost(src_file, m)
        if lost:
            issues.append(make_issue(
                "EMBED_MEDIA_LOST",
                f"内嵌子文档图片 {len(lost)}/{total} 个未落盘(源媒体约 {src_img_n} 个；"
                f"分母已排除 EMF/WMF，判定含解码像素回退): {', '.join(lost[:3])}"))
    if ext == "pptx":
        _judge_pptx_render(src_file, md_text, issues)
    if ext == "xlsx":
        _judge_xlsx_shapes(src_file, md_text, issues)
    if ext == "pdf":
        _judge_pdf_structure(src_file, md_text, m, issues, exp)
    _judge_expectations(md_text, m, issues, exp)


def judge_code_tables(md_text: str, issues, exp=None):
    """Verilog/代码被识别成 Markdown 表格（每行含代码特征且占多数）。"""
    hits, first = 0, 0
    for start, rows in _table_blocks(md_text.splitlines()):
        data = [r for r in rows if not TABLE_SEP_RE.match(r)]
        if len(data) < CODE_TABLE_MIN_ROWS:
            continue
        code_rows = sum(1 for r in data if CODE_TABLE_RE.search(r))
        if code_rows >= CODE_TABLE_MIN_ROWS and code_rows / len(data) >= CODE_TABLE_RATIO:
            hits += 1
            first = first or start
    if hits > (_exp_int(exp, "max_code_tables") or 0):
        issues.append(make_issue(
            "CODE_AS_TABLE",
            f"{hits} 个表格块是 Verilog/代码被切成的伪表格(首个在第 {first} 行)"
            + _exp_msg(exp, "max_code_tables", hits)))


def judge_emphasis_noise(md_text: str, issues, exp=None):
    """** 不成对残留 + 非标准 ==高亮==（只看围栏外）。"""
    outside, _fenced = split_fenced(md_text)
    odd = [ln for ln, l in enumerate(outside, 1)
           if l.count("**") % 2 == 1 and len(l) < 200 and not BOLD_PAIR_RE.search(l)]
    if len(odd) >= EMPHASIS_ODD_MIN:
        issues.append(make_issue(
            "EMPHASIS_ODD",
            f"{len(odd)} 行 ** 不成对(残留字面量，行号 {', '.join(map(str, odd[:2]))})"))
    hl, sample = 0, ""
    for l in outside:
        found = HIGHLIGHT_PAIR_RE.findall(l)
        hl += len(found)
        sample = sample or (found[0] if found else "")
    mx = _exp_int(exp, "max_highlights")
    if hl >= HIGHLIGHT_MIN or (mx is not None and hl > mx):
        issues.append(make_issue(
            "NONSTD_HIGHLIGHT",
            f"非标准 ==高亮== 标记 {hl} 对(如 {sample[:30]})"
            + _exp_msg(exp, "max_highlights", hl)))


def judge_toc_levels(src_file: Path, md_text: str, issues):
    """PDF：书签层级 vs 标题 # 层数（容差 ±1）。"""
    if src_file.suffix.lower() != ".pdf":
        return
    try:
        import pymupdf
        doc = pymupdf.open(src_file)
    except Exception:
        return
    try:
        entries = [e for e in (doc.get_toc() or [])
                   if len(e) >= 3 and e[0] <= 2 and (e[2] or 0) > 0]
        if len(entries) < 20:
            return
        heads = _md_headings(md_text)
        checked = mism = 0
        for level, title, _page in entries:
            key = _norm_ws(str(title))
            if not key:
                continue
            for h, n_hash, _raw in heads:
                if key in h:
                    checked += 1
                    lo, hi = (1, 2) if level <= 1 else (2, 3)
                    if not (lo <= n_hash <= hi):
                        mism += 1
                    break
        if checked and mism / checked >= TOC_LEVEL_MISMATCH_RATIO:
            issues.append(make_issue(
                "TOC_LEVEL_MISMATCH",
                f"{mism}/{checked} 条书签标题的 # 层数与书签层级不符"))
    finally:
        try:
            doc.close()
        except Exception:
            pass


def judge_xlsx_cell_folding(src_file: Path, md_text: str, issues, exp):
    """xlsx：单元格内部换行被折叠进同一行 MD（用 <br> 保留换行的不算折叠）。"""
    if src_file.suffix.lower() != ".xlsx":
        return
    try:
        from openpyxl import load_workbook
        wb = load_workbook(str(src_file), read_only=True, data_only=True)
    except Exception:
        return
    cells, seen = [], set()
    try:
        for ws in wb.worksheets:
            for row in ws.iter_rows(values_only=True):
                for v in row:
                    if isinstance(v, str) and "\n" in v and v.strip() and v not in seen:
                        seen.add(v)
                        cells.append(v)
            if len(cells) >= 2000:
                break
    finally:
        try:
            wb.close()
        except Exception:
            pass
    if not cells:
        return
    md_lines = [l for l in md_text.splitlines() if l.lstrip().startswith("|")]
    br_re = re.compile(r"<br\s*/?>|&#10;", re.I)
    folded = 0
    for v in cells:
        parts = [p.strip() for p in v.splitlines() if p.strip()]
        if len(parts) < 2:
            continue
        head, tail = parts[0][:10], parts[-1][:10]
        if len(head) < 4 or len(tail) < 4:
            continue          # 锚点太短（"1"/"2"）会在无关行里撞上，判不准
        for line in md_lines:
            if head not in line or tail not in line:
                continue
            seg = line[line.find(head):line.find(tail) + len(tail)]
            if not br_re.search(seg):     # 中间没有换行标记 → 确实被折叠
                folded += 1
            break
    mx = _exp_int(exp, "max_folded_cells")
    if folded > (mx if mx is not None else 20):
        issues.append(make_issue(
            "TABLE_CELL_FOLDED",
            f"{folded} 个 xlsx 单元格的内部换行被折叠进同一行"
            + _exp_msg(exp, "max_folded_cells", folded)))


def judge_bullet_levels(src_file: Path, md_text: str, issues, exp):
    """pptx 源项目符号有层级，但 MD 里没有任何缩进列表 → 层级被拍平。"""
    if src_file.suffix.lower() != ".pptx" or not (exp or {}).get("require_nested_bullets"):
        return
    try:
        from pptx import Presentation
        prs = Presentation(str(src_file))
    except Exception:
        return
    deepest = 0
    for slide in prs.slides:
        for shape in slide.shapes:
            if not getattr(shape, "has_text_frame", False):
                continue
            for para in shape.text_frame.paragraphs:
                if para.text.strip():
                    deepest = max(deepest, getattr(para, "level", 0) or 0)
    outside, _fenced = split_fenced(md_text)
    has_indented = any(re.match(r"^\s{2,}[-*]\s", l) for l in outside)
    if deepest >= 1 and not has_indented:
        issues.append(make_issue(
            "BULLET_LEVEL_FLAT",
            f"源项目符号最深层级 {deepest}，但 MD 无缩进列表(层级被拍平)"))


def judge_running_head(md_text: str, issues, exp=None):
    """书眉/运行标题残留为纯加粗短行。"""
    outside, _fenced = split_fenced(md_text)
    texts = [l.strip() for l in outside if BOLD_SHORT_RE.match(l.strip())]
    n = len(texts)
    top_text, top_n = Counter(texts).most_common(1)[0] if texts else ("", 0)
    mx = _exp_int(exp, "max_bold_short_lines")
    fire = n > (mx if mx is not None else 29) or (mx is None and top_n >= 4)
    if fire:
        issues.append(make_issue(
            "RUNNING_HEAD",
            f"纯加粗短行 {n} 行(疑似书眉/运行标题残留，如「{top_text[:30]}」×{top_n})"
            + _exp_msg(exp, "max_bold_short_lines", n)))


# ---------------------------------------------------------------- 金标准 / 回归基线
def load_expectations(path: Path) -> dict:
    """读取逐文件金标准；缺失/损坏只打印提示，不影响其余检查。"""
    if not path.is_file():
        print(f"[expectations] 未找到 {path}，跳过金标准检查", flush=True)
        return {}
    try:
        data = json.loads(path.read_text("utf-8"))
        print(f"[expectations] 已加载 {path}（{len(data.get('files') or {})} 个文件条目）",
              flush=True)
        return data if isinstance(data, dict) else {}
    except Exception as e:
        print(f"[expectations] 解析失败 {path}: {e}（跳过金标准检查）", flush=True)
        return {}


def expect_for(name: str) -> dict:
    files = EXPECTATIONS.get("files") if isinstance(EXPECTATIONS, dict) else None
    return (files or {}).get(name) or {}


def load_baseline(path: Path) -> dict:
    if not path.is_file():
        print(f"[baseline] 未找到 {path}，跳过回归对比", flush=True)
        return {}
    try:
        data = json.loads(path.read_text("utf-8"))
        return data if isinstance(data, dict) else {}
    except Exception as e:
        print(f"[baseline] 解析失败 {path}: {e}（跳过回归对比）", flush=True)
        return {}


def compare_baseline(prev: dict, cur: list) -> dict:
    """按文件名对齐问题码集合，产出新增/已修复（只比 code，不比 message）。

    依赖金标准的码（GOLDEN_*/OCR 期望码）在未加载金标准的运行里不会产生，
    必须从两侧剔除，否则会得到假的「已修复」。
    """
    prev_files = {f.get("name"): f for f in (prev.get("files") or [])}
    out = {"files": {}, "totals": {"new": 0, "fixed": 0}, "skipped_exp_codes": False}
    for rec in cur:
        name = rec.get("name")
        cur_applied = bool((rec.get("golden") or {}).get("applied"))
        prev_rec = prev_files.get(name) or {}
        # 基线未记录 golden_applied（旧格式）时按「有金标准」处理，保持原有可追踪性
        prev_applied = prev_rec.get("golden_applied", True)
        drop = set() if (cur_applied and prev_applied is True) else EXP_GATED_CODES
        if drop:
            out["skipped_exp_codes"] = True
        now = {i["code"] for i in (rec.get("issues") or [])} - drop
        before = {i["code"] for i in (prev_rec.get("issues") or [])} - drop
        new, fixed = sorted(now - before), sorted(before - now)
        if new or fixed:
            out["files"][name] = {"new": new, "fixed": fixed}
            out["totals"]["new"] += len(new)
            out["totals"]["fixed"] += len(fixed)
    return out


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


def kill_process_tree(proc):
    """终止 CLI 及其全部子进程。

    Windows 上 Popen.kill() 只 TerminateProcess 掉 xberg.exe 本身；Windows Media 输入走
    外部 ffmpeg 解码时，解码子进程会继续跑（占着源文件、在 %TEMP% 写 WAV），污染后续文件的
    耗时统计。taskkill /T 覆盖子进程；失败时退回 proc.kill()。
    """
    if os.name == "nt":
        done = subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                              capture_output=True, check=False)
        if done.returncode == 0:
            return
    proc.kill()


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
                kill_process_tree(proc)
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
    if OCR_CONFIG:
        cfg["ocr"] = OCR_CONFIG
    if LAYOUT_CONFIG is not None:
        cfg["layout"] = LAYOUT_CONFIG
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


def _encode_markdown_dir(name: str) -> str:
    """把目录名里会终止/破坏 CommonMark 链接目标的部分转成百分号编码。

    与 CLI（`prefix_image_refs`）同一张表：源文件名带空格或括号时（`my doc (1).docx`
    → `my doc (1)_images`），不加编码写出的 `![](my doc (1)_images/image_0.png)` 在任何
    渲染器里都不是有效链接；`%` 也要编码，否则渲染器会把 `100%25` 解码成另一个目录名。
    引擎侧 `sanitize_marker_url` 共用同样的字符集，但它重写的是文档自带的（已百分号编码
    的）关系目标，所以那里不能编码 `%`。
    """
    table = {" ": "%20", "(": "%28", ")": "%29", "<": "%3C", ">": "%3E",
             '"': "%22", "`": "%60", "%": "%25"}
    return "".join(table.get(ch, ch) for ch in name if not ch.iscontrol())


def save_markdown(out_dir: Path, stem: str, md_text: str, img_dirname: str):
    """把 markdown 里的图片引用加上子目录前缀后再保存，保证打开时图片可见。"""
    prefix = _encode_markdown_dir(img_dirname)
    saved = re.sub(r"(!\[[^\]]*\]\()([^)/]+)\)",
                   lambda match: f"{match.group(1)}{prefix}/{match.group(2)})", md_text)
    (out_dir / f"{stem}.md").write_text(saved, encoding="utf-8")


def convert_one(cli: Path, src_file: Path, out_dir: Path, timeout: int, env: dict,
                transcription: bool):
    """转换单个文件（只用本地编译的 CLI，不做任何回退）。返回 (md_text, meta, elapsed, returncode, used_cli)。"""
    stem = src_file.stem
    img_dir = out_dir / f"{stem}_images"
    # 上一次运行留下的图片必须清空：残留文件会掩盖真实丢图（IMG_LOST/IMG_MISSING 都以
    # 该目录内容为判据），中断运行留下的半截文件还会误报 IMG_CORRUPT。
    shutil.rmtree(img_dir, ignore_errors=True)
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
                         "cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api")
        first_err = next((l for l in err_text.splitlines()
                          if re.search(r"ERROR|Error|error \[", l)), err_text.splitlines()[:1])
        return ("", {"warnings": [f"退出码 {rc}: {first_err}"], "counts": {},
                     "languages": [], "notes": notes},
                elapsed, rc, cli)

    try:
        result, meta = parse_envelope(out)
    except Exception as parse_error:  # noqa: BLE001 - 单个文件的解析失败不能中止整轮验收
        # stdout 混入非 JSON 行、或 envelope 缺 result 时，按该文件失败处理（走 rc!=0 的
        # 判定路径），而不是抛 traceback 让整轮报告写不出来。
        notes.append(f"JSON 解析失败: {parse_error}")
        return ("", {"warnings": [f"JSON 解析失败: {parse_error}"], "counts": {},
                     "languages": [], "notes": notes},
                elapsed, 1, cli)
    md_text = result.get("content", "") or ""
    n_imgs = write_images_from_json(result, img_dir)
    meta["notes"] = notes + ([f"由 Python 从 JSON 内联数据落盘 {n_imgs} 张图片"] if n_imgs else [])
    save_markdown(out_dir, stem, md_text, img_dir.name)
    return md_text, meta, elapsed, rc, cli


# ---------------------------------------------------------------- 报告
def _fmt_issues(issues):
    return "; ".join(f"[{i['code']}] {i['message']}" for i in issues)


def report_file(name, verdict, m, recall_info, meta, elapsed, issues, used_cli, ocr_info=None):
    ok = "✅" if verdict == "PASS" else ("⚠️ " if verdict == "WARN" else "❌")
    emit(f"\n{'='*72}")
    emit(f"{ok} [{verdict}] {name}   ({elapsed:.1f}s)")
    emit(f"  内容: {m['chars']} 字符 | 非空行 {m['nonempty']} | 标题 {m['headings']} | "
         f"表格行 {m['table_rows']} | 图片引用 {m['img_refs']}(落盘 {m['imgs_on_disk']}) | "
         f"代码围栏 {m['fences']} | 表格块 {m.get('tables', 0)}")
    if m.get("max_page"):
        emit(f"  MD页标题: {len(m.get('page_titles') or [])} 个 | 最大 Page {m['max_page']}")
    if ocr_info:
        want = " (期望 paddle-ocr/pp-ocrv6 或含 chi_sim)" if ocr_info.get("expect_chinese") else ""
        emit(f"  OCR 通道: backend={ocr_info.get('backend') or '-'} "
             f"language={ocr_info.get('language') or '-'}{want}")
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
    elif ri.get("src_images_note"):
        emit(f"  源内嵌图片数: 跳过 [{ri['src_images_note']}]")
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
                     "请先执行: cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api\n"
                     f"  CLI: {cli}")
        print("[preflight] transcription feature 就绪（音视频将用本地编译版转写）", flush=True)
    else:
        print("[preflight] 无音视频文件，跳过 transcription 探测", flush=True)


def main():
    global RECALL_GOOD, RECALL_FAIL, NUM_RECALL_FAIL, EXPECTATIONS, OCR_CONFIG, LAYOUT_CONFIG

    ap = argparse.ArgumentParser(description="xberg Markdown 转换集成验收")
    ap.add_argument("--cli", default=str(DEFAULT_CLI))
    ap.add_argument("--src", default=str(DEFAULT_SRC))
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--pkg-dir", default=str(PKG_DIR),
                    help="提供 models/ 的目录（用于 HF_HUB_CACHE 与 PATH），默认仓库内打包目录")
    ap.add_argument("--keep-going", action="store_true", help="遇到 FAIL 不终止，继续测完")
    ap.add_argument("--strict", action="store_true",
                    help="WARN 也视为验收失败（CI 门禁用）")
    ap.add_argument("--recall-fail", type=float, default=RECALL_FAIL,
                    help=f"bigram 召回率 FAIL 阈值（默认 {RECALL_FAIL}）")
    ap.add_argument("--recall-good", type=float, default=RECALL_GOOD,
                    help=f"bigram 召回率「良好」阈值（默认 {RECALL_GOOD}）")
    ap.add_argument("--num-recall-fail", type=float, default=NUM_RECALL_FAIL,
                    help=f"数字 token 召回率 FAIL 阈值（默认 {NUM_RECALL_FAIL}）")
    ap.add_argument("--expectations", default=str(EXPECTATIONS_DEFAULT),
                    help="逐文件金标准 JSON（仓外文件；不存在则跳过金标准检查）")
    ap.add_argument("--baseline", default=str(BASELINE_DEFAULT),
                    help="回归基线 JSON（仓外文件；不存在则跳过对比）")
    ap.add_argument("--save-baseline", action="store_true",
                    help="跑完后把本次结果写入基线文件")
    ap.add_argument("--no-expectations", action="store_true", help="强制跳过金标准检查")
    args = ap.parse_args()
    RECALL_FAIL = args.recall_fail
    RECALL_GOOD = args.recall_good
    NUM_RECALL_FAIL = args.num_recall_fail
    EXPECTATIONS = {} if args.no_expectations else load_expectations(Path(args.expectations))
    run_cfg = EXPECTATIONS.get("run") or {}
    OCR_CONFIG = run_cfg.get("ocr_config") or {}
    # 键存在即注入（{} 表示用引擎默认开 layout）；不写该键 = 不碰 layout
    LAYOUT_CONFIG = run_cfg["layout_config"] if "layout_config" in run_cfg else None
    if OCR_CONFIG:
        print(f"[expectations] OCR 覆盖配置: {json.dumps(OCR_CONFIG, ensure_ascii=False)}",
              flush=True)
    if LAYOUT_CONFIG is not None:
        print(f"[expectations] layout 覆盖配置: {json.dumps(LAYOUT_CONFIG, ensure_ascii=False)}",
              flush=True)

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
        # 引擎所有模型都走 hf-hub，缓存根目录是 HF_HUB_CACHE；HF_HOME 会被解析成
        # $HF_HOME/hub，指向包内不存在的子目录，等于让引擎回退到用户缓存或联网下载。
        env["HF_HUB_CACHE"] = str(pkg_dir / "models")

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
        # 超时记录也要带 golden.applied，否则基线与本次的问题码集合按「未加载金标准」剔除，
        # 该文件会显示成一批假「已修复」。
        exp = expect_for(f.name)
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
                # 与正常路径保持同一组键：按统一 schema 读取 _quality-report.json 的消费方
                # 不会在超时文件上 KeyError，也无需区分「超时」与「字段缺失」。
                "char_ratio": None, "src_images": None, "src_images_note": None,
                "source_pages": None,
                "metrics": json_metrics(m), "issues": issues,
                "warnings": meta.get("warnings") or [], "notes": meta.get("notes") or [],
                "golden": {"applied": bool(exp)},
                "ocr": None, "counts": {}, "extraction_method": None,
            })
            progress_line(i, "TIMEOUT", elapsed, t_start)
            if not args.keep_going:
                stopped_early, early_reason = True, f"{f.name} 超时"
                break
            continue

        m = structural_metrics(md_text, img_dir)
        issues = []
        ocr_info = None
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
            judge_markdown_semantics(md_text, m, issues, exp, src_file=f)
            judge_duplication(md_text, issues)
            judge_code_tables(md_text, issues, exp)
            judge_emphasis_noise(md_text, issues, exp)
            judge_running_head(md_text, issues, exp)
            ocr_info = judge_ocr_channel(
                f, md_text, out_dir / f"{f.stem}.err.txt", issues, exp, OCR_CONFIG, m=m)

        recall_info = {"note": None, "bigram": None, "num_recall": None,
                       "ident_recall": None, "missing_nums": [], "missing_id": [],
                       "char_ratio": None, "src_images": None, "src_images_note": None,
                       "recall_good": RECALL_GOOD, "recall_fail": RECALL_FAIL}
        source_pages = None
        if rc == 0:
            print("  抽取源文本基准并计算召回率 ...", flush=True)
            src_text, note, extras = extract_source_text(f)
            source_pages = extras.get("pages") if extras else None
            src_img_n, src_img_note = count_source_images(f)
            recall_info["src_images"] = src_img_n
            if src_img_note and src_img_n is None:
                # 探测失败只记录，不阻断——但必须把原因带进报告：否则 src_images=null 与
                # 「该格式本来就没有基准抽取器」不可区分，四方图片对账会静默失效。
                recall_info["src_images_note"] = src_img_note
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

            # 源文保真（内嵌子文档/标题备注/图形文本/书签/金标准）
            if src_text is not None:
                judge_hash_glyph(src_for_tokens, md_text, issues)
                judge_ident_fragmentation(src_for_tokens, md_text, issues)
            judge_source_fidelity(f, md_text, src_text, m, issues, exp, src_img_n)
            judge_toc_levels(f, md_text, issues)
            judge_xlsx_cell_folding(f, md_text, issues, exp)
            judge_bullet_levels(f, md_text, issues, exp)

        verdict = issues_to_verdict(issues)
        report_file(f.name, verdict, m, recall_info, meta, elapsed, issues, used_cli,
                    ocr_info=ocr_info)
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
            "src_images_note": recall_info.get("src_images_note"),
            "source_pages": source_pages,
            "metrics": json_metrics(m),
            "ocr": ocr_info,
            "golden": {"applied": bool(exp)},
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

    # 回归对比：只呈现在报告里，不追加 issue、不影响 verdict
    prev_baseline = load_baseline(Path(args.baseline))
    regression = compare_baseline(prev_baseline, JSON_RESULTS) if prev_baseline else None
    if regression is None:
        emit("\n## 与基线对比")
        emit("  （未找到基线文件，未做回归对比；用 --save-baseline 生成）")
    else:
        emit(f"\n## 与基线对比（基线: {args.baseline}，"
             f"生成于 {prev_baseline.get('generated_at') or '未知时间'}）")
        if regression.get("skipped_exp_codes"):
            emit("  （本次有文件未加载金标准：其依赖金标准的检查码"
                 "（GOLDEN_*/OCR/阈值类）已从对比中剔除）")
        for name, d in regression["files"].items():
            emit(f"  {name}: 新增 [{', '.join(d['new'])}] | 已修复 [{', '.join(d['fixed'])}]")
        emit(f"  合计: 新增 {regression['totals']['new']} 项 | "
             f"已修复 {regression['totals']['fixed']} 项")

    if args.save_baseline:
        Path(args.baseline).write_text(json.dumps({
            "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
            "cli": str(cli),
            "files": [
                {"name": r["name"], "verdict": r["verdict"],
                 "golden_applied": bool((r.get("golden") or {}).get("applied")),
                 "issues": [{"code": i["code"], "message": i["message"]}
                            for i in r["issues"]],
                 "metrics": r["metrics"]}
                for r in JSON_RESULTS
            ],
        }, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"回归基线已保存: {args.baseline}")

    emit(f"\n总耗时 {time.time()-t_start:.0f}s")
    header = (f"# xberg 转换质量报告\n\n- CLI: `{cli}`\n- 源目录: `{src}`\n"
              f"- 生成时间: {time.strftime('%Y-%m-%d %H:%M:%S')}\n"
              f"- 阈值: recall_fail={RECALL_FAIL} recall_good={RECALL_GOOD} "
              f"num_recall_fail={NUM_RECALL_FAIL} char_ratio={CHAR_RATIO_FAIL}/{CHAR_RATIO_WARN} "
              f"page_cover={PAGE_COVER_FAIL}/{PAGE_COVER_WARN} strict={args.strict}\n"
              f"- 金标准: {args.expectations}（{len(EXPECTATIONS.get('files') or {})} 个文件条目；"
              f"每个文件应用的键见报告内问题码）\n")
    report_path = out_dir / "_quality-report.md"
    report_path.write_text(header + "\n".join(REPORT_LINES) + "\n", encoding="utf-8")
    json_path = out_dir / "_quality-report.json"
    json_path.write_text(json.dumps({
        "cli": str(cli),
        "src": str(src),
        "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "expectations_path": args.expectations,
        "ocr_requested": OCR_CONFIG,
        "layout_requested": LAYOUT_CONFIG,
        "regression": regression,
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
