# AGENTS.md

## 项目概况与权威文档

- 本仓库由 AI Agent 开发与维护：后续 agent 按本文件执行开发、验证与文档同步，质量由自动化验收（`fulltest.py` / `slowtest.py` 的质量报告）保证，不依赖用户人工读代码或人工回归。
- **两份权威文档，职责分开**：`AGENTS.md`（本文件）＝开发规则与操作手册；`fork.md`（仓库根）＝需求权威，记录本 fork 相对上游必须长期保留的功能、默认行为与验收条件。具体需求、功能规划、需求变化只写 `fork.md`，不写进本文件。
- **文档联动**：需求或用户可见行为变化 → 必须同步 `fork.md`（不得只改代码）；仅实现方式变化（需求不变）→ 不制造需求变更，也不得为了让文档符合现状而改写需求、把缺陷合理化；入口 / 命令 / 开发规则变化 → 同步本文件；同步上游后 → 按 `fork.md` 逐项核对仍有效的本地需求，而不只是看有没有 Git 冲突。

## 仓库性质

- 本仓库是 fork：origin = `https://github.com/jchanghong023/xberg.git`（个人仓库），上游 = `https://github.com/xberg-io/xberg.git`（remote 名 `upstream`）。同步上游用普通 `git merge upstream/main`（历史上即如此）。
- **fork 定制清单在 `fork.md`（仓库根，入库）**：记录本仓库相对上游的全部功能差异，是判断「这段代码是不是 fork 特有」的唯一索引。改动 fork 定制能力时必须同步更新 `fork.md`（保持精简）。
- **合并上游的冲突策略**：解决冲突时对照 `fork.md`——冲突文件命中 fork 改动面的，保住 fork 行为；冲突过大难以逐行合并时，**先接受上游版本**，再按 `fork.md` 逐项判断哪些 fork 定制需要在上游新代码上重新实现，重做完由用户决定何时跑 fulltest 验证。
- **仅支持 Windows**：个人代码和包只在 Windows 11 上使用。本地编译与 GitHub 流水线（见 `.github/workflows/build-windows-cli.yml`）都只针对 Windows，不要为其他平台做适配或测试。
- **AGENTS.md 跟踪入库（fork 特有约定）**：上游的 `.gitignore` 故意忽略 `AGENTS.md`，本 fork 已删除该忽略条目以保留本文件；merge 上游时若把这条忽略规则带了回来，必须再次移除，保住本文件的入库状态。

## 硬性约束（必须遵守）

- **总工作流（用户的固定需求，以后无需重新解释）**：日常开发改完代码 → 用户明确要求时跑 `fulltest.py`（本地编译的二进制，快速验证转换质量）；发布前用户明确要求时跑 `slowtest.py`（完整打包 → 打包版全量端到端测试 → 质量报告），是否发布由用户根据报告决定。
- **严禁私自跑费时操作**：未经用户明确点名要求，不得运行任何编译（`cargo build`/`clippy`/`test` 等一切编译类命令，**唯一例外见下一条的 `cargo check`**）、测试（`fulltest.py`、`slowtest.py`、端到端测试）或打包（`package-cli-windows.ps1`）。何时测试由用户自己决定。
- **agent 可主动跑的类型检查（唯一编译例外）**：改完 Rust 代码后允许（并建议）跑
  `cargo check -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`
  ——分钟级、只做类型/借用检查、不产出二进制不跑测试，用于把编译错误挡在交付前（fork 推送到 main 无任何自动编译 CI：上游 `ci-rust.yaml` 的 job 都有 `github.repository == 'xberg-io/xberg'` 守卫，在本 fork 全部 skip）。其余编译类命令仍需用户点名。
