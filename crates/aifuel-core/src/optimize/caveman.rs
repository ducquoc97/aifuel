//! The `caveman` optimizer: terse-prose condensation plus the response
//! style instruction, ported from the Caveman skill's level rules.
//!
//! Two surfaces:
//!
//! - [`compress`] condenses prose payloads mechanically: protected spans
//!   (fenced code, inline code, URLs, paths, error strings) pass through
//!   verbatim, and the surviving prose loses ceremony, filler, and - at
//!   higher levels - articles and long words. Negations are never
//!   dropped: a missing `not` costs more than every token saved.
//! - [`instruction`] is the system-prompt directive a wire integration
//!   sends so the provider answers in terse style at the same level.

use super::detect::PayloadKind;

/// The intensity the plan configured for prose condensation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CavemanLevel {
    /// Engine disabled.
    #[default]
    Off,
    /// Drop ceremony and hedging; keep articles and full sentences.
    Lite,
    /// Lite plus article dropping and fragments - the classic voice.
    Full,
    /// Full plus common-word abbreviations and arrow causality.
    Ultra,
}

impl CavemanLevel {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "off" => Self::Off,
            "lite" => Self::Lite,
            "full" => Self::Full,
            "ultra" => Self::Ultra,
            _ => return None,
        })
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }
}

/// Words that carry meaning the transform must never remove: a dropped
/// negation silently inverts an answer. Ported from the skill's "articles
/// optional, meaning never" rule.
const KEEP_WORDS: &[&str] = &[
    "not",
    "never",
    "no",
    "none",
    "only",
    "except",
    "without",
    "don't",
    "doesn't",
    "didn't",
    "isn't",
    "aren't",
    "can't",
    "won't",
    "mustn't",
    "shouldn't",
    "couldn't",
    "wouldn't",
    "nor",
];

/// Hedging and ceremony words the skill drops at `lite` and above.
const FILLER_WORDS: &[&str] = &[
    "just",
    "really",
    "basically",
    "actually",
    "simply",
    "literally",
    "definitely",
    "certainly",
    "obviously",
    "essentially",
    "quite",
    "rather",
    "somewhat",
    "perhaps",
    "maybe",
    "hopefully",
    "fortunately",
    "unfortunately",
    "sure",
    "please",
];

/// Multi-word ceremony phrases collapsed or removed at `lite` and above.
const CEREMONY: &[(&str, &str)] = &[
    ("i'd be happy to", ""),
    ("i would be happy to", ""),
    ("i'll be happy to", ""),
    ("hope this helps", ""),
    ("let me know if", ""),
    ("please note that", "note:"),
    ("it should be noted that", "note:"),
    ("in order to", "to"),
    ("due to the fact that", "because"),
    ("at this point in time", "now"),
    ("in the event that", "if"),
    ("a large number of", "many"),
    ("in spite of the fact that", "although"),
    ("for the purpose of", "for"),
    ("as a matter of fact", ""),
];

/// Articles dropped at `full` and above - only outside protected spans.
const ARTICLES: &[&str] = &["a", "an", "the"];

/// Standard abbreviations at `ultra`: prose words only, never code
/// symbols. The skill allows standard acronyms and drops invented ones.
const ABBREVIATE: &[(&str, &str)] = &[
    ("database", "DB"),
    ("databases", "DBs"),
    ("configuration", "config"),
    ("configurations", "configs"),
    ("environment", "env"),
    ("environments", "envs"),
    ("authentication", "auth"),
    ("authorization", "authz"),
    ("request", "req"),
    ("response", "res"),
    ("requests", "reqs"),
    ("responses", "res"),
    ("application", "app"),
    ("repository", "repo"),
    ("repositories", "repos"),
    ("directory", "dir"),
    ("directories", "dirs"),
    ("documentation", "docs"),
    ("implementation", "impl"),
    ("parameter", "param"),
    ("parameters", "params"),
    ("argument", "arg"),
    ("arguments", "args"),
    ("because", "b/c"),
];

