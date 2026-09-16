# fork.md — 本 fork 相对上游的定制清单

上游 = `https://github.com/xberg-io/xberg.git`（remote 名 `upstream`）。本文件是 **merge 上游时解决冲突的依据**和 fork 改动的唯一索引：改动下列任何能力，必须同步更新本文件。保持精简，细节以代码与提交历史为准（`git log upstream/main..main`）。

## 定位

仅 Windows 11 的个人 fork：只做「文件 → Markdown」+ OCR + 音视频转写 + HTTP 服务（`xberg serve`）。不维护上游的语言绑定、heic、pdfium、embedding、NER、MCP 等能力（feature 集见 AGENTS.md）。

## 引擎定制（crates/，冲突时保 fork 行为）

- **默认 OCR = PaddleOCR pp-ocrv6 tiny**，Tesseract 编入作后备（`paddle_ocr/`、`core/config/ocr.rs`）。
- **图片 OCR 渲染**：OCR 文本与图片路径合并进一个保持原位的围栏块（`rendering/ocr_layout.rs`，fork 新增）；`ocr_text_only` 开启时也保留该块。
- **EMF/WMF 图元文件 → PNG**：新 crate `crates/xberg-windows-metafile`（纯 GDI 栅格化，fork 新增），Office 内嵌图元文件因此可落盘、可 OCR。
- **Visio 抽取**：`.vsd`/`.vsdx`/`.vsdm` 形状文本（`extraction/visio.rs`、`extractors/visio.rs`，fork 新增）。
- **xlsx 内嵌图片**：提取并 OCR（`extraction/excel/images.rs`，fork 新增）。
- **OOXML/OLE 内嵌对象**：内嵌 Word OLE 文本合并进宿主文档、PPTX OLE/PresentationML fallback 图片抽取（`extraction/ooxml_embedded.rs`）。
- **PPTX 质量**：标题/演讲者备注读取与归属守卫（`extractors/pptx.rs`、`extraction/pptx/`）。
- **PDF 质量大改**：表格重建（`pdf/table_reconstruct.rs`）、原生文本抽取（`pdf/native/text.rs`）、结构分类/段落/页眉页脚剔除（`pdf/structure/`）；有原生文本的页面不做破坏性整页 OCR 回退。
- **Windows Media 转写**：Media Foundation 读 ASF/WMV 音轨，解码失败回退外部 ffmpeg（`transcription/container.rs`、`transcription/wmf.rs`，fork 新增）。
- **Markdown 渲染**：图片 alt 路径清理、图片与 OCR 文本同块等输出修复（`rendering/markdown.rs`、`rendering/comrak_bridge.rs`）。

## 打包 / CI（fork 独有或大改）

- `.github/workflows/build-windows-cli.yml`（新增）：fork 自有的打包流水线（仅手动 `workflow_dispatch` 触发），调用 `package-cli-windows.ps1`。上游 ci-rust / ci-e2e 等编译测试类 workflow 被仓库守卫 skip；ci-lint / ci-docs / ci-scripts 无守卫，push 命中路径仍会自动跑。
- `scripts/publish/cli/package-cli-windows.ps1`（大改）：模型随包分发（Whisper tiny + RT-DETR + TATR + PaddleOCR tiny）、MSVC CRT 部署、并行度与打包门禁；**fork feature 集的来源之一**。
- `scripts/publish/cli/offline-smoke.ps1`（新增）：离线 smoke 与空缓存缺模型诊断探测。
- `scripts/ci/lib/pe-imports.ps1`（新增）+ `scripts/ci/verify-windows-dll-closure.ps1`（改造）：PE 导入闭包校验。

## 验收与仓库约定（fork 独有）

- `fulltest.py` / `slowtest.py`（仓库根）：本仓库的验收标准，规则见 AGENTS.md。
- `AGENTS.md` 入库（上游 `.gitignore` 忽略它，fork 删掉了该条；见 AGENTS.md「仓库性质」）。
- `.gitignore` 追加 fork 本地产物忽略（打包输出、转换 scratch、`.tmp/` 等）。

## 构建配置刻意背离上游的点

- feature 集固定为 `formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,layout-detection,api`（`--no-default-features`；AGENTS.md 与打包脚本两边保持一致）。
- 根 `Cargo.toml`：`[profile.release]` `lto = false`、`codegen-units = 256`（上游 `lto = "fat"`，打包墙钟时间不可接受）；`[profile.dev]` `debug = 1`；workspace 新增 member `crates/xberg-windows-metafile`。
- `.cargo/config.toml`：`jobs = 28`（同步自 `alef.toml` 的 build_jobs）。

## 文档计数同步

引擎新增格式会连带改 README / `templates/` / `packages/*/README.md` / `plugin/` 中的计数与格式表（如 107→110 formats、141→146 extensions、WMV/ASF 转写行）。这些是引擎改动的伴生同步，不是独立功能；merge 冲突时按最新事实重算即可。