- **fastcheck（agent 可自主跑的快速门，≤60 秒）**：`python testgate.py fastcheck`——三级测试门中唯一无需授权的层级（内容见「三级测试门」一节）：测试脚本语法自检、`fulltest.py --selftest`、打包/CI 关键 PS1 解析、两个干净 crate 的 `cargo fmt --check`；60 秒墙钟硬超时，超时杀进程树判失败、绝不报成功。它只是快速反馈，不代表完整验证；`cargo check` 例外（分钟级）不属于它，仍按上条单独跑。
- **交付 Rust 改动时必须报告验证状态**：跑了 `cargo check` 就报结果；没跑就必须显式标注「未编译验证」并列出静态审查覆盖点（读过哪些调用方、检查过哪些类型/feature 门控），由用户决定何时编译。禁止在未验证时暗示"已修复"。
- **验收资产修改必须显式披露**：修改 `fulltest.py` 的判定逻辑/阈值/问题码、`_expectations.json` 的任何键、或重设基线时，必须在回复中**单独列出改动点并给出实测依据**（哪个文件哪次实测值支持这次调整），不允许夹在引擎改动里静默带过。fulltest 报告与基线已记录金标准 sha256 与代码 commit，资产被动过是可对账的。
- **报告闭环（声称修复前必须核对）**：用户跑完 fulltest 后，后续 agent 会话在声称任何修复生效前，必须先读 `D:\测试转markdown转换效果\测试文档_md_fulltest\_quality-report.json`，逐码核对「与基线对比」的新增/已修复/恶化与自己的声明一致；不一致不得声称已修复，只能报告"已实施、待用户验证"。
- **临时脚本 / 临时目录 / 临时文件只准放 `./.tmp`**：仓库内的一次性脚本、临时目录、临时文件一律建在仓库根目录的 `.tmp\`（不存在先 `mkdir -p .tmp`），不得散落在仓库根、`scratch*` 或其他任何目录——根目录只保留仓库资产，避免 `git status` 噪音和误提交；`.tmp/` 已在 `.gitignore` 中（不入库）。验证完自行清理临时产物。
- **`fulltest.py`（仓库根目录）只在用户明确要求时才运行**。它是文档转换效果的集成测试：遍历 `D:\测试转markdown转换效果\测试文档`，逐文件调用本地编译的 CLI 转成 Markdown，打印五层质量评估（结构启发式、用 pymupdf/python-docx/python-pptx/openpyxl 对源文件算文本召回率、xberg 元数据警告、深检与逐文件金标准断言、对抗语料失败路径），结果输出到 `D:\测试转markdown转换效果\测试文档_md_fulltest`。运行它只需本地编译出 exe（不需要打包），默认遇 FAIL 立即终止，`--keep-going` 跑完；音视频转写超时默认 1800s。音视频也只用本地编译版：预检会用 max_bytes=1 快速探测 transcription feature，缺 feature 直接报错退出并提示 `cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`，**不回退打包版**。
- **`slowtest.py`（仓库根目录）同样只在用户明确要求时才运行**。慢速全量验证，测**打包版 CLI**：① 跑 `scripts/publish/cli/package-cli-windows.ps1` 打完整 zip；② 解压到临时目录，对解压出的 xberg.exe 跑 fulltest.py（`--keep-going` 全量测完，`--pkg-dir` 指向解压目录）；③ 输出转码质量报告（报告副本存 `target/slowtest-report-<时间戳>.md`）。包含完整 release 编译与打包，耗时可能 30 分钟以上；`--skip-package` 可复用已有 zip。**用户根据该报告决定是否发布版本。**

## 测试要求（对 agent 的硬性要求）

- **功能性开发和功能性修改必须有自动化验证**：UT 验证局部逻辑（源码内 `#[cfg(test)]` 与 crate 的 `tests/`）；E2E 验证从真实公开入口到可观察结果的完整链路——本 fork 的 E2E 就是 `fulltest.py`（真实 `xberg.exe extract` → 落盘 Markdown/图片 + 逐文件金标准断言 + 对抗语料失败路径）与 `slowtest.py`（打包版跑同一套）。跨模块交互按需增加集成测试。
- **编译、`cargo check`、clippy、局部模拟都不能替代 E2E**：它们只证明类型或局部逻辑，不证明转换质量；桩和模拟可以补充测试，但绕过的真实边界（真实文件、真实 OCR / 转写模型、真实 CLI 入口）必须说明，不能当作端到端结论。
- **测试要对需求与验收条件负责**：覆盖核心成功路径与相关关键失败路径（失败路径＝`_adversarial/` 那套语义：损坏 / 截断 / 空输入必须优雅失败），不得只复述实现或只断言"没 panic"。
- **验证状态必须如实区分**：已实现 / 验证通过 / 验证失败 / 未验证（写明未验证范围与原因）。环境、依赖或权限不足时说明未验证部分，不能用"已修复"描述未验证的改动（与「硬性约束」的交付要求一致）。
- **Windows 上可用的 UT 入口**（均属编译类命令，只在用户点名时运行）：`cargo test -p xberg`、`cargo test -p xberg-cli`，或 `task test:quick`（= `cargo test --locked --lib --workspace --exclude xberg-php --exclude xberg-node --exclude xberg-wasm`，只跑 lib 单测）。`task test` / `task test:ci` 在 `.task/languages/rust.yml` 里只声明了 linux / darwin 平台，在 Windows 上不执行。
- **test_documents 语料二进制不进 git**：`test_documents` 子模块只含源码与 `corpus.lock.json`（693 个对象的 sha256/大小清单），.pdf/.docx/.pst 等二进制须先从公开 bucket 拉取到工作树（约 618 MB，一次性）：`cd test_documents && python scripts/fetch_corpus.py`（已正确文件只做哈希校验；语料路径被子模块 .gitignore 忽略，不会弄脏父仓）。缺语料时 cargo test 会出现大量 "fixture not found" 失败；testgate fulltest 门的 `test-documents-corpus` 阶段会先做存在性预检并给出该修复命令。
- **现状与缺口（如实记录）**：本 fork 没有自动跑 Rust 测试的 CI（上游 `ci-rust.yaml` 等编译 / 测试 workflow 被仓库守卫 skip，无守卫的 `ci-lint` 只跑治理 / 文档 / 脚本类检查，不编译 Rust、不跑 Rust 测试）；fork 新增模块多数自带 UT（`extraction/visio.rs`、`rendering/ocr_layout.rs`、`extraction/markdown_utils.rs`、`extraction/excel/images.rs`、`crates/xberg-windows-metafile`），但部分模块（如 `transcription/wmf.rs`）没有 UT，只有 fulltest 的 E2E 覆盖；上游 `e2e/` + `fixtures/` 的语言绑定 e2e 在本 fork 不运行、不作为验收依据。2026-09-19 首次授权运行 testgate fulltest 门并修到全绿（含此前从未在 Windows 验证过的全部 lib/集成测试；发现并修复的缺陷见当次提交）。

