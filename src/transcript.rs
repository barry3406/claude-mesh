//! Read a Claude Code session transcript (JSONL) and turn it into a compact,
//! human-readable context slice. This is the only place transcript bytes are
//! touched, and it happens on the owning machine.

use serde_json::Value;

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

/// A short task label: the first substantive user message.
pub fn derive_task(transcript_path: &str) -> String {
    let Ok(content) = std::fs::read_to_string(transcript_path) else {
        return String::new();
    };
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

/// The recent conversation as a compact transcript slice.
pub fn read_context(transcript_path: &str, max_msgs: usize, max_chars: usize) -> String {
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(e) => return format!("(could not read transcript: {})", e),
    };

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
        out = format!("…(older context trimmed)…\n\n{}", char_tail(&out, max_chars));
    }
    if out.is_empty() {
        out = "(no readable conversation yet)".to_string();
    }
    out
}

/// Earlier messages whose text matches keywords from the question.
pub fn relevant_lines(transcript_path: &str, question: &str, max: usize) -> Vec<String> {
    // Keep CJK tokens of length >= 2 (meaningful words) and ASCII tokens of
    // length >= 4 (skip English stop-words like "the"/"you"). Splitting only on
    // whitespace + ASCII punctuation preserves CJK phrases.
    let kws: Vec<String> = question
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .map(|w| w.trim())
        .filter(|w| {
            let n = w.chars().count();
            let has_cjk = w.chars().any(|c| !c.is_ascii());
            (has_cjk && n >= 2) || (!has_cjk && n >= 4)
        })
        .map(|w| w.to_lowercase())
        .collect();
    if kws.is_empty() {
        return vec![];
    }
    let Ok(content) = std::fs::read_to_string(transcript_path) else {
        return vec![];
    };
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
