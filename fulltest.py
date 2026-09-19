#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""fulltest.py — xberg 集成验收：把测试目录下所有文件转换为 Markdown 并做可机读质量判定。

目标：不靠人工读正文，也能判断「转换是否可接受」。

五层检查（每层都独立出码）：
  1. 进程/结构 —— CLI 退出码、空结果、乱码/控制字符/格式泄漏、Markdown 围栏与表格列、
     图片引用可解析性、落盘图片文件合法性（大小 + magic bytes）
  2. 源文对齐   —— 基准去页眉页脚后 bigram 召回 + 数字/标识边界匹配 + 正文字符量比值 +
     分页/分片内容覆盖（源页有字而 MD 几乎对不上 → FAIL）
  3. 交叉核对   —— 源页数 vs 引擎 counts vs `## Page N`；源文件媒体清单 vs 引擎图片数 vs
     磁盘落盘 vs MD 引用（四方对账）；音视频文件体积 vs 转写文本量
  4. 深检       —— Markdown 语义噪声、内嵌对象与子文档保真、PPTX 标题/备注、xlsx 图形文本、
     PDF 书签与表格数、OCR 通道、逐文件金标准断言（_expectations.json）
  5. 失败路径   —— 对抗语料 `_adversarial/`（源目录下子目录，损坏/截断/空文件）主队列后
     追加：必须优雅失败（非零退出+诊断）或干净转换，panic/静默失败/吐垃圾都判红（ADV_* 码）

用法:
    python fulltest.py [--cli PATH] [--src DIR] [--out DIR] [--timeout SECS]
                       [--keep-going] [--strict]
                       [--recall-fail F] [--recall-good F] [--num-recall-fail F]
                       [--save-baseline [--force]] [--selftest]

附加机制:
    - `--selftest`         判定器自测（合成样例，不需要 CLI/语料/金标准）
    - 金标准加载自检        未知键/pattern 卫生告警；报告与基线记录金标准 sha256 与代码 commit
    - `--save-baseline` 护栏 存在 FAIL、金标准未加载或本轮提前终止（半截结果）时拒绝保存
                           （`--force` 覆盖）；基线对比含「恶化」计数（同码次数增加）

默认调用 target\\debug\\xberg.exe，输出到 D:\\测试转markdown转换效果\\测试文档_md_fulltest。
每个文件打印质量报告；结束时写 `_quality-report.md` 与 `_quality-report.json`。
默认遇 FAIL 立即终止；`--keep-going` 跑完；`--strict` 把 WARN 也算失败（适合 CI 验收门禁）。
"""

import argparse
import hashlib
import html
import json
import os
import re
import shutil
import subprocess
import sys
import time
import unicodedata
from collections import Counter
from pathlib import Path

# ---------------------------------------------------------------- 默认配置
# 仓库根 = 本脚本所在目录（曾硬编码为 D:\code1111111111\xberg，仓库移动到 E:\xberg 后
# 该路径变成空目录，e2e 门在预检就报「找不到 CLI」）。测试语料与输出仍在 D:\ 上。
REPO = Path(__file__).resolve().parent
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
    "IMG_LOST": "FAIL",         # 唯一引用目标数 > 落盘数（丢图；同一图片多次引用不算）
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
    "JUDGE_CRASH": "FAIL",      # 判定器自身异常——单文件问题不得放大为整轮崩溃，报告与基线对比仍可完成
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
    # 失败路径（对抗语料 _adversarial/：损坏/截断/空文件必须优雅失败，不得 panic）
    "ADV_PANIC": "FAIL",             # 对抗文件触发引擎 panic/backtrace
    "ADV_SILENT_FAIL": "FAIL",       # 对抗文件非零退出但无任何诊断输出
    "ADV_GARBAGE_OK": "FAIL",        # 对抗文件「成功」但输出含 FAIL 级结构问题
    "ADV_EMPTY_OK": "WARN",          # 对抗文件「成功」且输出为空/过短（信息可见，不阻断）
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
        # openpyxl 只支持 xlsx 系；这里只放行 xlsx——图形文本/单元格折叠等深检门
        # 严格只认 xlsx，半放开 xlsm 会造成「召回层绿了、深检层静默缺失」的假全检
        # （zip 级的图片对账/内嵌保真另收 ods，不依赖 openpyxl）。其余表格格式落到
        # 函数末尾的「无对应基准抽取器」路径。
        if ext == "xlsx":
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

# 嵌入对象文本区：`Embedded object: <name>` caption 行之后、直到下一个结构性元素
# （标题/围栏/表格行/图片行/下一个 caption）之前的松散短行是内嵌流程图的框标签
# （ATPG/DFT 实测：eddx 流程图 14 个同名框 ARCH41_CORE4 各排一行，是源事实而非
# 页眉刷屏）——重复行统计跳过该区间。
EMBED_CAPTION_RE = re.compile(r"^Embedded object:\s+\S")

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
# 金标准文件指纹与加载期自检结果（load_expectations 填充；进报告与基线，供篡改可见性）
EXPECTATIONS_SHA256: str | None = None
EXPECTATIONS_WARNINGS: list = []


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
    """源文本字符 bigram 在 markdown 中的覆盖率（0~1）。空源返回 -1 表示无法评估。

    两侧先剔除 `|` 再做空白归一：MD 表格的单元格分隔符是渲染结构不是内容——
    纯表格源（xlsx 逐单元格拼接）里相邻单元格构成的 bigram 会被 MD 必然插入的
    `|` 打断，造成结构性低估（实测 DFT收集方案 84.3% → 剔除后 98.2%，缺失
    区间全部 ≤4 个 bigram、形态为「版本|日期|修订」跨格拼接，无真丢内容）。
    """
    src, out = _norm_ws(source.replace("|", "")), _norm_ws(md.replace("|", ""))
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


def _is_windows_noise(path: Path) -> bool:
    """资源管理器/Office 在语料目录留下的系统垃圾文件。

    图片密集目录会被 Thumbs.db 跟踪，打开过的 Word 文档留下 ~$ 锁文件；它们进入
    主队列只会产出 CMD_FAILED 假红并触发默认提前终止，与转换质量无关。
    """
    name = path.name
    return (name.lower() in {"thumbs.db", "desktop.ini", ".ds_store"}
            or name.startswith("~$"))


def structural_metrics(md_text: str, img_dir: Path):
    lines = md_text.splitlines()
    # 围栏内外先拆开（本函数后半段深检度量也要用）：图片引用对账只看围栏外——
    # 围栏内的 ![](...) 是代码/OCR 字面量示例，不是引用；外链目标也排除，因为
    # Path("https://example.com/a/diagram.png").name 在 Windows 会解析出
    # "diagram.png"，与落盘集合比对产生假 IMG_MISSING。
    outside_lines, fenced_blocks = split_fenced(md_text)
    outside_text = "\n".join(outside_lines)
    chars = len(md_text)
    headings = sum(1 for l in lines if re.match(r"^#{1,6}\s", l))
    table_rows = sum(1 for l in lines if l.lstrip().startswith("|"))
    # 图片引用只数本地目标：外链（http(s):/data: 等 scheme）不是落盘资产，计入会让
    # IMG_LOST 假红、judge_source_images 的 recovered 虚高掩盖真丢图——与 broken_refs
    # 的排除口径对齐。目标解析遵循 CommonMark：剥 <> 包裹、可选 title 取首段。
    ref_targets = []
    for _ref in re.findall(r"!\[[^\]]*\]\(([^)]+)\)", outside_text):
        target = _ref.strip()
        if target.startswith("<") and ">" in target:
            target = target[1:target.index(">")]
        else:
            target = target.split()[0] if target.split() else ""
        if target:
            ref_targets.append(target)
    local_ref_targets = [t for t in ref_targets
                         if not re.match(r"[A-Za-z][A-Za-z0-9+.\-]*:", t)]
    img_refs = len(local_ref_targets)
    # 唯一目标数：与「落盘文件数」同量纲。同一图片的多次引用（页眉 logo 每页一次）
    # 是合法形态，按出现次数对账会把 2 次引用 1 份文件误判成丢图。
    img_refs_unique = len(set(local_ref_targets))
    fences = sum(1 for l in lines if l.lstrip().startswith("```"))
    nonempty = sum(1 for l in lines if l.strip())
    empty = len(lines) - nonempty
    replacement = md_text.count("\ufffd")
    ctrl = sum(1 for c in md_text if ord(c) < 32 and c not in "\n\r\t")
    img_dir_files = [p for p in img_dir.iterdir() if p.is_file()] if img_dir.is_dir() else []
    img_names = {p.name for p in img_dir_files}
    broken_refs = [ref for ref in local_ref_targets
                   if Path(ref).name not in img_names]
    # 重复非平凡行（页眉页脚刷屏）。表行/标题/围栏不算——xlsx 合法重复行很常见。
    line_counts = Counter()
    in_fence = False
    in_embed = False
    for l in lines:
        raw = l.strip()
        if raw.startswith("```"):
            in_fence = not in_fence
            in_embed = False
            continue
        if in_fence or not raw:
            continue
        if raw.startswith("|") or raw.startswith("#"):
            in_embed = False                  # 结构性行结束嵌入对象区间
            continue
        if raw.startswith("!["):
            in_embed = False
            continue
        if EMBED_CAPTION_RE.match(raw):
            in_embed = True                   # caption 行开启嵌入对象区间
            continue
        if in_embed:
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
    # （outside_lines / fenced_blocks 已在函数开头拆好，图片对账与深检共用同一次拆分）
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
        "img_refs": img_refs, "img_refs_unique": img_refs_unique,
        "fences": fences, "nonempty": nonempty, "empty": empty,
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
        "img_refs": m["img_refs"], "img_refs_unique": m["img_refs_unique"],
        "imgs_on_disk": m["imgs_on_disk"],
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
    # 丢图：唯一引用目标多于落盘。落盘多于引用不算问题（PPTX 剥重复装饰图引用，
    # 数据仍保留）；引用按唯一目标计——同一图片多次合法引用不是丢图，具体哪条
    # 引用断了由 IMG_MISSING 逐条对账。
    if m["img_refs_unique"] > m["imgs_on_disk"]:
        issues.append(make_issue(
            "IMG_LOST",
            f"图片引用({m['img_refs_unique']}个唯一目标)多于导出图片数({m['imgs_on_disk']})"))
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
    # 引用按唯一目标计：与磁盘/元数据同为「图片个数」量纲——出现次数会把单图
    # 多次引用虚高成恢复量，掩盖真实缺口。
    refs = m["img_refs_unique"]
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
    # exp 上限替换默认阈值（放行方向：exp 存在时以 exp 为准，缺省回退 >=ESCAPE_NOISE_MIN，
    # 与旧判定一致）；收紧方向另有 GOLDEN_METRIC 的 max_escapes 规格（metric_specs 读同一
    # 键、同一 m["escapes"] 口径），两处不会互相矛盾。
    esc_limit = _exp_int(exp, "max_escapes")
    if n_esc > (esc_limit if esc_limit is not None else ESCAPE_NOISE_MIN - 1):
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
    """内嵌子文档 / embeddings 下直接内嵌的图片：返回 [(zip 内完整路径, md5, 扩展名), …]。

    用完整路径而不是 basename 作标识：不同子文档常各自带同名 media（image1.png），
    basename 键会让像素回退比对拿到另一张图的字节（跨子文档同名互相覆盖）。
    """
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
                    out.append((name,
                                hashlib.md5(z.read(name)).hexdigest(),
                                low.rsplit(".", 1)[-1]))
                elif low.endswith((".docx", ".xlsx", ".pptx")):
                    try:
                        with zipfile.ZipFile(io.BytesIO(z.read(name))) as z2:
                            for n2 in z2.namelist():
                                if "/media/" in n2.lower() and n2.lower().endswith(exts):
                                    out.append((f"{name}!{n2}",
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


def _exempt_media_hashes(path: Path):
    """顶层文档自身 media 的 md5/像素哈希——EMBED_MEDIA_LOST 的跨子文档豁免集。

    子文档页眉 rels 引用的图片常与顶层文档 media 里同一张图字节完全相同
    （第九课 docx 实测：子文档页眉 image8/14.jpeg ≡ 主文档页眉 image34.jpeg，
    sha256 1d0fd52c…，36135B，仅被页眉 rels 引用）。同字节图片已属于顶层文档
    的媒体清单（顶层侧丢图由 IMG_SRC_GAP/IMG_SRC_EMPTY 对账把守），不应因子
    文档页眉再引用一次就报「子文档图片丢失」。
    """
    import hashlib, zipfile
    prefix = {"docx": "word/media/", "pptx": "ppt/media/",
              "xlsx": "xl/media/"}.get(path.suffix.lower().lstrip("."))
    md5s, pxs = set(), set()
    if not prefix:
        return md5s, pxs
    try:
        with zipfile.ZipFile(path) as z:
            for name in z.namelist():
                low = name.lower()
                if low.startswith(prefix) and low.endswith(
                        (".png", ".jpg", ".jpeg", ".bmp")):
                    data = z.read(name)
                    md5s.add(hashlib.md5(data).hexdigest())
                    px = _pixel_hash(data)
                    if px:
                        pxs.add(px)
    except Exception:
        return set(), set()
    return md5s, pxs


def _embedded_media_lost(path: Path, m):
    """内嵌图片是否落盘。PNG 先比字节 md5，其余/重编码过的一律比解码像素。

    豁免集有两路：① 本文件落盘图片（引擎可能把同一张图去重后只写一份）；
    ② 顶层文档自身 media（`_exempt_media_hashes`，跨子文档共享的页眉装饰图）。
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
                    raw[name] = z.read(name)
                elif low.endswith((".docx", ".xlsx", ".pptx")):
                    try:
                        with zipfile.ZipFile(io.BytesIO(z.read(name))) as z2:
                            for n2 in z2.namelist():
                                if "/media/" in n2.lower() and n2.lower().endswith(
                                        (".png", ".jpg", ".jpeg", ".bmp")):
                                    raw[f"{name}!{n2}"] = z2.read(n2)
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
    top_md5, top_px = _exempt_media_hashes(path)
    lost = []
    for name, md5, _ext in media:
        if md5 in disk_md5 or md5 in top_md5:
            continue
        px = _pixel_hash(raw.get(name, b"")) if name in raw else None
        if px and (px in disk_px or px in top_px):
            continue
        # 报告里显示成可读的 basename（完整路径仅作内部键，防跨子文档同名覆盖）。
        lost.append(name.rsplit("/", 1)[-1].rsplit("!", 1)[-1])
    return lost, len(media)