## 三级测试门（`testgate.py`，fork 特有）

三级入口统一在仓库根 `testgate.py`，层级语义固定（fastcheck=AI 自主快速反馈；fulltest=当前平台完整本地验证；slowtest=再叠加打包与远程阶段）。三者与验收脚本的称呼区分见末条。

- **fastcheck ＝ `python testgate.py fastcheck`**：agent 可自主执行，无需授权，60 秒墙钟硬超时。内容：`fulltest.py`/`slowtest.py`/`testgate.py` 语法自检 → `fulltest.py --selftest`（判定器自测）→ `package-cli-windows.ps1`/`offline-smoke.ps1`/`verify-windows-dll-closure.ps1` 的 PS1 解析 → 三 crate 的 `cargo fmt --check`（2026-09-19 经用户授权执行 `cargo fmt -p xberg` 清掉 155 文件量级既有漂移后，xberg crate 纳入 fastcheck）。fastcheck 通过≠完整验证。
- **fulltest 门 ＝ `python testgate.py fulltest`**：当前平台（Windows）完整本地验证，**仅限用户对本次运行明确授权**。阶段独立汇报、任一 FAIL 即门失败：三 crate `cargo fmt --check` → `cargo build -p xberg-cli`（fork feature 集）→ `python fulltest.py --keep-going`（依赖 build 成功）→ `test-documents-corpus` 存在性预检（缺语料给出 fetch 命令，见「测试要求」）→ `cargo test`（`-p xberg --features formats-no-heic,analysis,ocr,paddle-ocr,transcription,api`、`-p xberg-cli --no-default-features --features <fork 集>`、`-p xberg-windows-metafile`；test 阶段以 `CARGO_BUILD_JOBS=8` 限编译并行度，防 jobs=28 耗尽页面文件 os error 1455，覆盖语义不变）→ `cargo clippy`（同三目标，`-D warnings`）。2026-09-19 首次授权运行即修到全绿（此前 fmt-xberg 155 文件漂移、clippy 既有告警等已一并清理，见当次提交）。
- **slowtest 门 ＝ `python testgate.py slowtest`**：最高级验证，**仅限用户对本次运行明确授权**。先跑完整 fulltest 门（失败即止，后续阶段记 SKIPPED_PRIOR_FAIL，不在已知失败状态上打包/发布），再跑 `python slowtest.py`（完整打包 + 打包版 fulltest.py；解压目录固定在仓库 `target/slowtest-tmp/`，**成功即删**并只保留 `target/slowtest-report-<时间戳>.md|.json` 副本，失败或加 `--keep-tmp` 才保留供检查——旧行为是每轮在系统 `%TEMP%` 留 0.6~0.9 GB 且永不清）。远程阶段：`build-windows-cli.yml` 会创建带时间戳 tag 的**公开 GitHub Release**（真实发布副作用），因此**只有用户明确授权发布目标并显式加 `--with-release-ci`** 才触发并轮询到最终结论（要求工作区干净且 HEAD 已推到 origin）；默认该阶段记 SKIPPED_NOT_AUTHORIZED，此时只能宣称「本地部分通过」，不得说完整 slowtest 已通过。WSL/跨平台：SKIPPED_NOT_APPLICABLE（本 fork 仅支持 Windows，见「仓库性质」）。该 workflow 只调打包脚本、不回调 testgate，无远程递归。
- **称呼区分（消歧规则）**：用户点名「fulltest.py / slowtest.py」（带 .py）＝只跑那两个脚本本身，既有语义与验收地位不变；点名「fulltest / slowtest 门」「完整本地验证」「testgate xxx」＝跑对应的门。口语「跑 fulltest」按既有习惯默认指 fulltest.py 脚本。

## fulltest.py 的作用（本仓库的验收标准）

