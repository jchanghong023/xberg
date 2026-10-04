# 同进程常驻模型与双场景 worker 接口

## 需求背景

Xberg 通过一个本地 `worker` 进程同时提供文档转换与截图 OCR，已加载的模型在该进程内常驻并复用，避免逐请求启动进程或重复加载模型。2026-09-29 用户明确要求：文档转换进行中，同一个进程仍须能够执行截图模型推理，不能等文档转换结束，也不能靠另起一个 Xberg 进程满足。两种场景只能启动并共用一个 Xberg 进程，禁止按场景、模型或批次另开 Xberg 实例或进程池；不能以多个实例各自常驻来满足本要求。该决定取代旧的全请求串行和仅限单批次生命周期约束。

本接口通过本机 stdio 通信，不新增 HTTP 监听要求。调用方可以保持同一进程跨多个文档批次与截图请求复用；何时结束进程由调用方决定，不能因一批文档结束或切换场景自动卸载另一场景的模型。

## 接口协议

### 2026-09-29 共享 worker 扩展（实现与发布验收中）

共享 worker 的新增协议与职责边界：

- `extract.mode` 支持 `normal` / `fast`，缺省为引擎自动。fast 仅在该请求配置副本上关闭 Layout、图片 OCR（包括扫描页 OCR），保留原生文本、表格与图片提取；不会修改截图配置，也不会卸载模型。**2026-10-04 变更：页数分流决策权从 JchTools 移入引擎**——缺省 mode 时引擎自行轻量探测页数，超过 `auto_fast_pages`（默认 500）自动走 fast 并在 `processing_warnings` 记录（`auto_mode` 来源，含页数与阈值）；显式 `mode:"normal"` 表示该请求不降级（等价禁用自动分流），显式 `fast` 行为不变。阈值语义与探测口径见 [FORK.md](FORK.md)「大文档自动降级」。
- 工作请求可带正整数 `timeout_ms`，从接收入队起计时；未指定时使用启动配置的 `extraction_timeout_secs`。`cancel` 请求带 `target_id`，仅取消对应在途请求；响应中的 `accepted` 表示收到取消请求，不等于已停止。被取消/超时任务实际返回后才产生唯一终态，`error_kind` 为 `cancelled` / `timeout`，不再发送成功结果。
- 取消是协作式：排队任务不进入处理器；运行任务在解析、解码或推理检查点停止，当前不可中断原生调用需先返回，不能承诺严格毫秒级终止。若原生库永久挂起，无法在安全保留同一进程的同时强制终止该线程；该边界必须在发布报告披露。
- `formats` 返回当前二进制注册的格式；`capabilities` 返回协议版本、命令、模式与取消语义；`model_state` 返回模型状态。查询走已有连接，不启动其他 Xberg 进程。
- 在途 ID 不得重复；取消失败、队列满、单任务失败不破坏其他请求。文档与媒体共用文档通道，截图使用独立推理通道。
- SQLite、保存目录、重启恢复、界面配置、应用级确保仅启动一个 Xberg 均属于 JchTools；Xberg 不实现这些调用方职责。
- 交付必须区分源码实现、类型检查、真实模型 E2E 与发布包验证。新增接口须覆盖取消运行/排队任务、超时后复用、模式隔离、进程内查询及 ID 关联；未运行发布包验收不得声称实际发布版本已满足。