def _toc_title_key(title: str) -> str:
    """标题/书签与 `_md_headings` 的比对 key：剥强调/代码标记再空白归一。

    `_md_headings` 侧已剥 `*`/`_`/`` ` ``/`~`；标题侧 key 若只做空白归一，
    标识符常态的下划线（Func_mbist / DFT_TOP / scan_out）会被 MD 侧剥 `_` 后的
    归一化文本永久错开——内容明明都在，却恒判「缺失」（判定器 bug，非引擎丢
    内容）。PPTX 幻灯片标题（第三课实测 5 张「丢失」标题全部带下划线，其实都
    以 `## Func_mbist` 形式存在）与 PDF 书签（tessent 手册书签 scan_out 同病）
    共用本 key。
    """
    return _norm_ws(re.sub(r"[*_`~]", "", title or ""))


def _pptx_title_key(title: str) -> str:
    """幻灯片标题的比对 key：`_toc_title_key` 归一化后截前 20 字。"""
    return _toc_title_key(title)[:20]


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
            key = _pptx_title_key(title)
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
    """xlsx 浮动图形（drawing xml）里的文本是否进入 MD。

    `a:t` 是 XML 文本节点，`<`/`>`/`&` 以实体形式存储（`&lt;`/`&gt;`/`&amp;`），
    比较前必须解码：拿 `&gt;` 这类字面量去比对，旧引擎输出的实体垃圾 `\\&gt;`
    恰好含该子串=假 PASS，一旦引擎改为输出正确的 `\\>` 反而报「丢失」。
    长度过滤仍按原始文本（候选集口径不变，避免把 `&gt;` 解码成 1 字符后被静默跳过）。
    """
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
    cand = [(raw, html.unescape(raw)) for raw in texts if len(raw) >= 2]
    if not cand:
        return
    md_nos = _norm_ws(md_text)
    lost = [dec for _raw, dec in cand if _norm_ws(dec) not in md_nos]
    if not lost:
        return
    ratio = len(lost) / len(cand)
    issues.append(make_issue(
        "XLSX_SHAPE_TEXT_MISSING",
        f"xlsx 浮动图形文本丢失 {len(lost)}/{len(cand)}({ratio:.0%}): "
        f"{', '.join(lost[:3])}",
        severity=None if ratio >= XLSX_SHAPE_LOST_RATIO else "WARN"))


def _toc_heading_miss(entries, md_text):
    """书签条目中未以标题形式出现的标题（比对 key 与 `_md_headings` 同归一化）。

    同 `_pptx_title_key` 的判定 bug：书签标题含 `_`（如 scan_out）时只做空白归一
    会在 MD 侧（已剥 `_`）恒不命中，TOC_HEADING_GAP 假性偏高。
    """
    heads = [h for h, _n, _raw in _md_headings(md_text)]
    return [str(t) for _lvl, t, _pg in entries
            if _toc_title_key(str(t))
            and not any(_toc_title_key(str(t)) in h for h in heads)]


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
            miss = _toc_heading_miss(entries, md_text)
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


GOLDEN_CJK_TOKEN_MAX_GAP = 6  # CJK token 相邻字符间允许的交错字符数（图形框/OCR 版面交错）


def _golden_token_hit(token: str, md_nos: str) -> bool:
    """金标准 token 命中判定：空白归一后整串命中；中文 token 再容忍版面交错。

    图形/OCR 的保版面输出会把不同文本框交错进同一行（已知不自动判定形态③
    「中文词内部被插空」的交错变体），token 字符齐全且有序却不再连续。对含
    中文的 token 允许相邻字符之间夹最多 GOLDEN_CJK_TOKEN_MAX_GAP 个其他字符
    （须全部按序出现，窗口有界，不会把「宏连接」误配成无关文本）。纯拉丁
    token（标识符/短语）仍要求整串连续，避免子串虚高；混合 token 的拉丁
    片段与中文字符同样逐字容差——版面交错对拉丁片段一样打乱顺序。

    逐字匹配用可达位置集合的线性 DP：reach 是「前缀已按 ≤GAP 间隔匹配到
    该字符」的全部结束位置，与 `.{0,GAP}` 逐字正则的回溯语义严格等价（正则
    能找到一条链 ⟺ DP 的 reach 非空走完全串；生产域内 md_nos 已剥除全部
    空白，正则 `.` 不跨行的差异不会出现），但没有回溯引擎在「token 前
    缀高频重复字 + 文本同字长 run + token 真缺失」叠加时的指数回溯形态——
    DP 的代价上界是 O(文本长 × token 长)，最坏也只是慢，不会挂起。
    """
    t = _norm_ws(token)
    if t in md_nos:
        return True
    if len(t) < 2 or not re.search(r"[\u4e00-\u9fff]", t):
        return False
    reach = {i for i, ch in enumerate(md_nos) if ch == t[0]}
    for ch in t[1:]:
        reach = {j for i in reach for j in range(i + 1, i + GOLDEN_CJK_TOKEN_MAX_GAP + 2)
                 if j < len(md_nos) and md_nos[j] == ch}
        if not reach:
            return False
    return True