- **它就是验收标准本身**：`fulltest.py` 产出的 `_quality-report.md` / `_quality-report.json` 即转换质量判定——**报告里的红项（FAIL/WARN）＝当前待修清单，主队列 11 个文件全部 PASS 且 `_adversarial` 失败路径全 PASS（"全绿"）＝达标**。转换器改动一律以「红项减少 / 无新增码 / 无恶化」评估，不靠人工逐文档复核。
- **检查分五层**（每层独立出码）：① 进程/结构（退出码、空结果、乱码与标记泄漏、围栏与表格列、落盘图片合法性）；② 源文对齐（去页眉后 bigram 召回、数字与标识召回、正文体量比、分页覆盖）；③ 交叉核对（源页数/媒体清单 vs 引擎 counts vs 落盘 vs MD 引用、音视频转写量）；④ 深检（Markdown 语义噪声、内嵌对象与子文档保真、PPTX 标题与备注、xlsx 图形文本、PDF 书签与表格数、OCR 通道、逐文件金标准断言）；⑤ 失败路径（对抗语料 `_adversarial/`：损坏/截断/空文件必须优雅失败——非零退出+诊断 或 干净转换，panic/静默失败/吐垃圾都判红，码 `ADV_*`）。
- **仓外数据文件（不入 Git）**：
  - `D:\测试转markdown转换效果\_expectations.json`：逐文件金标准。键有 `required_tokens` / `forbidden_patterns` / `order` / `min_*`、`max_*` 指标 / `require_chinese_ocr` / `require_nested_bullets` / `toc_heading_min_recall` 等（合法键集见 fulltest 的 `KNOWN_FILE_KEYS`）；顶层 `run.ocr_config` 是 OCR 覆盖配置（默认空＝不覆盖 CLI 的 OCR 设置），**本 fork 已不使用 layout/table 模型**（2026-09-21 需求变更：feature 集与打包脚本都移除了 `layout-detection`，RT-DETR/TATR 不再随包）——因此 `run` 段**不要**再加 `layout_config`（该 feature 未编译，CLI 收到 `layout` 字段会直接报错）；图片的版面信息由 `rendering/ocr_layout.rs` 的 ```text 网格围栏承载。改判定的阈值仍按实测校准，见各文件 `_note_*`。**加载期自检**：拼错的键、含控制字符/编译失败/匹配空串的 pattern、过短 token 都会打「配置告警」（进报告）——键拼错＝检查静默不生效，2026-09-14 曾实证 `forbidden_patterns` 写 `\b`（JSON 退格转义）导致回归守卫失效。
  - `D:\测试转markdown转换效果\_quality-baseline.json`：回归基线，`--save-baseline` 生成；报告 `## 与基线对比` 输出「新增/已修复/恶化」——新增/修复比问题码集合，**恶化比同码出现次数**（如 `DUP_SPAM` 2→5 算恶化，只比集合会吞掉质量劣化）。未加载金标准的运行里，依赖金标准的码（`GOLDEN_*`、OCR 期望码、阈值类）会从对比两侧剔除，避免出现假的"已修复"；金标准 sha256 与基线不一致时会显式提示（阈值类对比仅供参考）。基线还记录生成时的 git commit / 阈值 / argv，报告可对上「哪次代码跑出来的」。
- **常用命令**：`python fulltest.py --keep-going` 跑完不停；`--save-baseline [--force]` 更新基线；`--selftest` 只跑判定器自测（合成样例+随机等价对照，不需要 CLI/语料，改判定函数后必跑）；`--no-expectations` / `--expectations <path>` 为降级跑法（请同时用 `--out` 指向独立目录，别覆盖标准报告）；`--strict` 把 WARN 也算失败。
- **`--save-baseline` 有护栏**：存在 FAIL 判定文件、本轮提前终止（半截结果）或金标准未加载时拒绝保存（把坏状态存成回归基准会静默遮蔽真回归）；确认红项可接受后加 `--force` 重设。
- **改完检查判定/阈值后必须 `--save-baseline` 重设基线**：用的还是旧判定的话，修正掉的假阳会继续显示成"已修复"，污染转换改进的读数。
- **改脚本时的约定**：阈值问题先调常量或期望值，不删检查；新增「是否出码依赖金标准」的检查必须把码名登记进 `fulltest.py` 的 `EXP_GATED_CODES`；新增问题码要同时加进 `ISSUE_META` 并给严重度；**改判定核心函数（`_golden_token_hit` / `save_markdown` / `_validate_expectations` / `compare_baseline` / `baseline_block_reason` / `classify_adversarial_failure` / `finalize_adversarial_success`）必须同步加/改 `run_selftest` 用例并跑 `python fulltest.py --selftest`**（该命令属脚本级自检，agent 可主动运行）；改 expectations 合法键集时同步 `KNOWN_FILE_KEYS`。
- **已知不自动判定的形态（盲区 backlog，修相关模块时评估能否补检查）**：T1 同一内容二次 OCR 输出的重复（两遍乱码不同，8-gram 无交集 → `DUP_CONTENT` 不报，仅当围栏块与正文/其他围栏块高度相似才报）；T2 源图被截图浮层遮挡/裁掉的像素（如被上传按钮盖住的 `()`）；T3 中文词内部被插空（与 `IDENT_FRAGMENTED` 的拉丁标识符判据不同，误报率高）；T4 图表题注顺序颠倒（缺稳定锚点）；T5 归属错位类（内嵌对象文本堆在文末、不插回所属页）；T6 行内 `| --- |` 形式的"表格被压成一行正文"（长度阈值够不着）。这些在报告里不会出现，需要人工或后续新增检查。
- **对抗语料（失败路径层）**：`D:\测试转markdown转换效果\测试文档\_adversarial\`（empty.pdf / truncated.pdf / corrupt.docx，可按需扩充；子目录不进主队列，主队列跑完后追加）。判定语义：非零退出且 stderr 有诊断＝优雅失败 PASS；panic/backtrace＝`ADV_PANIC`；非零退出无输出＝`ADV_SILENT_FAIL`；"成功"但输出垃圾＝`ADV_GARBAGE_OK`；"成功"且空/过短＝`ADV_EMPTY_OK`（WARN）。对抗文件的优雅失败**不需要**金标准条目。
- **阈值来源与校准**：`_expectations.json` 里的 `min_fenced_cjk` 等取自 chi_sim 实测值的一半（OCR 退化回英文输出仍判红）；`min_tables`/`min_headings` 以源文档事实为准（如 tessent 取文档自带 `Table N-M.` 题注 54 个的量级，而不是 `find_tables` 的 468 个——后者约 350 个是页眉框）。改阈值前先按 `_note_*` 注释确认推导依据。
- `slowtest.py` 只是把同一份 `fulltest.py` 换成打包版 CLI 再跑一遍（参数不变），新检查对打包版自动生效。

## 架构与目录组织

**数据流一句话**：`xberg extract <输入>` → MIME 探测（`core/mime.rs`）→ `extractors/` 按格式路由 → 底层解析（`extraction/`；PDF 的底层在 `pdf/`、音视频转写在 `transcription/`；按需叠加 OCR（图片 OCR 的版面由 ```text 网格围栏承载，见 `rendering/ocr_layout.rs`））→ `core/pipeline/` 汇聚 → `rendering/` 产出 Markdown（或 json/html 等输出）。改一个格式的转换行为，通常落在该格式的底层解析模块 + `extractors/` + `rendering/` 三处。

