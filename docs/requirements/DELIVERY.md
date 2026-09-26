# Windows 交付与自动化验收差异需求

适用范围、上游和状态见 [README.md](README.md)。本文件维护交付与报告的可观察要求；命令、授权和维护规则见 [AGENTS.md](../../AGENTS.md)。

## 标准能力与离线包

- 开发 CLI 和出厂包采用同一标准 feature 集：`formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`，关闭 default features；HTTP 服务 `xberg serve` 必须存在。
- 不含 heic、pdfium、candle、MCP、embedding、NER 或 `layout-detection`，不附带 pdfium.dll。lib 测试可保留上游 layout 专属能力以维持测试覆盖，这不是向出厂包恢复 layout 能力。
- Windows zip 内含可运行的 xberg.exe、所需 ONNX Runtime/MSVC CRT DLL、Whisper tiny 和 PaddleOCR pp-ocrv6 tiny 模型；Paddle 模型含 det、rec、dict、textline-cls。模型须校验 sha256/大小，并以包内 `models/` 的 HF 缓存布局离线可用。
- 不分发 RT-DETR/TATR，也不把未使用的 SLANeXT、SLANet_plus、table_classifier、pp_doclayout_v3 纳入必需模型。OCR 位置保真以 [OCR.md](OCR.md) 为准。
- 打包必须验证干净 PATH 下的 `--version`、DLL 导入闭包以及带 OCR 配置的离线转换。正向 smoke 要求转换成功、出现 `PaddleOCR engine initialized successfully` 且无离线缺模型诊断；有必需模型时必须追加空缓存负向探测，并出现 HF 离线缺模型诊断。负向要求是诊断，不保证进程必须非零退出。
- 验收：离线包不依赖用户机器偶然已有的 DLL/模型缓存，抽取/OCR/Whisper 可用；远程与本地使用同一打包脚本。外部 ffmpeg 回退前提见 [TRANSCRIPTION.md](TRANSCRIPTION.md)。

## 发布与测试门结果

- fork 自有 Windows 发布 workflow 仅手动触发，调用统一打包入口并创建带时间戳 tag 的公开 Release。是否发布由用户根据报告决定，具体操作授权见 AGENTS.md。
- fastcheck 是 60 秒墙钟硬超时的快速反馈，覆盖脚本语法、判定器自测、关键 PS1 解析和三个 crate 的格式检查；超时必须终止进程树并判失败，不能伪报成功，也不代表完整验证。
- fulltest 门覆盖当前 Windows 平台的本地编译、转换 E2E、语料存在性、Rust 测试与 clippy，各阶段独立报告，任一 FAIL 即门失败。其转换阶段沿用默认 `fulltest.py --keep-going`，音视频覆盖限制必须如实说明。
- slowtest 门叠加完整 fulltest 门、打包版验收和经授权的远程发布阶段；本地失败时后续打包/发布记 `SKIPPED_PRIOR_FAIL`，不在已知失败状态发布。未授权远程阶段记 `SKIPPED_NOT_AUTHORIZED`，只能称本地部分通过；非适用平台记 `SKIPPED_NOT_APPLICABLE`。workflow 不回调 testgate，防止远程递归。
- `slowtest.py` 必须使用打包版 CLI 跑 `--keep-going --deep`，保存 Markdown/JSON 报告副本；解压目录成功时清理，失败或显式保留时留供检查。完整打包验收不得因加速而漏掉音视频。

## 转换质量报告与金标准

- `fulltest.py` 是转换验收标准：真实 `xberg.exe extract` → Markdown/图片落盘 → 自动检查与逐文件金标准；`slowtest.py` 用打包版执行同一判定。普通运行不回退打包版，缺 transcription 能力时预检给出重编诊断。
- FAIL/WARN 都是待修项。主队列所有适用文件及 `_adversarial` 全部 PASS 才是该覆盖范围的全绿；旧完整主队列记录为 11 个文件，默认跳过音视频时不能宣称完整 11 文件验收。转换改进按红项减少、无新增问题码、无同码次数恶化评估，不能靠人工逐文档回归兜底。
- 五层检查继续存在：进程/结构（退出码、空结果、乱码与标记、围栏、表格、图片合法性）；源文对齐（去页眉 bigram、数字/标识、正文体量、分页）；交叉核对（源页/媒体、引擎 counts、落盘、引用、转写量）；深检（Markdown 噪声、内嵌对象与子文档、PPTX 标题/备注、xlsx 图形、PDF 书签/表格、OCR、金标准）；对抗失败路径。
- 仓外 `_expectations.json` 的 required/forbidden/order/min/max、中文 OCR、嵌套列表、目录标题召回等逐文件要求必须真正生效。未知键、控制字符、编译失败或匹配空串的 pattern、过短 token 在加载时告警并进入报告。OCR 运行配置默认空即不覆盖 CLI；不得给未编译 layout 的出厂 CLI 注入 `layout_config`。
- 基线比较同时呈现新增、已修复、恶化：前两者比较问题码集合，恶化比较同码次数。未加载金标准时，两侧均排除依赖金标准的码，避免假修复；金标准哈希变化须明确提示阈值类比较仅供参考。
- 报告/基线需能核对金标准 sha256、代码 commit、阈值及 argv。存在 FAIL、提前终止或未加载金标准时拒绝保存基线，只有显式 force 才能绕过护栏；如何授权重设及披露依据见 AGENTS.md。
- 报告记录逐文件转换 `elapsed_s`、检查 `check_time_s`、源文基准 `source_time_s`；Markdown 呈现转换/检查/对抗/框架开销与最慢 Top10，JSON 提供 `timing` 块。
- 验收：成功、失败、提前结束、缺金标准、金标准变化和同码次数增加等情况有自动化覆盖，报告不能把少测、失败或阈值变化当成转换器修复。

## 失败路径

损坏、截断、空文件必须优雅处理。非零退出并有 stderr 诊断为优雅失败 PASS；干净的成功转换也可通过。panic/backtrace 为 `ADV_PANIC`，非零退出且无诊断为 `ADV_SILENT_FAIL`，成功但垃圾输出为 `ADV_GARBAGE_OK`，成功但空/过短为 `ADV_EMPTY_OK`（WARN）。这些优雅失败不要求金标准条目，不能以“没有崩溃”代替全部判定。

## 已知覆盖缺口与状态边界

- 本 fork 未自动运行 Rust 编译/测试 CI；push 可触发的 lint/docs/scripts 检查不能替代本地门。上游语言绑定 `e2e/`、`fixtures/` 不作为本 fork 的验收依据。
- `fulltest.py`/`slowtest.py` 当前验证转换 CLI，不能据此宣称真实 `xberg serve` HTTP 链路、性能日志或所有回退路径已完成 E2E；这些能力修改后仍需对应自动化验证。
- 既有检查盲区继续记录：T1 两次不同乱码 OCR 导致重复内容检测漏报（只有围栏与正文/其他围栏高度相似才报）；T2 图片被浮层遮挡或裁剪；T3 中文词内插空；T4 图表题注顺序颠倒；T5 内嵌文本归属错位而堆到文末；T6 行内 `| --- |` 表格压为一行。修相关模块时评估补充自动检查，不据此取消内容保真要求，也不把人工回归变为常规质量保证。
- 当前 OCR 同行对齐的已知缺口见 [OCR.md](OCR.md)。完整需求达标仍需对应 UT/E2E 证据；程序退出成功、门通过及转换质量全绿分别报告，不混为一谈。
