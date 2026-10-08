//! One hook, several harnesses.
//!
//! The canonical event and tool names are Claude Code's, because that is the
//! vocabulary every branch in `report.rs` and `gate.rs` already speaks; a
//! foreign harness is translated at the two edges — what comes in, and what it
//! reads back — and nowhere else. The harness is named on argv by the config
//! Collide generates, never sniffed from the payload: a sniffer that guesses
//! wrong fails silently, and silence is the one failure these hooks cannot
//! afford. No flag means Claude Code, so every artifact already committed
//! keeps working untouched.

use std::sync::OnceLock;

use serde_json::{json, Map, Value};

static HARNESS: OnceLock<String> = OnceLock::new();

/// The harness named on argv, defaulting to Claude Code.
pub fn of(args: &[String]) -> String {
    let mut found = "claude".to_string();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--harness" {
            if let Some(name) = args.get(i + 1) {
                let name = name.trim().to_ascii_lowercase();
                if !name.is_empty() {
                    found = name;
                }
            }
        }
    }
    found
}

/// Remember the harness for this process. First call wins.
pub fn set(name: &str) {
    let _ = HARNESS.set(name.to_string());
}

pub fn current() -> &'static str {
    HARNESS.get().map(String::as_str).unwrap_or("claude")
}

fn event_name(harness: &str, event: &str) -> String {
    let table: &[(&str, &str)] = match harness {
        "cursor" => &[
            ("sessionStart", "SessionStart"),
            ("sessionEnd", "SessionEnd"),
            ("beforeSubmitPrompt", "UserPromptSubmit"),
            ("preToolUse", "PreToolUse"),
            ("postToolUse", "PostToolUse"),
            ("stop", "Stop"),
        ],
        "hermes" => &[
            ("on_session_start", "SessionStart"),
            // Hermes fires this at the end of every turn: the turn's Stop
            ("on_session_end", "Stop"),
            ("on_session_finalize", "SessionEnd"),
            ("pre_llm_call", "UserPromptSubmit"),
            ("pre_tool_call", "PreToolUse"),
            ("post_tool_call", "PostToolUse"),
        ],
        _ => &[],
    };
    table
        .iter()
        .find(|(from, _)| *from == event)
        .map(|(_, to)| (*to).to_string())
        .unwrap_or_else(|| event.to_string())
}

fn tool_name(harness: &str, tool: &str) -> String {
    let table: &[(&str, &str)] = match harness {
        "cursor" => &[("Shell", "Bash"), ("Delete", "Write")],
        "hermes" => &[
            ("write_file", "Write"),
            ("edit_file", "Edit"),
            ("str_replace", "Edit"),
            ("patch", "Edit"),
            ("read_file", "Read"),
            ("grep", "Grep"),
            ("glob", "Glob"),
            ("list_dir", "Glob"),
            ("terminal", "Bash"),
            ("shell", "Bash"),
            ("bash", "Bash"),
        ],
        _ => &[],
    };
    table
        .iter()
        .find(|(from, _)| *from == tool)
        .map(|(_, to)| (*to).to_string())
        .unwrap_or_else(|| tool.to_string())
}

