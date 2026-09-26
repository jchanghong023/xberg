# AGENTS.md

## 项目概况与权威体系

- 本仓库由 AI Agent 开发与维护，质量必须由自动化验证保证，不能依赖用户人工读代码或人工回归。
- 本文件只维护开发规则、实现入口、命令与操作约束。固定需求目录为 [docs/requirements/](docs/requirements/README.md)，其中按功能边界维护一至多份权威需求文档；具体需求、规划和需求变化只写在该目录。根目录 `fork.md` 仅为兼容已有引用的迁移链接，不是权威副本。
- 需求或预期用户可见行为变化时，必须检查并同步对应需求文档；已有合适文档则更新，新独立功能域才新增。每条需求只有一个权威维护位置，跨域使用引用。目录或权威体系缺失时先补齐，再修改实现。
- 仅实现方式变化且需求不变时，不制造需求变更，不得改写需求来合理化缺陷；入口、命令或开发规则变化时同步本文件。保留已有有效规则、需求、规划和用户改动。

## 仓库性质与上游同步

- 本仓库是持续同步上游的 fork：origin = `https://github.com/jchanghong023/xberg.git`；upstream = `https://github.com/xberg-io/xberg.git`，跟踪 `upstream/main`。通常使用 `git merge upstream/main`；共同基线用 `git merge-base HEAD upstream/main` 核对。本地引用不代表已核验远程最新状态。
- 本地差异的唯一需求索引是 [需求目录](docs/requirements/README.md)。同步后必须逐项核对仍有效的本地需求，不能只检查 Git 冲突；不把上游未同步的变化、格式化或纯重构当作本地需求。
- 仅在已获授权的同步任务中操作：先保全本地改动和差异需求，优先可靠合并。相关冲突难以可靠解决时，可基于该冲突部分的上游当前实现，按需求目录重新实现仍有效的本地行为；不得覆盖唯一需求依据、丢弃无关本地改动或重置整个仓库。重建后须通过相应 UT/E2E 才能宣称同步完成；运行时机仍由用户决定，未获测试授权时报告“合并/实现已实施，待验证”。
- 开发与验证仅针对 Windows 11，不为其他平台适配或测试；产品边界见需求目录。
- `AGENTS.md` 必须跟踪入库；上游忽略它的 `.gitignore` 条目若被带回，须移除。保留本地产物忽略项（`.tmp/`、打包输出、转换 scratch、`.zcode/`、`**/logs/` 等），不提交临时产物。
- 上游生态资产不主动维护，合并时保留；新增格式时同步 README、templates、packages、plugin 等格式表/计数及 WMV/ASF 等能力说明，按最新事实重算，不能沿用旧计数。`.ai-rulez/` 与 plugin 的生成 bundle 使用项目固定版本 ai-rulez 重新生成，不手工制造不同步副本。

## 硬性约束（必须遵守）

- **总工作流（用户的固定需求，以后无需重新解释）**：日常开发改完代码 → 用户明确要求时跑 `fulltest.py`（本地编译的二进制，快速验证转换质量）；发布前用户明确要求时跑 `slowtest.py`（完整打包 → 打包版全量端到端测试 → 质量报告），是否发布由用户根据报告决定。
- **严禁私自跑费时操作**：未经用户明确点名要求，不得运行任何编译（`cargo build`/`clippy`/`test` 等一切编译类命令，**唯一例外见下一条的 `cargo check`**）、测试（`fulltest.py`、`slowtest.py`、端到端测试）或打包（`package-cli-windows.ps1`）。何时测试由用户自己决定。
- **agent 可主动跑的类型检查（唯一编译例外）**：改完 Rust 代码后允许（并建议）跑
  `cargo check -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`
  ——分钟级、只做类型/借用检查、不产出二进制不跑测试，用于把编译错误挡在交付前（fork 推送到 main 无任何自动编译 CI：上游 `ci-rust.yaml` 的 job 都有 `github.repository == 'xberg-io/xberg'` 守卫，在本 fork 全部 skip）。其余编译类命令仍需用户点名。
