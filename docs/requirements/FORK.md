# 文件转换与 CLI 差异需求

适用范围、上游和状态口径见 [README.md](README.md)。本文件承接原权威清单中的转换行为；OCR、转写、性能日志和交付验收分别见同目录对应文档。以下为仍需本地维护的要求，不代表已经完成本轮验收。

## 默认输出与配置

- 库 API 默认输出 Markdown；CLI `xberg extract` 不指定内容格式时仍输出纯文本。配置文件只覆盖提供的键，省略 `output_format` 时保留 CLI 的纯文本基准，不因库默认值而改变。
- `--config-json` 按顶层字段替换后整体反序列化和校验；未提供的顶层字段保留原基准。未编译能力对应的配置字段必须明确报错，不能静默忽略。（该接口边界从原 AGENTS.md 迁入。）
- **零配置模型定位（2026-10-04，承接 JchTools 下沉）**：调用方不带配置与环境变量即获得全部已编译能力。截图 OCR 沿用既有回退链（`--models` → `snapshot_ocr.models_dir` → `XBERG_SNAPSHOT_MODEL_DIR` → exe 旁 `models/snapshot-ocr`）；文档 OCR 与转写模型新增 exe 旁 `models/`（HF hub 布局）作为 `HF_HUB_CACHE`/`HF_HOME` 均未设置时的缓存根回退；onnxruntime 在 `ORT_DYLIB_PATH` 未设置时新增 exe 旁 DLL 回退（Windows `onnxruntime.dll`）。显式环境变量与显式配置始终优先。打包版 exe 与 models/DLL 同包，天然命中。
- **大文档自动降级（2026-10-04）**：引擎在抽取前做轻量页数探测（PDF 读结构页数；docx 读 `docProps/app.xml` 的 `<Pages>`；pptx 数 `ppt/slides/` 条目；xlsx 数 `xl/worksheets/` 条目），页数超过 `auto_fast_pages`（默认 500）时该请求副本自动走快速档（关 OCR；本 fork 未编译 Layout，无此项）。阈值默认值依据标准语料实测最大 358 页（tessent，取自 fulltest 质量报告），保证常规文档零触发、转换质量不变；`0` 禁用。探测失败或页数未知一律回常规模式，探测本身不产生错误或告警。降级必须在 `processing_warnings` 记录页数与阈值（`auto_mode` 来源），供 fulltest 与调用方观测。worker 请求显式 `mode:"normal"` 等价禁用该请求的自动降级。阈值与降级内容属引擎默认值：任何调整以实测效果为依据（fulltest 0 错误 0 告警为底线），不继承调用方历史数值。
- 嵌入完整性与媒体引用的效果验收口径（R3/R6，2026-10-04 扩展）：**全部宿主**的嵌入文档正文并入宿主 Markdown——OOXML（docx/xlsx/pptx，既有 `append_embedded_object_text`）与此前 children-only 的五类宿主（归档 zip/7z/tar/gz 的文档成员、遗留 `.ppt` 的 OLE、PDF `/EmbeddedFiles`、PST 附件、eml 的 `message/rfc822` 嵌套信件，经共享 `merge_children_into_body`）。正文以 `Embedded object: <名>` 说明行 + 原样 RawBlock 进入（文件名不冒充宿主标题，子件 Markdown 结构逐字保留、图片引用重编号到宿主表）；children 列表照旧保留供结构化消费者。归档文本成员以 `=== path ===` 段落的既有行为不变。容器内联有数量预算 `MAX_EMBEDDED_CHILDREN_INLINE`（默认 20）：超出的正文留在 children 并记一条 `processing_warning`（来源标签 + 内联/剩余计数），内容-只读调用方由此可观测，不静默丢失。**邮件（eml/MSG）附件同样经 `merge_children_into_body` 内联**（2026-10-04 二次修复：旧内联路径只搬文本，附件图片字节留在子件、引用被转义成字面量且编号指向子件表）；附件文本预算裁剪语义（`email_attachment_inlining` 警告）保留。
- **EPUB 图片引用与交付名对齐（R4，2026-10-04）**：EPUB markup 片段中的图片引用一律重写为交付名 `image_{index}.{format}`（与 `images[]` 的 image_index/format 一一对应），不再保留调用方无法解析的 EPUB 内部路径；`data:image/...` 内联引用解码为图片（与 HTML 抽取同一交付合同），不再以巨长 data URI 残留正文。注意 `InternalDocumentBuilder::push_image` 返回的是元素索引而非图片索引，交付名以独立计数为准。
- 媒体 `image_{index}.{format}` 引用与内联字节一一对应、正文不内联 Base64、围栏与行内代码内的字面量不改写。JchTools 侧应删除其分节兜底与重复实现，落盘采用「引用名 + 字节 + 调用方目录前缀」三步（fulltest `write_images_from_json`/`save_markdown` 为参考实现）。
- 验收：库默认结果有 Markdown 结构；CLI 缺省及加载未含 `output_format` 的配置后仍是纯文本；显式格式覆盖生效；出厂二进制不接受未编译的 `layout` 配置及依赖该能力的 `pdf_options.reading_order`；零 env 零 config 的裸启动完成含 OCR 的 extract 且 `version` 报告模型 `exists:true`；合成超过阈值的文档自动降级并带 `auto_mode` warning、`auto_fast_pages=0` 与显式 `mode:"normal"` 均不降级、探测失败回常规；fulltest 保持 0 错误 0 告警。

