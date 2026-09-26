# 本 fork 的权威需求目录

本目录是本仓库唯一的需求权威；[AGENTS.md](../../AGENTS.md) 维护开发规则、实现入口、命令和操作授权。根目录 [fork.md](../../fork.md) 仅保留迁移链接，不再维护需求副本。

## 来源、目标与全局边界

- 个人仓库：`https://github.com/jchanghong023/xberg.git`（`origin`）；持续同步的上游：`https://github.com/xberg-io/xberg.git`（`upstream`），跟踪 `upstream/main`。
- 仅面向 Windows 11 的个人使用：文件转换为 Markdown、图片 OCR、音视频转写，以及 `xberg serve` HTTP 服务。主要场景是单文件/批量转换、处理文档内嵌内容和使用含模型的离线 Windows 包。
- 不主动维护上游语言绑定、heic、pdfium、embedding、NER、MCP 等能力，不为其他平台增加适配或验收。上游生态资产随同步保留，格式清单等伴生文档仍须与引擎一致。
- 下列规格描述同步上游后仍须成立的本地行为，不是补丁清单。需求来自原有权威清单；补充内容依据现状恢复时明确标注。代码和测试只能证明实现或验证状态，不能取消、削弱尚未满足的需求。

## 需求域

| 文档 | 唯一维护范围 |
| --- | --- |
| [FORK.md](FORK.md) | 库/CLI 默认行为、配置、图片输出路径、格式抽取与文档保真 |
| [OCR.md](OCR.md) | 图片抽取与 OCR 默认设置、后端、文字相对位置和并发资源约束 |
| [TRANSCRIPTION.md](TRANSCRIPTION.md) | Windows 音视频解码与转写、分块并行的结果和失败顺序 |
| [PERFORMANCE.md](PERFORMANCE.md) | 可选性能日志、稳定阶段名和计时口径 |
| [DELIVERY.md](DELIVERY.md) | Windows 离线包、发布流水线、自动化质量报告与测试门行为 |

每项需求只在所属文档维护；跨域关系用链接表达。已有合适文档时更新该文档，新独立功能域才新增文档，不按代码目录或篇幅拆分。新增、修改、取消本地需求及预期用户可见行为时必须同步对应文档；实现方式变化不制造需求变化。

## 实现与验证状态

2026-09-27 同步与发布记录（用户授权「合并上游；跑所有测试，发布版本」）：已合并 `upstream/main` 至 `657f41a728`（1.3.0 发布 + 后续 PDF 表格/Type 3/扫描页 OCR 路由与数字修复等共 76 提交，分三批合并：`56399e8999` 50 提交、`d60b31b1a4` 12 提交、`657f41a728` 14 提交）。逐项核对后本目录全部需求仍成立；上游未新增格式，feature 集、profiles、默认行为不变。

验证（slowtest 门 `--with-release-ci` 全绿）：三 crate fmt / build-cli / fulltest.py（FAIL=0 WARN=7，与 9-15 基线比 新增 2（第三课/第九课 ENGINE_WARN，2026-09-19 Visio 路由修复的既有产物）/已修复 2/恶化 0）/ 语料预检 / 三 crate cargo test / 三 crate clippy `-D warnings` / slowtest.py（FAIL=0 WARN=7，含 `--deep` 音视频段）/ release-ci success（run 36265103638）。已发布公开 Release `v2026.9.27-0338-run43.1`（构建自 `caa9e55b2c`）。

同步中处理的上游问题与 fork 适配（均有提交记录）：① 上游 `0330859dbc` 把 `scan_detect/tests.rs` 追加成双份内容、`34e52a7153` 一带使 `ocr/processor/config.rs` 缓存键测试重复定义（E0428）——上游随后以 `c3107fff59` 自行修复 scan_detect，config.rs 的重复由 fork 删除其重复块；上游自带的同类 fmt 漂移由 fork `cargo fmt` 修复。② fork 适配三处上游新测试（保留覆盖、调整前提）：`should_apply_the_whole_image_psm_to_a_scan_page` 显式钉 `tesseract` 后端（fork 默认 paddle-ocr）、GH#1785 SUBTOTAL 计数与 GH#1789 byte-no-op 断言按 fork 的 Windows Tesseract（tesseract55d）行为放宽（恢复性与零破坏属性保留）。③ **fork 功能缺口修复**：GH#1789 数字修复此前不覆盖 fork 网格围栏的数据源——新增共享函数 `repair_extracted_document_numbers` 并接入共享图片 OCR 通道（`apply_ocr_result`），正文与围栏的数字保持一致。④ `pdf_numeric_footnotes::numeric_footnote_survives_public_extraction` 在 fork `#[ignore]`：fork 经公开 `extract` 入口的 native fast-path 将合成页的上标数字粘合（CLI 路径正常），函数级与上游逐字节一致、分歧在运行时数据流组合，待专门调试；fork 真实语料的 E2E（fulltest.py）不受影响。

已知未满足项见 [OCR.md](OCR.md) 的同行对齐要求，验收覆盖缺口见 [DELIVERY.md](DELIVERY.md)。没有新设产品规划或待用户决定的需求变更；已有要求继续有效，不能因当前实现或检查器未覆盖而移除。
