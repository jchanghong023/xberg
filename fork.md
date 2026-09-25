# fork.md — 本 fork 相对上游的定制清单

上游 = `https://github.com/xberg-io/xberg.git`（remote 名 `upstream`）。本文件是 **merge 上游时解决冲突的依据**、fork 改动的唯一索引，也是本仓库的需求权威（`AGENTS.md` 只写开发规则）：改动下列任何能力，必须同步更新本文件。保持精简，细节以代码与提交历史为准（`git log upstream/main..main`）。

**状态与核对约定**：本清单描述「同步上游后仍需在本 fork 成立的行为」，不是 Git 差异摘要或开发日志；对照基线 = `git merge-base main upstream/main`。下列条目由现行实现与本地提交恢复而来，验收 = `fulltest.py` 对应检查不回归 + 各条给出的可观察行为。同步上游后必须逐项复核（上游可能已独立实现或改变同一行为），不能只看有没有 Git 冲突。最近一次复核：2026-09-26 对 upstream/main（1.2.9 后 20 提交至 e2cf0865d5：native-pdf Type 3 字形矩阵/固定字距词距（GH#1770）/xref 重建后对象流恢复（GH#1774）/数字上标标记（#1773）/悬挂页面属性继承（#1777）、OCR 结果缓存键折入 resolved tessdata 目录（#1787，结果 schema v11）、抽取器注册与图片分类、rtf/mdx/ppt/json 渲染复杂度重构）逐项核对——冲突 2 文件：`extractors/mod.rs` 采纳上游 register_baseline/office/container_format 三函数注册重构，fork 的 VisioExtractor 挪进 `register_office_extractors`、测试计数断言补 `visio-extractor`；`ocr/processor/config.rs` 双方测试并存，fork 的 security_limits 缓存键断言迁移到上游 #1787 双参数 `hash_config(config, resolved_tessdata)` 签名。清单全部条目仍成立：默认输出/图片抽取/OCR 后端与 `ocr_embedded_images` 语义、`effective_pipeline()` 默认 `None`、16 MiB 栈、perf span 契约、profiles（110 formats 由 `FORMATS.len()` 派生，本次合并未新增格式）；合并对 docx/pptx 内嵌图路径只是 `image_kind::classify` 机械签名改动。验证：`cargo check` + fastcheck PASS；fulltest.py FAIL=0 WARN=7、对抗全 PASS；slowtest.py 通过（FAIL=0 WARN=7，含 `--deep` 音视频段，343s）；对 9-15 基线 新增 2（第三课/第九课的 ENGINE_WARN，系 2026-09-19 Visio 内容路由修复让内嵌 VSD 如实报错的既有产物，非本次合并引入）/已修复 2（tessent `CAPTION_IN_TABLE`、测试识别.png `GOLDEN_METRIC`）/恶化 0。

## 定位

仅 Windows 11 的个人 fork：只做「文件 → Markdown」+ OCR + 音视频转写 + HTTP 服务（`xberg serve`）。**不使用版面/表格模型**（不编 `layout-detection`，RT-DETR/TATR 不编入也不随包；图片输入只走 OCR，见「图片 OCR 渲染」）。不维护上游的语言绑定、heic、pdfium、embedding、NER、MCP 等能力（feature 集见 AGENTS.md）。

## 用户可见默认行为（与上游不同）

- **默认输出格式**：库默认 `OutputFormat::Markdown`（上游 `Plain`）；CLI 反过来把自己的基准钉成 `Plain`（`xberg extract` 不带参数仍打印纯文本）。配置文件与 `--config-json` 只覆盖写了的键，未写的键保留基准值、不重置成库默认（`ExtractionConfig::from_file_over` / `discover_over`）。验收：库 API 默认渲染 Markdown；CLI 默认输出纯文本；配置文件省略 `output_format` 时 CLI 仍输出纯文本。
- **默认抽取图片**：`PdfConfig::extract_images` 默认 `true`；缺省 `images` 段视为抽取图片（`needs_image_data()` / `needs_image_processing()` 默认真）；`ImageExtractionConfig::append_ocr_text` 默认 `true`（`ocr_text_only = true` 仍可只留文本）。验收：默认配置下 PDF / Office 图片会被抽取、落盘并被 OCR；显式 `extract_images = false` 能关掉。
- **默认 OCR 后端 = PaddleOCR pp-ocrv6 tiny**（编入 `paddle-ocr` 时为默认，否则 tesseract）；**不合成古典回退 pipeline**——上游「默认后端时自动拼 `[tesseract@100, paddleocr@50]`」的逻辑已移除（弱引擎的误读会在默认引擎正确地"没找到"的位置胜出；需要混合时由调用方显式配置 `pipeline`）；Tesseract 语言代码统一解析成 pack 名（`zh` → `chi_sim`）后再交给各消费者。1.2.9 起上游给内嵌图 OCR 加了显式开关 `ocr_embedded_images`，fork 语义下未显式设置时视为开（无 `ocr` 块也 OCR 内嵌图片，回退默认后端），显式 `false` 可关。验收：默认后端为 `paddle-ocr`；未显式配置 `pipeline` 时 `effective_pipeline()` 返回 `None`。

