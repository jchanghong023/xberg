# AGENTS.md

## 仓库性质

- 本仓库是 fork：origin = `https://github.com/jchanghong023/xberg.git`（个人仓库），上游 = `https://github.com/xberg-io/xberg.git`（remote 名 `upstream`）。同步上游用普通 `git merge upstream/main`（历史上即如此）。
- **仅支持 Windows**：个人代码和包只在 Windows 11 上使用。本地编译与 GitHub 流水线（见 `.github/workflows/build-windows-cli.yml`）都只针对 Windows，不要为其他平台做适配或测试。
- **AGENTS.md 跟踪入库（fork 特有约定）**：上游的 `.gitignore` 故意忽略 `AGENTS.md`，本 fork 已删除该忽略条目以保留本文件；merge 上游时若把这条忽略规则带了回来，必须再次移除，保住本文件的入库状态。

## 硬性约束（必须遵守）

- **总工作流（用户的固定需求，以后无需重新解释）**：日常开发改完代码 → 用户明确要求时跑 `fulltest.py`（本地编译的二进制，快速验证转换质量）；发布前用户明确要求时跑 `slowtest.py`（完整打包 → 打包版全量端到端测试 → 质量报告），是否发布由用户根据报告决定。
- **严禁私自跑费时操作**：未经用户明确点名要求，不得运行任何编译（`cargo build`/`check`/`clippy`/`test` 等一切编译类命令）、测试（`fulltest.py`、`slowtest.py`、端到端测试）或打包（`package-cli-windows.ps1`）。何时测试由用户自己决定。
- **修改代码时不要编译本地代码（效率很低），尽快交付改动**。修改后的验证以静态手段为限：阅读代码、diff 审查、脚本类语法检查。
- **`fulltest.py`（仓库根目录）只在用户明确要求时才运行**。它是文档转换效果的集成测试：遍历 `D:\测试转markdown转换效果\测试文档`，逐文件调用本地编译的 CLI 转成 Markdown，打印三层质量评估（结构启发式、用 pymupdf/python-docx/python-pptx/openpyxl 对源文件算文本召回率、xberg 元数据警告），结果输出到 `D:\测试转markdown转换效果\测试文档_md_fulltest`。运行它只需本地编译出 exe（不需要打包），默认遇 FAIL 立即终止，`--keep-going` 跑完；音视频转写超时默认 1800s。音视频也只用本地编译版：预检会用 max_bytes=1 快速探测 transcription feature，缺 feature 直接报错退出并提示 `cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,transcription`，**不回退打包版**。
- **`slowtest.py`（仓库根目录）同样只在用户明确要求时才运行**。慢速全量验证，测**打包版 CLI**：① 跑 `scripts/publish/cli/package-cli-windows.ps1` 打完整 zip；② 解压到临时目录，对解压出的 xberg.exe 跑 fulltest.py（`--keep-going` 全量测完，`--pkg-dir` 指向解压目录）；③ 输出转码质量报告（报告副本存 `target/slowtest-report-<时间戳>.md`）。包含完整 release 编译与打包，耗时可能 30 分钟以上；`--skip-package` 可复用已有 zip。**用户根据该报告决定是否发布版本。**

## 项目结构

- Rust workspace，核心 crate：`crates/xberg`（核心库）、`crates/xberg-cli`（二进制 `xberg`，clap 定义在 `src/main.rs`）；其余 `xberg-ffi` / `xberg-node` / `xberg-wasm` / `xberg-py` 等为语言绑定。
- `docs-site/`（Docusaurus 文档）、`e2e/` + `fixtures/`（端到端测试）、`.ai-rulez/`（ai-rulez 管理的 AI 规则/技能，改规则后需用固定版本的 ai-rulez 重新生成 bundle）、`round1~4-convert.sh`（历史转换脚本，fulltest.py 的参照）。

## 编译 / 打包 / 测试（仅在用户明确要求时执行）

编译和打包是两条独立路径。**跑 fulltest.py 只需要编译，不需要打包**——编译出 exe 直接 `python fulltest.py` 就能测。