### Rust workspace（`crates/`）

| crate | 角色 |
|---|---|
| `crates/xberg` | 核心库：配置、抽取、OCR、渲染、转写、HTTP API 都在这里面 |
| `crates/xberg-cli` | 二进制 `xberg`：clap 定义在 `src/main.rs`，子命令在 `src/commands/`（extract / cache / config / doctor / server 等） |
| `crates/xberg-windows-metafile` | **fork 新增**：纯 GDI 把 EMF/WMF 栅格化成 PNG |
| `xberg-paddle-ocr` / `xberg-tesseract` / `xberg-candle-ocr` | OCR 后端 crate（本 fork 只用前两个，candle 不编） |
| `xberg-native-pdf` / `xberg-libheif` / `xberg-gliner` | PDF 原生解析 / heic（fork 不编）/ NER gliner（fork 不编） |
| `xberg-ffi` / `xberg-node` / `xberg-wasm` / `xberg-py` / `xberg-php` / `xberg-jni` | 语言绑定，上游资产，fork 不维护 |

workspace 还含 `packages/dart/rust`、`packages/swift/rust`、`tools/benchmark-harness`。

### `crates/xberg/src/` 主要模块（日常改动集中地）

- `core/`——配置系统（`config/` 按 extraction / ocr / pdf / concurrency 等分段定义与合并，`--config-json` 的顶层替换在 `config/merge.rs`）、转换管线 `pipeline/`、MIME 探测、图片编码。
- `extractors/`——对外抽取器：按格式路由、embedded 子文档调度；`extraction/`——各格式底层解析（docx / pptx / excel / visio / ooxml_embedded / image_ocr / markdown_utils 等，fork 改动密集区）。
- `rendering/`——输出渲染：Markdown（`markdown.rs`、`comrak_bridge.rs`）、HTML / djot / plain、图片 OCR 布局块（`ocr_layout.rs`，fork 新增）。
- `pdf/`——PDF 专属：原生文本 `native/`、结构分析 `structure/`（分类/段落/页眉页脚）、表格重建 `table_reconstruct.rs`。
- `ocr/` + `paddle_ocr/`——OCR 后端与调度；`transcription/`——Whisper 转写（`container.rs`/`wmf.rs` 为 fork 新增的 Media Foundation 通道）。
- `api/`——axum HTTP API（`xberg serve`）；`engine/`——引擎编排入口；`presets/`——预设；`doctor/`——诊断。

### 顶层目录

- `fork.md`——fork 相对上游的定制清单（见「仓库性质」）；`fulltest.py` / `slowtest.py`——fork 验收标准（见下节）；`testgate.py`——三级测试门入口（见「三级测试门」）。
- `scripts/`——`publish/cli/package-cli-windows.ps1`（打包唯一入口，也是 fork feature 集的来源之一）、`publish/cli/offline-smoke.ps1`、`ci/`（PE/DLL 闭包校验）。
- `.github/workflows/build-windows-cli.yml`——fork 自有的 Windows 打包 CI，仅手动 `workflow_dispatch` 触发。上游编译/测试类 workflow（ci-rust、ci-e2e 等）带仓库守卫在本 fork 全 skip；push 命中路径会自动跑的只有无守卫的 ci-lint / ci-docs / ci-scripts（其余无守卫 workflow 是 workflow_dispatch / release / issue-PR 事件触发，不随 push 跑）。
- `docs-site/`（Astro + Starlight 文档）、`e2e/` + `fixtures/`、`.ai-rulez/`（ai-rulez 管理的 AI 规则/技能，改规则后需用固定版本的 ai-rulez 重新生成 bundle）。**`e2e/` 与 `fixtures/` 是上游的语言绑定 e2e 资产**（csharp/dart/go/...），本 fork 的验收不走它们（走 fulltest.py），日常不要为它们做适配；merge 上游带进来的改动原样保留即可。
- `packages/` / `integrations/` / `plugin/` / `charts/` / `templates/`——上游生态资产（语言包、第三方集成、Claude 插件、Helm chart、README 生成模板），fork 不主动维护，merge 时原样保留（引擎新增格式时的计数同步除外，见 `fork.md` 末节）。