- 启动：`xberg worker --config-json <固定配置>`（同时支持 `--config` / `--config-json-base64` / `--no-config-discovery`，语义与 `extract` 相同）。**2026-10-04 起零配置启动为一等公民**：`--config-json` 可整体省略，全部行为用引擎内置默认（含图片 PNG 重编码、OCR 默认后端、大文档自动降级），模型与 onnxruntime DLL 按 exe 相对定位解析（见 [FORK.md](FORK.md)「零配置模型定位」），调用方无需设置任何环境变量；显式配置仅用于覆盖默认。启动配置同时包含文档转换配置和独立的 `snapshot_ocr` 截图配置，两者分别固定；请求仅通过 mode 选择文档处理方式，不接受任意配置覆盖。两场景的模型集、模型路径、推理参数与会话分别生效；截图配置不覆盖文档 OCR 配置，文档设置也不覆盖截图配置，不为共用进程而强制两套模型或参数相同。
- `--config-json` 还接受两个 worker 专属顶层键（在合并前被剥离，`ExtractionConfig` 的 `deny_unknown_fields` 不会看到；其余未知顶层字段仍按原样拒绝，保留拼写错误防护）：
  - `owner_token`（字符串，P2）：调用方属主标识，原样回报在 `capabilities.owner`，用于认领/区分同一台机器上的多个 xberg 进程；缺省时该键不出现；
  - `idle_timeout_ms`（正整数毫秒，P4）：无在途请求且无任何请求流量持续该时长后引擎自行退出，退出码 87；缺省不启用。
- 请求（stdin，逐行一个 JSON）：

  ```json
  {"id":1,"command":"extract","path":"C:\\docs\\a.pdf","mode":"normal"}
  ```

  - `id`：任意 JSON 值，响应原样回显，用于关联；调用方须为同时在途的请求使用可区分的 id。
  - `command`：支持 `extract`、`ocr_snapshot`、`snapshot_state`、`transcribe`、`cancel`、`formats`、`capabilities`、`model_state`、`keepalive`、`shutdown`、`version`。
  - `path`：本地文件路径（`extract` / `transcribe` 使用）。
  - `image_base64`：图像字节（PNG 或等价无损编码；`ocr_snapshot` 使用，字节只驻内存）。
  - `mode`：可省略；`normal` 按启动配置处理，`extract` 还支持 `fast`；其他命令不接受 fast。
  - `grace_ms`（`shutdown` 专用，正整数毫秒，可省略）：停机宽限上限；缺省用引擎内置默认（4 秒）。
  - 未知字段忽略；空白行跳过。
  - 每个通道的待处理队列有界；队列满时以对应 `id` 返回明确失败并提示稍后重试，不阻塞另一通道或状态查询，不静默丢请求。

