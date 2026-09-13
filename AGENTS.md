# AGENTS.md

## 仓库性质

- 本仓库是 fork：origin = `https://github.com/jchanghong023/xberg.git`（个人仓库），上游 = `https://github.com/xberg-io/xberg.git`（remote 名 `upstream`）。同步上游用普通 `git merge upstream/main`（历史上即如此）。
- **仅支持 Windows**：个人代码和包只在 Windows 11 上使用。本地编译与 GitHub 流水线（见 `.github/workflows/build-windows-cli.yml`）都只针对 Windows，不要为其他平台做适配或测试。
- **AGENTS.md 跟踪入库（fork 特有约定）**：上游的 `.gitignore` 故意忽略 `AGENTS.md`，本 fork 已删除该忽略条目以保留本文件；merge 上游时若把这条忽略规则带了回来，必须再次移除，保住本文件的入库状态。

## 硬性约束（必须遵守）

- **总工作流（用户的固定需求，以后无需重新解释）**：日常开发改完代码 → 用户明确要求时跑 `fulltest.py`（本地编译的二进制，快速验证转换质量）；发布前用户明确要求时跑 `slowtest.py`（完整打包 → 打包版全量端到端测试 → 质量报告），是否发布由用户根据报告决定。
- **严禁私自跑费时操作**：未经用户明确点名要求，不得运行任何编译（`cargo build`/`check`/`clippy`/`test` 等一切编译类命令）、测试（`fulltest.py`、`slowtest.py`、端到端测试）或打包（`package-cli-windows.ps1`）。何时测试由用户自己决定。
- **修改代码时不要编译本地代码（效率很低），尽快交付改动**。修改后的验证以静态手段为限：阅读代码、diff 审查、脚本类语法检查。
- **临时脚本 / 临时目录 / 临时文件只准放 `./.tmp`**：仓库内的一次性脚本、临时目录、临时文件一律建在仓库根目录的 `.tmp\`（不存在先 `mkdir -p .tmp`），不得散落在仓库根、`scratch*` 或其他任何目录——根目录只保留仓库资产，避免 `git status` 噪音和误提交；`.tmp/` 已在 `.gitignore` 中（不入库）。验证完自行清理临时产物。
- **`fulltest.py`（仓库根目录）只在用户明确要求时才运行**。它是文档转换效果的集成测试：遍历 `D:\测试转markdown转换效果\测试文档`，逐文件调用本地编译的 CLI 转成 Markdown，打印四层质量评估（结构启发式、用 pymupdf/python-docx/python-pptx/openpyxl 对源文件算文本召回率、xberg 元数据警告、深检与逐文件金标准断言），结果输出到 `D:\测试转markdown转换效果\测试文档_md_fulltest`。运行它只需本地编译出 exe（不需要打包），默认遇 FAIL 立即终止，`--keep-going` 跑完；音视频转写超时默认 1800s。音视频也只用本地编译版：预检会用 max_bytes=1 快速探测 transcription feature，缺 feature 直接报错退出并提示 `cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api`，**不回退打包版**。
- **`slowtest.py`（仓库根目录）同样只在用户明确要求时才运行**。慢速全量验证，测**打包版 CLI**：① 跑 `scripts/publish/cli/package-cli-windows.ps1` 打完整 zip；② 解压到临时目录，对解压出的 xberg.exe 跑 fulltest.py（`--keep-going` 全量测完，`--pkg-dir` 指向解压目录）；③ 输出转码质量报告（报告副本存 `target/slowtest-report-<时间戳>.md`）。包含完整 release 编译与打包，耗时可能 30 分钟以上；`--skip-package` 可复用已有 zip。**用户根据该报告决定是否发布版本。**

## fulltest.py 的作用（本仓库的验收标准）

- **它就是验收标准本身**：`fulltest.py` 产出的 `_quality-report.md` / `_quality-report.json` 即转换质量判定——**报告里的红项（FAIL/WARN）＝当前待修清单，11 个文件全部 PASS（"全绿"）＝达标**。转换器改动一律以「红项减少 / 无新增码」评估，不靠人工逐文档复核。
- **检查分四层**（每层独立出码）：① 进程/结构（退出码、空结果、乱码与标记泄漏、围栏与表格列、落盘图片合法性）；② 源文对齐（去页眉后 bigram 召回、数字与标识召回、正文体量比、分页覆盖）；③ 交叉核对（源页数/媒体清单 vs 引擎 counts vs 落盘 vs MD 引用、音视频转写量）；④ 深检（Markdown 语义噪声、内嵌对象与子文档保真、PPTX 标题与备注、xlsx 图形文本、PDF 书签与表格数、OCR 通道、逐文件金标准断言）。
- **仓外数据文件（不入 Git）**：
  - `D:\测试转markdown转换效果\_expectations.json`：逐文件金标准。键有 `required_tokens` / `forbidden_patterns` / `order` / `min_*`、`max_*` 指标 / `require_chinese_ocr` / `require_nested_bullets` / `toc_heading_min_recall` 等；顶层 `run.ocr_config` 是 OCR 覆盖配置（默认空＝不覆盖 CLI 的 OCR 设置），`run.layout_config` 是 layout 覆盖配置（**不写该键＝不注入 layout**；写 `{}`＝给 CLI 注入 layout 默认配置，用于 `测试识别.png` 的 `min_tables=2` 这类需要版面/表格模型的目标；改判定的阈值按实测校准，见各文件 `_note_*`）。
  - `D:\测试转markdown转换效果\_quality-baseline.json`：回归基线，`--save-baseline` 生成；报告 `## 与基线对比` 输出「新增/已修复」（只比问题码集合）。未加载金标准的运行里，依赖金标准的码（`GOLDEN_*`、OCR 期望码、阈值类）会从对比两侧剔除，避免出现假的"已修复"。