## CLI 图片输出与 Windows 运行

- 提取图片默认统一重编码为 PNG（`images.output_format` 缺省即 `png`，2026-10-04 需求变更）：CLI、`worker`、HTTP 所有调用方零配置即得到字节与扩展名一致的 `image_N.png`，fulltest 与 worker 默认启动的行为天然对齐，不再各自携带覆盖配置。需要源格式直通（不重编码）时显式配置 `{"images":{"output_format":{"type":"native"}}}`。上游默认为 native，属须在合并中保留的 fork 差异。
- text/toon 输出可将抽取的图片落盘。显式 `--output-dir` 时，图片引用包含该目录，空格、括号等按 CommonMark 百分号编码；围栏内原有文本不改写。只有确有可写图片时才给引用添加目录。
- 未指定目录时，单文档图片写到当前目录、保留仅文件名的引用。批处理每份结果使用 `<dir>/doc_<N>/`（未指定时以当前目录为基准），避免不同文档的 `image_N.ext` 互相覆盖；显式基目录必须存在，逐文档子目录可自动创建。
- `--format json` 内联图片字节，不因 `--output-dir` 落盘图片；图片抽取/OCR 开关的含义见 [OCR.md](OCR.md)。上述格式边界依据既有操作说明与 CLI 路径恢复，不能把“默认抽图”解释为所有输出封装都会写文件。
- 保持 Windows CLI 对递归解析器的栈容量保障（既有预算 16 MiB），单文件和批处理不能因 Windows 主线程栈限制而溢出。
- 验收：至少两个含同名图片的文档批量转换后，图片不覆盖、引用可解析；包含空格或括号的目录可用；围栏内容不受路径改写影响；JSON 保留图片数据且不额外写图；不传 `--config-json` 时 `images[].format` 为 `png` 且字节为 PNG 重编码产物，显式 `native` 恢复源格式直通。

## 格式抽取与文档保真

| 能力 | 必须保留的行为及验收条件 |
| --- | --- |
| Office 图元文件 | 内嵌 EMF/WMF 能栅格化为 PNG、落盘并在 Markdown 中引用，供 OCR 使用。 |
| 内嵌 SVG | 文档内嵌的 SVG 成员在 OCR 前经本地 resvg 栅格化（外部 href 一律禁用、不取网络资源），识别文本进入既有 OCR 输出路径；`.svg` 媒体成员与引用保持原样不替换。栅格化含 `<text>` 文本渲染（系统字体，进程内共享加载一次）。 |
| Visio | `.vsd`、`.vsdx`、`.vsdm` 均能路由到对应抽取能力并提取形状文本，扩展名与 MIME 登记一致。截断的 Visio LZW 流按已解码前缀部分恢复文本（块扫描在首个不完整块处自然停止），不得因截断整体丢弃。 |
| Excel | xlsx 内嵌图片能抽取、落盘并 OCR；OCR 输出遵循 [OCR.md](OCR.md)。空工作表仍输出 `## <表名>` 标题与占位说明行（文案以人工修复金标准为准），不得静默丢表。 |
| OOXML/OLE | 内嵌 Word OLE 文本合并到宿主文档对应位置；PPTX OLE/PresentationML fallback 图片能够抽出。归属位置仍是要求，不能以“文末出现过文本”代替。自带 `OlePres000`/`EPRINT` 预览流的 OLE 对象：其可见内容由预览图承载，文本层抽取失败（如 CorelDRAW 无文本抽取器、Visio 流截断）不作为内容丢失告警。 |
| DOCX 页眉页脚图 | 页眉/页脚部件里的图片按「页眉图在正文前、页脚图在正文后」输出，图片关系用部件自身 `_rels` 解析；图片不受 `include_headers/include_footers` 文本过滤影响（页眉文字仍按开关过滤）。 |
| 老版 .doc | piece 表非压缩片段按 UTF-16 解码；「CJK 密度高」本身不得触发 CP1252 回退（仅当 CJK 区码元的低位字节几乎全部是可打印 ASCII、即 ASCII 配对伪影形态时才回退）。`Data` 流内自闭合 PNG/JPEG 块与 `ObjectPool` 各存储的 EPRINT(EMF) 预览按流序抽取落盘。带 `sprmPFInTable` 的段落按表格输出（单元格以制表符分界），不得以正文段落形态散落。 |
| PPTX 备注与页码 | 演讲者备注只取正文占位符文本；日期/页码占位符的 `a:fld` 缓存值（如 `2026/7/31 N`）不进正文。幻灯片上“字面量 Page + slidenum 域”的页码家具按幻灯片真实序号输出 `Page N`，不用域缓存值。 |
| HTML 内联图片 | `data:image/...;base64` 引用解码为图片文件并以 `image_N.ext` 引用（不再内联巨长 data URI）；指向源文件旁不存在目标的本地相对引用剔除（不留死链），文件存在时保留原引用。 |
| 内嵌 CAD 文本 | 内嵌对象的 CAD/DXF 交换文本（超长 AcDb 行形态）不内联展开，以一行 `>` 引用说明代替；负载保留在子文档结果里供结构化消费者。 |
| Markdown 输出噪声 | 行中 `\#` 与有序列表形态的 `\.` 反转义（行首护义 `\#` 保留）；连续 NBSP 按空白折叠；空白 run 上的 `**` 强调标记不输出。 |
| PPTX | 标题与演讲者备注归属到正确幻灯片，不串页、不重复。txBody 段落绑定到 DrawingML（`a:p`）或 PresentationML（`p:p`）前缀均提取——后者是部分生成器的真实产物（JchTools A08 夹具形态），runs 与 `a:fld` 缓存字段值按文档流顺序输出，不得静默丢弃。 |
| PDF | 有原生文本的页面不做破坏性整页 OCR 回退；保留原生文本、重建表格并按配置剔除页眉页脚。`content_filter.include_headers/include_footers` 在扁平文本与结构化两条路径一致生效，跨页重复页眉/页脚剔除也受这些开关约束。 |
| Markdown | 图片 alt 不残留本地路径垃圾；围栏内原有文本保持原样。图片 marker 与 OCR 围栏的关系见 [OCR.md](OCR.md)。 |
| XML/OPML | 元素标题以 ` (k: v, …)` 内联 `id`、`type`、`_note` 等属性；过滤 `xmlns*`、空值及 `xberg:` 内部标记。plain 与 Markdown 保持一致，默认配置下相关 OPML note 与 XML 保真断言成立。 |

