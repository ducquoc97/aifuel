//! Payload classification for the optimizer pipeline.
//!
//! Ported from the Caveman Engine's `detect.go`: a deterministic router
//! that picks the content type a compressor should treat the bytes as.
//! Anything the detector is not confident about reports `Text`, which
//! routes to the conservative prose path - detection never guesses.

/// The content class a payload is treated as by the optimizer stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// Strict JSON (object or array) that fully parses.
    Json,
    /// Unified diff output (`diff --git`, `@@`, `---`/`+++` markers).
    Diff,
    /// Source code: keyword and symbol density above prose thresholds.
    Code,
    /// Leveled/timestamped log lines.
    Log,
    /// Test-runner output (`test x ... ok`, `test result:`, `FAILED`).
    TestOutput,
    /// Grep/search result lines (`path:line[:col]:match`).
    SearchResult,
    /// Raw terminal output: ANSI escapes or progress-bar carriage returns.
    Terminal,
    /// Ordinary prose - the low-confidence fallback.
    Text,
}

const LOG_LEVEL_WORDS: &[&str] = &[
    "trace", "debug", "info", "warn", "error", "fatal", "panic", "crit",
];
const CODE_KEYWORDS: &[&str] = &[
    "func",
    "package",
    "import",
    "def",
    "class",
    "function",
    "return",
    "const",
    "let",
    "var",
    "public",
    "private",
    "protected",
    "static",
    "void",
    "struct",
    "interface",
    "namespace",
    "module",
    "fn",
    "impl",
    "trait",
    "export",
    "async",
    "await",
];

/// Classify a payload. The order is strict-JSON, terminal, diff, test,
/// log, code, search, then text - the same precedence the Caveman Engine
/// uses, plus a test-runner class for `rtk`'s highest-volume target.
pub fn detect(input: &str) -> PayloadKind {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PayloadKind::Text;
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return PayloadKind::Json;
    }
    if looks_like_terminal(input) {
        return PayloadKind::Terminal;
    }
    if looks_like_diff(input) {
        return PayloadKind::Diff;
    }
    if looks_like_test_output(input) {
        return PayloadKind::TestOutput;
    }
    if looks_like_log(input) {
        return PayloadKind::Log;
    }
    if looks_like_code(input) {
        return PayloadKind::Code;
    }
    if looks_like_search_result(input) {
        return PayloadKind::SearchResult;
    }
    PayloadKind::Text
}

/// A raw ANSI/CSI escape is the conclusive terminal signal - nothing but
/// terminal or command output legitimately embeds one. A dense run of bare
/// carriage returns (a progress bar redrawing in place) is the secondary
/// signal; CRLF endings are excluded so `\r\n` text never trips it.
fn looks_like_terminal(input: &str) -> bool {
    if input.contains('\u{1b}') {
        return true;
    }
    let bare_cr = input.matches('\r').count() - input.matches("\r\n").count();
    bare_cr >= 3
}

fn looks_like_diff(input: &str) -> bool {
    let marked = input.contains("\n@@ ")
        || input.contains("diff --git ")
        || (input.contains("\n--- ") && input.contains("\n+++ "));
    if !marked {
        return false;
    }
    input
        .lines()
        .take(64)
        .filter(|line| {
            line.starts_with("diff --git ")
                || line.starts_with("@@ ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || (line.len() > 1
                    && (line.starts_with('+') || line.starts_with('-'))
                    && !line.starts_with("++")
                    && !line.starts_with("--"))
        })
        .count()
        >= 4
}

/// Test-runner output repeats status-per-test lines and ends with a
/// counted summary. Five status lines or a `test result:`/`failures:`
/// summary is enough evidence; a lone "ok" never trips it.
fn looks_like_test_output(input: &str) -> bool {
    let status_lines = input
        .lines()
        .filter(|line| {
            let line = line.trim_end();
            (line.starts_with("test ") && (line.ends_with(" ok") || line.ends_with("FAILED")))
                || line.ends_with("... ok")
                || line.ends_with("... FAILED")
                || line.starts_with("FAIL\t")
                || line.starts_with("ok  \t")
        })
        .count();
    if status_lines >= 5 {
        return true;
    }
    input.contains("test result:") || input.contains("failures:") || input.contains("FAILED:")
}