fn first_string(map: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = map.get(*key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// A foreign harness's payload in the canonical shape.
pub fn normalize(payload: Value) -> Value {
    let harness = current();
    if harness == "claude" || harness == "codex" {
        return payload;
    }
    let Some(map) = payload.as_object() else { return payload };
    let mut out = map.clone();
    let event = first_string(map, &["hook_event_name", "event", "hook"]);
    out.insert("hook_event_name".into(), json!(event_name(harness, &event)));
    let tool = first_string(map, &["tool_name", "tool"]);
    if !tool.is_empty() {
        out.insert("tool_name".into(), json!(tool_name(harness, &tool)));
    }
    for key in ["tool_input", "tool_args", "arguments", "args", "input"] {
        if map.get(key).map(Value::is_object).unwrap_or(false) {
            out.insert("tool_input".into(), map[key].clone());
            break;
        }
    }
    for key in ["tool_response", "tool_output", "output", "result"] {
        if let Some(value) = map.get(key) {
            out.insert("tool_response".into(), value.clone());
            break;
        }
    }
    // Hermes names the file `path` (relative, absolute or ~/), and its patch
    // tool is an Edit unless it carries a V4A patch, which is an apply_patch
    if harness == "hermes" {
        let cwd = first_string(map, &["cwd"]);
        if let Some(input) = out.get_mut("tool_input").and_then(Value::as_object_mut) {
            let path = input.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            if !path.is_empty() && !input.contains_key("file_path") {
                let expanded = match path.strip_prefix("~/") {
                    Some(rest) => std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or(path.clone()),
                    None => path.clone(),
                };
                let absolute = if std::path::Path::new(&expanded).is_absolute() || cwd.is_empty() {
                    expanded
                } else {
                    std::path::Path::new(&cwd).join(&expanded).to_string_lossy().to_string()
                };
                input.insert("file_path".into(), json!(absolute));
            }
            if tool == "patch" {
                if let Some(v4a) = input.get("patch").and_then(Value::as_str).map(str::to_string) {
                    input.insert("command".into(), json!(v4a));
                    out.insert("tool_name".into(), json!("apply_patch"));
                }
            }
        }
    }
    // Cursor names the project in a list (and runs user-level hooks from
    // ~/.cursor, so the process's own directory is no guide)
    if first_string(map, &["cwd"]).is_empty() {
        if let Some(root) = map.get("workspace_roots").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) {
            out.insert("cwd".into(), json!(root));
        }
    }
    for (canonical, sources) in [
        ("session_id", &["sessionId", "session", "conversation_id"][..]),
        ("cwd", &["workspace_root", "workspaceRoot", "root"][..]),
        ("prompt", &["user_message", "message", "text"][..]),
    ] {
        if first_string(map, &[canonical]).is_empty() {
            let value = first_string(map, sources);
            if !value.is_empty() {
                out.insert(canonical.into(), json!(value));
            }
        }
    }
    Value::Object(out)
}

/// What this harness reads back as context, or empty when it reads nothing.
///
/// Hermes discards a `post_tool_call` return value by design, so there is no
/// channel there at all; the note stays queued for the next turn rather than
/// being written into a pipe that throws it away.
/// Characters this process put into the agent's context. Every one is
/// carried on every later message, so all of it counts as Collide's cost,
/// not only the briefing (report::run settles it per session).
static INJECTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn note_injected(text: &str) {
    INJECTED.fetch_add(text.len(), std::sync::atomic::Ordering::Relaxed);
}

pub fn take_injected() -> usize {
    INJECTED.swap(0, std::sync::atomic::Ordering::Relaxed)
}

pub fn render_context(event: &str, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if !matches!(current(), "hermes") || event == "SessionStart" || event == "UserPromptSubmit" {
        note_injected(text);
    }
    match current() {
        "cursor" => json!({"additional_context": text}).to_string(),
        "hermes" => {
            if event == "SessionStart" || event == "UserPromptSubmit" {
                json!({"context": text}).to_string()
            } else {
                String::new()
            }
        }
        _ => {
            if event == "SessionStart" || event == "UserPromptSubmit" {
                text.to_string()
            } else {
                json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}).to_string()
            }
        }
    }
}

/// `(stdout, stderr, exit code)` for this harness's allow/block contract.
///
/// Claude Code and Codex read the exit code and stderr; Cursor and Hermes read
/// a JSON decision on stdout and treat a non-zero exit as an error that allows.
pub fn render_decision(allow: bool, reason: &str) -> (String, String, i32) {
    match current() {
        "cursor" => {
            if allow {
                (json!({"permission": "allow"}).to_string(), String::new(), 0)
            } else {
                (
                    json!({"permission": "deny", "user_message": "Collide blocked this write",
                           "agent_message": reason})
                    .to_string(),
                    String::new(),
                    0,
                )
            }
        }
        "hermes" => {
            // nothing to say lets the call through: Hermes's `approve` is not
            // an allow, it sends the call to the person for approval
            if allow {
                (String::new(), String::new(), 0)
            } else {
                (json!({"action": "block", "message": reason}).to_string(), String::new(), 0)
            }
        }
        _ => {
            if allow {
                (String::new(), String::new(), 0)
            } else {
                (String::new(), reason.to_string(), 2)
            }
        }
    }
}