以上要求用真实转换结果和对应自动化断言验收。既有关键测试包括 `issue_131_opml_note_attribute`、`xml_embedding_quality`；完整质量判据见 [DELIVERY.md](DELIVERY.md)。没有逐项通过的当前报告时，不把代码存在或历史报告写成“已验收”。

## 实现与验证边界

- 2026-09-30 引擎行为批（当日 fulltest 修复）：xlsx 空表占位、HTML data-URI/死链、DOCX 页眉页脚图、.doc piece 表 CJK 判别收窄 + Data/EPRINT 图抽取 + sprmPFInTable 表格（单段行已验；多段单元格行与 `sprmPFTtp` 行界未完成，ECS 手册 min_tables 仍红）、PPT 图片索引对齐（push_image 强制 image_index=压栈位）、PPTX 备注域过滤与 Page N 家具、Visio 截断流部分恢复、预览流 OLE 不告警、渲染层 `\#`/`\.` 反转义与 NBSP 折叠、内嵌 CAD 省略行。均以 fulltest.py 实测为准（见当日质量报告）；.doc 表格行界与 tessent PDF 书签结构为已知未达项。
- 判定器变更（2026-09-30）：fulltest.py 新增逐文件 `tolerate_codes`（金标准自身触发的码按实测证据放行，依据写在期望文件 `_note_tolerate_codes`）；EMBED_TEXT_LOSS 比对两侧归一化剥 `<br>` 与标记字符。`run_selftest` 已覆盖；ECS min_images 66→63（原 .doc CFB 实测清单）与 tessent max_bold_short_lines 0→464（金标准实测）为披露的阈值校准。
- 既有 2026-09-28 记录：PPTX `p:p` 段落抽取与内嵌 SVG 的 OCR 栅格化已实施，新增 6 个 UT 经红→绿、相关模块 168 个 UT 及 fastcheck 通过。PNM 六种表示的合成边界测试通过；JchTools A17 的 P6 夹具仅含 9/27 字节，属于截断输入，既有优雅失败不作为引擎缺陷。对应 fulltest 的覆盖限制见 [DELIVERY.md](DELIVERY.md)，不能以其问题码未变化代替这些新路径的正向验收。
- 2026-09-29 源码核对：PDF 数字脚注的公开抽取回归测试 `numeric_footnote_survives_public_extraction` 已在 `7697e63075` 恢复启用，结构化路径接入数字边界保护；旧索引中“仍被 ignore、待调试”的状态已过时。本次未执行该测试，不追加通过声明。
- 既有同步记录中的 PDF 数字修复已接入图片 OCR 输出，使正文与网格围栏共同使用修复结果；这不消除 [OCR.md](OCR.md) 中仍有效的同行对齐缺口。详细补丁与上游测试前提调整由 Git 历史保存，不作为额外产品要求。
