# 同进程常驻模型与双场景 worker 接口

## 需求背景

Xberg 通过一个本地 `worker` 进程同时提供文档转换与截图 OCR，已加载的模型在该进程内常驻并复用，避免逐请求启动进程或重复加载模型。2026-09-29 用户明确要求：文档转换进行中，同一个进程仍须能够执行截图模型推理，不能等文档转换结束，也不能靠另起一个 Xberg 进程满足。两种场景只能启动并共用一个 Xberg 进程，禁止按场景、模型或批次另开 Xberg 实例或进程池；不能以多个实例各自常驻来满足本要求。该决定取代旧的全请求串行和仅限单批次生命周期约束。

本接口通过本机 stdio 通信，不新增 HTTP 监听要求。调用方可以保持同一进程跨多个文档批次与截图请求复用；何时结束进程由调用方决定，不能因一批文档结束或切换场景自动卸载另一场景的模型。

## 接口协议

### 2026-09-29 共享 worker 扩展（实现与发布验收中）

共享 worker 的新增协议与职责边界：

- `extract.mode` 支持 `normal` / `fast`，缺省 normal。fast 仅在该请求配置副本上关闭 Layout、图片 OCR（包括扫描页 OCR），保留原生文本、表格与图片提取；不会修改截图配置，也不会卸载模型。是否超过 200 页及逐文件模式选择由 JchTools 决定。
- 工作请求可带正整数 `timeout_ms`，从接收入队起计时；未指定时使用启动配置的 `extraction_timeout_secs`。`cancel` 请求带 `target_id`，仅取消对应在途请求；响应中的 `accepted` 表示收到取消请求，不等于已停止。被取消/超时任务实际返回后才产生唯一终态，`error_kind` 为 `cancelled` / `timeout`，不再发送成功结果。
- 取消是协作式：排队任务不进入处理器；运行任务在解析、解码或推理检查点停止，当前不可中断原生调用需先返回，不能承诺严格毫秒级终止。若原生库永久挂起，无法在安全保留同一进程的同时强制终止该线程；该边界必须在发布报告披露。
- `formats` 返回当前二进制注册的格式；`capabilities` 返回协议版本、命令、模式与取消语义；`model_state` 返回模型状态。查询走已有连接，不启动其他 Xberg 进程。
- 在途 ID 不得重复；取消失败、队列满、单任务失败不破坏其他请求。文档与媒体共用文档通道，截图使用独立推理通道。
- SQLite、保存目录、重启恢复、界面配置、应用级确保仅启动一个 Xberg 均属于 JchTools；Xberg 不实现这些调用方职责。
- 交付必须区分源码实现、类型检查、真实模型 E2E 与发布包验证。新增接口须覆盖取消运行/排队任务、超时后复用、模式隔离、进程内查询及 ID 关联；未运行发布包验收不得声称实际发布版本已满足。

- 启动：`xberg worker --config-json <固定配置>`（同时支持 `--config` / `--config-json-base64` / `--no-config-discovery`，语义与 `extract` 相同）。启动配置同时包含文档转换配置和独立的 `snapshot_ocr` 截图配置，两者分别固定；请求仅通过 mode 选择文档处理方式，不接受任意配置覆盖。两场景的模型集、模型路径、推理参数与会话分别生效；截图配置不覆盖文档 OCR 配置，文档设置也不覆盖截图配置，不为共用进程而强制两套模型或参数相同。
- 请求（stdin，逐行一个 JSON）：

  ```json
  {"id":1,"command":"extract","path":"C:\\docs\\a.pdf","mode":"normal"}
  ```

  - `id`：任意 JSON 值，响应原样回显，用于关联；调用方须为同时在途的请求使用可区分的 id。
  - `command`：支持 `extract`、`ocr_snapshot`、`snapshot_state`、`transcribe`、`cancel`、`formats`、`capabilities`、`model_state`。
  - `path`：本地文件路径（`extract` / `transcribe` 使用）。
  - `image_base64`：图像字节（PNG 或等价无损编码；`ocr_snapshot` 使用，字节只驻内存）。
  - `mode`：可省略；`normal` 按启动配置处理，`extract` 还支持 `fast`；其他命令不接受 fast。
  - 未知字段忽略；空白行跳过。
  - 每个通道的待处理队列有界；队列满时以对应 `id` 返回明确失败并提示稍后重试，不阻塞另一通道或状态查询，不静默丢请求。