/// A payload dominated by log-level or timestamped lines. The threshold
/// is a fraction of non-empty lines so a short log still classifies.
fn looks_like_log(input: &str) -> bool {
    let lines: Vec<&str> = input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() < 3 {
        return false;
    }
    let hits = lines
        .iter()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            LOG_LEVEL_WORDS.iter().any(|word| lower.contains(word))
                || line
                    .trim_start()
                    .chars()
                    .take(4)
                    .all(|c| c.is_ascii_digit() || c == '-' || c == '/')
                    && line
                        .trim_start()
                        .chars()
                        .take(4)
                        .any(|c| c.is_ascii_digit())
        })
        .count();
    hits * 2 >= lines.len()
}

fn looks_like_code(input: &str) -> bool {
    if looks_like_log(input) {
        return false;
    }
    if input.trim_start().starts_with("#!") {
        return true;
    }
    let keywords = CODE_KEYWORDS
        .iter()
        .map(|word| count_word(input, word))
        .sum::<usize>();
    let symbols = input.matches("=>").count()
        + input.matches("::").count()
        + input.matches("->").count()
        + input.matches("){").count()
        + input.matches(") {").count();
    keywords + symbols >= 6 && keywords >= 2
}

/// Whole-word count for an identifier-length token, so `return` inside
/// `returned` does not score.
fn count_word(input: &str, needle: &str) -> usize {
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    input
        .as_bytes()
        .windows(needle.len())
        .enumerate()
        .filter(|(index, window)| {
            *window == needle.as_bytes()
                && (*index == 0 || !is_ident(input.as_bytes()[index - 1]))
                && (*index + needle.len() == input.len()
                    || !is_ident(input.as_bytes()[index + needle.len()]))
        })
        .count()
}

/// Grep-style `path:line[:col]: text` lines in volume.
fn looks_like_search_result(input: &str) -> bool {
    let lines: Vec<&str> = input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() < 3 {
        return false;
    }
    let hits = lines.iter().filter(|line| grep_line(line)).count();
    hits * 3 >= lines.len() * 2
}

fn grep_line(line: &str) -> bool {
    let mut parts = line.splitn(4, ':');
    let (Some(path), Some(line_no), Some(_rest)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !path.is_empty()
        && path.len() <= 240
        && !path.contains(' ')
        && line_no.chars().all(|c| c.is_ascii_digit())
        && !line_no.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_json_only_when_the_whole_payload_parses() {
        assert_eq!(detect("{\"a\": 1}"), PayloadKind::Json);
        assert_eq!(detect("[1, 2]"), PayloadKind::Json);
        // A brace at the start of prose is not JSON.
        assert_eq!(detect("{note} about the run"), PayloadKind::Text);
    }

    #[test]
    fn detects_diff_and_test_output() {
        let diff = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n-old\n+new\n rest\n";
        assert_eq!(detect(diff), PayloadKind::Diff);

        let tests = "test a ... ok\ntest b ... ok\ntest c ... ok\ntest d ... ok\ntest e ... FAILED\n\ntest result: FAILED. 4 passed; 1 failed\n";
        assert_eq!(detect(tests), PayloadKind::TestOutput);
    }

    #[test]
    fn detects_logs_before_code_so_log_messages_with_keywords_route_to_log() {
        let log = "2026-01-01T10:00:00 INFO return handler ready\n2026-01-01T10:00:01 ERROR class load failed\n2026-01-01T10:00:02 INFO done\n";
        assert_eq!(detect(log), PayloadKind::Log);
    }

    #[test]
    fn terminal_signal_is_ansi_not_crlf() {
        assert_eq!(detect("a\r\nb\r\nc"), PayloadKind::Text);
        assert_eq!(detect("\u{1b}[32mok\u{1b}[0m"), PayloadKind::Terminal);
    }

    #[test]
    fn grep_results_detect_but_short_lists_do_not() {
        let results = "src/a.rs:10: hit one\nsrc/b.rs:22: hit two\nsrc/c.rs:30: hit three\nsrc/d.rs:41: hit four\n";
        assert_eq!(detect(results), PayloadKind::SearchResult);
        assert_eq!(detect("a:1: one line only"), PayloadKind::Text);
    }
}