- **fastcheck（agent 可自主跑，≤60 秒）**：`python testgate.py fastcheck`；三个 crate 的 fmt、脚本语法、PS1 解析和判定器自测，内容见下方操作表。超时杀进程树并判失败。仅快速反馈，不代表完整验证；分钟级 `cargo check` 按上条单独授权。
- **交付 Rust 改动时必须报告验证状态**：跑了 `cargo check` 就报结果；没跑就必须显式标注「未编译验证」并列出静态审查覆盖点（读过哪些调用方、检查过哪些类型/feature 门控），由用户决定何时编译。禁止在未验证时暗示"已修复"。
- **验收资产修改必须显式披露**：修改 `fulltest.py` 的判定逻辑/阈值/问题码、`_expectations.json` 的任何键、或重设基线时，必须在回复中**单独列出改动点并给出实测依据**（哪个文件哪次实测值支持这次调整），不允许夹在引擎改动里静默带过。fulltest 报告与基线已记录金标准 sha256 与代码 commit，资产被动过是可对账的。
- **报告闭环（声称修复前必须核对）**：用户跑完 fulltest 后，后续 agent 会话在声称任何修复生效前，必须先读 `D:\测试转markdown转换效果\测试文档_md_fulltest\_quality-report.json`，逐码核对「与基线对比」的新增/已修复/恶化与自己的声明一致；不一致不得声称已修复，只能报告"已实施、待用户验证"。
- **临时脚本 / 临时目录 / 临时文件只准放 `./.tmp`**：仓库内的一次性脚本、临时目录、临时文件一律建在仓库根目录的 `.tmp\`（不存在先 `mkdir -p .tmp`），不得散落在仓库根、`scratch*` 或其他任何目录——根目录只保留仓库资产，避免 `git status` 噪音和误提交；`.tmp/` 已在 `.gitignore` 中（不入库）。验证完自行清理临时产物。
- **`fulltest.py` 仅在明确要求时运行**：本地开发版转换验收，不需要打包。默认跳过音视频，`--deep` 才包含；不得回退打包版。命令、语料和报告路径见下方操作说明。
- **`slowtest.py` 同样仅在明确要求时运行**：完整打包后对解压版运行 `fulltest.py --keep-going --deep`。用户根据报告决定是否发布；运行该脚本本身不授权远程发布。

## 测试要求（对 agent 的硬性要求）

- **功能性开发和功能性修改必须有自动化验证**：UT 验证局部逻辑（源码内 `#[cfg(test)]` 与 crate 的 `tests/`）；E2E 验证从真实公开入口到可观察结果的完整链路——本 fork 的 E2E 就是 `fulltest.py`（真实 `xberg.exe extract` → 落盘 Markdown/图片 + 逐文件金标准断言 + 对抗语料失败路径）与 `slowtest.py`（打包版跑同一套）。跨模块交互按需增加集成测试。
- **UT、编译、`cargo check`、clippy、局部模拟都不能替代 E2E**：它们只证明类型或局部逻辑，不证明转换质量；桩和模拟可以补充测试，但绕过的真实边界（真实文件、真实 OCR / 转写模型、真实 CLI 入口）必须说明，不能当作端到端结论。
- **测试要对需求与验收条件负责**：覆盖核心成功路径与相关关键失败路径（失败路径＝`_adversarial/` 那套语义：损坏 / 截断 / 空输入必须优雅失败），不得只复述实现或只断言"没 panic"。
- **验证状态必须如实区分**：已实现 / 验证通过 / 验证失败 / 未验证（写明未验证范围与原因）。环境、依赖或权限不足时说明未验证部分，不能用"已修复"描述未验证的改动（与「硬性约束」的交付要求一致）。
- **Windows 上可用的 UT 入口**（均属编译类命令，只在用户点名时运行）：`cargo test -p xberg`、`cargo test -p xberg-cli`，或 `task test:quick`（= `cargo test --locked --lib --workspace --exclude xberg-php --exclude xberg-node --exclude xberg-wasm`，只跑 lib 单测）。`task test` / `task test:ci` 在 `.task/languages/rust.yml` 里只声明了 linux / darwin 平台，在 Windows 上不执行。
- **test_documents 语料二进制不进 git**：`test_documents` 子模块只含源码与 `corpus.lock.json`（693 个对象的 sha256/大小清单），.pdf/.docx/.pst 等二进制须先从公开 bucket 拉取到工作树（约 618 MB，一次性）：`cd test_documents && python scripts/fetch_corpus.py`（已正确文件只做哈希校验；语料路径被子模块 .gitignore 忽略，不会弄脏父仓）。缺语料时 cargo test 会出现大量 "fixture not found" 失败；testgate fulltest 门的 `test-documents-corpus` 阶段会先做存在性预检并给出该修复命令。
- **现状与缺口（如实记录）**：本 fork 没有自动跑 Rust 测试的 CI（上游 `ci-rust.yaml` 等编译 / 测试 workflow 被仓库守卫 skip，无守卫的 `ci-lint` 只跑治理 / 文档 / 脚本类检查，不编译 Rust、不跑 Rust 测试）；fork 新增模块多数自带 UT（`extraction/visio.rs`、`rendering/ocr_layout.rs`、`extraction/markdown_utils.rs`、`extraction/excel/images.rs`、`crates/xberg-windows-metafile`），但部分模块（如 `transcription/wmf.rs`）没有 UT，只有 fulltest 的 E2E 覆盖；上游 `e2e/` + `fixtures/` 的语言绑定 e2e 在本 fork 不运行、不作为验收依据。
- 复用已有有效测试，不为每处修改机械新增用例；纯文档等非功能变更按实际影响验证，不运行无关完整测试。
- 当前转换脚本只覆盖真实 CLI 抽取，不能据此声称 HTTP 服务、性能日志或每条解码回退路径完成 E2E；覆盖盲区及未满足需求见 [DELIVERY.md](docs/requirements/DELIVERY.md) 和对应需求文档。修改相关能力时补齐必要验证，未经授权不执行费时测试。
- fork 行为改变上游测试前提时，依据需求调整前提/断言并保留有效覆盖，不删除测试来掩盖失败；与 fork 差异无关的上游测试语义保留。