- 响应（stdout，每个请求恰好一行完整 JSON，写出即 flush；并发完成时各行不得交错损坏，以 `id` 关联请求，允许按完成顺序返回）：
  - 成功或部分提取（`extract`）：`{"id":..,"ok":true,"document":{...},"warnings":[...]}`。`document` 与 `xberg extract --format json` 的 `result` 字段同构（`ExtractedDocument` 原样序列化，图片字节内联）；`warnings` 是 `document.processing_warnings` 的副本。部分提取（个别阶段失败但产出了文档）属于 `ok:true` + 非空 `warnings`。
  - 截图识别（`ocr_snapshot`）成功：`{"id":..,"ok":true,"text":"<布局文本>","records":N,"elapsed_ms":M}`（`error_kind` 键仅在有类别情形时出现，普通成功省略该键）；无文字图片 `ok:true`、`text:""`、`error_kind:"no_text"`。
  - 转写（`transcribe`）成功：`{"id":..,"ok":true,"markdown":"<SV-06 全文>","segments":[{"start_ms":..,"end_ms":..,"text":..}],"duration_ms":..,"has_audio":true}`；`markdown` 标题为输入路径的完整文件名（含扩展名）。
  - 状态查询（`snapshot_state`）：`{"id":..,"ok":true,"state":"uninitialized|loading|ready|error","error":null}`。
  - 保活探测（`keepalive`，P6）：`{"id":..,"ok":true,"models":{"snapshot":"..","document":"..","transcription":bool}}`。语义边界：**只**证明事件循环存活并应答，不触发模型加载、不查询重量级缓存、不重置任何请求级计时器；`idle_timeout_ms` 启用时任何请求（含 keepalive）都会刷新空闲计时。调用方仅应断言 `ok:true`。
  - 优雅停机（`shutdown`，P3）：先回 `{"id":..,"ok":true,"accepted":true}`（写出即 flush），随后停止接受新请求（宽限窗口内到达的请求按 `id` 回 `ok:false` "worker is shutting down"；空闲态则直接退出，不再应答），在途请求最多等到 `grace_ms`（缺省 4 秒）后取消并退出，退出码 0。引擎不得以「还有在途任务」为由拒绝退出；卡在不可中断原生调用中的线程由进程退出回收。
  - 版本清单（`version`，P5）：`{"id":..,"ok":true,"version":"..","build":{"os":..,"arch":..,"debug":bool},"models":{"snapshot":{...},"transcription":{...}}}`。模型清单按引擎自身解析顺序报告每个成员的路径、是否存在、大小与 sha256（缺失即 `exists:false`，不报错、不加载、不联网）；无 transcription feature 的构建报告 `"state":"unavailable"`。诊断通道选择：本命令 + stderr 现状（tracing 单行文本）即满足 P5 的「二选一」，stderr 改结构化 JSON 流未实施。
  - 失败：`{"id":..,"ok":false,"error":"一行错误描述",...}`，无载荷字段；`ocr_snapshot` 失败额外带 `error_kind`（`model_not_ready|asset_invalid|input_invalid|cancelled|internal`，SNAP-15 类别），消息不含图像内容。协议级错误 `duplicate_id`（在途 `id` 重复被调度层拒绝）与 `unsupported_command`（未知命令，附结构化 `command` 键）不属于 SNAP-15 引擎类别，任意命令均可能携带。未知命令的 `error` 文案必须保留 `unsupported command '<name>'` 前缀——run49.1 旧版直通（capabilities 失败文案含该前缀即放行）依赖它，两侧不得删。冷启动期间后续截图的即时响应与重试语义见 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md) SNAP-15。
  - 请求行无法解析为 JSON 时：`id` 回 `null`，`ok:false`；有效 JSON 的字段校验失败仍回显可解析的 id。
  - 能力握手（`capabilities`）除既有能力键外回报进程身份（P2，全部可选、缺任一调用方按旧语义处理）：`instance_id`（进程生命周期内稳定的 UUID 形态标识）、`started_at`（RFC3339 启动时刻）、`config_digest`（生效配置序列化后的 sha256）、`owner`（仅当启动配置带 `owner_token` 时出现，原样回报）。`commands` 数组增量列出全部命令（含 keepalive/shutdown/version）；`protocol_version` 保持 2——本轮均为向后兼容增量，不升版本。

- 退出（2026-09-30 P1 起的完整语义）：
  - **正常会话结束**（调用方关 stdin 或 `shutdown`）：在途文档请求收尾后退出，退出码 0；但收尾有硬上限——从触发点起引擎内置 4 秒（合同 ≤5 秒，留拆除余量），到点取消一切在途请求并由进程退出回收卡在原生调用中的线程，不再等待、不再补发结果。EOF 后截图在途取消遵循 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md) SNAP-16。
  - **宿主消失**（stdout 写失败、stdout 管道断裂、父进程死亡、stdin 读失败）：取消全部在途请求、不再写任何响应，以**固定退出码 86** 退出。检测通道（Windows）：每秒一次对 stdout 原生句柄的 `FlushFileBuffers` + 零字节 `WriteFile` 探测（只认断管族错误，console/重定向句柄的其余错误忽略，不影响手工运行）；父进程死亡用可等待句柄监视（toolhelp 快照取父 PID 后 `WaitForSingleObject`，取不到即放弃该通道，不误报——父 PID 被复用只会漏报不会误杀）。进程同时死掉时 stdin EOF 与 86 路径存在微秒级竞态，宿主被杀场景的验收只断言「进程 ≤5 秒消失」，退出码不作为区分依据。
  - **空闲自退**（P4，仅当配置 `idle_timeout_ms`）：无在途请求且无任何请求流量持续该时长后以**退出码 87** 退出（区别于 86）。
  - 退出码路由：非 0 码（86/87）经进程内静态变量由 `main` 在 `run_cli` 正常展开（tracing guard 已 flush）之后 `std::process::exit` 应用；普通取消/超时仍走请求协议，不关闭共享连接。

