# 文件转换与 CLI 差异需求

适用范围、上游和状态口径见 [README.md](README.md)。本文件承接原权威清单中的转换行为；OCR、转写、性能日志和交付验收分别见同目录对应文档。以下为仍需本地维护的要求，不代表已经完成本轮验收。

## 默认输出与配置

- 库 API 默认输出 Markdown；CLI `xberg extract` 不指定内容格式时仍输出纯文本。配置文件只覆盖提供的键，省略 `output_format` 时保留 CLI 的纯文本基准，不因库默认值而改变。
- `--config-json` 按顶层字段替换后整体反序列化和校验；未提供的顶层字段保留原基准。未编译能力对应的配置字段必须明确报错，不能静默忽略。（该接口边界从原 AGENTS.md 迁入。）
- 验收：库默认结果有 Markdown 结构；CLI 缺省及加载未含 `output_format` 的配置后仍是纯文本；显式格式覆盖生效；出厂二进制不接受未编译的 `layout` 配置及依赖该能力的 `pdf_options.reading_order`。

## CLI 图片输出与 Windows 运行

- text/toon 输出可将抽取的图片落盘。显式 `--output-dir` 时，图片引用包含该目录，空格、括号等按 CommonMark 百分号编码；围栏内原有文本不改写。只有确有可写图片时才给引用添加目录。
- 未指定目录时，单文档图片写到当前目录、保留仅文件名的引用。批处理每份结果使用 `<dir>/doc_<N>/`（未指定时以当前目录为基准），避免不同文档的 `image_N.ext` 互相覆盖；显式基目录必须存在，逐文档子目录可自动创建。
- `--format json` 内联图片字节，不因 `--output-dir` 落盘图片；图片抽取/OCR 开关的含义见 [OCR.md](OCR.md)。上述格式边界依据既有操作说明与 CLI 路径恢复，不能把“默认抽图”解释为所有输出封装都会写文件。
- 保持 Windows CLI 对递归解析器的栈容量保障（既有预算 16 MiB），单文件和批处理不能因 Windows 主线程栈限制而溢出。
- 验收：至少两个含同名图片的文档批量转换后，图片不覆盖、引用可解析；包含空格或括号的目录可用；围栏内容不受路径改写影响；JSON 保留图片数据且不额外写图。

## 格式抽取与文档保真

| 能力 | 必须保留的行为及验收条件 |
| --- | --- |
| Office 图元文件 | 内嵌 EMF/WMF 能栅格化为 PNG、落盘并在 Markdown 中引用，供 OCR 使用。 |
| Visio | `.vsd`、`.vsdx`、`.vsdm` 均能路由到对应抽取能力并提取形状文本，扩展名与 MIME 登记一致。 |
| Excel | xlsx 内嵌图片能抽取、落盘并 OCR；OCR 输出遵循 [OCR.md](OCR.md)。 |
| OOXML/OLE | 内嵌 Word OLE 文本合并到宿主文档对应位置；PPTX OLE/PresentationML fallback 图片能够抽出。归属位置仍是要求，不能以“文末出现过文本”代替。 |
| PPTX | 标题与演讲者备注归属到正确幻灯片，不串页、不重复。 |
| PDF | 有原生文本的页面不做破坏性整页 OCR 回退；保留原生文本、重建表格并按配置剔除页眉页脚。`content_filter.include_headers/include_footers` 在扁平文本与结构化两条路径一致生效，跨页重复页眉/页脚剔除也受这些开关约束。 |
| Markdown | 图片 alt 不残留本地路径垃圾；围栏内原有文本保持原样。图片 marker 与 OCR 围栏的关系见 [OCR.md](OCR.md)。 |
| XML/OPML | 元素标题以 ` (k: v, …)` 内联 `id`、`type`、`_note` 等属性；过滤 `xmlns*`、空值及 `xberg:` 内部标记。plain 与 Markdown 保持一致，默认配置下相关 OPML note 与 XML 保真断言成立。 |

以上要求用真实转换结果和对应自动化断言验收。既有关键测试包括 `issue_131_opml_note_attribute`、`xml_embedding_quality`；完整质量判据见 [DELIVERY.md](DELIVERY.md)。没有逐项通过的当前报告时，不把代码存在或历史报告写成“已验收”。
