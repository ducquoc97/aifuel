//! The `rtk` optimizer: structural compression for command and tool
//! output, ported from RTK's (Rust Token Killer) filter strategies.
//!
//! The pipeline is deterministic and never guesses at content: a detected
//! [`PayloadKind`] picks the transform, every transform preserves failure
//! signals (errors, failures, changed lines) verbatim, and the shared
//! `never_worse` guard returns the original when compression does not win.

use super::detect::PayloadKind;
use std::collections::BTreeMap;

/// Line-length truncation bounds: `standard` keeps generous context,
/// `ultra` trades context for bytes.
const TRUNCATE_STANDARD: usize = 240;
const TRUNCATE_ULTRA: usize = 120;
/// Total line caps per level - a cap keeps the head, never drops errors.
const CAP_STANDARD: usize = 200;
const CAP_ULTRA: usize = 80;

/// The aggressiveness the plan configured for this engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RtkLevel {
    /// Engine disabled.
    #[default]
    Off,
    /// Noise strip, dedupe, truncation, line caps, and the high-signal
    /// transforms (test collapse, diff context trim).
    Standard,
    /// Standard plus lossy structure: error-only logs, per-file search
    /// grouping, homogeneous JSON array elision, tighter caps.
    Ultra,
}

impl RtkLevel {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "off" => Self::Off,
            "standard" => Self::Standard,
            "ultra" => Self::Ultra,
            _ => return None,
        })
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Standard => "standard",
            Self::Ultra => "ultra",
        }
    }
}

/// Compress `input` as `kind` at `level`. Kind detection happens once in
/// the plan pipeline; `Off` and `Text` are identities.
pub fn compress(input: &str, kind: PayloadKind, level: RtkLevel) -> String {
    if level == RtkLevel::Off {
        return input.to_owned();
    }
    let (truncate_at, cap) = match level {
        RtkLevel::Off => unreachable!(),
        RtkLevel::Standard => (TRUNCATE_STANDARD, CAP_STANDARD),
        RtkLevel::Ultra => (TRUNCATE_ULTRA, CAP_ULTRA),
    };
    let stripped = strip_ansi(input);
    let text = match kind {
        PayloadKind::TestOutput => compress_tests(&stripped),
        PayloadKind::Diff => compress_diff(&stripped, level),
        PayloadKind::Log | PayloadKind::Terminal => compress_log(&stripped, level),
        PayloadKind::SearchResult if level == RtkLevel::Ultra => compress_search(&stripped),
        PayloadKind::Json if level == RtkLevel::Ultra => compress_json(&stripped),
        _ => compress_lines(&stripped, cap),
    };
    let text = dedupe_normalized(&text);
    truncate_and_cap(&text, truncate_at, cap)
}

/// `filtered`, or `raw` when filtering failed to shrink - the
/// never-worse guard both source projects enforce.
fn never_worse<'a>(raw: &'a str, filtered: String) -> String {
    if filtered.len() >= raw.len() {
        raw.to_owned()
    } else {
        filtered
    }
}

/// Apply the guard at the plan boundary too: `compress` internals are
/// free to rearrange, but the caller-visible result must never grow.
pub fn guarded(raw: &str, filtered: String) -> String {
    never_worse(raw, filtered)
}

/// CSI/OSC ANSI escapes, replaced inline so line structure survives.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip ESC [ ... final-byte and ESC ] ... BEL/ST sequences.
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (prev == '\u{1b}' && c == '\\') {
                            break;
                        }
                        prev = c;
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Lines that exist only to animate progress: percentage redraws,
/// spinners, and download chatter. The phrase list stays narrow on
/// purpose - a broad list eats real output.
fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let progress = trimmed.ends_with('%')
        && trimmed
            .split_whitespace()
            .last()
            .is_some_and(|tail| tail.trim_end_matches('%').parse::<f64>().is_ok());
    let spinner = trimmed.chars().count() <= 200
        && ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']
            .iter()
            .any(|spin| trimmed.contains(*spin));
    let download = trimmed.starts_with("Downloading ") || trimmed.starts_with("download ");
    progress || spinner || download
}

