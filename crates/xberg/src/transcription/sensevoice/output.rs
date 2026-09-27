//! 输出组装：固定术语映射、时间戳格式与 SV-06 Markdown 结构。
//!
//! 移植自 JchTools `optional/markdown-media-worker/src/main.rs` 的
//! `normalize_terms` / `timestamp` / `markdown`，字段顺序与措辞逐项保持。

use super::vad::TranscriptLine;

pub(crate) fn normalize_terms(text: &str) -> String {
    let mut terms = [
        ("扫描压缩", "scan compression"),
        ("扫描使能", "scan enable"),
        ("扫描链", "scan chain"),
        ("静态时序分析", "STA"),
        ("标准延时格式", "SDF"),
        ("标准测试接口语言", "STIL"),
        ("固定型故障", "stuck-at"),
        ("固定故障", "stuck-at"),
        ("转换故障", "transition fault"),
        ("故障覆盖率", "coverage"),
        ("扫描测试", "scan"),
        ("威格尔", "WGL"),
        ("迪弗蒂", "DFT"),
        ("迪弗特", "DFT"),
        ("艾特皮吉", "ATPG"),
        ("A T P G", "ATPG"),
        ("M B I S T", "MBIST"),
        ("L B I S T", "LBIST"),
        ("B I S T", "BIST"),
        ("S T A", "STA"),
        ("S D F", "SDF"),
        ("S T I L", "STIL"),
        ("V e r i l o g", "Verilog"),
        ("系統维罗格", "Verilog"),
        ("dft", "DFT"),
        ("atpg", "ATPG"),
        ("mbist", "MBIST"),
        ("lbist", "LBIST"),
        ("bist", "BIST"),
        ("verilog", "Verilog"),
        ("systemverilog", "SystemVerilog"),
        ("vcs", "VCS"),
        ("verdi", "Verdi"),
        ("primetime", "PrimeTime"),
        ("tessent", "Tessent"),
        ("innovus", "Innovus"),
        ("genus", "Genus"),
    ];
    // Match the legacy converter: longer phrases win before their shorter
    // components (for example `systemverilog` before `verilog`).
    terms.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    let mut normalized = text.to_string();
    for (from, to) in terms {
        normalized = normalized.replace(from, to);
    }
    normalized
}

pub(crate) fn timestamp(seconds: f32) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{hours:02}:{minutes:02}:{secs:02}.{millis:03}")
}

/// 「音频时长」一行的措辞：有音轨时为格式化时长，无音轨时为固定说明。
/// 与 JchTools `markdown()` 保持逐字一致（无音轨时该行写「无音频轨道」，
/// 不是 `00:00:00.000`）。
pub(crate) fn duration_line(has_audio: bool, duration_seconds: f32) -> String {
    if has_audio {
        format!("- 音频时长: {}", timestamp(duration_seconds))
    } else {
        "- 音频时长: 无音频轨道".to_string()
    }
}

/// 无音轨 / 无语音时「## 转录」下的固定说明行。
pub(crate) const NO_AUDIO_NOTE: &str = "（无音频轨道）";
pub(crate) const NO_SPEECH_NOTE: &str = "（未检测到语音）";

/// 结构化段（毫秒时间戳）的转录行文本：`[start --> end] text`，与
/// [`markdown`] 中逐段行的措辞一致（供文档元素与 JSON 消费方使用）。
pub(crate) fn segment_line_ms(start_ms: u32, end_ms: u32, text: &str) -> String {
    format!(
        "[{} --> {}] {}",
        timestamp(start_ms as f32 / 1000.0),
        timestamp(end_ms as f32 / 1000.0),
        text
    )
}

/// 组装 SV-06 Markdown：`# <文件名>`、音频时长行、语音片段行、`## 转录`
/// 与逐段 `[start --> end] text`。与 JchTools `markdown()` 逐字一致。
pub(crate) fn markdown(name: &str, duration: f32, lines: &[TranscriptLine], has_audio: bool) -> String {
    let mut out = vec![format!("# {name}"), String::new()];
    out.push(duration_line(has_audio, duration));
    out.push(format!("- 语音片段: {}", lines.len()));
    out.extend([String::new(), "## 转录".to_string(), String::new()]);
    if !has_audio {
        out.push(NO_AUDIO_NOTE.to_string());
    } else if lines.is_empty() {
        out.push(NO_SPEECH_NOTE.to_string());
    } else {
        for line in lines {
            out.push(format!(
                "[{} --> {}] {}",
                timestamp(line.start),
                timestamp(line.end),
                line.text
            ));
        }
    }
    out.push(String::new());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_legacy_format() {
        assert_eq!(timestamp(3661.5), "01:01:01.500");
        assert_eq!(timestamp(-1.0), "00:00:00.000");
    }

    #[test]
    fn terminology_replacements_are_fixed() {
        assert_eq!(
            normalize_terms("扫描链和静态时序分析，A T P G 与未知词"),
            "scan chain和STA，ATPG 与未知词"
        );
        assert_eq!(normalize_terms("systemverilog"), "SystemVerilog");
    }

    #[test]
    fn markdown_has_empty_audio_states() {
        assert!(markdown("silent.mp4", 0.0, &[], true).contains("（未检测到语音）"));
        assert!(markdown("video.mp4", 0.0, &[], false).contains("无音频轨道"));
    }

    #[test]
    fn markdown_structure_matches_sv06() {
        let lines = vec![TranscriptLine {
            start: 0.0,
            end: 1.5,
            text: "scan chain测试".to_string(),
        }];
        let md = markdown("sample.mp4", 61.25, &lines, true);
        let expected = "# sample.mp4\n\n\
                        - 音频时长: 00:01:01.250\n\
                        - 语音片段: 1\n\n\
                        ## 转录\n\n\
                        [00:00:00.000 --> 00:00:01.500] scan chain测试\n";
        assert_eq!(md, expected);
    }
}
