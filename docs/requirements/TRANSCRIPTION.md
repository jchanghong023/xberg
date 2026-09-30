# Windows 音视频转写差异需求

适用范围和状态见 [README.md](README.md)；模型交付见 [DELIVERY.md](DELIVERY.md)；批次接口见 [WORKER.md](WORKER.md)。以下要求继续本地维护。

2026-09-27 重大变更（用户跨仓库重构目标授权）：Whisper tiny 转录链路（含 30 秒分块并行、RMS 静音跳过、symphonia 解码、Media Foundation/外部 ffmpeg 回退）已被移除；唯一转写链路改为 JchTools `markdown-media-worker` 的现行离线 CPU 链路：**FFmpeg 原生共享库解码与重采样 → Silero VAD → SenseVoice INT8**。新旧实现在移除前完成了同批合成样本对照（记录见文末）。

## 转写链路与行为

- 唯一链路：FFmpeg 共享库（avutil-61 / swresample-7 / avcodec-63 / avformat-63，随包分发、运行期加载）解码并重采样为 16 kHz 单声道 s16 PCM（按原比例转 f32，完整处理解码器与重采样器尾部样本）→ Silero VAD 切分 → SenseVoice INT8 识别（经 sherpa-onnx v1.13.6 C API 运行期加载）→ 固定术语映射 → 段级时间戳输出。选第一条音轨；不提供云端、自动语言选择或后端切换。
- VAD 参数固定：threshold 0.25、min_silence 0.5 s、min_speech 0.5 s、window 512、max_speech 10.0 s、buffer 60 s（16 kHz）。段时间戳语义 = VAD 段起止样本位置 ÷ 16000（不是 token 级时间戳）。
- 识别固定：feature 80 维、16 kHz、language=zh、use_itn=1、provider cpu；模型、token 表、VAD 模型的版本/大小/SHA-256 钉定在二进制内清单并加载前校验（见 [DELIVERY.md](DELIVERY.md)）；摘要不符即失败，不做任何回退。
- 转录正文应用固定字面术语映射（37 条，长源词优先、区分大小写、只作用于片段正文；如「扫描链」→`scan chain`、「固定型故障」→`stuck-at`、`systemverilog`→`SystemVerilog`），映射表随实现锁定，不扩大为通用润色，未列项保持原文。
- 输出结构固定（JchTools T-20 语义）：`# <输入文件全名（含扩展名）>`、`- 音频时长: HH:MM:SS.mmm`、`- 语音片段: N`、`## 转录`，每段一行 `[start --> end] text`（`HH:MM:SS.mmm`）。无音轨时时长行为 `- 音频时长: 无音频轨道` 且正文给出「（无音频轨道）」；有音轨但无语音时给出「（未检测到语音）」——两者是明确说明，不是失败。解码失败属于失败，不留下半成品。JSON 输出（`extract --format json`、worker `transcribe`）同时提供结构化段列表（`start_ms`/`end_ms`/`text`）。
- 配置：`transcription.enabled`、`backend`（仅接受 `"sensevoice"`）、`model_dir`（缺省回退 `XBERG_SENSEVOICE_MODEL_DIR` 及 exe 旁候选）、`max_duration_ms`、`max_bytes`、`timeout_ms`。Whisper 专属键（`model`、`language`、`timestamps`、`model_cache_dir`、`allow_network`、`verify_hash`）已退役，传入时明确报错并提示 `backend:"sensevoice"`。
- 原生库查找顺序：显式配置 → `XBERG_SHERPA_DLL_DIR` / `XBERG_FFMPEG_DLL_DIR` → exe 旁候选目录 → PATH；sherpa DLL 加载前先加载同目录 `onnxruntime.dll`，防 PATH 上旧运行库抢载。外部 ffmpeg 不再是任何路径的运行前提；`XBERG_FFMPEG`、`XBERG_ASF_DECODER` 环境变量退役。
- WMV/ASF 仍可读（经 FFmpeg DLL 解码，替代原 Media Foundation 路径）。
- 模型会话在进程内复用（worker 批次、serve），键含模型目录、线程配置与 DLL 路径；摘要校验在会话首次创建时执行。同一识别器上的解码+推理串行，不同模型根互不阻塞。
- 普通 extract 的 `transcription.timeout_ms` 仍是其既有墙钟边界。共享 worker 的超时/取消以 [WORKER.md](WORKER.md) 为准：每个请求持有独立 token，FFmpeg 包读取、VAD 窗口及语音段之间停止，当前原生调用先返回；等待真实处理结束才发终态，不能杀共享进程或只丢弃结果。

## 验收

- 真实模型：加载、摘要校验、断网环境完成 MP4/M4A 转录；断言转录文本、段起止时间戳与片段数——不以模型加载成功或程序未崩溃代替转写结果断言。
- 失败与边界：无音轨、无语音、损坏/截断输入、超时、调用方断连各有可区分结果；44.1 kHz 立体声等非常规参数经重采样路径验证。
- `fulltest.py --deep` 的判定与转写量交叉核对继续生效；媒体资产经 `--media-assets`（或 `XBERG_MEDIA_ASSETS`/仓库 `.tmp/assets`）注入，缺席时音视频按 `AV_NO_ASSETS`（WARN）如实记为未验证，不伪装成通过。
- worker `transcribe`（见 [WORKER.md](WORKER.md)）与 `extract` 两条路径的输出语义一致（同一 SV-06 结构）。

## 验证边界与历史记录

- 2026-09-29 常驻扩展（已实施、未运行发布包验证）：SenseVoice 与 Silero VAD 会话一起缓存，资产只在缓存未命中时校验加载。文件开始/结束（含取消、异常返回）在会话锁内 Reset/Clear VAD 流状态，下一文件不重新加载模型且时间轴从零开始；缓存查询不等待加载锁。FFI 使用固定 [sherpa-onnx v1.13.6 C API](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.6/sherpa-onnx/c-api/c-api.h) 的 Reset/Clear 接口，真实流隔离与取消后复用由 worker E2E 验证，未执行前不声称通过。

- 多音轨取第一条、长音频（小时级）内存曲线、奇异容器未逐一实测；wmv/asf 的 FFmpeg 解码路径有实现与扩展映射，公开语料（wmv/mp4）经 `--deep` 覆盖。
- 移除前对照（2026-09-27，同机单次运行，非统计结论；样本为公开合成 SAPI 中文语音 + 正弦/静音/无音轨/截断）：两个实现全部样本可用；whisper 输出为平坦段落（`timestamps=true` 亦无时刻值），SenseVoice 为段级时间戳；72.76 s 中文样本峰值内存 whisper 833.9 MB vs SenseVoice 355.0 MB；英文样本在 whisper 强制 zh 时出现重复引号幻觉、SenseVoice 逐词正确（单样本事实，不外推）；静音/纯音/无音轨/截断行为两实现均可区分。原始数据 `E:\xberg\.tmp\contrast\`（report.md / results.json）。
- 旧条目（MF 优先、30 秒分块并行、Session 互斥、RMS 静音跳过、`transcription/wmf.rs` 无 UT 的记录、17 分钟历史测量）随 Whisper 链路移除一并退役；`whisper_*` 性能阶段名在 [PERFORMANCE.md](PERFORMANCE.md) 保留定义但不再产生。
