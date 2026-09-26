//! (fork) 独立性能日志：仅 `perf-tracing` feature 下编译（见 `main.rs` 的 `mod perf;`）。
//!
//! 性能 span 统一使用 target `"perf"`（`PERF_LOG_TARGET`；lib 侧 span 属性里写的是同一
//! 字面量——这是一份对外契约：改名/改义会让不同时期的耗时数据无法比较）。本模块构建的
//! fmt layer 只放行该 target，并写入独立滚动日志文件，不进 stderr、不改变业务日志的
//! 等级/格式/位置。`FmtSpan::CLOSE` 在每个 span 关闭时输出 `time.busy` / `time.idle`
//! 耗时字段，这就是性能日志的耗时来源（span 本身不额外记 elapsed 字段）。
//!
//! 日志目录默认 `logs/`（仓库 `.gitignore` 的 `**/logs/` 已覆盖，不会进 Git），可用环境
//! 变量 `XBERG_PERF_LOG_DIR` 指到别的目录。按天滚动，文件形如 `perf.log.2026-09-21`。

use tracing::Level;
use tracing::Subscriber;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

/// 性能 span 的统一 target；lib 侧 span 属性里的 `"perf"` 字面量必须与本值一致。
pub const PERF_LOG_TARGET: &str = "perf";

/// 性能日志目录的环境变量覆盖。
const PERF_LOG_DIR_ENV: &str = "XBERG_PERF_LOG_DIR";

/// 默认目录：`.gitignore` 已忽略 `**/logs/`，性能日志不会进 Git。
const DEFAULT_LOG_DIR: &str = "logs";

/// 建独立性能日志：按天滚动的文件 appender + 非阻塞 writer + 只放行 `perf` target 的
/// fmt layer（span 关闭事件带 `time.busy`/`time.idle` 耗时字段）。
///
/// 返回 `(WorkerGuard, layer)`。guard 是退出前 flush 后台队列的保证，必须绑定到存活至
/// 进程退出前的真实变量（不能写成 `let _ = …`，那会立即 drop 掉 flush 保证）；调用方
/// 在 `run_cli` 作用域持有它。目录创建失败时返回 `None`——性能日志缺失只提示、绝不
/// 打断业务流程。
pub fn init_perf_layer<S>() -> Option<(WorkerGuard, impl Layer<S>)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let dir = std::env::var_os(PERF_LOG_DIR_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_LOG_DIR));

    if let Err(error) = std::fs::create_dir_all(&dir) {
        // 此时尚未安装任何 subscriber，tracing 事件无人接收，只能直接写 stderr 提示。
        #[expect(clippy::print_stderr, reason = "perf log init runs before any subscriber exists")]
        {
            eprintln!("xberg: perf log disabled, cannot create {}: {error}", dir.display());
        }
        return None;
    }

    let (writer, guard) = tracing_appender::non_blocking(rolling::daily(&dir, "perf.log"));

    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .with_filter(Targets::new().with_target(PERF_LOG_TARGET, Level::TRACE));

    Some((guard, layer))
}