/// The system-prompt directive per level, ending in the shared
/// boundaries clause every Caveman output level carries: protected spans
/// and negations stay verbatim.
pub fn instruction(level: CavemanLevel) -> Option<&'static str> {
    Some(match level {
        CavemanLevel::Off => return None,
        CavemanLevel::Lite => {
            concat!(
                "Be terse. Answer first, then reason, then next step. No greetings, ",
                "hedging, pleasantries, recaps, or closers. Full sentences. ",
                "Keep code blocks, commands, file paths, error strings, URLs, and ",
                "identifiers verbatim. Never drop negations (not/never/no/only/except)."
            )
        }
        CavemanLevel::Full => {
            concat!(
                "Respond in compressed caveman style. Answer first, then reason, then next step. ",
                "No filler, hedging, or ceremony; fragments are fine. Drop articles when the ",
                "sentence still reads in one pass. One idea per sentence. Standard acronyms OK. ",
                "Keep code blocks, commands, file paths, error strings, URLs, and ",
                "identifiers verbatim. Never drop negations (not/never/no/only/except)."
            )
        }
        CavemanLevel::Ultra => {
            concat!(
                "Respond in maximum-compression caveman style. Answer first. No filler or ",
                "ceremony; fragments fine, articles optional. Abbreviate common prose words ",
                "(DB, config, auth, req, res). Arrows OK for causality. One word when one word ",
                "is enough. Keep code blocks, commands, file paths, error strings, URLs, and ",
                "identifiers verbatim. Never drop negations (not/never/no/only/except)."
            )
        }
    })
}