## 三级测试门操作

所有命令默认在仓库根目录、Windows PowerShell 执行，需要对应 Python/Rust/PowerShell 工具及依赖。以下入口经源码核对；本次文档更新未运行这些命令，列出入口不代表当前环境已通过。

| 入口 | 授权与操作 |
| --- | --- |
| `python testgate.py fastcheck` | agent 可自主执行，60 秒总预算：三 Python 脚本语法、`fulltest.py --selftest`、打包/offline-smoke/DLL 闭包 PS1 解析、xberg/xberg-cli/xberg-windows-metafile 三 crate `cargo fmt --check`。 |
| `python testgate.py fulltest` | 每次须用户明确授权。三 crate fmt → fork feature 集 build-cli → `fulltest.py --keep-going` → 语料存在性 → 三 crate test → 三 crate clippy `-D warnings`。阶段依赖与独立结果以 `FULLTEST_STAGES` 为准；任一失败即门失败。 |
| `python testgate.py slowtest` | 每次须用户明确授权。先完整 fulltest 门；本地失败则后续跳过，成功再运行 slowtest.py。远程阶段默认未授权，仅能报告本地部分。 |

- xberg 的 test/clippy 用 `FEATURES_LIB = formats-no-heic,analysis,ocr,paddle-ocr,transcription,layout-detection,api`，保留上游 layout 专属测试与配置校验；CLI 用标准 fork 集且 `--no-default-features`。测试 feature 集不等于出厂能力。test 阶段 `CARGO_BUILD_JOBS=8`，防止 Windows 页面文件耗尽（os error 1455），不缩减覆盖。
- 远程发布有真实公开副作用：只有用户明确授权发布目标并显式加 `--with-release-ci` 才能触发；要求工作区干净、HEAD 已推到 origin。workflow 为 `.github/workflows/build-windows-cli.yml`，需轮询最终结果。不得将未授权跳过的远程阶段描述为通过，不执行 WSL/跨平台验证。
- 消歧：点名 `fulltest.py` / `slowtest.py`＝仅脚本；点名“fulltest / slowtest 门”“完整本地验证”“testgate xxx”＝对应门；口语“跑 fulltest”默认指 fulltest.py。
- 门的跳过状态、超时和质量报告语义见 [DELIVERY.md](docs/requirements/DELIVERY.md)，不得把门退出成功等同于转换质量全绿。

## 转换验收资产与维护操作

质量判据、报告字段、金标准校验、失败路径与盲区统一见 [DELIVERY.md](docs/requirements/DELIVERY.md)。维护时使用以下实际入口，不降低该文档的验收标准。