### 编译（供开发与 fulltest.py 使用）

本 fork 只需要「文件 → Markdown」+ OCR + 音视频转写，**不要** `all` / heic / pdfium / embedding / NER / MCP / api。

- **标准调试构建（推荐，fulltest 用这个）**：

  ```bat
  cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,transcription
  ```

  产物：`target\debug\xberg.exe`。

  | feature | 覆盖 |
  |---|---|
  | `core-cli` | CLI 入口 |
  | `formats-no-heic` | docx / pptx / xlsx / pdf 等（无 libheif） |
  | `analysis` | 转换管线 |
  | `ocr` | 图片 → 文字（Tesseract） |
  | `transcription` | 音视频 → 文字（Whisper + ORT） |

- **不要** `cargo build -p xberg-cli`（default 带 embeddings/paddle/candle/layout 等，比上述集重）。
- **不要** `--features all`（额外拉 api/mcp/heic/pdfium/ner/summarization，且 heic 在 Windows 常编不过）。
- 音视频与本地编译版：fulltest.py 对所有文件（含音视频）只用 `--cli` 指定的本地 CLI，无任何打包回退；缺 transcription feature 时预检即失败并提示重编。

### 打完整包（发布用，含模型与 DLL）

- 一条命令：`pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1`。本地默认并行度 min(30, 逻辑核数)，可用 `-Jobs N` 覆盖；CI（`.github/workflows/build-windows-cli.yml`）调用的是同一脚本，是打包的唯一入口。
- 产物：`xberg-cli-x86_64-pc-windows-msvc\` 目录（xberg.exe + onnxruntime/CRT DLL + Whisper tiny 模型）并压成同名 zip。
- **feature 集与开发编译一致**：`formats-no-heic,core-cli,analysis,ocr,transcription`（`--no-default-features`）。**不含** heic、pdfium、paddle-ocr、layout、candle、api/mcp、embedding、NER。因此包内**不**附带 pdfium.dll 与 PaddleOCR/版面 ONNX 模型，只附带 Whisper tiny（离线转写）。
- 打包前有门禁：干净 PATH 的 `--version` 探测、MSVC CRT 部署、PE 导入闭包校验（`scripts/ci/verify-windows-dll-closure.ps1`）、离线 smoke（默认抽取，不依赖 paddle/layout）。
- 用打包版跑音视频时：`HF_HOME` 指向包目录下 `models`，`PATH` 前置包目录（fulltest.py 已自动处理）。

### 测试入口与发布流程

- **默认快速验证**：`python fulltest.py`——用本地编译的二进制快速验证（只需编译，无需打包），质量报告打印到终端并写入 `<输出目录>/_quality-report.md`。
- **发布前慢速验证**：`python slowtest.py`——① 跑完整打包生成 zip；② 解压到临时目录，对打包版 CLI 全量跑端到端文档转换 + 音频/视频转写测试（`--keep-going`）；③ 输出转码质量报告。**是否发布版本，以该报告为准。**

### 其他验证（同样仅在明确要求时执行）

- Rust 测试：`cargo test -p xberg`；lint：`cargo clippy`；task runner 为 `Taskfile.yml`（`task build`、`task test` 等）。

## 已知坑

- CLI `--format json` 模式**不落盘图片**（只有 text/toon 模式调用 `write_extracted_images`）；JSON 里图片是内联字节数组（`result.images[].data`），需要自己解码写出。
- `--config-json` 只做顶层字段替换后整体反序列化 + 校验（`crates/xberg/src/core/config/merge.rs`），feature 未编译的字段会直接报错。
- heic 默认关闭（Windows 无 libheif 构建路径），本 fork **不要**开 `heic`；pdfium 后端同样不要（`pdf-pdfium-surface` 仅编 wrapper，还要另供 libpdfium）。
- 编译/打包统一用最小 feature 集 `formats-no-heic,core-cli,analysis,ocr,transcription`；需要改能力时先改 AGENTS.md 与 `scripts/publish/cli/package-cli-windows.ps1` 的 `$Features`，两边保持一致。
