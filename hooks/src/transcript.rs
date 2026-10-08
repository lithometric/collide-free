//! Reading the harness transcript's tail: what this turn cost, and whether the
//! agent just ran out of quota. Only the tail is read, so both stay well
//! inside the hook's budget; any problem yields nothing rather than failing.

use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

pub const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const LIMIT_TAIL_LINES: usize = 40;
/// The harness's limit notice is one short line that says the limit was hit
/// and when it resets. An agent writing ABOUT limits — "rate-limiting for
/// the free tier" in a summary of its own work — is prose: long, and saying
/// neither. Without these two tests that prose read as "hit its usage
/// limit" on every teammate's dashboard. Twin of the Python hook's.
const LIMIT_MAX_ENTRY_CHARS: usize = 300;
const LIMIT_WORDS: [&str; 4] = ["reset", "reached", "exceeded", "hit your"];

fn limit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)((?:session|usage|weekly|daily|monthly|5-hour|rate)[ -]limit\b[^\n]{0,120})")
            .expect("limit pattern")
    })
}

fn tail(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    file.seek(SeekFrom::Start(size.saturating_sub(TRANSCRIPT_TAIL_BYTES))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Every usage counter a turn is billed on, plus `tokens` — the pre-v20
/// field, which was uncached input + output only. Cleared together when a
/// turn spent nothing (a limit hit).
pub const USAGE_KEYS: [&str; 5] =
    ["tokens", "input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"];

/// Per-turn cost from the transcript: the latest assistant message's usage,
/// its model, and a turn id so the server can dedup the many tool calls that
/// share one turn.
///
/// All four counters are sent, not one sum: cache creation costs 1.25x an
/// input token and a cache read a tenth of one, and in a cached agent loop
/// those two ARE most of the bill, so no single blended rate over
/// uncached-input + output can stand in for them. `tokens` keeps its old
/// meaning for a server that predates the four.
pub fn turn_meta(path: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    let Some(text) = tail(path) else { return meta };
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.contains("\"assistant\"") {
            continue;
        }
        // a partial first line from the tail cut simply fails to parse
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let message = entry.get("message").cloned().unwrap_or(Value::Null);
        let usage = message.get("usage").cloned().unwrap_or(Value::Null);
        if !usage.is_object() || usage.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            continue;
        }
        let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0);
        let (inp, out) = (count("input_tokens"), count("output_tokens"));
        let (cache_creation, cache_read) =
            (count("cache_creation_input_tokens"), count("cache_read_input_tokens"));
        let turn_id = entry
            .get("uuid")
            .and_then(Value::as_str)
            .or_else(|| message.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let model = message.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        // the context the next message would replay: everything read, cached or not
        let context = inp + cache_creation + cache_read;
        meta.insert("tokens".into(), Value::from(inp + out));
        meta.insert("input_tokens".into(), Value::from(inp));
        meta.insert("output_tokens".into(), Value::from(out));
        meta.insert("cache_read_tokens".into(), Value::from(cache_read));
        meta.insert("cache_creation_tokens".into(), Value::from(cache_creation));
        meta.insert("context".into(), Value::from(context));
        meta.insert("turn_id".into(), Value::from(turn_id));
        meta.insert("model".into(), Value::from(model));
        return meta;
    }
    codex_turn_meta(&text)
}