| 对象 | 位置与用途 |
| --- | --- |
| 标准语料 | `D:\测试转markdown转换效果\测试文档`；`_adversarial/` 子目录不进主队列，主队列后追加。现有失败样例为 empty.pdf、truncated.pdf、corrupt.docx。 |
| 标准报告 | `D:\测试转markdown转换效果\测试文档_md_fulltest\_quality-report.md` / `.json`。 |
| 逐文件金标准 | `D:\测试转markdown转换效果\_expectations.json`，仓外不入 Git；合法键以 `KNOWN_FILE_KEYS` 为准。 |
| 回归基线 | `D:\测试转markdown转换效果\_quality-baseline.json`，仓外不入 Git。 |
| 打包版报告副本 | `target/slowtest-report-<时间戳>.md` / `.json`。 |

- 本地验收命令：`python fulltest.py` 默认遇 FAIL 即止；`--keep-going` 跑完；`--deep` 包含音视频，音视频默认超时 1800 秒；`--strict` 将 WARN 也算作失败。只需开发版 exe，不需打包；`--cli` 可明确本地 exe。
- `python fulltest.py --selftest` 仅为判定器自测，无需 CLI/语料，agent 可主动运行。`--no-expectations` / `--expectations <path>` 为降级或替代金标准跑法，须配 `--out` 独立目录，不覆盖标准报告，也不放宽运行授权。
- 改判定或阈值后必须在获准运行验收时用 `--save-baseline` 重设基线，避免假阳修正被计作引擎修复；尚未完成时明确记录待重设。已有 FAIL、提前终止或缺金标准会被护栏拒绝，只有用户确认红项可接受后才加 `--force`。运行验收本身不等于授权接受红项。
- 阈值先调常量/期望，不删除检查。新问题码加入 `ISSUE_META` 并给严重度；依赖金标准的码同时加入 `EXP_GATED_CODES`；合法键变化同步 `KNOWN_FILE_KEYS`。
- 修改 `_golden_token_hit` / `save_markdown` / `_validate_expectations` / `compare_baseline` / `baseline_block_reason` / `classify_adversarial_failure` / `finalize_adversarial_success` 时，同步新增/调整 `run_selftest` 并运行 `python fulltest.py --selftest`。
- 改阈值前按金标准 `_note_*` 核对推导依据：`min_fenced_cjk` 等基于 chi_sim 实测；tessent 表格量级取源文 `Table N-M.` 题注 54 个，而非含页眉框的 `find_tables` 468 个。无实测依据不调整；验收资产变更的单独披露要求见硬性约束。
- 标准构建不支持 layout 配置，不给 `run` 注入 `layout_config`；模型/输出要求见 OCR 与 DELIVERY 文档。不要把检查器盲区转为要求用户日常人工回归。

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

- `docs/requirements/`——按功能域维护的 fork 差异需求（从 README 索引定位）；`fulltest.py` / `slowtest.py`——fork 验收标准（见上方操作说明）；`testgate.py`——三级测试门入口（见「三级测试门」）。
- `scripts/`——`publish/cli/package-cli-windows.ps1`（打包唯一入口，也是 fork feature 集的来源之一）、`publish/cli/offline-smoke.ps1`、`ci/`（PE/DLL 闭包校验）。
- `.github/workflows/build-windows-cli.yml`——fork 自有的 Windows 打包 CI，仅手动 `workflow_dispatch` 触发。上游编译/测试类 workflow（ci-rust、ci-e2e 等）带仓库守卫在本 fork 全 skip；push 命中路径会自动跑的只有无守卫的 ci-lint / ci-docs / ci-scripts（其余无守卫 workflow 是 workflow_dispatch / release / issue-PR 事件触发，不随 push 跑）。
- `docs-site/`（Astro + Starlight 文档）、`e2e/` + `fixtures/`、`.ai-rulez/`（ai-rulez 管理的 AI 规则/技能，改规则后需用固定版本的 ai-rulez 重新生成 bundle）。**`e2e/` 与 `fixtures/` 是上游的语言绑定 e2e 资产**（csharp/dart/go/...），本 fork 的验收不走它们（走 fulltest.py），日常不要为它们做适配；merge 上游带进来的改动原样保留即可。
- `packages/` / `integrations/` / `plugin/` / `charts/` / `templates/`——上游生态资产（语言包、第三方集成、Claude 插件、Helm chart、README 生成模板），fork 不主动维护，merge 时原样保留（引擎新增格式时的计数同步除外，见本文件「仓库性质与上游同步」）。
- CLI 运行入口：`target\debug\xberg.exe extract <输入>`、`batch`；HTTP 入口 `target\debug\xberg.exe serve`，路由在 `crates/xberg/src/api/router.rs`（如 `/extract`、`/health`）。调用参数先按源码/对应二进制帮助核对，运行服务或转换仍应属于用户任务范围。
- OOXML 内嵌对象入口目前为 `extraction/ooxml_embedded/mod.rs`；测试可能外移到同名目录的 `tests.rs`，查找时同时检查 inline tests 与外置测试模块。
- workspace member 变化时检查上游 `docker/` 与 `.dockerignore` 的构建上下文，保留 `xberg-windows-metafile` 的对应登记；这属于同步已有资产，不扩展本 fork 的支持平台。