- **常用命令**：`python fulltest.py --keep-going` 跑完不停；`--save-baseline` 更新基线；`--no-expectations` / `--expectations <path>` 为降级跑法（请同时用 `--out` 指向独立目录，别覆盖标准报告）；`--strict` 把 WARN 也算失败。
- **改完检查判定/阈值后必须 `--save-baseline` 重设基线**：`## 与基线对比` 只比问题码集合，用的还是旧判定的话，修正掉的假阳会继续显示成"已修复"，污染转换改进的读数。
- **改脚本时的约定**：阈值问题先调常量或期望值，不删检查；新增「是否出码依赖金标准」的检查必须把码名登记进 `fulltest.py` 的 `EXP_GATED_CODES`；新增问题码要同时加进 `ISSUE_META` 并给严重度。
- **已知不自动判定的形态**（改脚本前先看这里，别把它们当成已覆盖）：① 同一内容二次 OCR 输出的重复（两遍乱码不同，8-gram 无交集 → `DUP_CONTENT` 不报，仅当围栏块与正文/其他围栏块高度相似才报）；② 源图被截图浮层遮挡/裁掉的像素（如被上传按钮盖住的 `()`）；③ 中文词内部被插空（与 `IDENT_FRAGMENTED` 的拉丁标识符判据不同，误报率高）；④ 图表题注顺序颠倒（缺稳定锚点）；⑤ 归属错位类（内嵌对象文本堆在文末、不插回所属页）；⑥ 行内 `| --- |` 形式的"表格被压成一行正文"（长度阈值够不着）。这些在报告里不会出现，需要人工或后续新增检查。
- **阈值来源与校准**：`_expectations.json` 里的 `min_fenced_cjk` 等取自 chi_sim 实测值的一半（OCR 退化回英文输出仍判红）；`min_tables`/`min_headings` 以源文档事实为准（如 tessent 取文档自带 `Table N-M.` 题注 54 个的量级，而不是 `find_tables` 的 468 个——后者约 350 个是页眉框）。改阈值前先按 `_note_*` 注释确认推导依据。
- `slowtest.py` 只是把同一份 `fulltest.py` 换成打包版 CLI 再跑一遍（参数不变），新检查对打包版自动生效。

