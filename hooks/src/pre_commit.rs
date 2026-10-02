//! The git pre-commit gate — the universal chokepoint.
//!
//! Unlike an agent's PreToolUse hook (one harness only), a git hook fires for
//! ANY agent and any human: git is the one place every writer funnels through.
//! This blocks a commit whose staged files land on a symbol another agent has
//! a confidence-certain, in-flight collision with.
//!
//! Exit 0 allows, exit 1 blocks with the collisions on stderr. HARD FAIL-OPEN:
//! any error, timeout, missing config or unreachable server allows the commit.
//! A gate must never wedge your ability to commit; override a genuine false
//! positive with `git commit --no-verify`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;

const TOTAL_BUDGET: Duration = Duration::from_secs(3);
const MAX_FILES: usize = 50;
const LANGUAGE_ALLOWLIST: [&str; 16] = [
    ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".rb", ".php", ".cs", ".cpp",
    ".c", ".h", ".css", ".md",
];

/// Paths staged for this commit (added/copied/modified/renamed), filtered to
/// the language allowlist — repo-relative, exactly how Collide keys them.
fn staged_files(root: &Path) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACMR"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| LANGUAGE_ALLOWLIST.iter().any(|ext| line.ends_with(ext)))
        .map(str::to_string)
        .collect()
}

pub fn run(env: &Env) -> (i32, String) {
    let started = Instant::now();
    let cwd = {
        let pwd = config::get(env, "PWD");
        if pwd.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(pwd)
        }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or(cwd);

    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return (0, String::new());
    }

    let mut blocked: Vec<(String, String)> = Vec::new();
    for path in staged_files(&root).into_iter().take(MAX_FILES) {
        let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        // fail open per file: one slow check cannot wedge the commit
        let Ok(result) = http::post(
            &cfg.server,
            "/gate",
            &cfg.token,
            "collide-pre-commit/1",
            &json!({"repo_id": cfg.repo_id, "path": path}),
            remaining,
        ) else {
            continue;
        };
        if !result.get("allow").and_then(Value::as_bool).unwrap_or(true) {
            let reason = result
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .unwrap_or("a certain collision touches this path")
                .to_string();
            blocked.push((path, reason));
        }
    }
    if blocked.is_empty() {
        return (0, String::new());
    }
    let listing = blocked
        .iter()
        .map(|(path, reason)| format!("  {path}: {reason}"))
        .collect::<Vec<_>>()
        .join("\n");
    (
        1,
        format!(
            "Collide blocked this commit — {} staged file(s) collide with in-flight work:\n\
{listing}\n\
Reconcile with the latest names/signatures (check_collisions), then re-commit. To override a \
false positive: git commit --no-verify\n",
            blocked.len()
        ),
    )
}