## CLI 行为

- **`--output-dir` 语义**：显式给出目录时，输出里的图片引用带上该目录（空格、括号等按 CommonMark 百分号编码；围栏内文本不改写）；批处理把每份结果的图片写进 `<dir>/doc_<N>/`，否则多文档的 `image_N.ext` 会互相覆盖、引用全指向最后一份。验收：批处理 ≥2 个文档时各文档图片不互相覆盖且引用可解析。
- **主线程栈**：CLI 主体跑在 16 MiB 栈的 worker 线程（`xberg-main`）上——Windows 主线程栈容不下 crate 的递归解析器，否则 Windows 与 Unix 行为不一致。

## 引擎定制（crates/，冲突时保 fork 行为）

- **OCR 后端与调度**：`paddle_ocr/`、`core/config/ocr.rs`——承载上节的默认后端、语言解析与线程预算规则。
- **图片 OCR 渲染（版面信息的唯一来源）**：每张图片的 marker 是独立段落，其后紧跟装 OCR 文本的 ` ```text ` 围栏块——网格布局优先（`rendering/ocr_layout.rs`，fork 新增：按每个 OCR 行的 bbox 还原到等宽网格的行/列，CJK 记 2 列，行高/半行高换算行距与列距），放不下整行或行数超上限时**整体回退平文本而不是静默丢行**；`ocr_text_only` 开启时两部分都保留。**需求（2026-09-21）**：不用 layout/table 模型，图片的版面相对位置就靠这条网格承载，所以「尽量保留文字相对位置」= 网格优先且不得截断丢行。**独立图片（`image/*` 直接输入）与文档内嵌图片一视同仁**：独立图片同样只走 OCR，同样必须产出网格围栏（2026-09-26 实测：独立图片已产出 ```text 网格围栏、单元格列偏移在位，`min_fenced_cjk` 金标准通过——旧"回退平文本"缺陷已不存在；残留局限：`测试识别.png` 各单元格 OCR 块 y 带不齐，同行单元格落在相邻行而非合并为一行，列位置仍正确）。验收：marker 段落与其 OCR 围栏块相邻且位置不变，围栏内既有文本不被改写，OCR 文本不因段落去重而丢失，网格拒绝时平文本完整；**独立图片输入（如 `测试识别.png`）的围栏块必须含列对齐行（同一视觉行上的多个文本块落在同一输出行）**。
- **EMF/WMF 图元文件 → PNG**：新 crate `crates/xberg-windows-metafile`（纯 GDI 栅格化，fork 新增），Office 内嵌图元文件因此可落盘、可 OCR（`docker/`、`.dockerignore` 需同步该 member）。验收：Office 文档里的 EMF/WMF 以 PNG 落盘并在 Markdown 中被引用。
- **Visio 抽取**：`.vsd`/`.vsdx`/`.vsdm` 形状文本（`extraction/visio.rs`、`extractors/visio.rs`，fork 新增；MIME 表同步登记）。验收：三类扩展名都能抽到形状文本并走到 Visio 抽取器。
- **xlsx 内嵌图片**：提取并 OCR（`extraction/excel/images.rs`，fork 新增）。验收：表格内嵌图片落盘并被 OCR。
- **OOXML/OLE 内嵌对象**：内嵌 Word OLE 文本合并进宿主文档、PPTX OLE/PresentationML fallback 图片抽取（`extraction/ooxml_embedded.rs`）。验收：内嵌 OLE 文本出现在宿主文档对应位置，fallback 图片被抽出。
- **PPTX 质量**：标题/演讲者备注读取与归属守卫（`extractors/pptx.rs`、`extraction/pptx/`）。验收：标题与备注归属到正确幻灯片，不串页、不重复。
- **PDF 质量大改**：表格重建（`pdf/table_reconstruct.rs`）、原生文本抽取（`pdf/native/text.rs`）、结构分类/段落/页眉页脚剔除（`pdf/structure/`）。验收：有原生文本的页面不做破坏性整页 OCR 回退（原生文本保留），表格按重建结果输出、页眉页脚被剔除；`content_filter.include_headers/include_footers` 在扁平文本与结构化两条路径上一致生效（跨页页眉/页脚的 streak 剔除同样受其约束）。
- **Windows Media 转写**：Media Foundation 读 ASF/WMV 音轨，解码失败回退外部 ffmpeg（`transcription/container.rs`、`transcription/wmf.rs`，fork 新增）。验收：`.wmv`/`.asf` 能转写出音轨文本；MF 读不了的文件回退 ffmpeg 后仍能出文本。
- **Whisper 分块并行推理（2026-09-22，fork 性能改动）**：`WhisperEngine::transcribe_segments` 把 30s 分块交给最多 8 个 worker（`available_parallelism` 与块数取小）并发推理、按块序号还原顺序——输出与顺序版逐条一致，失败返回最低失败块序号的错误（与顺序版语义一致）。实测 17 分钟 mp4 转换 96.7s→29.9s（−69%），整轮 fulltest 转换合计 −32.9%。验收：fulltest `--deep` 下 mp4/wmv 的 PASS 判定与转写量交叉核对不回归。
- **Markdown 渲染**：图片 alt 路径清理、图片 marker 独立段落与 OCR 文本围栏等输出修复（`rendering/markdown.rs`、`rendering/comrak_bridge.rs`、`extraction/markdown_utils.rs`）。验收：输出不残留本地路径垃圾，改动围栏内文本的改写不发生。
- **标题属性内联**：XML/OPML 抽取器把元素的 `id`/`type`/`_note` 等属性挂在元素上而非文本里，plain 与 Markdown 两个渲染器共用 `rendering::inline_attributes_suffix` 把属性以 ` (k: v, …)` 内联进标题（过滤 `xmlns*`、空值与 `xberg:` 内部标记），两渲染器不再漂移（`# Item (_note: …)`）。验收：`issue_131_opml_note_attribute` 与 `xml_embedding_quality` 的默认配置断言成立。
- **性能打点体系**：`perf-tracing` feature（fork 新增，上游无）：`xberg` 纯 marker + `xberg-cli` 侧 `tracing-appender` 独立日志（`crates/xberg-cli/src/perf.rs`）。启用时给单/批抽取整体、引擎、文件/字节抽取、格式分发、pipeline、图片 OCR、Whisper 转写与输出渲染、PaddleOCR 初始化/推理加 `target="perf"` span，按天滚动落 `logs/perf.log.<日期>`（`XBERG_PERF_LOG_DIR` 可覆盖，`**/logs/` 已 gitignore）；耗时 = `FmtSpan::CLOSE` 的 `time.busy`/`time.idle`。默认构建零影响（出厂集不含该 feature），不随包分发。注意 otel 在本 fork 经 `core-cli → cli → services` 恒开，perf span 不得用 `not(feature = "otel")` 守卫（会全部静默失效）；`format_extract` 与 otel stage span 的调用点三分支互斥且 perf 优先。验收：默认构建依赖图与告警不变；性能构建跑 `xberg extract` 在日志目录产出含各阶段耗时记录的性能日志，stderr 业务日志不变。

## 性能 / 资源策略

- **PaddleOCR 并发**：把线程预算拆给多个并发引擎实例（每模型键有槽位上限，默认 8），而不是给单个会话堆 intra-op 线程——实测（32 核）把单会话 intra-op 预算 8→32，八文档 OCR 批量只快约 12%（会话带互斥锁，同一时刻只能跑一张图），引擎槽位才转成图级吞吐。每实例复制模型权重与 ORT arena，故槽位数有上限。
- 线程预算门控 `active_thread_budget()` 同时覆盖 `paddle_ocr`（不只上游的 `sceptre_ocr`），使 `ConcurrencyConfig::max_threads` 对模型会话生效。

## 打包 / CI（fork 独有或大改）

- `.github/workflows/build-windows-cli.yml`（新增）：fork 自有的打包流水线（仅手动 `workflow_dispatch` 触发），调用 `package-cli-windows.ps1`，含带时间戳的 Windows release 发布。上游 ci-rust / ci-e2e 等编译测试类 workflow 被仓库守卫 skip；ci-lint / ci-docs / ci-scripts 无守卫，push 命中路径仍会自动跑。
- `scripts/publish/cli/package-cli-windows.ps1`（大改）：模型随包分发（Whisper tiny + PaddleOCR pp-ocrv6 tiny；2026-09-21 起不再含 RT-DETR/TATR）、MSVC CRT 部署、并行度与打包门禁；**fork feature 集的来源之一**。
- `scripts/publish/cli/offline-smoke.ps1`（新增）：离线 smoke 与空缓存缺模型诊断探测；正向断言 2026-09-21 起为 `PaddleOCR engine initialized successfully`（原 `Layout detection completed`，随 layout 移除而换）。
- `scripts/ci/lib/pe-imports.ps1`（新增）+ `scripts/ci/verify-windows-dll-closure.ps1`（改造）：PE 导入闭包校验。
- `.ai-rulez/` 与 `plugin/` 的规则 bundle：格式计数等随引擎改动伴生同步，其中 release profile 的描述按 fork 的 LTO 设置改写（bundle 由固定版本 ai-rulez 生成，改规则后须用同一版本重新生成，见 AGENTS.md）。

## 验收与仓库约定（fork 独有）

- `fulltest.py` / `slowtest.py`（仓库根）：本仓库的验收标准，规则见 AGENTS.md。
- `testgate.py`（仓库根）：三级测试门入口 fastcheck / fulltest / slowtest——fastcheck（≤60 秒，含 fmt/PS1/判定器自测子集）可由 agent 自主跑，fulltest / slowtest 门须用户逐次授权；阶段构成与远程发布流水线的授权边界见 AGENTS.md「三级测试门」。
- `AGENTS.md` 入库（上游 `.gitignore` 忽略它，fork 删掉了该条；见 AGENTS.md「仓库性质」）。
- `.gitignore` 追加 fork 本地产物忽略（打包输出、转换 scratch、`.tmp/`、`.zcode/` 等）。
- 上游测试：fork 行为改变了上游断言时改断言而不是删测试（如 CLI 批处理错误路径在 Windows 的 `{:?}` 转义渲染、格式/扩展名/MIME 计数常量）；Rust 测试不在本 fork 的 CI 中运行（见 AGENTS.md「测试要求」）。

## 构建配置刻意背离上游的点

- **出厂 feature 集**（开发编译 / 打包 / 三级门）固定为 `formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`（`--no-default-features`；AGENTS.md 与打包脚本两边保持一致）。2026-09-21 需求变更移除 `layout-detection`（同时删掉打包脚本的 RT-DETR/TATR 模型条目与 layout smoke 断言）。
- **lib 测试集例外**：`cargo test -p xberg` 用的 `FEATURES_LIB` 仍带 `layout-detection`——上游有一批 layout 专属测试（`reading_order.rs`、`layout_for_markdown.rs`、`pdf_two_column_reading_order.rs` …）以及 `pdf_options.reading_order` 的校验都要求该 feature，按「不改上游断言」的约定，出厂集与测试集分离；这也意味着 `pdf_options.reading_order` 在出厂二进制里是配置错误（`core/config/pdf.rs` 的校验）。
- 根 `Cargo.toml`：`[profile.release]` `lto = false`、`codegen-units = 256`（上游 `lto = "fat"`，打包墙钟时间不可接受）；`[profile.dev] debug = 0` 与 `[profile.test] debug = 0`（2026-09-20：`debug = 1` 时一次冷 `cargo test` 写 88 GB PDB，见 AGENTS.md「Rust 构建优化规则」）；workspace 新增 member `crates/xberg-windows-metafile`。
- `.cargo/config.toml`：`jobs = 28`（同步自 `alef.toml` 的 build_jobs）。

## 文档计数同步

引擎新增格式会连带改 README / `templates/` / `packages/*/README.md` / `plugin/` 中的计数与格式表（如 107→110 formats、141→146 extensions、WMV/ASF 转写行）。这些是引擎改动的伴生同步，不是独立功能；merge 冲突时按最新事实重算即可。