def _judge_expectations(md_text: str, m, issues, exp):
    """按逐文件金标准断言：必需文本 / 禁止形态 / 顺序 / 数量指标。"""
    if not exp:
        return
    md_nos = _norm_ws(md_text)
    req = exp.get("required_tokens") or []
    missing = [str(t) for t in req if not _golden_token_hit(str(t), md_nos)]
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
    # order 与 required_tokens 同口径：剥掉 *、_、`、~ 等强调标记后再去空白。标题常被渲染成
    # `## **Fault** 类型`，原始子串匹配会被 ** 挡住，把「顺序正确」误判成 GOLDEN_ORDER。
    # 只做最低限度归一，不做 CJK 交错容差（交错是 _golden_token_hit 的专属语义）。
    md_order = _norm_ws(re.sub(r"[*_`~]+", "", md_text))
    for pair in exp.get("order") or []:
        if not isinstance(pair, (list, tuple)) or len(pair) != 2:
            continue
        a, b = str(pair[0]), str(pair[1])
        ia, ib = md_order.find(_norm_ws(a)), md_order.find(_norm_ws(b))
        if ia < 0 or ib < 0 or ia >= ib:
            bad_order.append(f"{a[:20]} 应在 {b[:20]} 之前")
    if bad_order:
        issues.append(make_issue("GOLDEN_ORDER", "; ".join(bad_order)))
    metric_specs = (
        ("min_headings", "headings", "min"), ("max_headings", "headings", "max"),
        ("min_tables", "tables", "min"), ("max_tables", "tables", "max"),
        ("min_chars", "chars", "min"), ("max_chars", "chars", "max"),
        # 图片个数量纲 = 唯一引用目标（与 IMG_LOST/四方对账同口径）；出现次数会把
        # 单图多次引用虚高成图片数。阈值按唯一口径实测校准（ATPG中小特性方案.docx
        # 实测出现 27 = 唯一 27，min_images=22 在两口径下等价）。
        ("min_images", "img_refs_unique", "min"), ("max_images", "img_refs_unique", "max"),
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
    # exp 上限替换默认阈值（放行方向）；exp 缺省回退 >=HIGHLIGHT_MIN，与旧判定一致。
    # 旧判定 `hl >= MIN or (mx 有且 hl > mx)` 的问题在 mx >= MIN 时：MIN 那条腿先放行，
    # 期望的上限形同虚设（如 max_highlights=10、hl=6 仍报），替换后 exp 才真正生效。
    if hl > (mx if mx is not None else HIGHLIGHT_MIN - 1):
        issues.append(make_issue(
            "NONSTD_HIGHLIGHT",
            f"非标准 ==高亮== 标记 {hl} 对(如 {sample[:30]})"
            + _exp_msg(exp, "max_highlights", hl)))


def _toc_level_stats(entries, md_text):
    """书签标题 vs MD 标题 # 层数（容差 ±1）的核对统计，返回 (checked, mismatch)。

    命中口径与 `_toc_heading_miss` 相同（`_toc_title_key` 归一化），保证
    TOC_LEVEL_MISMATCH 的 checked 子集不漏掉带下划线等标记的书签。
    """
    heads = _md_headings(md_text)
    checked = mism = 0
    for level, title, _page in entries:
        key = _toc_title_key(str(title))
        if not key:
            continue
        for h, n_hash, _raw in heads:
            if key in h:
                checked += 1
                lo, hi = (1, 2) if level <= 1 else (2, 3)
                if not (lo <= n_hash <= hi):
                    mism += 1
                break
    return checked, mism


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
        checked, mism = _toc_level_stats(entries, md_text)
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
# 判定代码实际读取的逐文件键（含 metric_specs 与各 judge 的 exp.get/_exp_int/_exp_msg 全量）。
# 维护约定：新增读取 exp 的键必须同步登记；拼错的键 = 检查静默不生效（全绿假象），
# 加载期自检会按本表对未知键告警。`_note` 前缀是给人看的推导依据，豁免。
KNOWN_FILE_KEYS = {
    "max_pseudo_headings", "max_escapes", "max_dash_lines", "max_merged_cells",
    "max_page_footer_body", "require_chinese_ocr", "min_cjk", "min_fenced_cjk",
    "toc_heading_min_recall", "required_tokens", "forbidden_patterns", "order",
    "min_headings", "max_headings", "min_tables", "max_tables", "min_chars",
    "max_chars", "min_images", "max_images", "max_line_len", "max_code_tables",
    "max_highlights", "max_folded_cells", "max_bold_short_lines",
    "require_nested_bullets",
}
KNOWN_RUN_KEYS = {"ocr_config", "layout_config"}
KNOWN_TOP_KEYS = {"version", "note", "files", "run"}

# 期望键的值类型契约：键存在但类型写错时检查同样静默失效——字符串 "3" 不是阈值
# （_exp_int 只认 int）、字符串 "false" 在 truthy 读取下恒为真、裸字符串
# forbidden_patterns 会被逐字符迭代成单字符正则。与键名拼错同等可见。
_EXP_LIST_KEYS = {"required_tokens", "forbidden_patterns", "order"}
_EXP_BOOL_KEYS = {"require_chinese_ocr", "require_nested_bullets"}
_EXP_NUMBER_KEYS = {"toc_heading_min_recall"}


def _validate_expectations(data: dict) -> list:
    """金标准自检：未知键 / pattern 卫生 / 值类型。返回配置级告警（打印并进报告，不进逐文件 issues）。

    动机：期望文件在仓外、无版本控制——键名拼错 = 检查静默不生效；JSON 的 \\b 是退格
    转义，曾把 forbidden 正则变成「退格字面量」，回归守卫永远打不中（2026-09-14 实例）。
    """
    warns = []
    if not isinstance(data, dict):
        return ["金标准顶层不是 JSON 对象"]
    for k in data:
        if k not in KNOWN_TOP_KEYS:
            warns.append(f"顶层未知键「{k}」（判定代码不读取）")
    files = data.get("files")
    for name, ent in files.items() if isinstance(files, dict) else []:
        if not isinstance(ent, dict):
            warns.append(f"{name}: 文件条目不是对象")
            continue
        for k in ent:
            if k.startswith("_note") or k in KNOWN_FILE_KEYS:
                continue
            warns.append(f"{name}: 未知键「{k}」（判定代码不读取，疑似拼写错误→检查静默不生效）")
        for k, v in ent.items():
            if k.startswith("_note"):
                continue
            if k in _EXP_LIST_KEYS and not isinstance(v, list):
                warns.append(f"{name}: 键「{k}」应为数组，实为 {type(v).__name__}"
                             "（消费口径改变→检查静默失效或乱报）")
            elif k in _EXP_BOOL_KEYS and not isinstance(v, bool):
                warns.append(f"{name}: 键「{k}」应为布尔，实为 {type(v).__name__}"
                             "（truthy 读取下非空值恒为真）")
            elif k in _EXP_NUMBER_KEYS and (isinstance(v, bool) or not isinstance(v, (int, float))):
                warns.append(f"{name}: 键「{k}」应为数值，实为 {type(v).__name__}")
            elif (k in KNOWN_FILE_KEYS and k not in _EXP_LIST_KEYS
                  and k not in _EXP_BOOL_KEYS and k not in _EXP_NUMBER_KEYS
                  and (isinstance(v, bool) or not isinstance(v, int))):
                warns.append(f"{name}: 键「{k}」应为整数阈值，实为 {type(v).__name__}"
                             "（_exp_int 只认 int→检查静默不生效）")
        order = ent.get("order")
        if isinstance(order, list):
            for pair in order:
                if not (isinstance(pair, list) and len(pair) == 2
                        and all(isinstance(x, str) for x in pair)):
                    warns.append(f"{name}: order 元素应为两个字符串的数组，实为 {pair!r}"
                                 "（GOLDEN_ORDER 跳过该对→顺序守卫静默消失）")
        # list 键写成真值标量（true/1）时上面的类型契约告警已记，这里跳过迭代——
        # 直接 `or []` 会对 int/bool 迭代抛 TypeError，把整份配置自检炸掉。
        forbidden = ent.get("forbidden_patterns")
        for pat in forbidden if isinstance(forbidden, list) else []:
            p = str(pat)
            if not p:
                warns.append(f"{name}: forbidden_patterns 含空模式")
                continue
            if any(unicodedata.category(ch) == "Cc" for ch in p):
                warns.append(
                    f"{name}: forbidden 模式含控制字符 {p!r}"
                    "（JSON \\b/\\t/\\n 转义错误→永远匹配不到真实输出）")
                continue
            try:
                rx = re.compile(p)
            except re.error as e:
                warns.append(f"{name}: forbidden 模式编译失败「{p}」: {e}")
                continue
            if rx.search(""):
                warns.append(f"{name}: forbidden 模式能匹配空串「{p}」（会在任意输出上误报）")
        tokens = ent.get("required_tokens")
        for tok in tokens if isinstance(tokens, list) else []:
            if len(_norm_ws(str(tok))) < 2:
                warns.append(f"{name}: required token 过短，无法构成断言: {tok!r}")
    run = data.get("run")
    if run is not None and not isinstance(run, dict):
        warns.append(f"顶层 run 应为对象，实为 {type(run).__name__}"
                     "（真值标量/数组会让主流程崩溃或覆盖静默失效）")
    elif isinstance(run, dict):
        for k in run:
            if k.startswith("_note") or k in KNOWN_RUN_KEYS:
                continue
            warns.append(f"run: 未知键「{k}」（fulltest 不读取，覆盖配置疑似拼错→不生效）")
    return warns


def load_expectations(path: Path) -> dict:
    """读取逐文件金标准；缺失/损坏只打印提示，不影响其余检查。

    同时计算文件 sha256（进报告与基线，金标准被动过时对比两侧可对上号）并做
    配置自检（未知键 / pattern 卫生），告警只提示不阻断——由人决定是否修正。
    """
    global EXPECTATIONS_SHA256, EXPECTATIONS_WARNINGS
    if not path.is_file():
        print(f"[expectations] 未找到 {path}，跳过金标准检查", flush=True)
        return {}
    try:
        raw = path.read_bytes()
        EXPECTATIONS_SHA256 = hashlib.sha256(raw).hexdigest()
        data = json.loads(raw.decode("utf-8"))
        structural_warning = None
        if not isinstance(data, dict):
            # 顶层手误（整包套了一层数组等）与 files 写错同待遇：清空并显式告警，
            # 不许金标准层静默禁用。
            data = {}
            structural_warning = "金标准顶层不是 JSON 对象，金标准检查已禁用"
        else:
            # run / files 写成真值非对象时各自清空+告警（两个独立 if：同时坏时
            # 都要暴露，不许 elif 短路漏掉第二个）；run 不清空会让 main 的
            # `run_cfg.get` 让整轮带 traceback 崩溃，files 不清空会让
            # `.items()` 的 AttributeError 在 sha256 已置值后伪装成「解析失败」。
            if data.get("run") is not None and not isinstance(data.get("run"), dict):
                data["run"] = {}
                structural_warning = "顶层 run 不是对象，运行覆盖配置已忽略"
            if data.get("files") is not None and not isinstance(data.get("files"), dict):
                data["files"] = {}
                if structural_warning:
                    structural_warning += "；顶层 files 不是对象，金标准检查已禁用"
                else:
                    structural_warning = "顶层 files 不是对象，金标准检查已禁用"
        print(f"[expectations] 已加载 {path}（{len(data.get('files') or {})} 个文件条目，"
              f"sha256 {EXPECTATIONS_SHA256[:12]}）", flush=True)
        EXPECTATIONS_WARNINGS = _validate_expectations(data)
        if structural_warning:
            EXPECTATIONS_WARNINGS.append(structural_warning)
        for w in EXPECTATIONS_WARNINGS:
            print(f"[expectations] 配置告警: {w}", flush=True)
        return data
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


def _git_info() -> dict:
    """取当前代码指纹（commit + 是否有未提交改动），进报告与基线。

    报告能对上「哪次代码跑出来的」全靠它；git 不可用时不阻断，仅记 null。
    """
    try:
        c = subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"],
                           capture_output=True, text=True, timeout=15)
        s = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"],
                           capture_output=True, text=True, timeout=15)
        return {"commit": c.stdout.strip() or None,
                "dirty": bool(s.stdout.strip()) if s.returncode == 0 else None}
    except Exception:
        return {"commit": None, "dirty": None}


def compare_baseline(prev: dict, cur: list, expectations_loaded: bool = True) -> dict:
    """按文件名对齐问题码，产出新增/已修复（比 code 集合）与恶化（比出现次数）。

    依赖金标准的码（GOLDEN_*/OCR 期望码）在未加载金标准的运行里不会产生，
    必须从两侧剔除，否则会得到假的「已修复」。剔除只在真正不可比时发生：
    金标准未全局加载、或该文件在基线里有记录而本次没有金标准条目（条目被删/改名）。
    对抗语料文件本来就没有金标准条目，属设计内，不因此把整场对比标成「未加载金标准」。
    同一码次数增加（如 DUP_SPAM 2→5）不算新增但算恶化——只比集合会把它判成
    「无变化」，质量劣化被吞掉。
    """
    # 键含 adversarial 标记：主队列与对抗语料允许同名文件（把主队列某文件的损坏
    # 副本放进 _adversarial/ 是扩充语料的常见方式），只按名字建字典会让对抗条目
    # 覆盖主队列条目——对账错位、金标准码被误剔、读数失真。
    prev_files = {(f.get("name"), bool(f.get("adversarial"))): f
                  for f in (prev.get("files") or [])}
    out = {"files": {}, "totals": {"new": 0, "fixed": 0, "worsened": 0},
           "skipped_exp_codes": False, "expectations_changed": None}
    prev_sha = prev.get("expectations_sha256")
    if prev_sha and EXPECTATIONS_SHA256 and prev_sha != EXPECTATIONS_SHA256:
        out["expectations_changed"] = (prev_sha, EXPECTATIONS_SHA256)
    for rec in cur:
        name = rec.get("name")
        cur_applied = bool((rec.get("golden") or {}).get("applied"))
        prev_rec = prev_files.get((name, bool(rec.get("adversarial")))) or {}
        # 报告与基线 JSON 的输出键：对抗条目带后缀，避免与同名主队列条目互相覆盖
        out_key = f"{name} [_adversarial]" if rec.get("adversarial") else name
        # 基线未记录 golden_applied（旧格式）时按「有金标准」处理，保持原有可追踪性
        prev_applied = prev_rec.get("golden_applied", True)
        if not expectations_loaded:
            drop, flag = set(EXP_GATED_CODES), True
        elif not cur_applied:
            # 对抗记录没有金标准条目属设计内（golden.applied=False），基线侧同样带
            # adversarial 标记：不剔除、不置「未加载金标准」——否则基线一旦收录对抗
            # 文件，每轮报告都会误报降级提示（对抗码集与 EXP_GATED 无交集，剔除本来
            # 就是空操作）。旧基线无 adversarial 标记时保持保守：按「主队列条目被删/
            # 改名」的旧语义置 flag，宁可误报也不吞掉真丢失条目的告警。
            if rec.get("adversarial") or prev_rec.get("adversarial"):
                drop, flag = set(), False
            else:
                drop = set(EXP_GATED_CODES)
                flag = bool(prev_rec)
        elif prev_applied is not True:
            drop, flag = set(EXP_GATED_CODES), True
        else:
            drop, flag = set(), False
        if flag:
            out["skipped_exp_codes"] = True
        now_c = Counter(i["code"] for i in (rec.get("issues") or []))
        before_c = Counter(i["code"] for i in (prev_rec.get("issues") or []))
        for code in drop:
            now_c.pop(code, None)
            before_c.pop(code, None)
        now, before = set(now_c), set(before_c)
        new, fixed = sorted(now - before), sorted(before - now)
        worse = {c: (before_c[c], now_c[c])
                 for c in sorted(now & before) if now_c[c] > before_c[c]}
        if new or fixed or worse:
            out["files"][out_key] = {"new": new, "fixed": fixed, "worse": worse}
            out["totals"]["new"] += len(new)
            out["totals"]["fixed"] += len(fixed)
            out["totals"]["worsened"] += len(worse)
    return out


