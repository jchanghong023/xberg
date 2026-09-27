# 媒体转录 SenseVoice 链路需求（草案 · 待用户确认）

> **状态：草案。** 2026-09-27 起草，服务用户提出的跨仓库重构目标：把 Xberg 现行 Whisper tiny 媒体转录替换为 JchTools `markdown-media-worker` 现行离线 CPU 链路（FFmpeg 解码与重采样 → Silero VAD → SenseVoice INT8），语义固定为 JchTools `docs/requirements/ALL2MARKDOWN.md` 的 T-19/T-20 与附录 C/D。**未经用户逐条确认前不得实施**；本文不覆盖 [TRANSCRIPTION.md](TRANSCRIPTION.md) 既有条目，拟议修订见文末。参数、模型与原生库版本以 JchTools `resources/markdown-assets.json` 与 `optional/markdown-media-worker/` 实际实现为准。条目编号 `SV-xx` 仅为跨仓库引用方便。

## 链路与行为

- **SV-01（唯一链路）** 媒体转录固定为：FFmpeg 原生共享库解码与重采样（16 kHz 单声道 s16 PCM，按原比例转 f32）→ Silero VAD 切分 → SenseVoice INT8 中文识别（use_itn=1）→ 固定术语映射 → 段级时间戳输出。不提供云端、自动语言选择或后端切换。
- **SV-02（解码）** FFmpeg DLL（avutil-61 / swresample-7 / avcodec-63 / avformat-63，固定构建与摘要，随包分发）是唯一解码路径：选第一条音轨、流式解码、完整处理解码器与重采样器的尾部样本；无音轨可区分报告，损坏/截断输入优雅失败。WMV/ASF 仍可读（经 FFmpeg）。Media Foundation 与 symphonia 路径随 Whisper 一并退役；外部 ffmpeg 不再是运行前提。
- **SV-03（VAD 参数固定）** Silero VAD：threshold 0.25、min_silence_duration 0.5 s、min_speech_duration 0.5 s、window_size 512、max_speech_length 10.0 s、buffer_size 60 s（样本率 16 kHz）。时间戳语义 = VAD 段起止样本位置 ÷ 16000，不是 token 级时间戳。
- **SV-04（识别固定）** SenseVoice INT8：feature 80 维、16 kHz、language=zh、use_itn=1、provider cpu、线程数按并发预算配置；每段一次离线解码；空文本段丢弃并如实计数。不提供语言选择、繁简转换或模型补全。
- **SV-05（固定术语映射）** 转录正文应用 JchTools ALL2MARKDOWN 附录 C 的固定字面映射（34 条，长源词优先、区分大小写、只作用于片段正文，未列项保持原文）；映射表随本文件附录锁定，不扩大为通用润色。
- **SV-06（输出结构）** 媒体转录 Markdown 固定结构：`# <输入文件名（不含扩展名含义保留）>`、`- 音频时长: HH:MM:SS.mmm`、`- 语音片段: N`、`## 转录`，每段 `[start --> end] text`（`start`/`end` 为 `HH:MM:SS.mmm`）。无音轨时输出「无音频轨道」说明；有音轨无语音时输出「未检测到语音」说明；解码失败属于失败，不留下半成品。
- **SV-07（顺序与失败）** 段按时间顺序输出；多段失败时报告最低失败段，保持顺序版错误语义（沿用 TRANSCRIPTION.md 第 13 行精神）。

## 模型与资产

- **SV-08（模型清单）** 二进制内 manifest（沿用 PaddleOCR ModelManager 模式）新增：`model.int8.onnx` 239,233,841 字节 / SHA-256 `c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51`（HF `csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17`，pin `2365baeacb507f821a0c8120fcee3d484dba7a07`）；`tokens.txt` 315,894 字节 / `f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc`；`silero_vad.onnx` 643,854 字节 / `9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6`。加载前离线校验摘要，不符即失败不推理；`xberg cache manifest` 覆盖这些条目。
- **SV-09（原生库）** sherpa-onnx v1.13.6 Windows x64 shared C API（`sherpa-onnx-c-api.dll` 2,866,688 / `c3726570…`，`sherpa-onnx-cxx-api.dll` 128,512 / `a90a5e20…`）及其配套 `onnxruntime.dll` 16,720,896 / `4ee0ae76…`、`onnxruntime_providers_shared.dll` 10,752 / `cd724582…`，FFmpeg 四 DLL（avutil-61 3,016,704 / `26ee298e…`、swresample-7 734,720 / `aea51fb6…`、avcodec-63 91,182,080 / `9d82d5c8…`、avformat-63 22,764,032 / `9b41797f…`）随包分发并校验摘要；许可（Apache-2.0 / LGPL-2.1-or-later / MIT）随附。
- **SV-10（推理隔离）** SenseVoice/VAD 推理经 sherpa-onnx 原生接口进程内调用；迁移过渡期与 Whisper 会话互不影响；对照完成后 Whisper 会话与引擎表移除。

