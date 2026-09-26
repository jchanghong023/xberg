//! Self-reported peak resident-set-size (RSS) for the `xberg extract --format json` envelope.
//!
//! The benchmark harness (`tools/benchmark-harness`) compares memory usage across frameworks by
//! reading a `_peak_memory_bytes` field that every competitor's Python wrapper self-reports via
//! `resource.getrusage(resource.RUSAGE_SELF).ru_maxrss` — a kernel-tracked high-water mark that
//! cannot miss a transient allocation spike. The xberg CLI previously emitted no such field, so
//! the harness fell back entirely to its own `sysinfo`-based sampler (a 1-10ms polling loop, see
//! `tools/benchmark-harness/src/monitoring.rs`), which can miss allocations shorter-lived than the
//! sampling interval. That asymmetry biased the published memory comparison in xberg's favor: the
//! same class of transient spike was always visible to competitors and sometimes invisible for
//! xberg. This module closes the gap by reporting the same kernel-tracked `ru_maxrss` value the
//! competitors already report, under the same measurement method.

/// Pure conversion from a raw `ru_maxrss` value to bytes, parameterized explicitly by whether the
/// value came from a Linux `getrusage` call.
///
/// Kept separate from [`ru_maxrss_to_bytes`] (which pins `is_linux` to the actual build target via
/// `cfg!`) so both unit conventions below can be exercised by unit tests on any host platform,
/// without needing `#[cfg(target_os = ...)]` on the tests themselves.
///
/// Gated on `cfg(any(test, unix))` rather than plain `cfg(unix)`: the only non-test caller is
/// [`ru_maxrss_to_bytes`], itself only called from the `cfg(unix)` `peak_memory_bytes`, so on a
/// non-test, non-unix build (e.g. Windows release) this would otherwise be genuinely unused. The
/// `test` half of the predicate keeps it compiled for unit tests on every host platform, per the
/// doc comment above.
#[cfg(any(test, unix))]
fn convert_ru_maxrss(raw: i64, is_linux: bool) -> u64 {
    // Clamp before widening: `getrusage` should never report negative usage, but a negative value
    // has no meaningful byte count, so it saturates to `0` instead of wrapping to a huge `u64`.
    let raw = raw.max(0) as u64;
    if is_linux { raw.saturating_mul(1024) } else { raw }
}

/// Converts a raw `ru_maxrss` value (as read directly from `getrusage(2)`) to bytes.
///
/// # Platform units (the actual trap)
/// - **Linux**: `ru_maxrss` is reported in **kibibytes** — must be multiplied by 1024.
/// - **macOS / other BSD-derived libc**: `ru_maxrss` is reported in **bytes** already — must NOT
///   be scaled.
///
/// Mirrors the equivalent conversion in the benchmark harness's Python competitor wrappers (see
/// `tools/benchmark-harness/scripts/docling_extract.py::_get_peak_memory_bytes`), so both sides of
/// a memory comparison agree on units instead of one side silently being off by 1024x.
///
/// Gated on `cfg(any(test, unix))` for the same reason as [`convert_ru_maxrss`]: its only
/// non-test caller is the `cfg(unix)` `peak_memory_bytes`, so it is genuinely unused on a
/// non-test, non-unix (e.g. Windows release) build.
#[cfg(any(test, unix))]
pub fn ru_maxrss_to_bytes(raw: i64) -> u64 {
    convert_ru_maxrss(raw, cfg!(target_os = "linux"))
}

/// Returns this process's peak RSS in bytes via `getrusage(RUSAGE_SELF)`, or `None` if the
/// syscall failed (should not happen in practice on Unix).
///
/// The returned value covers the whole process's lifetime up to the call site, matching the
/// semantics of the competitors' `ru_maxrss` self-report described in the module docs.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn peak_memory_bytes() -> Option<u64> {
    // `rusage` is a C plain-old-data struct; zero-initializing it is always valid. `getrusage`
    // only ever populates its fields and returns a status code, which is checked below before the
    // (possibly still-zeroed) value is trusted.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if status != 0 {
        return None;
    }
    Some(ru_maxrss_to_bytes(usage.ru_maxrss as i64))
}

/// Windows arm of [`peak_memory_bytes`]: the kernel-tracked peak working set from
/// `GetProcessMemoryInfo` — the same whole-lifetime high-water-mark semantics as
/// `ru_maxrss`, keeping the benchmark-harness comparison symmetric on this fork's
/// only platform. (`ru_maxrss` counts resident pages; the peak working set is the
/// closest Windows equivalent.)
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn peak_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // `PROCESS_MEMORY_COUNTERS` is a C plain-old-data struct; zero-initializing it is
    // always valid, and the call's return code is checked before the values are trusted.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let process = unsafe { GetCurrentProcess() };
    let ok = unsafe {
        GetProcessMemoryInfo(
            process,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

/// Platforms with neither `getrusage` nor `GetProcessMemoryInfo`; report "unavailable"
/// rather than guessing.
#[cfg(not(any(unix, windows)))]
pub fn peak_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_ru_maxrss_scales_kibibytes_to_bytes_on_linux() {
        assert_eq!(convert_ru_maxrss(1024, true), 1_048_576);
    }

    #[test]
    fn convert_ru_maxrss_leaves_bytes_unscaled_on_macos() {
        assert_eq!(convert_ru_maxrss(1_048_576, false), 1_048_576);
    }

    #[test]
    fn convert_ru_maxrss_clamps_negative_values_to_zero_on_both_platforms() {
        assert_eq!(convert_ru_maxrss(-1, true), 0);
        assert_eq!(convert_ru_maxrss(-1, false), 0);
    }

    #[test]
    fn convert_ru_maxrss_handles_zero_identically_on_both_platforms() {
        assert_eq!(convert_ru_maxrss(0, true), 0);
        assert_eq!(convert_ru_maxrss(0, false), 0);
    }

    #[test]
    fn ru_maxrss_to_bytes_matches_the_current_host_platform_convention() {
        let expected = if cfg!(target_os = "linux") { 2048 } else { 2 };
        assert_eq!(ru_maxrss_to_bytes(2), expected);
    }

    #[test]
    fn peak_memory_bytes_reports_a_plausible_nonzero_value_on_unix() {
        // A running test process has already allocated stack/heap/binary pages, so a real
        // `getrusage` call must report something greater than zero.
        #[cfg(unix)]
        {
            let value = peak_memory_bytes().expect("getrusage must succeed on unix");
            assert!(
                value > 0,
                "peak RSS must be positive for a running process, got {value}"
            );
        }
    }
}