## 编译 / 打包 / 测试（仅在用户明确要求时执行）

### Rust 构建优化规则（用户强制，2026-09-20）

以下 9 条为用户定下的构建纪律，改 profile / 清产物 / 调 features 前先对照；每条都指向本仓库的具体落点。

1. **必须用 incremental 编译，禁止无理由 `cargo clean`**：`incremental = true` 在 `.cargo/config.toml`（alef 生成，DO NOT EDIT）。要清东西必须能说出「它属于哪个废弃 profile/feature/target」。
2. **优先保留当前有效 target 缓存，避免冷编译**：删除前先确认产物是否还是当前 profile/feature 的（`target/debug/deps` 里的 rlib/rmeta、`target/debug/build`、`target/debug/incremental` 是热缓存，默认不动）。
3. **dev 构建减少 debug/PDB，优先 `debug = 0`**：`Cargo.toml` 的 `[profile.dev] debug = 0`，`[profile.test] debug = 0`（test 显式钉死，防继承回潮）。要符号按次开：`CARGO_PROFILE_DEV_DEBUG=1` / `CARGO_PROFILE_TEST_DEBUG=1`。
4. **保持 toolchain / features / RUSTFLAGS / target / profile 稳定**：fork feature 集固定为上表那 7 个（不含 layout-detection）（改能力必须同步本文件与打包脚本 `$Features`）；不要临时加 `RUSTFLAGS` 或切换 `--target` 三元组（会另生成一整套 `target/<triple>/` 缓存）。
5. **Windows 链接优先 LLD**：`.cargo/config.toml [target.x86_64-pc-windows-msvc] linker = "rust-lld"`（本机 + 交叉目标都走这份配置；回退就删该段）。
6. **开发阶段禁止 LTO 等昂贵 release 优化**：日常验证/编译一律 dev profile，不要用 `--release` 跑 fulltest。
7. **LTO / strip / codegen-units / opt-level=3 只用于最终打包**：只在 `[profile.release]`（当前 `lto = false`、`codegen-units = 256`、`opt-level = 3`、`strip = true`），服务对象是 `package-cli-windows.ps1` 与 slowtest。
8. **磁盘不足按下述优先级删（先废弃、后缓存）**：① 废弃 profile 的产物——`target/debug/**/*.pdb`（PDB 是纯调试副产物，删了不影响增量构建正确性；注意本仓库 dev 与 test 共用 `target/debug/`，`cargo clean --profile test` 实测等于 `cargo clean`，禁用）；② 不再使用的 `target/<triple>/`（旧 feature 集或旧三元组）；③ 旧 release 产物；**最后**才考虑 dev 热缓存。`cargo clean` 无参数＝清空一切，视为最后手段。
9. **定位慢点用 `cargo build --timings`**（产物在 `target/cargo-timings/`）：先看慢 crate / `build.rs` / proc-macro / 最终链接，再决定是否调 `-Jobs`、拆 target 或换 profile，不要凭感觉加并行度。

配套实测（2026-09-20，见「已知坑」第 1 条）：`debug = 1` 下一次冷 `cargo test` 写 88 GB `.pdb` + 38 GB 测试 exe；`debug = 0` 后同一批 ~200 个测试目标仍会写 ~27 GB PDB（MSVC 链接器即便 `debuginfo = 0` 也生成公共符号表，无法归零），单份从 ~410 MB 降到 ~125–150 MB，链接时间同步下降。

**门禁实测（2026-09-20，用户授权连跑三级）**：`fastcheck` PASS 4.2s → `fulltest` 门 PASS 2789s（12 阶段全绿：build-cli 74s、e2e 258s、test-xberg 2226s、test-xberg-cli 159s、clippy ×3 共 67s）→ `slowtest` 门 PASS（`slowtest.py` 615s：打包 zip 361.7 MiB + PE 闭包/正负 smoke 全过 + 打包版 fulltest 验收 FAIL=0 WARN=7、与基线比 新增 2/已修复 0/恶化 0；远程发布阶段未授权记 SKIPPED_NOT_AUTHORIZED，WSL 记 SKIPPED_NOT_APPLICABLE）。滑动窗口内的一次失败值得记住：首轮 `fulltest` 门在 `build-cli` 阶段报 `failed to write ...librustfft-*.rmeta: 另一个程序正在使用此文件 (os error 32)`，同一条命令重跑即过——当时无第二个 cargo、无孤立 rustc，本机实时防护是 腾讯电脑管家（Defender 服务已停），建议把 `E:\xberg\target` 加入其排除项以消除这类随机文件锁。整套跑完 `target/debug` 从 58.9 GB 涨回 157.5 GB（含新旧两代产物），E: 可用空间 179 → 76 GB。