/// The same counters from a Codex rollout. A current Codex logs each model
/// response as a `token_usage_record` (its `response_id` is the call's id);
/// an older one as a `token_count` event, whose timestamp stands in for the
/// id. Either way the many tool calls of one response bill it once. The model
/// is in `turn_context`. OpenAI counts cached input inside `input_tokens`, so
/// the uncached part is the difference.
fn codex_turn_meta(text: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    let lines: Vec<&str> = text.lines().collect();
    for (at, line) in lines.iter().enumerate().rev() {
        if !line.contains("\"token_usage_record\"") && !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let payload = entry.get("payload").cloned().unwrap_or(Value::Null);
        let stamp = entry.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let (usage, turn_id) = match (entry.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
            (Some("token_usage_record"), _) => (
                payload.get("usage").cloned(),
                payload.get("response_id").and_then(Value::as_str).map(|id| format!("codex:{id}")).unwrap_or_default(),
            ),
            (Some("event_msg"), Some("token_count")) => (
                payload.get("info").and_then(|i| i.get("last_token_usage")).cloned(),
                if stamp.is_empty() { String::new() } else { format!("codex:{stamp}") },
            ),
            _ => continue,
        };
        let Some(usage) = usage.filter(Value::is_object) else { continue };
        let count = |field: &str| usage.get(field).and_then(Value::as_i64).unwrap_or(0).max(0);
        let (read, cached, written, out) =
            (count("input_tokens"), count("cached_input_tokens"), count("cache_write_input_tokens"), count("output_tokens"));
        let inp = (read - cached - written).max(0);
        let model = lines[..at].iter().rev()
            .filter(|l| l.contains("\"turn_context\""))
            .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .find_map(|e| e.get("payload").and_then(|p| p.get("model")).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        meta.insert("tokens".into(), Value::from(inp + out));
        meta.insert("input_tokens".into(), Value::from(inp));
        meta.insert("output_tokens".into(), Value::from(out));
        meta.insert("cache_read_tokens".into(), Value::from(cached));
        meta.insert("cache_creation_tokens".into(), Value::from(written));
        meta.insert("context".into(), Value::from(read));
        meta.insert("turn_id".into(), Value::from(turn_id));
        meta.insert("model".into(), Value::from(model));
        return meta;
    }
    meta
}

/// A Codex rollout line's assistant text: a `response_item` message's
/// output text, or an `agent_message` event's.
fn codex_reply(entry: &Value) -> String {
    let Some(payload) = entry.get("payload") else { return String::new() };
    match (entry.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
        (Some("response_item"), Some("message")) if payload.get("role").and_then(Value::as_str) == Some("assistant") => payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        (Some("event_msg"), Some("agent_message")) => {
            payload.get("message").and_then(Value::as_str).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

/// The human-readable text of one transcript line: message content when it
/// parses as JSON, else the raw line — so escapes never split a match.
fn entry_text(line: &str) -> String {
    let Ok(entry) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    let content = entry.get("message").and_then(|m| m.get("content"));
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => line.to_string(),
    }
}

/// The agent hit a usage limit: the harness wrote a limit message into the
/// transcript's last few entries. A limit from hours ago is not re-reported by
/// every later Stop.
pub fn limit_note(path: &str) -> String {
    let Some(text) = tail(path) else { return String::new() };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LIMIT_TAIL_LINES);
    for line in lines[start..].iter().rev() {
        let line = line.trim();
        let text = entry_text(line);
        if text == line && line.starts_with('{') {
            continue; // a record with no message text (tool results, metadata): never a notice
        }
        if text.chars().count() > LIMIT_MAX_ENTRY_CHARS {
            continue;
        }
        let lower = text.to_lowercase();
        if !LIMIT_WORDS.iter().any(|word| lower.contains(word)) {
            continue;
        }
        if let Some(found) = limit_re().captures(&text) {
            if let Some(m) = found.get(1) {
                return take_chars(m.as_str().trim(), 160);
            }
        }
    }
    String::new()
}

/// The agent's closing words for this turn: the text of the last assistant
/// message in the transcript's tail. The server keeps its first paragraph
/// as the note on whatever the turn changed (`/presence` action
/// "settled"); empty when there is none.
pub fn last_reply(path: &str) -> String {
    let Some(text) = tail(path) else { return String::new() };
    for line in text.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let codex = codex_reply(&entry);
        if !codex.trim().is_empty() {
            return take_chars(codex.trim(), 4000);
        }
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let said = entry_text(line);
        if !said.trim().is_empty() && said != line {
            return take_chars(said.trim(), 4000);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = r#"{"timestamp":"2026-10-04T06:00:00.000Z","type":"turn_context","payload":{"cwd":"/r","model":"gpt-5-codex"}}
{"timestamp":"2026-10-04T06:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}}
{"timestamp":"2026-10-04T06:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":90000,"cached_input_tokens":80000,"output_tokens":900},"last_token_usage":{"input_tokens":30000,"cached_input_tokens":25000,"output_tokens":400,"reasoning_output_tokens":100,"total_tokens":30400}}}}
{"timestamp":"2026-10-04T06:00:06.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Parked the funnels work."}]}}
"#;

    fn rollout() -> String {
        let path = std::env::temp_dir().join(format!("collide-rollout-{}.jsonl", std::process::id()));
        std::fs::write(&path, ROLLOUT).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_codex_rollout_bills_its_last_model_call() {
        let meta = turn_meta(&rollout());
        assert_eq!(meta["input_tokens"], 5000, "cached input is counted inside OpenAI's input_tokens");
        assert_eq!(meta["cache_read_tokens"], 25000);
        assert_eq!(meta["output_tokens"], 400);
        assert_eq!(meta["context"], 30000);
        assert_eq!(meta["model"], "gpt-5-codex");
        assert_eq!(meta["turn_id"], "codex:2026-10-04T06:00:05.000Z");
    }

    #[test]
    fn a_current_codex_rollout_bills_by_response_id() {
        let path = std::env::temp_dir().join(format!("collide-rollout-new-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{ROLLOUT}{}\n", r#"{"timestamp":"2026-10-04T06:00:07.000Z","type":"token_usage_record","payload":{"turn_id":"t1","response_id":"resp_9","usage":{"input_tokens":40000,"cached_input_tokens":30000,"cache_write_input_tokens":2000,"output_tokens":700,"reasoning_output_tokens":0,"total_tokens":40700}}}"#)).unwrap();
        let meta = turn_meta(&path.to_string_lossy());
        assert_eq!(meta["turn_id"], "codex:resp_9");
        assert_eq!(meta["input_tokens"], 8000);
        assert_eq!(meta["cache_creation_tokens"], 2000);
        assert_eq!(meta["model"], "gpt-5-codex");
    }

    #[test]
    fn a_codex_rollout_gives_its_closing_reply() {
        assert_eq!(last_reply(&rollout()), "Parked the funnels work.");
    }
}