- 响应（stdout，每个请求恰好一行完整 JSON，写出即 flush；并发完成时各行不得交错损坏，以 `id` 关联请求，允许按完成顺序返回）：
  - 成功或部分提取（`extract`）：`{"id":..,"ok":true,"document":{...},"warnings":[...]}`。`document` 与 `xberg extract --format json` 的 `result` 字段同构（`ExtractedDocument` 原样序列化，图片字节内联）；`warnings` 是 `document.processing_warnings` 的副本。部分提取（个别阶段失败但产出了文档）属于 `ok:true` + 非空 `warnings`。
  - 截图识别（`ocr_snapshot`）成功：`{"id":..,"ok":true,"text":"<布局文本>","records":N,"elapsed_ms":M}`（`error_kind` 键仅在有类别情形时出现，普通成功省略该键）；无文字图片 `ok:true`、`text:""`、`error_kind:"no_text"`。
  - 转写（`transcribe`）成功：`{"id":..,"ok":true,"markdown":"<SV-06 全文>","segments":[{"start_ms":..,"end_ms":..,"text":..}],"duration_ms":..,"has_audio":true}`；`markdown` 标题为输入路径的完整文件名（含扩展名）。
  - 状态查询（`snapshot_state`）：`{"id":..,"ok":true,"state":"uninitialized|loading|ready|error","error":null}`。
  - 失败：`{"id":..,"ok":false,"error":"一行错误描述",...}`，无载荷字段；`ocr_snapshot` 失败额外带 `error_kind`（`model_not_ready|asset_invalid|input_invalid|cancelled|internal`，SNAP-15 类别），消息不含图像内容。协议级错误 `duplicate_id`（在途 `id` 重复被调度层拒绝）不属于 SNAP-15 引擎类别，任意命令均可能携带。冷启动期间后续截图的即时响应与重试语义见 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md) SNAP-15。
  - 请求行无法解析为 JSON 时：`id` 回 `null`，`ok:false`；有效 JSON 的字段校验失败仍回显可解析的 id。
- 退出：调用方关闭 stdin 表示结束整个 worker 会话，而不是普通文档批次边界；在途文档请求收尾后正常退出（退出码 0），截图在途取消遵循 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md) SNAP-16。stdout 写失败表示客户端断连，取消任务并以非零退出码结束。普通取消/超时走请求协议，不关闭共享连接。

## 语义需求

1. **同进程双场景并发**：文档转换与截图 OCR 必须能在同一进程内重叠执行。文档转换尚未返回时，worker 仍须接收并处理截图请求及状态查询，截图推理不得被文档请求队列或覆盖整个转换的全局互斥阻塞。同类文档请求仍可顺序处理；底层模型各自的资源约束继续有效，但不能把两种场景重新串行为“一个完成后另一个开始”。
2. **逐请求响应与故障隔离**：成功、部分提取、失败都返回对应 `id`；单文件失败后进程继续提供两种场景的服务，不取消无关截图请求或清空另一通道模型。处理过程中的 panic 转为该文件的失败响应，不得终止 worker（库内已有的逐后端 catch_unwind 之外的进程级兜底）。
3. **进程内模型常驻与复用**：worker 复用进程级模型缓存（文档 OCR、截图 OCR、SenseVoice/VAD 等已编译能力的模型），可按需首次加载，加载后在进程内保留；相同配置的后续请求、下一文档批次和场景切换不得重复加载。同一进程须能同时保有文档 OCR 与截图 OCR 的独立模型会话，二者不互相替换或驱逐；模型隔离遵循 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md)。tokio runtime 按进程构建一次；截图与转写的第二次请求均应复用对应已加载模型。
4. **退出释放**：调用方结束整个 worker 会话后进程退出并释放模型；普通文档批次完成时进程与模型继续可用，生命周期由调用方管理。
5. **模型与配置隔离**：文档 OCR 和截图 OCR 使用各自模型及配置，按各自合同校验和执行；隔离必须在上述单进程内实现，不能通过再启动 Xberg 实例实现。并发或交替请求均不得串用模型、线程参数、预处理、阈值或布局配置。
6. **通信边界**：stdout 只输出协议消息；诊断日志（tracing、panic 输出）一律走 stderr。
7. **故障职责边界**：worker 实现请求级取消与超时，不自我重启。JchTools 管理连接和崩溃后的恢复、目录持久化、UI 配置与单实例启动。普通停止一项任务不得结束共享进程。

## 完成判据与验证