编译和打包是两条独立路径。**跑 fulltest.py 只需要编译，不需要打包**——编译出 exe 直接 `python fulltest.py` 就能测。**唯一例外**：`cargo check -p xberg-cli`（fork feature 集，见「硬性约束」）属类型检查，agent 改完 Rust 代码可主动跑，不算"费时操作"。

### 编译（供开发与 fulltest.py 使用）

本 fork 只需要「文件 → Markdown」+ OCR + 音视频转写 + HTTP 服务（`xberg serve`），**不要** `all` / heic / pdfium / embedding / NER / MCP。

- **标准调试构建（推荐，fulltest 用这个）**：

  ```bat
  cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api
  ```

  产物：`target\debug\xberg.exe`。

  | feature | 覆盖 |
  |---|---|
  | `core-cli` | CLI 入口 |
| `api` | `xberg serve`——HTTP API 服务（axum 纯 Rust 栈，不新增 DLL）；本地编译与打包都必须带 |
  | `formats-no-heic` | docx / pptx / xlsx / pdf 等（无 libheif） |
  | `analysis` | 转换管线 |
  | `ocr` | 图片 → 文字（Tesseract，后备） |
  | `paddle-ocr` | 图片 → 文字（PaddleOCR pp-ocrv6 **tiny**，默认 OCR 后端）；模型随包分发 |
  | `transcription` | 音视频 → 文字（Whisper + ORT） |

- **不要** `cargo build -p xberg-cli`（default 带 embeddings/candle 等，比上述集重）。
- **不要** `--features all`（在标准集之上额外拉 mcp/heic/pdfium/ner/summarization 等，且 heic 在 Windows 常编不过）。
- 音视频与本地编译版：fulltest.py 对所有文件（含音视频）只用 `--cli` 指定的本地 CLI，无任何打包回退；缺 transcription feature 时预检即失败并提示重编。

### 打完整包（发布用，含模型与 DLL）