## 语义需求

1. **同进程双场景并发**：文档转换与截图 OCR 必须能在同一进程内重叠执行。文档转换尚未返回时，worker 仍须接收并处理截图请求及状态查询，截图推理不得被文档请求队列或覆盖整个转换的全局互斥阻塞。同类文档请求仍可顺序处理；底层模型各自的资源约束继续有效，但不能把两种场景重新串行为“一个完成后另一个开始”。
2. **逐请求响应与故障隔离**：成功、部分提取、失败都返回对应 `id`；单文件失败后进程继续提供两种场景的服务，不取消无关截图请求或清空另一通道模型。处理过程中的 panic 转为该文件的失败响应，不得终止 worker（库内已有的逐后端 catch_unwind 之外的进程级兜底）。
3. **进程内模型常驻与复用**：worker 复用进程级模型缓存（文档 OCR、截图 OCR、SenseVoice/VAD 等已编译能力的模型），可按需首次加载，加载后在进程内保留；相同配置的后续请求、下一文档批次和场景切换不得重复加载。同一进程须能同时保有文档 OCR 与截图 OCR 的独立模型会话，二者不互相替换或驱逐；模型隔离遵循 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md)。tokio runtime 按进程构建一次；截图与转写的第二次请求均应复用对应已加载模型。
4. **退出释放**：调用方结束整个 worker 会话后进程退出并释放模型；普通文档批次完成时进程与模型继续可用，生命周期由调用方管理。
5. **模型与配置隔离**：文档 OCR 和截图 OCR 使用各自模型及配置，按各自合同校验和执行；隔离必须在上述单进程内实现，不能通过再启动 Xberg 实例实现。并发或交替请求均不得串用模型、线程参数、预处理、阈值或布局配置。
6. **通信边界**：stdout 只输出协议消息；诊断日志（tracing、panic 输出）一律走 stderr。
7. **故障职责边界**：worker 实现请求级取消与超时，不自我重启。JchTools 管理连接和崩溃后的恢复、目录持久化、UI 配置与单实例启动。普通停止一项任务不得结束共享进程。
8. **父进程死亡即自退（2026-09-30，调用方 P1）**：worker 默认携带断连自退语义，无需调用方传任何参数——stdin EOF / stdout 断裂 / 父进程死亡 / stdin 读失败任一触发，引擎在 ≤5 秒内自行终止（内置 4 秒硬上限 + 拆除余量）。退出码：正常会话结束 0；宿主断连 86；空闲自退 87。孤儿引擎不得继续存活占用资源或挡住下一次启动。
9. **优雅停机不拒退（P3）**：`shutdown` 先应答后退出；宽限（请求 `grace_ms` 或缺省 4 秒）是上限不是谈判空间，到点必须退。
10. **一切增量向后兼容**：新命令可发现（capabilities.commands 增量列出）、新字段可选、错误类别只增不改名；`unsupported command '<name>'` 文案前缀在结构化 error_kind 普及前必须保留；protocol_version 仅在语义不兼容时 +1。

## 完成判据与验证

