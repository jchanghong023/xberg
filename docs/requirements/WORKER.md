# JchTools 批次转换 worker 接口

## 需求背景

JchTools 批量转换文档时，逐文件调用 `xberg extract` 每个文件都付出一次进程启动与模型加载成本。Xberg 提供批次内常驻的本地工作进程子命令 `worker`：一个批次只付一次冷启动，文件之间在进程内复用已加载的模型会话。接口只面向本机 stdio，不含 HTTP 监听，也不在批次之间常驻。

## 接口协议

- 启动：`xberg worker --config-json <固定配置>`（同时支持 `--config` / `--config-json-base64` / `--no-config-discovery`，语义与 `extract` 相同）。抽取配置在启动时固定，请求中不再携带任何配置。
- 请求（stdin，逐行一个 JSON）：

  ```json
  {"id":1,"command":"extract","path":"C:\\docs\\a.pdf","mode":"normal"}
  ```

  - `id`：任意 JSON 值，响应原样回显，用于关联。
  - `command`：支持 `extract`、`ocr_snapshot`、`snapshot_state`、`transcribe`。
  - `path`：本地文件路径（`extract` / `transcribe` 使用）。
  - `image_base64`：图像字节（PNG 或等价无损编码；`ocr_snapshot` 使用，字节只驻内存）。
  - `mode`：可省略；当前仅支持 `normal`（按启动配置完整执行）。其他值返回失败响应，作为将来的扩展位。
  - 未知字段忽略；空白行跳过。

- 响应（stdout，每个请求恰好一行 JSON，写出即 flush）：
  - 成功或部分提取（`extract`）：`{"id":..,"ok":true,"document":{...},"warnings":[...]}`。`document` 与 `xberg extract --format json` 的 `result` 字段同构（`ExtractedDocument` 原样序列化，图片字节内联）；`warnings` 是 `document.processing_warnings` 的副本。部分提取（个别阶段失败但产出了文档）属于 `ok:true` + 非空 `warnings`。
  - 截图识别（`ocr_snapshot`）成功：`{"id":..,"ok":true,"text":"<布局文本>","records":N,"elapsed_ms":M,"error_kind":null}`；无文字图片 `ok:true`、`text:""`、`error_kind:"no_text"`。
  - 转写（`transcribe`）成功：`{"id":..,"ok":true,"markdown":"<SV-06 全文>","segments":[{"start_ms":..,"end_ms":..,"text":..}],"duration_ms":..,"has_audio":true}`；`markdown` 标题为输入路径的完整文件名（含扩展名）。
  - 状态查询（`snapshot_state`）：`{"id":..,"ok":true,"state":"uninitialized|loading|ready|error","error":null}`。
  - 失败：`{"id":..,"ok":false,"error":"一行错误描述",...}`，无载荷字段；`ocr_snapshot` 失败额外带 `error_kind`（`asset_invalid|input_invalid|cancelled|internal`，SNAP-15 类别），消息不含图像内容。
  - 请求行无法解析为 JSON 时：`id` 回 `null`，`ok:false`。
- 退出：调用方关闭 stdin 即批次结束；worker 完成在途请求后正常退出（退出码 0）。stdout 写失败（客户端断连）视为批次中止：对截图识别置取消标志（在瓦片/识别批次检查点生效），worker 以非零退出码结束，不再空转。`transcribe` 的断连由调用方按超时杀进程边界处理。

## 语义需求

1. **严格串行**：worker 写完上一个请求的最终响应后才读取下一个请求；调用方在收到响应前不得假定下一请求已开始。
2. **逐文件响应与故障隔离**：成功、部分提取、失败都返回对应 `id`；单文件失败后进程继续接收下一文件。处理过程中的 panic 转为该文件的失败响应，不得终止 worker（库内已有的逐后端 catch_unwind 之外的进程级兜底）。
3. **批内模型会话复用**：worker 复用与 `xberg serve` 相同的进程级模型缓存（OCR 引擎池、Tesseract processor、截图 OCR 模型会话、SenseVoice/VAD 会话、layout 模型缓存、后处理器快照等，按 key 惰性加载并常驻），同一批次内同配置的文件不得重复加载模型；tokio runtime 同样按进程构建一次，不逐请求重建。截图模型的第二次 `ocr_snapshot` 请求不得再次出现模型加载；`transcribe` 第二次请求不得再次出现 239 MB 模型加载。
4. **退出释放**：stdin EOF 后进程退出并释放模型；不在批次之间常驻，生命周期由调用方管理。
5. **通信边界**：stdout 只输出协议消息；诊断日志（tracing、panic 输出）一律走 stderr。
6. **故障职责边界**：单文件计时、超时杀进程、崩溃后重启新 worker、用户停止时不再发送下一条请求并关闭 stdin——均由调用方（JchTools）负责，worker 不实现超时或自我重启。

## 完成判据与验证

- **仅增加 `worker` 命令而未验证模型会话复用，不算完成这项优化。** 运行时验证口径：同一 worker 连续转换两个使用同一模型的文件，第二个文件不得再次出现模型加载（以耗时或 stderr 日志判断），或等价的 JchTools 侧集成验证。
- UT 覆盖：协议解析与字段语义（id 回显、mode 默认、未知字段忽略、缺字段拒绝）、串行循环与 EOF 退出、坏 JSON / 不支持的 command / 不支持的 mode / 缺失文件 / handler panic 的故障隔离、真实 extract 路径（真实临时文件成功 + 缺失文件失败）、响应 wire 形状；CLI 解析测试证明 `worker` 子命令在所有 feature profile 注册。
- fulltest 语料验收不覆盖 worker 协议（它只走 `extract`）；worker 的端到端验证以 UT + 调用方集成为准。

## 实现状态（2026-09-27）

- `extract`、`ocr_snapshot`、`snapshot_state`、`transcribe` 四命令已实现并注册；截图模型与 SenseVoice 会话的批内复用经运行时冒烟验证（第二次请求无再加载；冒烟脚本与对照数据存 `E:\xberg\.tmp\contrast\` 与 `.tmp/worker_smoke.py`）。
- 断连取消：stdout 写失败置共享取消标志，截图识别在瓦片/批次检查点终止后进程退出；「断连瞬间在途的那一次请求先完成再发现断连」是已知边界。

## 实现状态（2026-09-27，初版 worker）

- 已实现 `crates/xberg-cli/src/commands/worker.rs`（请求/响应类型、串行循环、panic 兜底、真实抽取 handler）并在 `main.rs` 注册 `worker` 子命令（无 feature 门控）。复用点：`commands/extract/runtime.rs::build_runtime`（放宽为 crate 可见，批次内只建一次 runtime）、`commands/extract/mod.rs::single_result_from_output`（放宽为 crate 可见）。
- 模型复用不新增机制：依赖既有进程级缓存（`plugins/registry`、`paddle_ocr` engine pool、Tesseract `OnceCell`、`transcription` ENGINES、`layout` ModelCache 等），`xberg serve` 多请求复用的同一套状态；worker 只需常驻进程。
- 验证状态：`cargo check`（fork feature 集）与 `fastcheck` 通过；`cargo test`（含新增 UT）与 fulltest 未运行（按仓库规则待用户授权）；模型会话复用的运行时验证与 JchTools 侧接入待做。
