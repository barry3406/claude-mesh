//! Read a Claude Code session transcript (JSONL) and turn it into a compact,
//! human-readable context slice. This is the only place transcript bytes are
//! touched, and it happens on the owning machine.
//!
//! Reads are bounded: the recent-context path reads only the file's tail and the
//! task label reads only its head, so a multi-megabyte transcript is never
//! slurped whole. The LLM only ever sees the trimmed slice produced here (never
//! the raw file), so transcript size does not affect token cost — only the
//! `max_chars` cap does.

use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};

/// Tail bytes scanned for recent context / keyword matches.
const TAIL_BYTES: u64 = 512 * 1024;
/// Head bytes scanned for the (immutable) first user message.
const HEAD_BYTES: usize = 64 * 1024;

/// Char-safe truncation with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Char-safe tail (keep the last `max` chars).
fn char_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    s.chars().skip(count - max).collect()
}

/// Read up to `max_bytes` from the start of the file (lossy UTF-8).
fn read_head(path: &str, max_bytes: usize) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = vec![0u8; max_bytes];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read up to `max_bytes` from the end of the file (lossy UTF-8). If we started
/// mid-file, drop the first (partial) line so callers only see whole lines.
fn read_tail(path: &str, max_bytes: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(idx) = s.find('\n') {
            return s[idx + 1..].to_string();
        }
    }
    s
}

/// Pull readable text out of a message's `content` (string or block array).
pub fn extract_text(message: Option<&Value>) -> String {
    let Some(m) = message else {
        return String::new();
    };
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for b in arr {
                match b.get("type").and_then(|x| x.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                        parts.push(format!("‹ran {}›", name));
                    }
                    Some("tool_result") => parts.push("‹tool result›".to_string()),
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// A short task label: the first substantive user message. Immutable once present,
/// so callers compute it once; only the file head is read.
pub fn derive_task(transcript_path: &str) -> String {
    let content = read_head(transcript_path, HEAD_BYTES);
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("user") {
            continue;
        }
        let t = extract_text(v.get("message"));
        let t = t.trim();
        if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with('‹') {
            continue;
        }
        return truncate(t, 100);
    }
    String::new()
}

/// The recent conversation as a compact transcript slice (reads only the tail).
pub fn read_context(transcript_path: &str, max_msgs: usize, max_chars: usize) -> String {
    let content = read_tail(transcript_path, TAIL_BYTES);

    let mut msgs: Vec<String> = Vec::new();
    for line in content.lines().rev() {
        if msgs.len() >= max_msgs {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let text = extract_text(v.get("message"));
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        msgs.push(format!("[{}] {}", typ, truncate(text, 1200)));
    }
    msgs.reverse();

    let mut out = msgs.join("\n\n");
    if out.chars().count() > max_chars {
        out = format!(
            "…(older context trimmed)…\n\n{}",
            char_tail(&out, max_chars)
        );
    }
    if out.is_empty() {
        out = "(no readable conversation yet)".to_string();
    }
    out
}

/// Earlier messages (within the scanned tail) whose text matches keywords from
/// the question.
pub fn relevant_lines(transcript_path: &str, question: &str, max: usize) -> Vec<String> {
    // Keep CJK tokens of length >= 2 (meaningful words) and ASCII tokens of
    // length >= 4 (skip English stop-words like "the"/"you"). Splitting only on
    // whitespace + ASCII punctuation preserves CJK phrases.
    let kws: Vec<String> = question
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .map(|w| w.trim())
        .filter(|w| {
            let n = w.chars().count();
            let has_cjk = !w.is_ascii();
            (has_cjk && n >= 2) || (!has_cjk && n >= 4)
        })
        .map(|w| w.to_lowercase())
        .collect();
    if kws.is_empty() {
        return vec![];
    }

    let content = read_tail(transcript_path, TAIL_BYTES);
    let mut hits = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let text = extract_text(v.get("message"));
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let low = trimmed.to_lowercase();
        if kws.iter().any(|k| low.contains(k)) {
            hits.push(truncate(trimmed, 300));
            if hits.len() >= max {
                break;
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, lines: &[String]) -> String {
        let dir = std::env::temp_dir().join("claude-mesh-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p.to_string_lossy().to_string()
    }

    /// On a transcript larger than the tail window, the task must still come from
    /// the head, and recent context from the tail must not drag in the bulk.
    #[test]
    fn bounded_reads_on_large_transcript() {
        let first =
            r#"{"type":"user","message":{"role":"user","content":"first task line"}}"#.to_string();
        let filler = "x".repeat(600_000); // one line bigger than TAIL_BYTES
        let big = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"{filler}"}}}}"#
        );
        let recent =
            r#"{"type":"assistant","message":{"role":"assistant","content":"RECENT_MARKER done"}}"#
                .to_string();
        let path = tmp("big.jsonl", &[first, big, recent]);

        assert_eq!(derive_task(&path), "first task line");

        let ctx = read_context(&path, 24, 5000);
        assert!(ctx.contains("RECENT_MARKER"), "recent tail message present");
        assert!(
            !ctx.contains("xxxxxxxxxx"),
            "the 600KB filler must be trimmed away by the tail read"
        );
    }

    /// CJK keywords (2-char words) must match; the old `>3 chars` filter dropped them.
    #[test]
    fn cjk_keywords_match() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":"我在改 auth 的 JWT 校验逻辑"}}"#.to_string();
        let path = tmp("cjk.jsonl", &[line]);
        let hits = relevant_lines(&path, "JWT 校验 性能", 4);
        assert!(!hits.is_empty(), "CJK keyword 校验 should match");
    }

    /// max_chars caps the slice regardless of transcript size.
    #[test]
    fn max_chars_caps_output() {
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"{}"}}}}"#,
            "据".repeat(4000)
        );
        let path = tmp("cap.jsonl", &[line]);
        let ctx = read_context(&path, 24, 500);
        assert!(
            ctx.chars().count() < 700,
            "output respects the max_chars cap"
        );
    }
}
