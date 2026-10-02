//! PreToolUse: ask Collide's /gate whether this write may proceed.
//!
//! Exit 0 allows, exit 2 blocks with the reason on stderr so the agent can
//! adapt. HARD FAIL-OPEN: the whole thing is budgeted at 200ms and any error,
//! timeout, missing configuration or non-200 response allows the write. The
//! server being down must never stop a write.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::shell;

pub const TOTAL_BUDGET: Duration = Duration::from_millis(200);

const GATE_TOOLS: [&str; 7] = [
    "Edit", "Write", "MultiEdit", "apply_patch", "Bash", "shell", "run_terminal_cmd",
];
const SHELL_TOOLS: [&str; 3] = ["Bash", "shell", "run_terminal_cmd"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Absolute paths this write touches: one for Claude Edit/Write, possibly
/// several for a Codex apply_patch, or the likely targets of a shell command.
fn edit_targets(tool_input: &Value, cwd: &Path, tool: &str) -> Vec<PathBuf> {
    let direct = {
        let file_path = text(tool_input, "file_path");
        if file_path.is_empty() { text(tool_input, "notebook_path") } else { file_path }
    };
    if !direct.is_empty() {
        return vec![PathBuf::from(direct)];
    }
    let command = text(tool_input, "command");
    if command.contains("*** ") {
        return shell::abs_targets(&shell::apply_patch_paths(&command), cwd);
    }
    if SHELL_TOOLS.contains(&tool) && !command.is_empty() {
        return shell::shell_gate_paths(&command, cwd);
    }
    Vec::new()
}

/// A file about to be CREATED whose name starts with a number, in a folder
/// that already numbers its files (a migration, an ADR): the highest number
/// already there, so the server can claim this one or steer to the next.
/// `None` for everything else, which is almost every write.
fn numbered_new_file(target: &Path) -> Option<i64> {
    if target.exists() {
        return None;
    }
    let leading = |name: &str| -> Option<i64> {
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty() && digits.len() <= 8).then(|| digits.parse().ok()).flatten()
    };
    leading(target.file_name()?.to_str()?)?;
    let siblings = std::fs::read_dir(target.parent()?).ok()?;
    siblings
        .flatten()
        .filter_map(|entry| leading(entry.file_name().to_str()?))
        .max()
}

/// Returns (exit_code, stderr_text). Never panics; every failure allows.
pub fn run(stdin_data: &str, env: &Env) -> (i32, String) {
    let started = Instant::now();
    let Ok(hook_input) = serde_json::from_str::<Value>(stdin_data).map(crate::harness::normalize) else {
        return (0, String::new());
    };
    let tool = text(&hook_input, "tool_name");
    if !GATE_TOOLS.contains(&tool.as_str()) {
        return (0, String::new());
    }
    let tool_input = hook_input.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = {
        let from_input = text(&hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let targets = edit_targets(&tool_input, &cwd, &tool);
    if targets.is_empty() {
        return (0, String::new());
    }

    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        targets[0].parent().map(Path::to_path_buf),
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });

    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return (0, String::new());
    }

    // gate every file the write touches (a Codex apply_patch can hit several);
    // the first certain collision blocks the whole tool call
    for target in &targets {
        let Some(path) = config::rel_path(target, &root) else { continue };
        let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
        if remaining.is_zero() {
            return (0, String::new());
        }
        let mut payload = json!({"repo_id": cfg.repo_id, "path": path, "session": text(&hook_input, "session_id")});
        if let Some(after) = numbered_new_file(target) {
            payload["claim"] = json!({"after": after});
        }
        let Ok(response) = http::post(
            &cfg.server,
            "/gate",
            &cfg.token,
            "collide-gate-hook/1",
            &payload,
            remaining,
        ) else {
            continue;
        };
        let allow = response.get("allow").and_then(Value::as_bool).unwrap_or(true);
        if !allow {
            let reason = {
                let stated = text(&response, "reason");
                if stated.is_empty() {
                    "Collide: a certain collision touches this path.".to_string()
                } else {
                    stated
                }
            };
            return (2, format!("Collide blocked this write ({path}): {reason}\nTell the user who is on it and what you will do instead.\n"));
        }
    }
    (0, String::new())
}