## 配置与接口

- **SV-11（配置迁移）** `transcription` 配置块改为 SenseVoice 语义；Whisper 模型枚举及 `model: "tiny"` 等旧值返回明确配置错误（不静默映射）；`timestamps`、`language` 配置项退役并明确报错（新语义固定输出段时间戳、固定中文）。超时、最大时长、最大字节等资源边界配置保留。
- **SV-12（worker 协议扩展）** `xberg worker` 新增 `transcribe` 命令：请求为本地媒体文件路径，响应为单行 JSON，含 SV-06 结构化结果（markdown 与段列表）；串行、模型会话批内复用、EOF 退出、故障隔离等既有 worker 语义适用。
- **SV-13（CLI）** `xberg extract` 对媒体的转录输出采用 SV-06 结构；`serve` 不新增能力。

## 打包

- **SV-14（包内容变化）** Windows zip 移除 Whisper tiny 五文件；新增 SV-08/SV-09 资产（HF 缓存布局或固定目录，实现时锁定）；空缓存负向探测覆盖转录链路；打包验证含一段真实媒体的离线转录 smoke（正向外加空缓存负向诊断）。
- **SV-15（环境变量退役）** `XBERG_FFMPEG`、`XBERG_ASF_DECODER` 随 MF/外部 ffmpeg 路径退役；如保留兼容期需在本文明确期限与行为。

## 性能与对照

- **SV-16（性能阶段名）** 新增 `ffmpeg_decode`、`vad_segment`、`sensevoice_model_load`、`sensevoice_infer`；`whisper_*` 阶段名保留定义不再产生（阶段名只增不改）。
- **SV-17（新旧对照）** 迁移过渡期 Whisper 与 SenseVoice 并存；用同一批公开合成 MP4/M4A 对比两者文本、时间戳、耗时与内存，报告并列呈现，不作「更好」的未经实测结论。对照通过并经用户确认后，移除 Whisper 推理代码（engine/model 的 Whisper 部分、symphonia 解码、MF/wmf.rs、RMS 静音跳过与 30 s 分块结构）。

## 验收

- 真实模型：加载、摘要校验、断网环境完成 MP4/M4A 转录；结果断言覆盖转录文本、段起止时间戳与片段数——不以模型加载成功或程序未崩溃代替转录结果断言。
- 失败与边界：空音轨、无语音、损坏/截断输入、超时、调用方断连各有可区分结果；WAV/立体声/非常规采样率经重采样路径验证。
- `fulltest.py --deep` 判定与转写量交叉核对适配 SV-06 结构（判定器改动按 AGENTS.md 披露要求单独列出）；wmv/asf 经 FFmpeg 路径可转录。
- 对照报告：同批合成样本的两种实现文本/时间戳/耗时/内存并列，明确「已实测」与「未实测」。

## 与既有需求的关系（待确认修订）

- [TRANSCRIPTION.md](TRANSCRIPTION.md) 全文修订：MF 优先条目改为 FFmpeg 唯一解码；30 s 分块并行、Session 互斥、RMS 静音跳过条目随 Whisper 退役；「验证边界」中 wmf.rs 无 UT 的记录随退役更新；17 分钟历史测量条目保留为历史。
- [DELIVERY.md](DELIVERY.md)：第 9 行包模型清单改写（去 Whisper、增 SenseVoice/VAD/DLL/FFmpeg）；第 12 行「抽取/OCR/Whisper 可用」改为「抽取/OCR/SenseVoice 转录可用」；第 8 行 feature 集是否更名/扩展（如 `transcription` feature 内部实现替换）随实现确认。
- [PERFORMANCE.md](PERFORMANCE.md)：阶段名追加（SV-16）。
- [WORKER.md](WORKER.md)：协议扩展（`transcribe` 命令）。
- [README.md](README.md)：需求域表新增本文件；音视频转写能力描述更新。