## 构建、打包与调试入口

除已列明的 `cargo check` 与 fastcheck 例外外，编译、测试、clippy、打包均仅在用户明确点名时执行。以下命令在仓库根执行，工具链按仓库配置，保持 feature/profile/target 稳定。

### 开发构建

```powershell
cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api
```

产物 `target\debug\xberg.exe`。标准能力的权威定义在 [DELIVERY.md](docs/requirements/DELIVERY.md)；命令是操作入口。不要使用不带 feature 限定的 `cargo build -p xberg-cli`（default 较重），也不要启用 `all`。`cargo check` 使用上面相同参数，仅将 build 换为 check。

### 可选性能构建

```powershell
cargo build -p xberg-cli --no-default-features --features formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api,perf-tracing
```

- 性能日志契约见 [PERFORMANCE.md](docs/requirements/PERFORMANCE.md)，实现入口为 `crates/xberg-cli/src/perf.rs`。标准集不改；仅为获准的性能验证按需启用。
- `xberg/perf-tracing` 是 marker；CLI feature 引入 `tracing-appender` 并转发该 marker。`WorkerGuard` 必须由 `run_cli` 持有至退出，不能写成 `let _ =`。
- 标准 CLI 已经由 `core-cli → xberg/cli → services → otel` 启用 OTel，perf 不能用 `not(feature = "otel")` 守卫。普通函数用 feature cfg_attr；`extract_bytes`/`run_pipeline` 与已有 instrument 嵌套；`format_extract` 同调用点使用互斥分支且 perf 优先。分析耗时按需求文档的 busy/idle 口径。

### 打包与发布前验证

```powershell
pwsh -NoProfile -File scripts/publish/cli/package-cli-windows.ps1
```

- 需要 PowerShell ≥7.4；本地 Jobs 默认 min(30, 逻辑核数)，CI 默认 6，可用 `-Jobs N` 覆盖。CI 调用同一脚本，不能另建不同打包路径。
- 输出 `xberg-cli-x86_64-pc-windows-msvc/` 和同名 zip。模型组成、离线正负 smoke 和 DLL 验收要求见 DELIVERY.md；实现入口为 `$Features`、`$RequiredModels`、`$TranscriptionFiles`，Paddle 清单从 `xberg.exe cache manifest` 取大小/sha256，Whisper 使用固定清单；模型正则必须恰好匹配一条。
- `target/package-models-<target>` 是跨次复用缓存，不随临时清理删除。包内执行时 `HF_HUB_CACHE` 指向 `<包>/models`，PATH 前置包目录；不要用会追加 `/hub` 的 `HF_HOME` 代替。fulltest.py 已处理这两项。
- `python slowtest.py` 执行打包及打包版 E2E，`--skip-package` 可复用已有 zip，`--keep-tmp` 保留解压目录；脚本固定使用 `target/slowtest-tmp/`，成功即清理，否则保留。该脚本管理的目录按其固定入口处理，agent 自建一次性文件仍必须放 `.tmp/`。
- Rust UT、`task test:quick`、`cargo clippy` 及 task runner 的其他编译入口仍须明确授权。`Taskfile.yml` / `.task/languages/rust.yml` 是入口依据，Windows 上不使用只声明 linux/darwin 的 `task test` / `task test:ci`。

### 已知构建问题与证据边界