/// Dedupe lines that differ only in volatile tokens (timestamps, UUIDs,
/// hex, long numbers): the first occurrence stands for the run and a
/// `×N` count preserves the repetition fact. Ported from RTK's
/// `analyze_logs` normalization.
fn dedupe_normalized(input: &str) -> String {
    let mut seen: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in input.lines() {
        let key = normalize(line);
        let entry = seen
            .entry(key.clone())
            .or_insert_with(|| (0, line.to_owned()));
        if entry.0 == 0 {
            order.push(key);
        }
        entry.0 += 1;
    }
    let mut out = String::new();
    for key in order {
        let (count, original) = &seen[&key];
        if *count > 1 {
            out.push_str(&format!("×{count} {original}\n"));
        } else {
            out.push_str(original);
            out.push('\n');
        }
    }
    out
}

/// Normalize a line for dedupe, ported from RTK's `normalize_log_line`:
/// a leading ISO timestamp drops entirely, then UUIDs, `0x` hex, paths,
/// and 4+ digit numbers collapse to placeholders so two runs of the
/// same event compare equal.
fn normalize(line: &str) -> String {
    let line = strip_leading_timestamp(line);
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = uuid_end(&chars, i) {
            out.push_str("<UUID>");
            i = end;
        } else if chars[i] == '0'
            && chars.get(i + 1).is_some_and(|c| *c == 'x' || *c == 'X')
            && chars.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit())
        {
            i += 2;
            while chars.get(i).is_some_and(|c| c.is_ascii_hexdigit()) {
                i += 1;
            }
            out.push_str("<HEX>");
        } else if chars[i].is_ascii_digit() {
            let start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            if i - start >= 4 {
                out.push('N');
            } else {
                out.extend(chars[start..i].iter());
            }
        } else if chars[i] == '/'
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && chars
                .get(i + 1)
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '/' | '_'))
        {
            while chars
                .get(i)
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '/' | '_'))
            {
                i += 1;
            }
            out.push_str("<PATH>");
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out.trim().to_owned()
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.fff]` (or `/`-separated date) at line start,
/// plus trailing whitespace - the timestamp span RTK erases.
fn strip_leading_timestamp(line: &str) -> &str {
    let chars: Vec<char> = line.chars().collect();
    let digit = |at: usize| chars.get(at).is_some_and(|c| c.is_ascii_digit());
    let sep = |at: usize| chars.get(at).is_some_and(|c| matches!(c, '-' | '/'));
    let stamped = digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && sep(4)
        && digit(5)
        && digit(6)
        && sep(7)
        && digit(8)
        && digit(9)
        && chars.get(10).is_some_and(|c| matches!(c, 'T' | ' '))
        && digit(11)
        && digit(12)
        && chars.get(13).is_some_and(|c| *c == ':')
        && digit(14)
        && digit(15)
        && chars.get(16).is_some_and(|c| *c == ':')
        && digit(17)
        && digit(18);
    if !stamped {
        return line;
    }
    let mut end = 19;
    // Optional fractional seconds: `.123` or `,123`.
    if chars.get(end).is_some_and(|c| matches!(c, '.' | ',' | ':'))
        && chars.get(end + 1).is_some_and(|c| c.is_ascii_digit())
    {
        end += 1;
        while chars.get(end).is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
    }
    while chars.get(end).is_some_and(|c| c.is_whitespace()) {
        end += 1;
    }
    &line[chars[..end].iter().map(|c| c.len_utf8()).sum::<usize>()..]
}

/// End index when `chars[i..]` opens a UUID (`8-4-4-4-12` hex groups).
fn uuid_end(chars: &[char], i: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = i;
    for (index, len) in GROUPS.iter().enumerate() {
        if index > 0 {
            if chars.get(at) != Some(&'-') {
                return None;
            }
            at += 1;
        }
        for _ in 0..*len {
            if !chars.get(at).is_some_and(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            at += 1;
        }
    }
    // A trailing hex char means this is a longer token, not a UUID.
    if chars.get(at).is_some_and(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(at)
}

/// The plain-line transform for kinds with no dedicated pass: cap lines.
fn compress_lines(input: &str, cap: usize) -> String {
    let total = input.lines().count();
    if total <= cap {
        return input.to_owned();
    }
    let kept: Vec<&str> = input.lines().take(cap - 1).collect();
    format!("{}\n… {} lines elided …", kept.join("\n"), total - cap + 1)
}

/// Test-runner output: failing detail stands verbatim, passing tests
/// collapse to a count - RTK's flagship `cargo test`/`pytest` behavior.
fn compress_tests(input: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut passed = 0usize;
    for line in input.lines() {
        let trimmed = line.trim_end();
        let passing = (trimmed.starts_with("test ") || trimmed.starts_with("running "))
            && (trimmed.ends_with(" ok") || trimmed.ends_with("... ok"))
            || trimmed.starts_with("ok  \t");
        if passing {
            passed += 1;
        } else {
            kept.push(line.to_owned());
        }
    }
    if passed > 0 {
        kept.push(format!("… {passed} tests passed …"));
    }
    kept.join("\n")
}

/// Diff: every `+`/`-` line is verbatim; long runs of unchanged context
/// collapse with an elision marker (1 line kept as boundary at `ultra`,
/// 3 at `standard`). Headers and hunk markers always survive.
fn compress_diff(input: &str, level: RtkLevel) -> String {
    let context_budget = if level == RtkLevel::Ultra { 1 } else { 3 };
    let mut out: Vec<String> = Vec::new();
    let mut context_run = 0usize;
    for line in input.lines() {
        let is_header = line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("@@ ");
        let is_change = !is_header && (line.starts_with('+') || line.starts_with('-'));
        if is_header || is_change {
            if context_run > context_budget {
                out.push(format!(
                    "… {} context lines …",
                    context_run - context_budget
                ));
            }
            context_run = 0;
            out.push(line.to_owned());
        } else {
            context_run += 1;
            if context_run <= context_budget {
                out.push(line.to_owned());
            }
        }
    }
    if context_run > context_budget {
        out.push(format!(
            "… {} context lines …",
            context_run - context_budget
        ));
    }
    out.join("\n")
}

/// Logs: `standard` strips progress noise (dedupe runs in the shared
/// post-pass); `ultra` additionally keeps ERROR/WARN/FATAL plus the
/// first and last informational lines, eliding the middle with a marker.
fn compress_log(input: &str, level: RtkLevel) -> String {
    let cleaned = input
        .lines()
        .filter(|line| !is_noise(line))
        .collect::<Vec<_>>()
        .join("\n");
    if level != RtkLevel::Ultra {
        return cleaned;
    }
    let lines: Vec<&str> = cleaned.lines().collect();
    if lines.len() <= 12 {
        return cleaned;
    }
    let is_signal = |line: &&str| {
        let lower = line.to_ascii_lowercase();
        [
            "error", "fatal", "panic", "crit", "warn", "severe", "alert", "emerg",
        ]
        .iter()
        .any(|word| lower.contains(word))
    };
    let mut out: Vec<String> = Vec::new();
    let mut elided = 0usize;
    let head = 2usize.min(lines.len());
    let tail = 2usize.min(lines.len() - head);
    for (index, line) in lines.iter().enumerate() {
        if index < head || index >= lines.len() - tail || is_signal(line) {
            if elided > 0 {
                out.push(format!("… {elided} lines elided …"));
                elided = 0;
            }
            out.push((*line).to_owned());
        } else {
            elided += 1;
        }
    }
    if elided > 0 {
        out.push(format!("… {elided} lines elided …"));
    }
    out.join("\n")
}

/// Search results at `ultra`: group hits per file, show the first two
/// hits each plus the remaining count - the RTK `grep` grouping shape.
fn compress_search(input: &str) -> String {
    let mut per_file: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut other: Vec<&str> = Vec::new();
    for line in input.lines() {
        match line.splitn(3, ':').collect::<Vec<_>>().as_slice() {
            [path, no, _] if !path.is_empty() && no.chars().all(|c| c.is_ascii_digit()) => {
                per_file.entry((*path).to_owned()).or_default().push(line);
            }
            _ => other.push(line),
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (file, hits) in &per_file {
        out.push(format!("{file}: {} matches", hits.len()));
        out.extend(hits.iter().take(2).map(|line| format!("  {line}")));
        if hits.len() > 2 {
            out.push(format!("  … {} more …", hits.len() - 2));
        }
    }
    out.extend(other.iter().map(|line| line.to_string()));
    out.join("\n")
}

/// JSON at `ultra`: collapse homogeneous arrays - element 0 stands, the
/// rest elide with a count - and drop pretty whitespace via re-encode.
/// Any parse failure or non-shrink returns the original (fail-closed).
fn compress_json(input: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) else {
        return input.to_owned();
    };
    collapse_arrays(&mut value);
    match serde_json::to_string(&value) {
        Ok(encoded) if encoded.len() < input.len() => encoded,
        _ => input.to_owned(),
    }
}

/// Recursively replace runs of similar items past the first with an
/// `… N similar items elided …` marker. Similarity is the object key
/// set (or scalar-ness); a run needs at least 4 to be worth eliding.
fn collapse_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                collapse_arrays(item);
            }
            if items.len() >= 6 {
                let signature = |item: &serde_json::Value| match item {
                    serde_json::Value::Object(map) => {
                        map.keys().cloned().collect::<Vec<_>>().join(",")
                    }
                    _ => item
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| "s".into()),
                };
                let first = signature(&items[0]);
                let similar = items[1..].iter().all(|item| signature(item) == first);
                if similar {
                    let elided = items.len() - 1;
                    items.truncate(1);
                    items.push(serde_json::Value::String(format!(
                        "… {elided} similar items elided (rtk) …"
                    )));
                }
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                collapse_arrays(item);
            }
        }
        _ => {}
    }
}

/// Final stage: truncate overlong lines head/tail, then cap total lines.
fn truncate_and_cap(input: &str, truncate_at: usize, cap: usize) -> String {
    let mut out: Vec<String> = input
        .lines()
        .map(|line| {
            let chars = line.chars().count();
            if chars > truncate_at {
                let head: String = line.chars().take(truncate_at / 2).collect();
                let tail: String = line.chars().skip(chars - truncate_at / 4).collect();
                format!(
                    "{head} … {} chars … {tail}",
                    chars - truncate_at / 2 - truncate_at / 4
                )
            } else {
                line.to_owned()
            }
        })
        .collect();
    if out.len() > cap {
        let elided = out.len() - cap + 1;
        out.truncate(cap - 1);
        out.push(format!("… {elided} lines elided (rtk) …"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_identity() {
        let input = "anything\nat all";
        assert_eq!(compress(input, PayloadKind::Log, RtkLevel::Off), input);
    }

    #[test]
    fn ansi_escapes_strip_without_eating_text() {
        let input = "\u{1b}[32mok\u{1b}[0m done";
        assert_eq!(strip_ansi(input), "ok done");
    }

    #[test]
    fn test_output_keeps_failures_and_counts_passes() {
        let raw = "test a ... ok\ntest b ... ok\ntest c ... FAILED\npanicked at x.rs:1\n\ntest result: FAILED. 2 passed; 1 failed\n";
        let out = compress_tests(raw);
        assert!(out.contains("test c ... FAILED"));
        assert!(out.contains("panicked at x.rs:1"));
        assert!(out.contains("2 tests passed"));
        assert!(!out.contains("test a ... ok"));
    }

    #[test]
    fn diff_keeps_changes_elides_long_context() {
        let mut raw = String::from("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,9 +1,9 @@\n");
        for _ in 0..6 {
            raw.push_str(" same line\n");
        }
        raw.push_str("-old\n+new\n");
        for _ in 0..6 {
            raw.push_str(" more same\n");
        }
        let out = compress_diff(&raw, RtkLevel::Ultra);
        assert!(out.contains("-old\n+new"));
        assert!(out.contains("context lines"));
        assert!(out.lines().count() < raw.lines().count());
    }

    #[test]
    fn ultra_log_keeps_signal_drops_info_middle() {
        let mut raw = String::new();
        for i in 0..40 {
            raw.push_str(&format!("2026-01-01T00:00:{i:02} INFO heartbeat {i}\n"));
        }
        raw.push_str("2026-01-01T00:01:00 ERROR disk full\n");
        let out = compress_log(&raw, RtkLevel::Ultra);
        assert!(out.contains("ERROR disk full"));
        assert!(out.contains("lines elided"));
        assert!(out.lines().count() < raw.lines().count());
    }

    #[test]
    fn dedupe_collapses_repeated_timestamped_lines() {
        let raw = "2026-01-01 10:00:00 tick happened\n2026-01-01 10:00:01 tick happened\n2026-01-01 10:00:02 tick happened\n";
        let out = dedupe_normalized(raw);
        assert!(out.starts_with("×3 "));
    }

    #[test]
    fn json_array_elides_similar_items() {
        let items: Vec<String> = (0..20)
            .map(|i| format!("{{\"id\": {i}, \"state\": \"ok\"}}"))
            .collect();
        let raw = format!("{{\"orders\": [{}]}}", items.join(","));
        let out = compress_json(&raw);
        assert!(out.contains("similar items elided"));
        assert!(out.len() < raw.len());
    }

    #[test]
    fn never_worse_returns_raw_when_filtered_grows() {
        assert_eq!(guarded("ab", "longer".to_owned()), "ab");
    }
}