def baseline_block_reason(results: list, expectations_loaded: bool,
                          stopped_early: bool = False,
                          early_file: str | None = None) -> str | None:
    """--save-baseline 的护栏：返回不可保存的原因，None 表示可以保存。

    把坏状态（带 FAIL、金标准未加载的降级跑、或 --strict/遇 FAIL 提前终止的半截
    结果）存成回归基准，会静默遮蔽之后的真回归——重设基线应发生在「确认当前红项
    为接受状态」之后，绕过护栏需显式 --force。
    """
    if stopped_early:
        return (f"本轮提前终止（于 {early_file or '?'} 命中阻断判定退出），"
                "之后的文件未测试——半截结果不能存成基线；确要保存请加 --force")
    fails = [r.get("name") for r in results if r.get("verdict") == "FAIL"]
    if fails:
        return ("存在 FAIL 判定文件: " + ", ".join(str(f) for f in fails[:5])
                + ("…" if len(fails) > 5 else "")
                + "——基线应记录达标/已接受状态；确认这些红项可接受后加 --force 保存")
    if not expectations_loaded:
        return ("金标准未加载（--no-expectations、文件缺失或 files 为空）：依赖金标准的码本次未产出，"
                "此时存的基线会把它们全判成「已修复」；确要保存请加 --force")
    return None


# ---------------------------------------------------------------- 失败路径（对抗语料）
ADV_PANIC_MARKERS = ("panicked at", "rust_backtrace")


def classify_adversarial_failure(rc: int, err_text: str) -> tuple:
    """对抗文件非零退出的分类。返回 (verdict, issues)。

    优雅失败 = 非零退出 + stderr 有诊断；panic/backtrace = 崩溃而非诊断，必须 FAIL；
    非零退出但 stderr 全空 = 静默失败，调试与排障无从下手，FAIL。
    （故意不含 "internal error"/"corrupt" 字样判定：它们在失败路径上是合法诊断文案。）
    """
    low = (err_text or "").strip().lower()
    if any(k in low for k in ADV_PANIC_MARKERS):
        return "FAIL", [make_issue(
            "ADV_PANIC", "对抗文件触发引擎 panic（失败路径必须优雅：给出诊断，而不是崩溃）")]
    if not low:
        return "FAIL", [make_issue(
            "ADV_SILENT_FAIL", f"对抗文件退出码 {rc} 但无任何诊断输出")]
    return "PASS", []


def finalize_adversarial_success(m, issues):
    """对抗文件 rc==0 的收尾判定：区分「输出垃圾」与「空/过短」并落 ADV 码。

    EMPTY/TOO_SHORT 在主队列是 FAIL，但对空/损坏文件「成功且空」是可辩护行为
    （ADV_EMPTY_OK 的 WARN 语义）；若保留 FAIL 级，issues_to_verdict 见 FAIL 即
    FAIL，ADV_EMPTY_OK 的 WARN 就永远落不到 verdict 上。因此：存在其他 FAIL 级
    结构问题 → ADV_GARBAGE_OK；仅空/过短 → ADV_EMPTY_OK；最后把 soft 码降级为
    WARN——可辩护但必须可见，不能静默吞掉。
    """
    soft = {"EMPTY", "TOO_SHORT"}
    hard = sorted({i["code"] for i in issues
                   if i["severity"] == "FAIL" and i["code"] not in soft})
    if hard:
        issues.append(make_issue(
            "ADV_GARBAGE_OK",
            f"对抗文件「成功」但输出含结构问题 [{', '.join(hard)}]"))
    elif any(i["code"] in soft for i in issues):
        issues.append(make_issue(
            "ADV_EMPTY_OK",
            f"对抗文件「成功」且输出为空/过短({m['chars']}字符)——静默空结果，请确认可接受"))
    # soft 码降级放最后：hard/soft 分支判定都依赖原始 FAIL 级别，先判完再降
    for i in issues:
        if i["code"] in soft:
            i["severity"] = "WARN"