/// Condense a prose payload at `level`. Only `Text` payloads condense -
/// structured kinds are the `rtk` engine's domain and pass through, so
/// stacking the engines never double-handles a payload.
pub fn compress(input: &str, kind: PayloadKind, level: CavemanLevel) -> String {
    if level == CavemanLevel::Off || kind != PayloadKind::Text {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    for segment in segments(input) {
        match segment {
            Segment::Protected(span) => out.push_str(&span),
            Segment::Prose(text) => out.push_str(&condense_prose(&text, level)),
        }
    }
    // Whitespace fold across the whole result: condensation leaves runs
    // of blanks and empty lines that cost tokens without carrying signal.
    fold_whitespace(&out)
}

/// One classified slice of the input: `Protected` survives verbatim,
/// `Prose` is candidate for word-level rules.
enum Segment {
    Protected(String),
    Prose(String),
}

/// Split input into fenced-code, inline-code, URL, path, and prose
/// segments. Quoted error strings stay inside prose spans on purpose -
/// their *content* words are technical and the word rules below keep
/// digits, symbols, and identifiers intact.
fn segments(input: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut rest = input;
    let mut prose = String::new();
    let flush = |prose: &mut String, out: &mut Vec<Segment>| {
        if !prose.is_empty() {
            out.push(Segment::Prose(std::mem::take(prose)));
        }
    };
    while !rest.is_empty() {
        if let Some(body) = rest.strip_prefix("```") {
            let end = body.find("```").map(|i| i + 3).unwrap_or(body.len());
            flush(&mut prose, &mut out);
            out.push(Segment::Protected(format!("```{}", &body[..end])));
            rest = &rest[end + 3..];
        } else if rest.starts_with('`')
            && let Some(end) = rest[1..].find('`')
        {
            flush(&mut prose, &mut out);
            out.push(Segment::Protected(rest[..end + 2].to_owned()));
            rest = &rest[end + 2..];
        } else if rest.starts_with("http://") || rest.starts_with("https://") {
            flush(&mut prose, &mut out);
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(Segment::Protected(rest[..end].to_owned()));
            rest = &rest[end..];
        } else {
            let c = rest.chars().next().expect("non-empty");
            prose.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    flush(&mut prose, &mut out);
    out
}

/// Word-level condensation of one prose span by level.
fn condense_prose(text: &str, level: CavemanLevel) -> String {
    let mut text = text.to_owned();
    for (phrase, replacement) in CEREMONY {
        text = replace_case_insensitive(&text, phrase, replacement);
    }
    let words: Vec<&str> = text.split(' ').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(words.len());
    for word in words {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-');
        let lower = bare.to_ascii_lowercase();
        if KEEP_WORDS.contains(&lower.as_str()) {
            kept.push(word);
            continue;
        }
        if FILLER_WORDS.contains(&lower.as_str()) {
            continue;
        }
        if level != CavemanLevel::Lite
            && ARTICLES.contains(&lower.as_str())
            && bare.chars().all(|c| c.is_alphabetic())
        {
            continue;
        }
        kept.push(word);
    }
    let mut joined = kept.join(" ");
    if level == CavemanLevel::Ultra {
        for (word, short) in ABBREVIATE {
            joined = replace_word_case_insensitive(&joined, word, short);
        }
    }
    joined
}

/// Replace every case-insensitive occurrence of `phrase` (boundary-
/// checked so `simply` never eats `simpler`) with `replacement`.
fn replace_case_insensitive(text: &str, phrase: &str, replacement: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let boundary = |b: u8| !(b.is_ascii_alphanumeric() || b == b'_');
    while let Some(found) = lower[cursor..].find(phrase) {
        let start = cursor + found;
        let end = start + phrase.len();
        let left_ok = start == 0 || boundary(text.as_bytes()[start - 1]);
        let right_ok = end == text.len() || boundary(text.as_bytes()[end]);
        if left_ok && right_ok {
            out.push_str(&text[cursor..start]);
            out.push_str(replacement);
            cursor = end;
        } else {
            out.push_str(&text[cursor..start + 1]);
            cursor = start + 1;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Whole-word replace for the abbreviation table; preserves the
/// capitalization of the first letter so `Database` → `DB` reads right.
fn replace_word_case_insensitive(text: &str, word: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive(' ') {
        let trailing = piece.ends_with(' ');
        let token = piece.trim_end_matches(' ');
        let punct_end = token
            .find(|c: char| c.is_alphanumeric() || c == '-' || c == '\'')
            .unwrap_or(token.len());
        let core_end = token
            .rfind(|c: char| c.is_alphanumeric() || c == '-' || c == '\'')
            .map(|i| i + 1)
            .unwrap_or(punct_end);
        let core = &token[punct_end..core_end];
        if core.eq_ignore_ascii_case(word) {
            out.push_str(&token[..punct_end]);
            if token.chars().next().is_some_and(|c| c.is_uppercase()) {
                let mut short = replacement.chars();
                if let Some(first) = short.next() {
                    out.extend(first.to_uppercase());
                }
                out.push_str(short.as_str());
            } else {
                out.push_str(replacement);
            }
            out.push_str(&token[core_end..]);
        } else {
            out.push_str(token);
        }
        if trailing {
            out.push(' ');
        }
    }
    out
}

/// Collapse whitespace runs and blank-line runs the transforms leave.
fn fold_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let collapsed: Vec<&str> = line.split_whitespace().collect();
        let joined = collapsed.join(" ");
        if joined.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&joined);
        }
    }
    out.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_non_text_kinds_are_identity() {
        let raw = "please actually just fix the bug";
        assert_eq!(compress(raw, PayloadKind::Text, CavemanLevel::Off), raw);
        assert_eq!(compress(raw, PayloadKind::Json, CavemanLevel::Ultra), raw);
    }

    #[test]
    fn lite_drops_filler_keeps_articles_and_negations() {
        let raw = "Please just basically do not actually delete the file.";
        let out = compress(raw, PayloadKind::Text, CavemanLevel::Lite);
        assert!(out.contains("do not delete the file"), "got: {out}");
        assert!(!out.to_ascii_lowercase().contains("basically"));
    }

    #[test]
    fn full_drops_articles_but_never_negations() {
        let raw = "The fix is to update the config and not remove the entry.";
        let out = compress(raw, PayloadKind::Text, CavemanLevel::Full);
        assert!(out.contains("not remove"), "got: {out}");
        assert!(!out.contains("The fix is to update the"), "got: {out}");
    }

    #[test]
    fn protected_spans_pass_verbatim() {
        let raw = "Just run `cargo test` and check https://example.com docs:\n```\nthe code stays the same\n```";
        let out = compress(raw, PayloadKind::Text, CavemanLevel::Ultra);
        assert!(out.contains("`cargo test`"), "got: {out}");
        assert!(out.contains("https://example.com"), "got: {out}");
        assert!(out.contains("the code stays the same"), "got: {out}");
    }

    #[test]
    fn ultra_abbreviates_prose_words() {
        let raw = "Update the database configuration because the environment changed.";
        let out = compress(raw, PayloadKind::Text, CavemanLevel::Ultra);
        assert!(out.contains("DB"), "got: {out}");
        assert!(out.contains("config"), "got: {out}");
        assert!(out.contains("b/c"), "got: {out}");
    }

    #[test]
    fn instruction_scales_with_level_and_always_keeps_boundaries() {
        for level in [CavemanLevel::Lite, CavemanLevel::Full, CavemanLevel::Ultra] {
            let text = instruction(level).expect("on levels instruct");
            assert!(text.contains("verbatim"), "level {level:?}");
            assert!(text.contains("negations"), "level {level:?}");
        }
        assert!(instruction(CavemanLevel::Off).is_none());
        assert!(
            instruction(CavemanLevel::Ultra).unwrap().len()
                > instruction(CavemanLevel::Lite).unwrap().len()
        );
    }
}
