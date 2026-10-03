# 本 fork 的权威需求目录

本目录是本仓库唯一的需求权威；[AGENTS.md](../../AGENTS.md) 维护开发规则、实现入口、命令和操作授权。根目录 [fork.md](../../fork.md) 仅保留迁移链接，不再维护需求副本。

## 来源、目标与全局边界

- 个人仓库：`https://github.com/jchanghong023/xberg.git`（`origin`）；持续同步的上游：`https://github.com/xberg-io/xberg.git`（`upstream`），跟踪 `upstream/main`。
- 仅面向 Windows 11 的个人使用：文件转换为 Markdown、图片 OCR、音视频转写，以及 `xberg serve` HTTP 服务。主要场景是单文件/批量转换、处理文档内嵌内容、截图 OCR 和使用含模型的离线 Windows 包。文档转换与截图推理使用不同模型和独立配置，但只能启动一个 Xberg 进程共同承接，模型须在该进程内常驻复用；并发与生命周期合同统一见 [WORKER.md](WORKER.md)。
- 不主动维护上游语言绑定、heic、pdfium、embedding、NER、MCP 等能力，不为其他平台增加适配或验收。上游生态资产随同步保留，格式清单等伴生文档仍须与引擎一致。
- 下列规格描述同步上游后仍须成立的本地行为，不是补丁清单。需求来自原有权威清单；补充内容依据现状恢复时明确标注。代码和测试只能证明实现或验证状态，不能取消、削弱尚未满足的需求。

## 需求域

| 文档 | 唯一维护范围 |
| --- | --- |
| [FORK.md](FORK.md) | 库/CLI 默认行为、配置、图片输出路径、格式抽取与文档保真 |
| [OCR.md](OCR.md) | 图片抽取与 OCR 默认设置、后端、文字相对位置和并发资源约束 |
| [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md) | 截图 OCR 第二通道：TextSnap 固定字节模型集、确定性瓦片/合并/布局行为、本地调用接口与打包 |
| [TRANSCRIPTION.md](TRANSCRIPTION.md) | Windows 音视频转写（FFmpeg → Silero VAD → SenseVoice INT8 唯一链路）、输出结构与失败语义 |
| [PERFORMANCE.md](PERFORMANCE.md) | 可选性能日志、稳定阶段名和计时口径 |
| [DELIVERY.md](DELIVERY.md) | Windows 离线包、发布流水线、自动化质量报告与测试门行为 |
| [WORKER.md](WORKER.md) | 本地常驻 stdio `worker` 接口：同进程文档转换与截图推理并发、协议、模型复用及退出语义 |

每项需求只在所属文档维护；跨域关系用链接表达。已有合适文档时更新该文档，新独立功能域才新增文档，不按代码目录或篇幅拆分。新增、修改、取消本地需求及预期用户可见行为时必须同步对应文档；实现方式变化不制造需求变化。

## 实现与验证状态的维护

实现状态、历史验证证据和未满足项分别维护在所属需求文档，索引不复制各域规格或开发日志：转换行为见 [FORK.md](FORK.md)，文档 OCR 见 [OCR.md](OCR.md)，截图 OCR 见 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md)，转写见 [TRANSCRIPTION.md](TRANSCRIPTION.md)，worker 见 [WORKER.md](WORKER.md)，测试门与发布记录见 [DELIVERY.md](DELIVERY.md)。截图通道原有条目级追认状态继续保留，本次整理不代替用户裁决；2026-09-29 用户新明确的同进程常驻与双场景并发要求已直接更新 [WORKER.md](WORKER.md)，取代旧的全请求串行约束。

2026-10-03 文档核对基于本地 HEAD `a4ce6ab9db`，与本地 `upstream/main` 的共同基线为 `df10b52e315758a467b20a1b36e2d1373925dce7`（自上次核对 `d28cbeb6ff` 起经历两次 upstream merge、一轮 fork 行为恢复与一轮 fork review 修复）。这是本地 Git 证据，未查询远程最新状态，也不代表本轮逐项功能验收。历史通过记录只适用于记录中的提交、模型和覆盖范围，不能外推到后续合并后的工作树。