## 项目结构

- Rust workspace，核心 crate：`crates/xberg`（核心库）、`crates/xberg-cli`（二进制 `xberg`，clap 定义在 `src/main.rs`）；其余 `xberg-ffi` / `xberg-node` / `xberg-wasm` / `xberg-py` 等为语言绑定。
- `docs-site/`（Astro + Starlight 文档）、`e2e/` + `fixtures/`（端到端测试）、`.ai-rulez/`（ai-rulez 管理的 AI 规则/技能，改规则后需用固定版本的 ai-rulez 重新生成 bundle）。

## 编译 / 打包 / 测试（仅在用户明确要求时执行）

编译和打包是两条独立路径。**跑 fulltest.py 只需要编译，不需要打包**——编译出 exe 直接 `python fulltest.py` 就能测。

### 编译（供开发与 fulltest.py 使用）

本 fork 只需要「文件 → Markdown」+ OCR + 音视频转写 + HTTP 服务（`xberg serve`），**不要** `all` / heic / pdfium / embedding / NER / MCP。

- **标准调试构建（推荐，fulltest 用这个）**：

  ```bat
  cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api
  ```

  产物：`target\debug\xberg.exe`。

  | feature | 覆盖 |
  |---|---|
  | `core-cli` | CLI 入口 |
| `api` | `xberg serve`——HTTP API 服务（axum 纯 Rust 栈，不新增 DLL）；本地编译与打包都必须带 |
  | `formats-no-heic` | docx / pptx / xlsx / pdf 等（无 libheif） |
  | `analysis` | 转换管线 |
  | `ocr` | 图片 → 文字（Tesseract，后备） |
  | `paddle-ocr` | 图片 → 文字（PaddleOCR pp-ocrv6 **tiny**，默认 OCR 后端）；模型随包分发 |
  | `transcription` | 音视频 → 文字（Whisper + ORT） |
  | `layout-detection` | 版面检测（RT-DETR）+ 表格结构（TATR）；图片/PDF 的表格只有它能产出。**需要 2 个 ONNX 模型**（见打包小节），且抽取时须带 `layout` 配置（`fulltest.py` 用期望文件 `run.layout_config` 注入） |

- **不要** `cargo build -p xberg-cli`（default 带 embeddings/candle 等，比上述集重）。
- **不要** `--features all`（在标准集之上额外拉 mcp/heic/pdfium/ner/summarization 等，且 heic 在 Windows 常编不过）。
- 音视频与本地编译版：fulltest.py 对所有文件（含音视频）只用 `--cli` 指定的本地 CLI，无任何打包回退；缺 transcription feature 时预检即失败并提示重编。

### 打完整包（发布用，含模型与 DLL）