- **仅有命令、模型缓存或交替调用成功，不算完成同进程并发要求。** 自动化 E2E 必须启动真实 worker，确认文档请求仍在执行时发入截图请求，并以同一进程标识、请求时序和实际结果证明截图推理确实与文档转换重叠；两边输出各自满足转换及截图质量断言；运行期间用于这两种场景的 Xberg 进程数量始终为 1。不能用两个 Xberg 子进程、预先生成截图结果或只入队未推理替代。
- 隔离验收：同一进程加载文档与截图各自规定的不同模型和配置，在文档转换与截图推理重叠期间核对模型身份、实际采用的配置及各自输出，确认没有互相覆盖或共享错误会话。
- 模型常驻验收：在同一个 worker 内连续转换使用同一模型的文档，穿插多次截图，并在下一文档批次再次使用，两套已加载模型均不重复加载或互相替换。用模型加载事件或等价可观测证据确认复用，单纯更快不足以证明；转写会话复用的既有要求继续有效。
- UT 覆盖：协议解析与字段语义（id 回显、mode 默认、未知字段忽略、缺字段拒绝）、跨场景调度、并发响应行完整性与 EOF 退出、坏 JSON / 不支持的 command / 不支持的 mode / 缺失文件 / handler panic 的故障隔离、真实 extract 路径（真实临时文件成功 + 缺失文件失败）、响应 wire 形状；CLI 解析测试证明 `worker` 子命令在所有 feature profile 注册。
- fulltest 语料验收不覆盖 worker 协议（它只走 `extract`）。UT 覆盖协议和局部处理；E2E 必须经真实 `xberg worker` 子进程或调用方集成跨过 stdin/stdout、模型加载与退出边界，UT 不能替代这一链路。
- 生命周期 E2E（无需模型，`python scripts/tests/worker_lifecycle.py`，仅用户点名时运行）：capabilities 身份字段与命令面、keepalive/version/formats/未知命令 wire 形状、真实 extract（normal+fast）、shutdown 空闲立即退（码 0）、关 stdin ≤5 秒退（码 0）、关 stdout 读端 ≤5 秒退（码 86）、杀宿主 ≤5 秒内进程消失、`idle_timeout_ms` 自退（码 87）、超时自报后进程存活、结束时无 debug 二进制残留进程。带真实模型的并发/取消/超时 E2E 仍以 `worker_concurrency.py` 为准。

## 实现与验证状态

- **共享协议扩展已实施，尚未发布验收**：新增 cancel / timeout_ms、extract fast、formats / capabilities / model_state；媒体解码/VAD 检查取消，SenseVoice 与 VAD 均保留模型会话，VAD 每流重置。新增 UT 与扩展真实模型 E2E，当前只运行标准 cargo check、fastcheck 和脚本语法检查，不代表 UT、真实模型或发布包通过。
- `model_state.models`：snapshot 返回完整加载状态；document 返回所选后端及可观测的 resident_sessions（Paddle，包含版本/模型/slot 键，其他后端显式 not_reported）；transcription 返回 SenseVoice/VAD 缓存状态与会话键。查询不触发模型加载。
- 取消不能安全强杀原生调用；终态只在处理器返回后发送，现有第三方阻塞路径仍需真实 E2E 覆盖。源码、类型检查和历史版本 smoke 均不能替代本次发布包验收。

- 四命令已实现并注册。worker 启动一次 runtime 并复用进程内模型状态；初版曾记录仅 cargo check / fastcheck 通过、UT 已编写待运行，不能把该初版记录当作后续所有提交的验收状态。
- 后续 2026-09-27 历史冒烟记录已验证截图模型和 SenseVoice 的批内复用（第二次请求无再加载；当时数据位于 `.tmp/contrast/` 与 `.tmp/worker_smoke.py`），因此“全部模型复用仍待验证”的旧笼统结论已过时。该记录不证明文档 OCR 各缓存路径或 JchTools 侧完整集成均已验收；这部分尚无本文记录的完整证据。
- **同进程并发已实施、待运行验收**：2026-09-29 改为独立 stdin 读取、文档工作线程、截图工作线程及单一响应出口。两工作线程保有各自配置与模型，状态查询不等待推理；响应通过 id 关联，可以先返回后到达的截图请求。同类文档顺序处理，满队列明确失败；未启动额外 Xberg 进程。历史 smoke 不代替新并发链路验收。
- stdin EOF 现在可在截图处理期间置取消标志，已有瓦片/识别批次检查点负责停止；EOF 后不输出该截图结果，文档请求正常收尾。stdout 写失败也会取消截图并结束调度，不等待下一次 stdin 输入。仅关闭 stdout 而保持 stdin 打开时仍须一次写入才能发现断连；其他截图接口边界见 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md)。
- 新增调度 UT（受控阻塞文档期间截图与查询完成、跨批次复用、EOF 取消、错误隔离、完整响应、队列满）及真实模型 E2E 脚本，入口见 [AGENTS.md](../../AGENTS.md)。UT 中的模型处理桩只证明调度；E2E 使用真实 CLI、文档和截图、内容断言及同 PID 起止事件，缺少模型或不能观察到重叠不能通过。

