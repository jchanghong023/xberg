# 可选性能日志差异需求

适用范围及验证状态见 [README.md](README.md)。此能力为本地可选能力，标准包的能力边界见 [DELIVERY.md](DELIVERY.md)。

- 通过编译期 `perf-tracing` 开关启用，覆盖单/批抽取整体、引擎、文件/字节抽取、格式分发、pipeline、图片 OCR、Whisper 加载和转写、输出渲染、PaddleOCR 初始化与推理。
- 日志独立写到相对当前目录的 `logs/perf.log.<日期>`，按天滚动；`XBERG_PERF_LOG_DIR` 可覆盖目录。退出时须刷出队列中的日志。性能日志不改变 stderr 业务日志的等级、格式或位置。
- `target` 固定为 `perf`。以下阶段名和含义只增不改，以便跨版本比较：`extract_command`、`batch_command`、`engine_extract`、`engine_extract_batch`、`extract_file`、`extract_bytes`、`format_extract`、`pipeline`、`image_ocr`、`whisper_model_load`、`whisper_transcribe`、`render_output`、`paddle_engine_init`、`paddle_ocr_infer`。路径、MIME、元素数等动态信息放在字段中，不嵌进阶段名。
- 每个阶段关闭时记录 `time.busy` / `time.idle`。busy 是进入该 span 的时间，不是整个父任务的墙钟时间；后台或子任务可能体现为父 span 的 idle，分析必须结合最深工作阶段读数。
- 标准构建不启用此开关，不增加该能力的依赖或告警，性能构建不随包分发。标准构建已经启用 OTel 的事实不得使性能构建的 span 静默失效。
- 依据现状恢复的失败边界：日志目录创建失败时给出诊断并禁用性能日志，业务转换继续。

验收：启用后执行真实单文件或批处理，在指定日志目录获得适用阶段及其耗时记录；默认构建依赖图和告警不变，stderr 业务日志不变；启用 OTel 时仍有性能记录；日志路径不可用不阻断转换。现有转换质量报告不等价于已验收性能日志，需单独核验上述行为。