- 一条命令：`pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1`。本地默认并行度 min(30, 逻辑核数)，可用 `-Jobs N` 覆盖；CI（`.github/workflows/build-windows-cli.yml`）调用的是同一脚本，是打包的唯一入口。
- 产物：`xberg-cli-x86_64-pc-windows-msvc\` 目录（xberg.exe + onnxruntime/CRT DLL + Whisper tiny 模型）并压成同名 zip。
- **feature 集与开发编译一致**：`formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api`（`--no-default-features`），打包版 exe 含 `xberg serve` HTTP API。**不含** heic、pdfium、candle、mcp、embedding、NER。因此包内**不**附带 pdfium.dll。
- **模型随包分发（离线可用）**：Whisper tiny（`$TranscriptionFiles` 固定校验和）+ 版面/表格 **RT-DETR 161.3 MiB + TATR 28.8 MiB** + **PaddleOCR pp-ocrv6 tiny ≈13 MiB**（det/rec/dict/textline-cls 四条正则，经 `xberg.exe cache manifest` 取二进制里的 sha256/大小后 stage 到 `models/` 的 HF 缓存布局）。`$RequiredModels` 非空会**启用**离线空缓存探测（`scripts/publish/cli/offline-smoke.ps1 -ExpectEmptyCacheFailure`）：该探测用带 `layout`+`ocr` 配置的抽取跑（`should_use_layout_ocr` 为真才会解析模型），空缓存下必须报出 HF 离线缺模型诊断；空缓存不保证进程一定失败（图片路径会退回整图 OCR，是否再失败取决于编入的 OCR 后端是否需要 HF 模型），所以断言的是诊断而非退出码。清单里另有 SLANeXT/SLANet_plus/table_classifier/pp_doclayout_v3，本构建不用，**不要**加进 `$RequiredModels`（每条正则必须命中且仅命中 1 条）。
- 包体参考：加 layout + paddle tiny 后 zip **≈ 368 MiB**（原 ≈355）。首次改动后 `target/package-models-<target>` 会多缓存 ~200 MiB（跨次复用）。
- 打包前有门禁：干净 PATH 的 `--version` 探测、MSVC CRT 部署、PE 导入闭包校验（`scripts/ci/verify-windows-dll-closure.ps1`）、离线 smoke（`offline-smoke.ps1`：带 `layout`+`ocr` 配置的抽取必须成功，且 stderr 出现 `Layout detection completed` 而无离线缺模型诊断；`$RequiredModels` 非空时再跑一次空缓存探测，stderr 必须报出 HF 离线缺模型诊断）。
- 用打包版跑音视频时：`HF_HUB_CACHE` 指向包目录下 `models`（hf-hub 的缓存根；`HF_HOME` 会被它解析成 `$HF_HOME/hub`，指不到包内模型），`PATH` 前置包目录（fulltest.py 已自动处理）。

### 测试入口与发布流程

- **默认快速验证**：`python fulltest.py`——用本地编译的二进制快速验证（只需编译，无需打包），质量报告打印到终端并写入 `<输出目录>/_quality-report.md`。
- **发布前慢速验证**：`python slowtest.py`——① 跑完整打包生成 zip；② 解压到临时目录，对打包版 CLI 全量跑端到端文档转换 + 音频/视频转写测试（`--keep-going`）；③ 输出转码质量报告。**是否发布版本，以该报告为准。**

### 其他验证（同样仅在明确要求时执行）

- Rust 测试：`cargo test -p xberg`；lint：`cargo clippy`；task runner 为 `Taskfile.yml`（`task build`、`task test` 等）。

## 已知坑

- CLI `--format json` 模式**不落盘图片**（只有 text/toon 模式调用 `write_extracted_images`）；JSON 里图片是内联字节数组（`result.images[].data`），需要自己解码写出。
- `--config-json` 只做顶层字段替换后整体反序列化 + 校验（`crates/xberg/src/core/config/merge.rs`），feature 未编译的字段会直接报错。
- heic 默认关闭（Windows 无 libheif 构建路径），本 fork **不要**开 `heic`；pdfium 后端同样不要（`pdf-pdfium-surface` 仅编 wrapper，还要另供 libpdfium）。
- 编译/打包统一用 fork feature 集 `formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api`；需要改能力时先改 AGENTS.md 与 `scripts/publish/cli/package-cli-windows.ps1` 的 `$Features`，两边保持一致。默认 OCR 后端是 **PaddleOCR pp-ocrv6 tiny**（`PaddleOcrConfig::new` 的 `model_tier`）；Tesseract 仍编进二进制作后备，需要时用 `--ocr-backend tesseract` 或 `ocr.backend` 覆盖。
- **layout 是配置门控**：只编 `layout-detection` 不会自动跑版面/表格模型——抽取配置必须带 `layout`（`ExtractionConfig.layout: Option<...>`，默认 None）。验收侧靠期望文件 `run.layout_config`（键存在即注入，`{}` = 引擎默认）开启；缺模型时离线会直接失败（这正是打包门禁要证明的）。