- 历史 Windows 冷测试生成大量 PDB：2026-09-20 记录 debug=1 时约 88 GB PDB + 38 GB exe，debug=0 仍约 27 GB PDB；profile 变更不会自动清旧产物。这是保留缓存和减少 debug 信息的依据，不是本次磁盘读数。
- dev/test 共用 `target/debug/`；`cargo clean --profile test` 历史 dry-run 会连 dev 缓存一起清，禁止当定点清理使用。删除旧 release 或模型缓存会导致下次重新构建/下载，先核对所属 profile/target、恢复方式及授权，遵循下方缓存纪律。
- 遇 os error 32 文件锁，先核实是否有其他 cargo/rustc 或安全软件占用；历史同命令重试曾恢复，不能据此假定当前原因或主动修改安全软件排除项。测试 os error 1455 先检查既定的 test jobs=8。
- `cargo check` 不执行 clippy lint，不能报告 lint 已通过。旧门禁/发布记录可从 Git 历史追溯，不作为当前工作树通过证据。
- 修改能力前按需求目录核对，注意 CLI wire format 与内容渲染格式不同；JSON 图片输出、配置替换和未编译字段的边界见 FORK.md。

### 构建与缓存纪律（Rust 构建优化规则，用户强制，2026-09-20）

**缓存纪律（改动 / 清理 / 提速前先过这四条）**：① 禁止无理由 `cargo clean`；② 禁止删除当前有效的 `target` 缓存（要删必须能指出它属于哪个废弃 profile / feature 集 / target 三元组，并按第 8 条的优先级定点删除、留痕）；③ 保持构建配置与 target 目录稳定——不为提速切换 profile / `--target-dir` / RUSTFLAGS，也不重建缓存来「让 fastcheck 更干净」；④ 最终打包 / 发布 profile 的构建只出现在 `fulltest` / `slowtest` 里（打包属这两级语义，`fastcheck` 只做 fmt / 解析 / 判定器自测这类秒级检查）。

以下 9 条为用户定下的构建纪律，改 profile / 清产物 / 调 features 前先对照；每条都指向本仓库的具体落点。

1. **必须用 incremental 编译，禁止无理由 `cargo clean`**：`incremental = true` 在 `.cargo/config.toml`（alef 生成，DO NOT EDIT）。要清东西必须能说出「它属于哪个废弃 profile/feature/target」。
2. **优先保留当前有效 target 缓存，避免冷编译**：删除前先确认产物是否还是当前 profile/feature 的（`target/debug/deps` 里的 rlib/rmeta、`target/debug/build`、`target/debug/incremental` 是热缓存，默认不动）。
3. **dev 构建减少 debug/PDB，优先 `debug = 0`**：`Cargo.toml` 的 `[profile.dev] debug = 0`，`[profile.test] debug = 0`（test 显式钉死，防继承回潮）。要符号按次开：`CARGO_PROFILE_DEV_DEBUG=1` / `CARGO_PROFILE_TEST_DEBUG=1`。
4. **保持 toolchain / features / RUSTFLAGS / target / profile 稳定**：标准 fork feature 集见上方编译命令（能力要求在 DELIVERY.md）；改能力须先更新需求，再同步本文件、testgate.py 和打包脚本 `$Features`；不要临时加 `RUSTFLAGS` 或切换 `--target` 三元组（会另生成一整套 `target/<triple>/` 缓存）。
5. **Windows 链接优先 LLD**：`.cargo/config.toml [target.x86_64-pc-windows-msvc] linker = "rust-lld"`（本机 + 交叉目标都走这份配置；回退就删该段）。
6. **开发阶段禁止 LTO 等昂贵 release 优化**：日常验证/编译一律 dev profile，不要用 `--release` 跑 fulltest。
7. **LTO / strip / codegen-units / opt-level=3 只用于最终打包**：只在 `[profile.release]`（当前 `lto = false`、`codegen-units = 256`、`opt-level = 3`、`strip = true`），服务对象是 `package-cli-windows.ps1` 与 slowtest。
8. **磁盘不足按下述优先级删（先废弃、后缓存）**：① 废弃 profile 的产物——`target/debug/**/*.pdb`（PDB 是纯调试副产物，删了不影响增量构建正确性；注意本仓库 dev 与 test 共用 `target/debug/`，`cargo clean --profile test` 实测等于 `cargo clean`，禁用）；② 不再使用的 `target/<triple>/`（旧 feature 集或旧三元组）；③ 旧 release 产物；**最后**才考虑 dev 热缓存。`cargo clean` 无参数＝清空一切，视为最后手段。
9. **定位慢点用 `cargo build --timings`**（产物在 `target/cargo-timings/`）：先看慢 crate / `build.rs` / proc-macro / 最终链接，再决定是否调 `-Jobs`、拆 target 或换 profile，不要凭感觉加并行度。