- 一条命令：`pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1`。需 PowerShell ≥ 7.4（`Start-Process -Environment` 等依赖，脚本头 `#Requires -Version 7.4` 已声明）。本地默认并行度 min(30, 逻辑核数)，可用 `-Jobs N` 覆盖；CI（`.github/workflows/build-windows-cli.yml`）调用的是同一脚本，是打包的唯一入口。
- 产物：`xberg-cli-x86_64-pc-windows-msvc\` 目录（xberg.exe + onnxruntime/CRT DLL + Whisper tiny 模型）并压成同名 zip。
- **feature 集与开发编译一致**：`formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`（`--no-default-features`），打包版 exe 含 `xberg serve` HTTP API。**不含** heic、pdfium、candle、mcp、embedding、NER。因此包内**不**附带 pdfium.dll。
- **模型随包分发（离线可用）**：Whisper tiny（`$TranscriptionFiles` 固定校验和）+ **PaddleOCR pp-ocrv6 tiny ≈13 MiB**（det/rec/dict/textline-cls 四条正则，经 `xberg.exe cache manifest` 取二进制里的 sha256/大小后 stage 到 `models/` 的 HF 缓存布局）。**layout 模型（RT-DETR 169.1 MB + TATR 30.2 MB）已随需求变更移除**：feature 集不编 `layout-detection`，`$RequiredModels` 里那两条正则也删了——不要加回来（加了也解析不到，且打包门禁会因为 OCR 断言通过而掩盖它）。`$RequiredModels` 非空会**启用**离线空缓存探测（`scripts/publish/cli/offline-smoke.ps1 -ExpectEmptyCacheFailure`）：该探测用带 `ocr` 配置的抽取跑，空缓存下必须报出 HF 离线缺模型诊断（不保证进程一定失败，所以断言的是诊断而非退出码）。清单里另有 SLANeXT/SLANet_plus/table_classifier/pp_doclayout_v3，本构建不用，**不要**加进 `$RequiredModels`（每条正则必须命中且仅命中 1 条）。
- 包体参考：含 layout + paddle tiny 时 zip 实测 **361.7 MiB**；2026-09-21 移除 layout 模型（RT-DETR 169.1 MB + TATR 30.2 MB，未压缩计）后应显著变小——**尚未重新打包实测**，下次打包以实测值更新此数。`target/package-models-<target>` 缓存跨次复用（删掉要重下）。
- 打包前有门禁：干净 PATH 的 `--version` 探测、MSVC CRT 部署、PE 导入闭包校验（`scripts/ci/verify-windows-dll-closure.ps1`）、离线 smoke（`offline-smoke.ps1`：带 `ocr` 配置的抽取必须成功，且 stderr 出现 `PaddleOCR engine initialized successfully` 而无离线缺模型诊断——2026-09-21 起 layout 断言换成 OCR 断言，因为本构建不再编 layout；`$RequiredModels` 非空时再跑一次空缓存探测，stderr 必须报出 HF 离线缺模型诊断）。
- 用打包版跑音视频时：`HF_HUB_CACHE` 指向包目录下 `models`（hf-hub 的缓存根；`HF_HOME` 会被它解析成 `$HF_HOME/hub`，指不到包内模型），`PATH` 前置包目录（fulltest.py 已自动处理）。

### 测试入口与发布流程

- **默认快速验证**：`python fulltest.py`——用本地编译的二进制快速验证（只需编译，无需打包），质量报告打印到终端并写入 `<输出目录>/_quality-report.md`。
- **发布前慢速验证**：`python slowtest.py`——① 跑完整打包生成 zip；② 解压到 `target/slowtest-tmp/`，对打包版 CLI 全量跑端到端文档转换 + 音频/视频转写测试（`--keep-going`）；③ 输出转码质量报告（报告副本 `target/slowtest-report-<时间戳>.md|.json`），成功后解压目录自动删除（`--keep-tmp` 保留）。**是否发布版本，以该报告为准。**

### 其他验证（同样仅在明确要求时执行）

- Rust 测试：`cargo test -p xberg` / `-p xberg-cli`（Windows 可用的 `task` 入口见「测试要求」）；lint：`cargo clippy`；task runner 为 `Taskfile.yml`（`task build`、`task lint:check` 等；`task test` / `task test:ci` 只声明了 linux/darwin 平台）。
- 这些命令与 `cargo build` 同类，属于「严禁私自跑费时操作」的范围：只在用户明确点名时执行；跑完按「硬性约束」如实报告验证状态。

## 已知坑

- **测试/构建产物体量（2026-09-20 实测，决定磁盘与速度）**：`crates/xberg` 有 ~200 个集成测试目标，每个都静态链接整个 xberg lib。
  - `Cargo.toml` 的 `[profile.dev] debug = 0` 与 `[profile.test] debug = 0` 就是为此：`debug = 1` 时每个测试二进制各写一份 ~410 MB PDB，实测一次冷 `cargo test` 在 `target/debug/deps` 留下 **88 GB `.pdb`（198 个文件 >300 MB）+ 38 GB 测试 exe**，`E:` 总共 240 GB，直接顶到只剩几十 GB；改 `debug = 0` 后（2026-09-20 复测）同一批 ~200 个测试目标仍写 **533 个 PDB / ~27 GB**（单份 ~125–150 MB）——**MSVC 链接器即使 `debuginfo = 0` 也会写公共符号表，PDB 压不到 0**，剩下的是行号/类型信息的减少。要栈符号时按次开：`CARGO_PROFILE_TEST_DEBUG=1 cargo test -p xberg --test <name>`、`CARGO_PROFILE_DEV_DEBUG=1 cargo build ...`。
  - profile 一变，cargo 的指纹目录也变，**旧 PDB 不会被自动回收**（它们只增不减）。清理按「Rust 构建优化规则」第 8 条的优先级：先删 `target/debug/**/*.pdb`（纯调试副产物，删掉不影响增量构建与测试正确性），最后才动 dev 热缓存。**不要用 `cargo clean --profile test`**：本仓库 dev 与 test 共用 `target/debug/`，2026-09-20 实测它的 dry-run 报「89361 files, 137.9 GiB」——等于 `cargo clean`，会把 dev 热缓存一并清掉（违反规则 1/2）。
  - `target/x86_64-pc-windows-msvc/`（release 产物）与 `target/package-models-x86_64-pc-windows-msvc/`（打包模型缓存，~200 MiB）属于第 8 条的②③档：删了下次打包要全量重下模型 + 全量 release 重编（30 分钟量级，且卡在联网阶段，2026-09-20 就撞上过：`target/package-cpu-*.csv` 只剩 ORT 一行标记就中断）。真要删，先确认缓存在别处有备份或接受这次冷编。
- CLI `--format json` 模式**不落盘图片**（只有 text/toon 模式调用 `write_extracted_images`）；JSON 里图片是内联字节数组（`result.images[].data`），需要自己解码写出。
- `--config-json` 只做顶层字段替换后整体反序列化 + 校验（`crates/xberg/src/core/config/merge.rs`），feature 未编译的字段会直接报错。
- heic 默认关闭（Windows 无 libheif 构建路径），本 fork **不要**开 `heic`；pdfium 后端同样不要（`pdf-pdfium-surface` 仅编 wrapper，还要另供 libpdfium）。
- 编译/打包统一用 fork feature 集 `formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`；需要改能力时先改 AGENTS.md 与 `scripts/publish/cli/package-cli-windows.ps1` 的 `$Features`，两边保持一致。默认 OCR 后端是 **PaddleOCR pp-ocrv6 tiny**（`PaddleOcrConfig::new` 的 `model_tier`）；Tesseract 仍编进二进制作后备，需要时用 `--ocr-backend tesseract` 或 `ocr.backend` 覆盖。
- **本 fork 不编译 layout-detection（2026-09-21 需求变更）**：RT-DETR（版面）+ TATR（表格结构）不再编入、不再随包、也不再被验收注入——图片输入只走 OCR，行位置由 `rendering/ocr_layout.rs` 的 ```text 网格围栏保留（放不下整行时回退平文本，绝不静默丢行）。给 CLI 传 `layout` 字段会直接报错（feature 未编译），所以 `_expectations.json` 的 `run` 段不要加 `layout_config`；`测试识别.png` 的表格数断言已按此撤销（见该文件 `_note_no_tables` 与 `run._note_layout_removed`）。