def run_adversarial(cli: Path, adv_dir: Path, out_dir: Path, timeout: int,
                    env: dict, tags: dict | None = None) -> list:
    """对抗语料（<src>/_adversarial/）：损坏/截断/空文件必须优雅失败或优雅处理。

    判定：非零退出且有诊断 → PASS（优雅失败）；非零退出无诊断/panic → FAIL；
    转换「成功」则跑结构检查后交给 finalize_adversarial_success 收尾——输出垃圾
    （乱码/泄漏/坏引用）→ ADV_GARBAGE_OK，输出为空/过短 → ADV_EMPTY_OK
    （WARN：对空文件而言空结果可辩护，但必须可见）。
    超时沿用 TIMEOUT（对抗文件应快速失败，超时上限取 min(timeout, 300)）。
    tags：主流程按「主队列+对抗文件」整体预生成的产物命名 tag（见 _stem_tags），
    缺省退回 stem。
    """
    files = sorted(p for p in adv_dir.iterdir() if p.is_file()
                   and p.suffix.lower().lstrip(".") not in AV_EXTS
                   and not _is_windows_noise(p))
    emit("\n## 失败路径（对抗）测试")
    emit(f"  语料: {adv_dir}（{len(files)} 个文件；损坏/截断/空样本必须优雅失败）")
    results = []
    for f in files:
        tag = (tags or {}).get(f) or f.stem
        emit(f"\n[对抗] {f.name} ({f.stat().st_size/1024:.1f} KB) ...")
        exp = expect_for(f.name)
        try:
            md_text, meta, elapsed, rc, used_cli = convert_one(
                cli, f, out_dir, min(timeout, 300), env, transcription=False, tag=tag)
        except subprocess.TimeoutExpired:
            m = structural_metrics("", out_dir / f"{tag}_images")
            issues = [make_issue("TIMEOUT", "对抗文件超时(疑似挂死而非快速失败)")]
            verdict = "FAIL"
            elapsed = float(min(timeout, 300))
            rc = None
            meta = {"warnings": [], "notes": []}
        else:
            m = structural_metrics(md_text, out_dir / f"{tag}_images")
            issues = []
            if rc == 0:
                judge_structure(m, issues)
                judge_md_integrity(md_text, issues)
                judge_tables(md_text, issues)
                # 与主队列同口径：落盘垃圾图片（IMG_CORRUPT）与引擎 panic 级警告
                # （ENGINE_FAILISH）都是 FAIL 级码，进 hard 集合触发 ADV_GARBAGE_OK。
                judge_image_files(m, issues)
                judge_engine_warnings(meta, issues)
                finalize_adversarial_success(m, issues)
                if not any(i["code"] in ("ADV_GARBAGE_OK", "ADV_EMPTY_OK")
                           for i in issues):
                    emit("  优雅处理：转换成功且无结构问题")
            else:
                err_path = out_dir / f"{tag}.err.txt"
                err_text = err_path.read_text("utf-8", errors="replace") \
                    if err_path.is_file() else ""
                adv_verdict, adv_issues = classify_adversarial_failure(rc, err_text)
                issues.extend(adv_issues)
                emit(f"  退出码 {rc}；诊断首行: "
                     f"{(err_text.strip().splitlines() or ['(无)'])[0][:120]}")
            verdict = issues_to_verdict(issues)
        mark = "✅" if verdict == "PASS" else ("⚠️ " if verdict == "WARN" else "❌")
        emit(f"{mark} [{verdict}] {f.name}   ({elapsed:.1f}s)"
             + (f"   问题: {_fmt_issues(issues)}" if issues else ""))
        results.append({
            "name": f.name, "verdict": verdict, "elapsed": elapsed,
            "recall": None, "num_recall": None, "issues": issues,
            "chars": m["chars"], "method": meta.get("extraction_method") if rc == 0 else None,
            "counts": meta.get("counts") or {},
        })
        JSON_RESULTS.append({
            "name": f.name, "verdict": verdict, "elapsed_s": round(elapsed, 2),
            "recall": None, "num_recall": None, "ident_recall": None,
            "missing_numbers": [], "missing_idents": [],
            "char_ratio": None, "src_images": None, "src_images_note": None,
            "source_pages": None,
            "metrics": json_metrics(m), "issues": issues,
            # 对抗路径 rc!=0 的真实原因只在 meta 里（err.txt 可能缺失/陈旧），保留进
            # JSON，别让「图片目录创建失败」这类环境故障在报告里消失。
            "warnings": meta.get("warnings") or [],
            "notes": meta.get("notes") or [],
            "golden": {"applied": bool(exp)},
            "ocr": None, "counts": {}, "extraction_method": None,
            "adversarial": True,
        })
    return results


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
                # 孙进程会继承管道写句柄，taskkill 失败时管道不会闭合——无超时的
                # communicate 会让超时保护退化成永久挂死。逐级降级：30s 收尾 →
                # proc.kill() → 5s 收尾 → 放弃残留输出（已触发 TimeoutExpired，输出不参与判定）。
                try:
                    out, err = proc.communicate(timeout=30)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    try:
                        out, err = proc.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        out, err = b"", b""
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
    渲染器里都不是有效链接；`%` 也要编码，否则渲染器会把 `100%25` 解码成另一个目录名；
    `?` 同理（渲染后的 URL 会把它当 query 起点截断目标）。
    本表与 CLI `prefix_image_refs` 逐字符一致。引擎侧 `sanitize_marker_url` 是第三张表，
    按各自输入面设计、并不相同：它多编码文档自带 target 的 `\\`/`[`/`]`、没有 `#`/`?`——
    对齐任何一张表时不要跨表外推。
    """
    table = {" ": "%20", "(": "%28", ")": "%29", "<": "%3C", ">": "%3E",
             '"': "%22", "`": "%60", "%": "%25", "#": "%23", "?": "%3F"}
    return "".join(table.get(ch, ch) for ch in name
                   if unicodedata.category(ch) != "Cc")


def save_markdown(out_dir: Path, stem: str, md_text: str, img_dirname: str):
    """把 markdown 里的图片引用加上子目录前缀后再保存，保证打开时图片可见。

    只对「裸文件名」引用加前缀：引擎自产的引用形如 `image_N.ext`，不含空白、
    括号或目录分隔。target 类曾只排除 `)/`，对含括号的带路径引用会在第一个
    `)` 处半匹配截断、把链接改坏——那种形态不是引擎产物，收紧为不匹配、
    原样保留，交给后面的引用对账判定。围栏内是字面量（OCR 围栏/代码示例里的
    `![](image_0.png)` 不是引用），与 CLI 的 `prefix_image_refs` 一样跳过——
    判定读内存 md_text 不受影响，这里只保证落盘审计副本与 CLI 口径一致。
    """
    prefix = _encode_markdown_dir(img_dirname)
    pattern = re.compile(r"(!\[[^\]]*\]\()([^\s()/]+)\)")
    fence = None  # (围栏字符, 长度)——当前打开的代码围栏
    saved_lines = []
    for line in md_text.splitlines(keepends=True):
        body = line.rstrip("\r\n")
        if fence is not None:
            saved_lines.append(line)
            stripped = body.lstrip(" ")
            run = len(stripped) - len(stripped.lstrip(fence[0]))
            if (len(body) - len(stripped) <= 3 and run >= fence[1]
                    and stripped[run:].strip(" \t") == ""):
                fence = None
            continue
        stripped = body.lstrip(" ")
        first = stripped[:1]
        run = len(stripped) - len(stripped.lstrip(first)) if first in ("`", "~") else 0
        # 与 Rust 侧 code_fence_open 同契约：≥3 个围栏字符、缩进 ≤3 空格、反引号
        # 围栏的 info string 里不得再出现反引号。
        if (len(body) - len(stripped) <= 3 and run >= 3
                and not (first == "`" and "`" in stripped[run:])):
            fence = (first, run)
            saved_lines.append(line)
            continue
        saved_lines.append(pattern.sub(
            lambda match: f"{match.group(1)}{prefix}/{match.group(2)})", line))
    (out_dir / f"{stem}.md").write_text("".join(saved_lines), encoding="utf-8")


def _stem_tags(main_files: list, adv_files: list | None = None) -> dict:
    """按 stem 分组生成产物命名 tag：组内唯一 stem 直接用 stem，重名追加「_扩展名」消歧；
    对抗文件恒再追加 `_adv`——主队列与 _adversarial 同名同扩展时仅靠扩展名仍会同 tag，
    `{tag}_images`/`{tag}.err.txt`/`{tag}.md` 会互相覆盖，审计产物与判定对象错位。
    判定键（f.name / expect_for / JSON_RESULTS 与基线的 name）一律用文件名，不受 tag 影响。
    """
    adv = list(adv_files or [])
    main_counts = Counter(p.stem for p in main_files)
    adv_counts = Counter(p.stem for p in adv)
    tags = {}
    used: set[str] = set()
    for p in main_files:
        base = p.stem if main_counts[p.stem] == 1 \
            else f"{p.stem}_{p.suffix.lower().lstrip('.')}"
        # 主队列内部也有撞名形态：X.pdf+X.docx 的退化 tag（X_pdf）会撞上唯一
        # stem X_pdf.pptx 的裸 tag——convert_one 的 rmtree 会互删审计产物。
        tag = base
        n = 2
        while tag in used:
            tag = f"{base}{n}"
            n += 1
        used.add(tag)
        tags[p] = tag
    # 对抗 tag 与主队列 tag 也可能撞名（主队列 X_adv.pdf ↔ 对抗 X.pdf 都得 X_adv），
    # 同样互删产物；撞名时退化到带扩展名的形式，再撞就加序号。
    for p in adv:
        base = p.stem if adv_counts[p.stem] == 1 \
            else f"{p.stem}_{p.suffix.lower().lstrip('.')}"
        tag = f"{base}_adv"
        if tag in used:
            ext = p.suffix.lower().lstrip('.')
            tag = f"{base}_{ext}_adv" if ext else f"{base}_adv"
            n = 2
            while tag in used:
                tag = f"{base}_{ext}_adv{n}" if ext else f"{base}_adv{n}"
                n += 1
        used.add(tag)
        tags[p] = tag
    return tags


def convert_one(cli: Path, src_file: Path, out_dir: Path, timeout: int, env: dict,
                transcription: bool, tag: str | None = None):
    """转换单个文件（只用本地编译的 CLI，不做任何回退）。返回 (md_text, meta, elapsed, returncode, used_cli)。

    tag：落盘产物（{tag}_images/{tag}.err.txt/{tag}.md）的命名键。同名 stem 的文件
    会互相覆盖审计产物，由 main 用 _stem_tags 预生成消歧 tag 传入；缺省退回
    src_file.stem。判定键始终用文件名，与 tag 无关。
    """
    stem = tag or src_file.stem
    img_dir = out_dir / f"{stem}_images"
    # 上一次运行留下的图片必须清空：残留文件会掩盖真实丢图（IMG_LOST/IMG_MISSING 都以
    # 该目录内容为判据），中断运行留下的半截文件还会误报 IMG_CORRUPT。
    shutil.rmtree(img_dir, ignore_errors=True)
    try:
        img_dir.mkdir(parents=True, exist_ok=True)
    except OSError as dir_error:  # noqa: BLE001 - 目录都建不出来时按该文件失败处理
        notes = [f"图片目录创建失败: {dir_error}"]
        # 尽力把真实原因落进 err.txt：对抗队列 rc!=0 时只从该文件读诊断，缺了会被
        # 误报成「无任何诊断输出」（静默失败）。
        try:
            (out_dir / f"{stem}.err.txt").write_bytes(f"{notes[0]}\n".encode("utf-8"))
        except OSError:
            pass
        return ("", {"warnings": notes, "counts": {}, "languages": [], "notes": notes},
                0.0, 1, cli)
    label = f"[{stem}]"
    notes = []

    cmd = build_cmd(cli, src_file, img_dir, transcription)
    rc, out, err, elapsed = run_with_ticker(label, cmd, env, timeout)
    try:
        (out_dir / f"{stem}.err.txt").write_bytes(err or b"")
    except OSError as err_log_error:  # noqa: BLE001 - 审计日志写不进不能炸整轮，记 note 继续
        notes.append(f"err.txt 落盘失败: {err_log_error}")
        # 上一轮运行可能留有同名旧诊断：留下它会让 OCR 通道判定与对抗分类读到陈旧
        # stderr——尽力删掉，宁可「无诊断」也不要「旧诊断」。
        try:
            (out_dir / f"{stem}.err.txt").unlink()
        except OSError:
            pass

    if rc != 0:
        err_text = (err or b"").decode("utf-8", errors="replace")
        if transcription and "unknown field `transcription`" in err_text:
            notes.append("当前 CLI 未启用 transcription feature，音视频必须用本地编译的全功能版测试："
                         "cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api")
        err_lines = err_text.splitlines()
        first_err = next((l for l in err_lines
                          if re.search(r"ERROR|Error|error \[", l)),
                         err_lines[0] if err_lines else "(无错误输出)")
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
    try:
        n_imgs = write_images_from_json(result, img_dir)
        meta["notes"] = notes + ([f"由 Python 从 JSON 内联数据落盘 {n_imgs} 张图片"] if n_imgs else [])
        save_markdown(out_dir, stem, md_text, img_dir.name)
    except Exception as disk_error:  # noqa: BLE001 - 落盘失败同样不能放大为整轮崩溃
        # images[].data 形态异常 / 磁盘满 / 权限错误：按该文件失败处理（走 rc!=0 的判定
        # 路径），报告与基线对比仍然完整——JUDGE_CRASH 的语义就是「单文件问题不得放大
        # 为整轮崩溃」。
        notes.append(f"落盘失败: {disk_error}")
        return ("", {"warnings": [f"落盘失败: {disk_error}"], "counts": {},
                     "languages": [], "notes": notes},
                elapsed, 1, cli)
    return md_text, meta, elapsed, rc, cli


# ---------------------------------------------------------------- 报告
def _fmt_issues(issues):
    return "; ".join(f"[{i['code']}] {i['message']}" for i in issues)


def report_file(name, verdict, m, recall_info, meta, elapsed, issues, used_cli, ocr_info=None):
    ok = "✅" if verdict == "PASS" else ("⚠️ " if verdict == "WARN" else "❌")
    emit(f"\n{'='*72}")
    emit(f"{ok} [{verdict}] {name}   ({elapsed:.1f}s)")
    emit(f"  内容: {m['chars']} 字符 | 非空行 {m['nonempty']} | 标题 {m['headings']} | "
         f"表格行 {m['table_rows']} | 图片引用 {m['img_refs']}/唯一 {m['img_refs_unique']}(落盘 {m['imgs_on_disk']}) | "
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


# ---------------------------------------------------------------- 判定器自测
def _golden_token_hit_reference(token: str, md_nos: str) -> bool:
    """selftest 专用参考实现：与 _golden_token_hit 的旧回溯正则语义逐字等价（含 CJK 闸门）。

    生产实现改为线性 DP 后，等价性靠随机对照持续验证（run_selftest）。
    """
    t = _norm_ws(token)
    if t in md_nos:
        return True
    if len(t) < 2 or not re.search(r"[\u4e00-\u9fff]", t):
        return False
    gap = ".{0,%d}" % GOLDEN_CJK_TOKEN_MAX_GAP
    pat = re.compile(gap.join(re.escape(ch) for ch in t))
    return bool(pat.search(md_nos))


def run_selftest() -> int:
    """判定器自测：合成样例 + 随机等价性对照；不需要 CLI / 语料 / 金标准。

    维护约定：改判定核心函数（_golden_token_hit / save_markdown / _validate_expectations /
    compare_baseline / baseline_block_reason / classify_adversarial_failure /
    finalize_adversarial_success，以及本行后续加入的 _toc_heading_miss /
    _toc_level_stats / structural_metrics 重复行口径 / _embedded_media_lost /
    _judge_xlsx_shapes）必须同步加/改本函数用例——验收器自身的回归同样算回归。
    """
    import random
    import tempfile

    fails = []

    def check(name: str, cond, detail: str = ""):
        print(f"  [{'ok' if cond else 'FAIL'}] {name}"
              + (f" | {detail}" if detail and not cond else ""), flush=True)
        if not cond:
            fails.append(name)

    print("selftest: 判定器自测开始", flush=True)

    # --- _golden_token_hit：命中语义（闸门/容差边界） ---
    check("token 精确子串命中", _golden_token_hit("include", "xx#includeyy"))
    check("token 缺失不命中", not _golden_token_hit("include", "xx#mcludefile"))
    check("纯拉丁不享容差(非空白打断)", not _golden_token_hit("Implements", "Impl-ements"))
    g = GOLDEN_CJK_TOKEN_MAX_GAP
    check(f"CJK 交错间隔={g} 命中",
          _golden_token_hit("中文", "中" + "x" * g + "文"))
    check(f"CJK 交错间隔={g + 1} 不命中",
          not _golden_token_hit("中文", "中" + "x" * (g + 1) + "文"))
    check("混合 token 拉丁片段同享容差",
          _golden_token_hit("第3课", "第abc3课"))
    check("单字符 token 走子串短路", _golden_token_hit("a", "xa y"))

    # --- _golden_token_hit：DP 与旧正则参考实现随机等价 ---
    rng = random.Random(20260914)
    alphabet = "abcxy数据测试01"
    mismatch = 0
    for _ in range(300):
        tok = "".join(rng.choice(alphabet) for _ in range(rng.randint(2, 6)))
        md = _norm_ws("".join(rng.choice(alphabet + " .,")
                              for _ in range(rng.randint(10, 60))))
        if _golden_token_hit(tok, md) != _golden_token_hit_reference(tok, md):
            mismatch += 1
    check("DP 与正则参考实现 300 组随机等价", mismatch == 0, f"{mismatch} 组不一致")

    # --- save_markdown：只给裸文件名加前缀，带路径/括号引用原样保留 ---
    tmp_root = REPO / ".tmp"
    tmp_root.mkdir(exist_ok=True)
    tmp = Path(tempfile.mkdtemp(prefix="fulltest-selftest-", dir=str(tmp_root)))
    try:
        md = ("![a](image_0.png)\n![b](sub/dir.png)\n![c](im (1).png)\n"
              "![d](image_1.png)\n")
        save_markdown(tmp, "t", md, "doc_images")
        saved = (tmp / "t.md").read_text("utf-8")
        check("裸文件名引用被加前缀", "](doc_images/image_0.png)" in saved
              and "](doc_images/image_1.png)" in saved)
        check("带路径引用原样保留", "](sub/dir.png)" in saved)
        check("含括号引用原样保留(不半匹配截断)", "](im (1).png)" in saved)
        md2 = "```text\n![](image_0.png)\n```\n![](image_1.png)\n"
        save_markdown(tmp, "t2", md2, "doc_images")
        saved2 = (tmp / "t2.md").read_text("utf-8")
        check("围栏内引用是字面量，不加前缀",
              "![](image_0.png)" in saved2 and "doc_images/image_0.png" not in saved2,
              saved2)
        check("围栏外引用仍加前缀", "](doc_images/image_1.png)" in saved2, saved2)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    # --- _validate_expectations：未知键 / pattern 卫生 ---
    bad = {
        "version": 1, "note": "t", "extra_top": 1,
        "files": {"a.pdf": {
            "min_table": 3, "min_tables": 3, "_note_min_tables": "依据",
            "forbidden_patterns": ["\bfi(?:x)\b", "[", "a|"],
            "required_tokens": ["x"],
        }},
        "run": {"ocr_config": {}, "layout_config": {}, "_note_x": "ok", "ocr_cfg": {}},
    }
    warns = "\n".join(_validate_expectations(bad))
    check("拼错键名被告警", "未知键「min_table」" in warns)
    check("顶层未知键被告警", "顶层未知键「extra_top」" in warns)
    check("run 未知键被告警", "run: 未知键「ocr_cfg」" in warns)
    check("JSON \\b 退格损坏的 pattern 被告警", "控制字符" in warns)
    check("非法正则被告警", "编译失败" in warns)
    check("过短 required token 被告警", "过短" in warns)
    good = {
        "version": 1,
        "files": {"a.pdf": {"min_tables": 3, "_note_min_tables": "依据"}},
        "run": {"ocr_config": {}, "_note_layout_config": "ok"},
    }
    check("合法配置零告警", _validate_expectations(good) == [],
          str(_validate_expectations(good)))

    # --- _validate_expectations：值类型告警（类型写错＝检查静默失效，与拼错键同等可见） ---
    bad_types = {
        "files": {"a.pdf": {
            "forbidden_patterns": "abc",      # 裸字符串：逐字符迭代成单字符正则→乱报
            "order": ["x", "y"],              # 扁平数组：GOLDEN_ORDER 静默跳过
            "required_tokens": "tok",         # 裸字符串：迭代成单字符 token
            "min_tables": "3",                # 字符串阈值：_exp_int 不认→检查不生效
            "require_chinese_ocr": "false",   # truthy 读取下恒为真
            "toc_heading_min_recall": "0.8",
        }},
    }
    warns_types = "\n".join(_validate_expectations(bad_types))
    check("forbidden_patterns 裸字符串被告警", "forbidden_patterns」应为数组" in warns_types)
    check("required_tokens 裸字符串被告警", "required_tokens」应为数组" in warns_types)
    check("扁平 order 数组按元素形态被告警", "两个字符串的数组" in warns_types)
    check("字符串阈值被告警", "应为整数阈值" in warns_types)
    check("字符串布尔被告警", "require_chinese_ocr」应为布尔" in warns_types)
    check("字符串召回率被告警", "toc_heading_min_recall」应为数值" in warns_types)
    bad_order_pair = {"files": {"a.pdf": {"order": [["a", "b"], ["c"]]}}}
    check("order 非二元组元素被告警",
          "两个字符串的数组" in "\n".join(_validate_expectations(bad_order_pair)))
    # list 键写成真值标量（true/1）：类型契约告警照记，自检不得被 TypeError 炸掉
    # ——炸了会连其余告警一起丢、还伪装成「解析失败」。
    bad_scalar_lists = {"files": {"a.pdf": {"forbidden_patterns": True, "required_tokens": 1}}}
    warns_scalar = _validate_expectations(bad_scalar_lists)
    check("list 键写成真值标量只告警不炸自检",
          any("forbidden_patterns」应为数组" in w for w in warns_scalar)
          and any("required_tokens」应为数组" in w for w in warns_scalar))
    check("金标准顶层不是对象要告警",
          _validate_expectations(["not", "a", "dict"]) == ["金标准顶层不是 JSON 对象"])
    check("files 写成数组不再炸自检",
          isinstance(_validate_expectations({"files": ["bad"]}), list))
    run_warns = _validate_expectations({"run": ["auto"]})
    check("run 写成真值非对象被告警",
          any("顶层 run 应为对象" in w for w in run_warns))
    good_types = {"files": {"a.pdf": {
        "min_tables": 3, "require_chinese_ocr": True, "toc_heading_min_recall": 0.8,
        "order": [["a", "b"]], "forbidden_patterns": ["x+"],
    }}}
    check("合法值类型零告警", _validate_expectations(good_types) == [],
          str(_validate_expectations(good_types)))

    # --- structural_metrics：外链与带 title 的图片引用口径 ---
    with tempfile.TemporaryDirectory() as _td:
        _tmp = Path(_td)
        _img_dir = _tmp / "imgs"
        _img_dir.mkdir()
        (_img_dir / "image_0.png").write_bytes(b"\x89PNG\r\n\x1a\n")
        _md = ("![](image_0.png)\n\n![](https://example.com/logo.png)\n\n"
               '![](image_0.png "题注")\n')
        _m = structural_metrics(_md, _img_dir)
        check("外链引用不计入 img_refs", _m["img_refs"] == 2, str(_m["img_refs"]))
        check("带 title 的引用按目标名对账", _m["broken_refs"] == [], str(_m["broken_refs"]))
        _m2 = structural_metrics("![](missing.png \"t\")\n", _img_dir)
        check("缺失目标仍判 broken", _m2["broken_refs"] == ["missing.png"], str(_m2["broken_refs"]))
        # 同一图片多次引用：出现次数 2、唯一目标 1——唯一数才与落盘文件数同量纲。
        _m3 = structural_metrics("![](image_0.png)\n![](image_0.png)\n", _img_dir)
        check("重复引用按唯一目标计", _m3["img_refs"] == 2 and _m3["img_refs_unique"] == 1,
              f"refs={_m3['img_refs']} unique={_m3['img_refs_unique']}")
        # judge 级接线：唯一目标 ≤ 落盘就不出 IMG_LOST——metric 用例不守护消费行本身。
        _issues3 = []
        judge_structure(_m3, _issues3)
        check("重复引用同一存在图片不判 IMG_LOST",
              not any(i["code"] == "IMG_LOST" for i in _issues3), str(_issues3))

    # --- compare_baseline：集合对比 + 同码次数恶化 + 金标准门控剔除 ---
    global EXPECTATIONS_SHA256
    saved_sha = EXPECTATIONS_SHA256
    try:
        EXPECTATIONS_SHA256 = "b" * 64
        prev = {"files": [
            {"name": "a.pdf", "golden_applied": True, "issues": [
                {"code": "DUP_SPAM"}, {"code": "DUP_SPAM"}, {"code": "NOTES_MISSING"}]},
            {"name": "b.pdf", "golden_applied": True,
             "issues": [{"code": "TOC_HEADING_GAP"}]},
        ]}
        cur = [
            {"name": "a.pdf", "golden": {"applied": True},
             "issues": [{"code": "DUP_SPAM"}] * 5},
            {"name": "b.pdf", "golden": {"applied": False},   # 本次未加载金标准
             "issues": []},
        ]
        out = compare_baseline(prev, cur, expectations_loaded=True)
        a = out["files"].get("a.pdf") or {}
        check("同码次数 2→5 判为恶化而非无变化",
              a.get("worse") == {"DUP_SPAM": (2, 5)} and not a.get("new"))
        check("消失的码仍判已修复", a.get("fixed") == ["NOTES_MISSING"])
        check("依赖金标准的码被剔除(不算已修复)",
              "b.pdf" not in out["files"] and out["skipped_exp_codes"])
        # 对抗语料文件没有金标准条目属设计内：不得把整场对比标成「未加载金标准」
        out_adv = compare_baseline(
            {"files": [{"name": "main.pdf", "golden_applied": True, "issues": []}]},
            [{"name": "main.pdf", "golden": {"applied": True}, "issues": []},
             {"name": "adv_empty.pdf", "golden": {"applied": False}, "issues": []}],
            expectations_loaded=True)
        check("对抗文件无金标准条目不触发「未加载金标准」",
              not out_adv["skipped_exp_codes"])
        # 基线已收录对抗文件（golden_applied=False + adversarial 标记）同样不得置 flag：
        # 406f922ddb 只修了「不在基线」的半边，本用例锁住「已在基线」的另一半
        out_adv_saved = compare_baseline(
            {"files": [{"name": "main.pdf", "golden_applied": True, "issues": []},
                       {"name": "adv_empty.pdf", "golden_applied": False,
                        "adversarial": True, "issues": []}]},
            [{"name": "main.pdf", "golden": {"applied": True}, "issues": []},
             {"name": "adv_empty.pdf", "golden": {"applied": False},
              "adversarial": True, "issues": [{"code": "ADV_PANIC"}]}],
            expectations_loaded=True)
        check("基线收录的对抗文件不触发「未加载金标准」",
              not out_adv_saved["skipped_exp_codes"])
        check("对抗码不因豁免剔除而被吞(新增照常计数)",
              out_adv_saved["files"].get("adv_empty.pdf [_adversarial]", {}).get("new") == ["ADV_PANIC"])
        check("金标准未加载仍触发剔除标记",
              compare_baseline({"files": []},
                               [{"name": "x", "golden": {"applied": False}, "issues": []}],
                               expectations_loaded=False)["skipped_exp_codes"])
        check("金标准 sha 不一致被标记",
              compare_baseline({"expectations_sha256": "a" * 64, "files": []}, [],
                               expectations_loaded=True)
              ["expectations_changed"] is not None)
        # 主队列与对抗语料同名：键含 adversarial 标记，两侧各对各的账。
        # 只按名字建字典时对抗条目覆盖主队列条目——主队列的金标准码被误剔、
        # 对抗码算到主队列头上，读数整体失真。
        out_same_name = compare_baseline(
            {"files": [
                {"name": "broken.pdf", "golden_applied": True,
                 "issues": [{"code": "GOLDEN_TOKEN_MISSING"}]},
                {"name": "broken.pdf", "golden_applied": False,
                 "adversarial": True, "issues": []}]},
            [{"name": "broken.pdf", "golden": {"applied": True}, "issues": []},
             {"name": "broken.pdf", "golden": {"applied": False},
              "adversarial": True, "issues": [{"code": "ADV_PANIC"}]}],
            expectations_loaded=True)
        check("同名主队列/对抗条目不互相覆盖",
              not out_same_name["skipped_exp_codes"]
              and out_same_name["files"].get("broken.pdf [_adversarial]", {}).get("new") == ["ADV_PANIC"]
              and out_same_name["files"]["broken.pdf"]["fixed"] == ["GOLDEN_TOKEN_MISSING"],
              str(out_same_name))
    finally:
        EXPECTATIONS_SHA256 = saved_sha

    # --- baseline_block_reason：护栏判定 ---
    check("存在 FAIL 时拒绝保存",
          "FAIL" in (baseline_block_reason(
              [{"name": "x.pdf", "verdict": "FAIL"}], True) or ""))
    check("金标准未加载时拒绝保存",
          "金标准" in (baseline_block_reason(
              [{"name": "x.pdf", "verdict": "WARN"}], False) or ""))
    check("干净结果允许保存",
          baseline_block_reason([{"name": "x.pdf", "verdict": "PASS"}], True) is None)
    check("提前终止的半截结果拒绝保存",
          "提前终止" in (baseline_block_reason(
              [{"name": "x.pdf", "verdict": "PASS"}], True, True, "y.pdf") or ""))

    # --- 判定口径修正：pptx 标题 key 同归一化；bigram_recall 剔除表格分隔符 ---
    check("pptx 标题 key 剥强调标记后可命中下划线标题",
          any(_pptx_title_key("Func_mbist") in h
              for h, _n, _raw in _md_headings("## Func_mbist\n")))
    check("bigram_recall 不把表格分隔符当内容缺失",
          bigram_recall("版本日期修订描述与测试覆盖率统计表格数据",
                        "| 版本 | 日期 | 修订 | 描述 | 与测 | 试覆 | 盖率 | 统计 | 表格 | 数据 |") == 1.0)

    # --- PDF 书签 key 与 _md_headings 同归一化（下划线书签恒不命中 → TOC 假缺失） ---
    check("书签含下划线可命中 MD 标题(不假缺失)",
          _toc_heading_miss([(1, "MUX Scan With scan_out", 3)],
                            "## MUX Scan With scan_out\n") == [])
    check("真缺失的书签仍报缺失",
          _toc_heading_miss([(1, "确实缺失的标题", 3)], "## 别的标题\n")
          == ["确实缺失的标题"])
    check("书签无下划线时命中口径不变",
          _toc_heading_miss([(1, "Fault types", 3)], "## Fault types\n") == [])
    checked, mism = _toc_level_stats(
        [(1, "Func_mbist", 3), (2, "Sub_a", 4)], "## Func_mbist\n### Sub_a\n")
    check("层级核对纳入下划线书签且层级相符(checked=2)", (checked, mism) == (2, 0))
    checked, mism = _toc_level_stats([(1, "Deep_title", 4)], "#### Deep_title\n")
    check("下划线书签层级不符仍计 mismatch(checked=1,mism=1)", (checked, mism) == (1, 1))

    # --- classify_adversarial_failure：失败路径分类 ---
    check("panic 判 FAIL",
          classify_adversarial_failure(101, "thread 'main' panicked at src\\main.rs:5")
          [0] == "FAIL")
    check("非零退出无诊断判 FAIL(静默失败)",
          classify_adversarial_failure(1, "   ")[0] == "FAIL")
    check("非零退出带诊断判 PASS(优雅失败)",
          classify_adversarial_failure(2, "Error: invalid PDF: unexpected end of file")
          [0] == "PASS")

    # --- finalize_adversarial_success：对抗「成功」收尾（空/过短降级，垃圾仍 FAIL） ---
    iss = [make_issue("EMPTY", "转换结果为空")]
    finalize_adversarial_success({"chars": 0}, iss)
    check("对抗仅 EMPTY → ADV_EMPTY_OK 且无 FAIL 级残留(verdict 将为 WARN)",
          any(i["code"] == "ADV_EMPTY_OK" for i in iss)
          and not any(i["severity"] == "FAIL" for i in iss))
    iss = [make_issue("TOO_SHORT", "过短"), make_issue("IMG_CORRUPT", "坏图")]
    finalize_adversarial_success({"chars": 3}, iss)
    check("对抗含 FAIL 级结构码 → ADV_GARBAGE_OK 且仍 FAIL",
          any(i["code"] == "ADV_GARBAGE_OK" for i in iss)
          and any(i["severity"] == "FAIL" for i in iss))
    iss = []
    finalize_adversarial_success({"chars": 100}, iss)
    check("对抗干净成功不追加任何 ADV 码", iss == [])

    # --- exp 上限替换默认阈值（放行方向）：ESCAPE_NOISE / NONSTD_HIGHLIGHT ---
    # 6 处围栏外 \# → n_esc=6：默认阈值(>=5)触发，exp max_escapes=10 放行
    md_esc = "\n".join(f"值 \\# {k}" for k in range(6))
    iss = []
    judge_markdown_semantics(md_esc, {}, iss, {})
    check("exp 缺省时 n_esc=6 仍触发 ESCAPE_NOISE(默认阈值不变)",
          any(i["code"] == "ESCAPE_NOISE" for i in iss))
    iss = []
    judge_markdown_semantics(md_esc, {}, iss, {"max_escapes": 10})
    check("exp max_escapes=10 放行 n_esc=6(不出码)",
          not any(i["code"] == "ESCAPE_NOISE" for i in iss))
    md_hl = "重点 ==A1== 和 ==B2== 与 ==C3== 或 ==D4== 加 ==E5== 与 ==F6=="
    iss = []
    judge_emphasis_noise(md_hl, iss, {})
    check("exp 缺省时 hl=6 仍触发 NONSTD_HIGHLIGHT(默认阈值不变)",
          any(i["code"] == "NONSTD_HIGHLIGHT" for i in iss))
    iss = []
    judge_emphasis_noise(md_hl, iss, {"max_highlights": 10})
    check("exp max_highlights=10 放行 hl=6(不出码)",
          not any(i["code"] == "NONSTD_HIGHLIGHT" for i in iss))

    # --- _judge_expectations：order 与 required_tokens 同口径（剥强调标记） ---
    iss = []
    _judge_expectations("## **Fault** 类型\n\n## **竞争冒险**\n", {}, iss,
                        {"order": [["Fault 类型", "竞争冒险"]]})
    check("order 命中不受 ** 强调标记干扰",
          not any(i["code"] == "GOLDEN_ORDER" for i in iss))
    iss = []
    _judge_expectations("## 竞争冒险\n\n## Fault 类型\n", {}, iss,
                        {"order": [["Fault 类型", "竞争冒险"]]})
    check("真逆序仍判 GOLDEN_ORDER",
          any(i["code"] == "GOLDEN_ORDER" for i in iss))

    # --- _stem_tags：同名 stem 的产物命名消歧；对抗文件恒带 _adv ---
    tags = _stem_tags([Path("d/a.pdf"), Path("d/a.docx"), Path("d/b.pdf")])
    check("重名 stem 生成带后缀 tag",
          tags[Path("d/a.pdf")] == "a_pdf" and tags[Path("d/a.docx")] == "a_docx")
    check("唯一 stem 沿用 stem", tags[Path("d/b.pdf")] == "b")
    adv_tags = _stem_tags([Path("d/a.pdf")], [Path("adv/a.pdf"), Path("adv/b.pdf")])
    check("对抗文件恒带 _adv(与主队列同名同扩展也分离)",
          adv_tags[Path("adv/a.pdf")] == "a_adv"
          and adv_tags[Path("d/a.pdf")] == "a")
    adv_dup = _stem_tags([], [Path("adv/a.pdf"), Path("adv/a.docx")])
    check("对抗内部重名 stem 先按扩展消歧再加 _adv",
          adv_dup[Path("adv/a.pdf")] == "a_pdf_adv"
          and adv_dup[Path("adv/a.docx")] == "a_docx_adv")
    # 对抗 tag 与主队列 tag 撞名（主队列 X_adv.pdf ↔ 对抗 X.pdf 都得 X_adv）：
    # convert_one 会 rmtree 图片目录，撞名会删掉主队列审计产物——必须退化消歧。
    clash = _stem_tags([Path("d/x_adv.pdf")], [Path("adv/x.pdf")])
    check("对抗 tag 与主队列撞名时退化带扩展名形式",
          clash[Path("d/x_adv.pdf")] == "x_adv"
          and clash[Path("adv/x.pdf")] == "x_pdf_adv"
          and clash[Path("d/x_adv.pdf")] != clash[Path("adv/x.pdf")])
    clash2 = _stem_tags([Path("d/x_adv.pdf"), Path("d/x_pdf_adv.pdf")], [Path("adv/x.pdf")])
    check("退化形式再撞名时加序号",
          clash2[Path("adv/x.pdf")] == "x_pdf_adv2")
    # 主队列内部：X.pdf+X.docx 的退化 tag（X_pdf）撞上唯一 stem X_pdf.pptx 的裸 tag。
    clash3 = _stem_tags([Path("d/x.pdf"), Path("d/x.docx"), Path("d/x_pdf.pptx")], [])
    check("主队列退化 tag 与唯一裸 stem 撞名时加序号",
          clash3[Path("d/x.pdf")] == "x_pdf"
          and clash3[Path("d/x.docx")] == "x_docx"
          and clash3[Path("d/x_pdf.pptx")] == "x_pdf2"
          and len(set(clash3.values())) == 3)

    # --- DUP_SPAM：嵌入对象区间（Embedded object: caption → 下一结构性元素）不计重复 ---
    spam = "ARCH41_CORE4"
    embed_md = "\n".join(
        ["正文引导行甲", "Embedded object: oleObject1.bin"]
        + [x for _ in range(14) for x in (spam, "")]
        + ["## 下一节", "正文收尾行乙"])
    iss = []
    judge_structure(structural_metrics(embed_md, Path("no_such_dir")), iss)
    check("嵌入对象区间内的 14 次同名框行不判 DUP_SPAM",
          not any(i["code"] == "DUP_SPAM" for i in iss))
    plain_md = "\n".join(
        ["正文引导行甲"] + [x for _ in range(14) for x in (spam, "")]
        + ["## 下一节", "正文收尾行乙"])
    iss = []
    judge_structure(structural_metrics(plain_md, Path("no_such_dir")), iss)
    check("无 caption 的同量重复行仍判 DUP_SPAM(不放宽真缺陷)",
          any(i["code"] == "DUP_SPAM" for i in iss))
    cap_head_md = "\n".join(
        ["Embedded object: a.bin", "## 下一节"]
        + [x for _ in range(14) for x in (spam, "")])
    iss = []
    judge_structure(structural_metrics(cap_head_md, Path("no_such_dir")), iss)
    check("caption 后紧跟标题则区间为空，其后重复行仍判 DUP_SPAM",
          any(i["code"] == "DUP_SPAM" for i in iss))

    # --- EMBED_MEDIA_LOST：与顶层文档 media 同字节/同像素的嵌入图片不算丢失 ---
    import io as _io
    import zipfile as _zip
    banner = b"\x89PNG\r\n\x1a\n" + b"B" * 200      # 伪字节（判定只看哈希，无需真图）
    figure = b"\x89PNG\r\n\x1a\n" + b"F" * 200
    onlysub = b"\x89PNG\r\n\x1a\n" + b"O" * 200
    sub_buf = _io.BytesIO()
    with _zip.ZipFile(sub_buf, "w") as zsub:
        zsub.writestr("word/media/banner.jpeg", banner)   # 与顶层 media 同字节（页眉横幅）
        zsub.writestr("word/media/figure.png", figure)    # 已落盘的正常子文档图
        zsub.writestr("word/media/lonely.png", onlysub)   # 真丢：磁盘/顶层都没有
    tmp_docx = Path(tempfile.mkdtemp(prefix="fulltest-selftest-", dir=str(tmp_root))) / "d.docx"
    with _zip.ZipFile(tmp_docx, "w") as ztop:
        ztop.writestr("word/media/image1.png", banner)    # 顶层文档自己的 media
        ztop.writestr("word/embeddings/sub.docx", sub_buf.getvalue())
    img_dir2 = tmp_docx.parent / "imgs"
    img_dir2.mkdir()
    (img_dir2 / "image_0.png").write_bytes(figure)
    lost, total = _embedded_media_lost(tmp_docx,
                                       {"img_dir_files": list(img_dir2.iterdir())})
    check("与顶层 media 同字节的嵌入图不算丢失，真丢仍报",
          (lost, total) == (["lonely.png"], 3), f"lost={lost} total={total}")
    shutil.rmtree(tmp_docx.parent, ignore_errors=True)
    try:
        from PIL import Image as _Img
        has_pil = True
    except Exception:
        has_pil = False
    if has_pil:
        buf = _io.BytesIO()
        _Img.new("RGB", (4, 4), (200, 30, 40)).save(buf, "PNG")
        png_bytes = buf.getvalue()
        buf = _io.BytesIO()
        _Img.open(_io.BytesIO(png_bytes)).save(buf, "BMP")
        bmp_bytes = buf.getvalue()                        # 同像素、不同容器字节
        sub2 = _io.BytesIO()
        with _zip.ZipFile(sub2, "w") as zsub:
            zsub.writestr("word/media/pic.png", png_bytes)
        tmp_docx2 = Path(tempfile.mkdtemp(prefix="fulltest-selftest-",
                                          dir=str(tmp_root))) / "d2.docx"
        with _zip.ZipFile(tmp_docx2, "w") as ztop:
            ztop.writestr("word/media/image1.bmp", bmp_bytes)
            ztop.writestr("word/embeddings/sub.docx", sub2.getvalue())
        lost2, _t2 = _embedded_media_lost(tmp_docx2, {"img_dir_files": []})
        check("与顶层 media 同像素(重编码)的嵌入图经像素回退豁免", lost2 == [],
              f"lost={lost2}")
        shutil.rmtree(tmp_docx2.parent, ignore_errors=True)

    # --- _judge_xlsx_shapes：a:t 是 XML 文本节点，实体解码后才可比对 ---
    import zipfile as _zipf
    with tempfile.TemporaryDirectory() as _td:
        _xlsx = Path(_td) / "shapes.xlsx"
        with _zipf.ZipFile(_xlsx, "w") as _z:
            _z.writestr(
                "xl/drawings/drawing1.xml",
                '<xdr:wsDr xmlns:a="x"><a:t>P&amp;ID 图</a:t>'
                "<a:t>&lt;</a:t><a:t>&gt;</a:t><a:t>丢失标记ABCD</a:t></xdr:wsDr>")
        _iss = []
        _judge_xlsx_shapes(_xlsx, "P&ID 图 \\< \\>\n", _iss)
        check("a:t 实体解码：转义输出可命中、只报真丢失(1/4)",
              [i["code"] for i in _iss] == ["XLSX_SHAPE_TEXT_MISSING"]
              and "1/4" in _iss[0]["message"], str(_iss))
        _iss2 = []
        _judge_xlsx_shapes(_xlsx, "P&ID 图 \\< \\> 丢失标记ABCD\n", _iss2)
        check("a:t 实体字面量不必出现在 MD 里(不假报丢失)",
              _iss2 == [], str(_iss2))

    # --- ISSUE_META 登记：新问题码必须有严重度 ---
    check("JUDGE_CRASH 登记为 FAIL 级", ISSUE_META.get("JUDGE_CRASH") == "FAIL")

    verdict = "全部通过" if not fails else f"{len(fails)} 项失败: {fails}"
    print(f"selftest: {verdict}", flush=True)
    return 1 if fails else 0


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
        try:
            probe = subprocess.run(
                [str(cli), "extract", str(av_files[0]),
                 "--no-config-discovery", "--format", "json",
                 "--config-json", json.dumps({"transcription": {**TRANSCRIPTION_CFG, "max_bytes": 1}})],
                capture_output=True, env=env, timeout=120)
        except (subprocess.TimeoutExpired, OSError) as e:
            sys.exit(f"transcription feature 探测失败: {e}")
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
    ap.add_argument("--selftest", action="store_true",
                    help="只跑判定器自测（合成样例，不需要 CLI/语料/金标准），退出码 0/1")
    ap.add_argument("--force", action="store_true",
                    help="跳过 --save-baseline 护栏（存在 FAIL 或金标准未加载时仍保存基线）")
    args = ap.parse_args()
    if args.selftest:
        sys.exit(run_selftest())
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
    # 配置级告警（未知键/pattern 卫生）必须进报告文件，不能只打在终端
    for w in EXPECTATIONS_WARNINGS:
        emit(f"[expectations] 配置告警: {w}")

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

    files = sorted(p for p in src.iterdir() if p.is_file() and not _is_windows_noise(p))
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
    # 对抗语料（子目录不进主队列；主队列全部跑完后追加失败路径测试）
    adv_dir = src / "_adversarial"
    # 与 run_adversarial 同一筛选（AV 不进失败路径队列、剔除 Windows 噪音文件），
    # 供 stem 消歧与预检计数
    adv_files = sorted(p for p in adv_dir.iterdir() if p.is_file()
                       and p.suffix.lower().lstrip(".") not in AV_EXTS
                       and not _is_windows_noise(p)) \
        if adv_dir.is_dir() else []
    if adv_files:
        print(f"[preflight] 对抗语料: {adv_dir}（{len(adv_files)} 个失败路径样本，主队列后追加）",
              flush=True)
    # 主队列+对抗文件统一预生成产物命名 tag（见 _stem_tags）：重名 stem 按扩展名消歧，
    # 对抗文件恒加 `_adv`——否则主队列与 _adversarial 同名同扩展仍会同 tag，
    # {tag}_images/{tag}.err.txt/{tag}.md 会互相覆盖审计产物；判定键仍用 f.name，不受影响。
    tags = _stem_tags(files, adv_files)

    preflight(cli, env, files)
    print(f"集成测试: {len(files)} 个文件 | 源: {src} | 输出: {out_dir} | "
          f"超时: {args.timeout}s | 遇 FAIL {'继续' if args.keep_going else '立即终止'} | "
          f"strict={args.strict}", flush=True)

    results = []
    stopped_early, early_reason = False, ""
    t_start = time.time()
    n = len(files)
    # 代码指纹（进报告与基线；失败不阻断，记 null）
    git_info = _git_info()

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
        # 产物命名 tag：与 convert_one 内部一致（同 stem 重名时带后缀，见 _stem_tags）
        tag = tags[f]
        img_dir = out_dir / f"{tag}_images"
        # 超时记录也要带 golden.applied，否则基线与本次的问题码集合按「未加载金标准」剔除，
        # 该文件会显示成一批假「已修复」。
        exp = expect_for(f.name)
        try:
            md_text, meta, elapsed, rc, used_cli = convert_one(cli, f, out_dir, args.timeout,
                                                               env, transcription=av, tag=tag)
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
        recall_info = {"note": None, "bigram": None, "num_recall": None,
                       "ident_recall": None, "missing_nums": [], "missing_id": [],
                       "char_ratio": None, "src_images": None, "src_images_note": None,
                       "recall_good": RECALL_GOOD, "recall_fail": RECALL_FAIL}
        source_pages = None
        # 判定链自身抛异常（第三方库对异常源文件崩溃、未预料的输入形态等）不得把
        # 整轮验收炸掉——报告落盘与基线对比还要继续。两段判定（结构/语义与源文对齐）
        # 整体包住，单文件降级为 JUDGE_CRASH（FAIL：该文件本轮判定不完整，属红项）。
        # m/meta/recall_info/source_pages 均已在 try 前初始化，except 后的结果收集照常执行。
        try:
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
                    f, md_text, out_dir / f"{tag}.err.txt", issues, exp, OCR_CONFIG, m=m)

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
        except Exception as e:  # noqa: BLE001 - 判定器异常只记单个文件，不中止整轮
            issues.append(make_issue(
                "JUDGE_CRASH", f"判定阶段异常({type(e).__name__}): {str(e)[:160]}"))
            emit(f"  [JUDGE_CRASH] 判定阶段异常({type(e).__name__}): {str(e)[:160]}"
                 "（该文件按 FAIL 记录，报告与基线对比照常）")

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

    # 对抗语料：主队列后追加，结果并入汇总/JSON/基线对比（对抗文件名作 key）；
    # 默认遇 FAIL 提前终止时同样跳过（与「立即终止」语义一致，--keep-going 才会跑到这里）。
    # 启用门与 run_adversarial 同一筛选（AV/Windows 噪音文件不算样本）。
    if not stopped_early and adv_dir.is_dir() and any(
        p.is_file() and p.suffix.lower().lstrip(".") not in AV_EXTS and not _is_windows_noise(p)
        for p in adv_dir.iterdir()
    ):
        results.extend(run_adversarial(cli, adv_dir, out_dir, args.timeout, env, tags=tags))

    n_fail, n_warn = print_summary(results, stopped_early, early_reason, strict=args.strict)

    # 回归对比：只呈现在报告里，不追加 issue、不影响 verdict
    prev_baseline = load_baseline(Path(args.baseline))
    regression = (compare_baseline(prev_baseline, JSON_RESULTS,
                                   bool(EXPECTATIONS.get("files")))
                  if prev_baseline else None)
    if regression is None:
        emit("\n## 与基线对比")
        emit("  （未找到基线文件，未做回归对比；用 --save-baseline 生成）")
    else:
        emit(f"\n## 与基线对比（基线: {args.baseline}，"
             f"生成于 {prev_baseline.get('generated_at') or '未知时间'}）")
        if regression.get("skipped_exp_codes"):
            emit("  （本次有文件未加载金标准：其依赖金标准的检查码"
                 "（GOLDEN_*/OCR/阈值类）已从对比中剔除）")
        if regression.get("expectations_changed"):
            prev_sha, cur_sha = regression["expectations_changed"]
            emit(f"  ⚠ 金标准文件与基线生成时不同（sha256 {prev_sha[:12]} → {cur_sha[:12]}）："
                 "阈值类对比仅供参考；若金标准确经校准修改，请确认依据后 --save-baseline 重设。")
        for name, d in regression["files"].items():
            worse_s = "，".join(f"{c}×{b}→{n}" for c, (b, n) in (d.get("worse") or {}).items())
            emit(f"  {name}: 新增 [{', '.join(d['new'])}] | 已修复 [{', '.join(d['fixed'])}]"
                 + (f" | 恶化 [{worse_s}]" if worse_s else ""))
        emit(f"  合计: 新增 {regression['totals']['new']} 项 | "
             f"已修复 {regression['totals']['fixed']} 项 | "
             f"恶化 {regression['totals']['worsened']} 项(同码次数增加)")

    if args.save_baseline:
        reason = baseline_block_reason(JSON_RESULTS, bool(EXPECTATIONS.get("files")),
                                       stopped_early=stopped_early,
                                       early_file=early_reason)
        if reason and not args.force:
            emit(f"\n--save-baseline 已跳过: {reason}")
            print(f"回归基线未保存（护栏拦截；确认后可加 --force）: {args.baseline}")
        else:
            if reason:
                emit(f"\n--save-baseline: {reason}（--force 已给定，仍按本次结果保存）")
            Path(args.baseline).write_text(json.dumps({
                "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
                "cli": str(cli),
                "git": git_info,
                "expectations_sha256": EXPECTATIONS_SHA256,
                "thresholds": {
                    "recall_fail": RECALL_FAIL, "recall_good": RECALL_GOOD,
                    "num_recall_fail": NUM_RECALL_FAIL, "strict": args.strict,
                    "char_ratio_fail": CHAR_RATIO_FAIL, "char_ratio_warn": CHAR_RATIO_WARN,
                    "page_cover_fail": PAGE_COVER_FAIL, "page_cover_warn": PAGE_COVER_WARN,
                },
                "argv": sys.argv[1:],
                "files": [
                    {"name": r["name"], "verdict": r["verdict"],
                     "golden_applied": bool((r.get("golden") or {}).get("applied")),
                     "adversarial": bool(r.get("adversarial")),
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
              f"- 代码: commit `{git_info['commit'] or '?'}`"
              f"{'（含未提交改动）' if git_info['dirty'] else ''}\n"
              f"- 阈值: recall_fail={RECALL_FAIL} recall_good={RECALL_GOOD} "
              f"num_recall_fail={NUM_RECALL_FAIL} char_ratio={CHAR_RATIO_FAIL}/{CHAR_RATIO_WARN} "
              f"page_cover={PAGE_COVER_FAIL}/{PAGE_COVER_WARN} strict={args.strict}\n"
              f"- 金标准: {args.expectations}（{len(EXPECTATIONS.get('files') or {})} 个文件条目；"
              f"sha256 {(EXPECTATIONS_SHA256 or '?')[:12]}；每个文件应用的键见报告内问题码）\n")
    report_path = out_dir / "_quality-report.md"
    report_path.write_text(header + "\n".join(REPORT_LINES) + "\n", encoding="utf-8")
    json_path = out_dir / "_quality-report.json"
    json_path.write_text(json.dumps({
        "cli": str(cli),
        "src": str(src),
        "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "git": git_info,
        "expectations_path": args.expectations,
        "expectations_sha256": EXPECTATIONS_SHA256,
        "expectations_warnings": EXPECTATIONS_WARNINGS,
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