## 实现与验证状态（2026-09-30 P1–P6 轮）

- **已实施（本轮，全部带 UT/E2E）**：
  - P1 断连自退：调度器新增停机状态机（EOF/断连/停机共用，触发即设硬上限）；`worker/monitor.rs` 的 Windows 双监视（父进程死亡句柄等待 + stdout 管道探测，探测原语有真实管道 UT）；退出码 0/86/87 经 `main` 尾部的 `FORCED_EXIT_CODE` 在 tracing guard 展开后应用。仅关闭 stdout 而无写入的场景由探测覆盖（探测原语在真实 OS 管道上 UT 验证 + 真实二进制 E2E 验证 1.11 秒内退出）。
  - P2：capabilities 增 `instance_id` / `started_at` / `config_digest` / `owner`（仅配置时）；`--config-json` 顶层 `owner_token` 在合并前剥离（其余未知字段仍拒绝）；未知命令回 `error_kind:"unsupported_command"` + 结构化 `command` 键，文案前缀保留。
  - P3：`shutdown`（`grace_ms` 可选，缺省 4 秒）先应答后停机，宽限内拒新请求，到点强制退出，码 0。
  - P4：`--config-json` 顶层 `idle_timeout_ms`（正整数，0/负数/非整数启动即拒）空闲自退，码 87。
  - P5：`version` 命令（构建信息 + 模型清单路径/存在/大小/sha256，不加载不联网）；stderr 保持 tracing 单行文本（结构化 JSON 流未实施，按「二选一」满足）。
  - P6：`keepalive` 命令（廉价就绪摘要，语义如上文）。
- **本轮验证（2026-09-30，Windows 11，标准 fork feature 集）**：标准 `cargo check` 通过；`cargo test -p xberg-cli`（标准集）全部通过，含 worker 模块 42 项（新增：shutdown 应答/拒新/排空/强制、EOF 硬上限、断连标志取消、空闲自退、stdin 读错误映射、keepalive/version 内联应答、capabilities 身份、启动键剥离、版本清单、退出码映射、探测原语真实管道检测）；`server_test`（mcp feature 腿重编）通过；真实二进制 E2E `scripts/tests/worker_lifecycle.py` 全部通过（关 stdin 0.05s 码 0、关 stdout 读端 1.11s 码 86、杀宿主 0.15s 消失、shutdown 空闲 0.05s 码 0、idle 1.26s 码 87、超时自报后存活）。
- **本轮未验证/边界**：本机无截图与转写模型资产，`ocr_snapshot`/`transcribe`/带模型的并发与取消重叠未在本轮 E2E 重放（既有覆盖以 `worker_concurrency.py` 为准，须用户点名）；宿主被杀时 stdin EOF（码 0）与断连（码 86）存在微秒级竞态，验收只断言进程消失；fulltest/slowtest 门与发布包验收未运行，不宣称发布版本已满足；磁盘不足时按缓存纪律清理了 `target/debug` 下 27.2 GB 历史 PDB（纯调试副产物，不影响增量构建）。