- **仅有命令、模型缓存或交替调用成功，不算完成同进程并发要求。** 自动化 E2E 必须启动真实 worker，确认文档请求仍在执行时发入截图请求，并以同一进程标识、请求时序和实际结果证明截图推理确实与文档转换重叠；两边输出各自满足转换及截图质量断言；运行期间用于这两种场景的 Xberg 进程数量始终为 1。不能用两个 Xberg 子进程、预先生成截图结果或只入队未推理替代。
- 隔离验收：同一进程加载文档与截图各自规定的不同模型和配置，在文档转换与截图推理重叠期间核对模型身份、实际采用的配置及各自输出，确认没有互相覆盖或共享错误会话。
- 模型常驻验收：在同一个 worker 内连续转换使用同一模型的文档，穿插多次截图，并在下一文档批次再次使用，两套已加载模型均不重复加载或互相替换。用模型加载事件或等价可观测证据确认复用，单纯更快不足以证明；转写会话复用的既有要求继续有效。
- UT 覆盖：协议解析与字段语义（id 回显、mode 默认、未知字段忽略、缺字段拒绝）、跨场景调度、并发响应行完整性与 EOF 退出、坏 JSON / 不支持的 command / 不支持的 mode / 缺失文件 / handler panic 的故障隔离、真实 extract 路径（真实临时文件成功 + 缺失文件失败）、响应 wire 形状；CLI 解析测试证明 `worker` 子命令在所有 feature profile 注册。
- fulltest 语料验收不覆盖 worker 协议（它只走 `extract`）。UT 覆盖协议和局部处理；E2E 必须经真实 `xberg worker` 子进程或调用方集成跨过 stdin/stdout、模型加载与退出边界，UT 不能替代这一链路。

## 实现与验证状态

- **共享协议扩展已实施，尚未发布验收**：新增 cancel / timeout_ms、extract fast、formats / capabilities / model_state；媒体解码/VAD 检查取消，SenseVoice 与 VAD 均保留模型会话，VAD 每流重置。新增 UT 与扩展真实模型 E2E，当前只运行标准 cargo check、fastcheck 和脚本语法检查，不代表 UT、真实模型或发布包通过。
- `model_state.models`：snapshot 返回完整加载状态；document 返回所选后端及可观测的 resident_sessions（Paddle，包含版本/模型/slot 键，其他后端显式 not_reported）；transcription 返回 SenseVoice/VAD 缓存状态与会话键。查询不触发模型加载。
- 取消不能安全强杀原生调用；终态只在处理器返回后发送，现有第三方阻塞路径仍需真实 E2E 覆盖。源码、类型检查和历史版本 smoke 均不能替代本次发布包验收。

- 四命令已实现并注册。worker 启动一次 runtime 并复用进程内模型状态；初版曾记录仅 cargo check / fastcheck 通过、UT 已编写待运行，不能把该初版记录当作后续所有提交的验收状态。
- 后续 2026-09-27 历史冒烟记录已验证截图模型和 SenseVoice 的批内复用（第二次请求无再加载；当时数据位于 `.tmp/contrast/` 与 `.tmp/worker_smoke.py`），因此“全部模型复用仍待验证”的旧笼统结论已过时。该记录不证明文档 OCR 各缓存路径或 JchTools 侧完整集成均已验收；这部分尚无本文记录的完整证据。
- **同进程并发已实施、待运行验收**：2026-09-29 改为独立 stdin 读取、文档工作线程、截图工作线程及单一响应出口。两工作线程保有各自配置与模型，状态查询不等待推理；响应通过 id 关联，可以先返回后到达的截图请求。同类文档顺序处理，满队列明确失败；未启动额外 Xberg 进程。历史 smoke 不代替新并发链路验收。
- stdin EOF 现在可在截图处理期间置取消标志，已有瓦片/识别批次检查点负责停止；EOF 后不输出该截图结果，文档请求正常收尾。stdout 写失败也会取消截图并结束调度，不等待下一次 stdin 输入。仅关闭 stdout 而保持 stdin 打开时仍须一次写入才能发现断连；其他截图接口边界见 [OCR-SNAPSHOT.md](OCR-SNAPSHOT.md)。
- 新增调度 UT（受控阻塞文档期间截图与查询完成、跨批次复用、EOF 取消、错误隔离、完整响应、队列满）及真实模型 E2E 脚本，入口见 [AGENTS.md](../../AGENTS.md)。UT 中的模型处理桩只证明调度；E2E 使用真实 CLI、文档和截图、内容断言及同 PID 起止事件，缺少模型或不能观察到重叠不能通过。本轮标准 feature 集 cargo check 与 fastcheck 已通过；按运行授权边界未执行 Rust UT、真实 worker E2E、fulltest、slowtest 或调用方集成，尚不宣称功能验收完成。
