//! `collide-hook setup <server> <code>`: the second half of one-command
//! setup (the server's setupone.rs is the first). Sends the repo's current
//! settings, MCP config, rules files and .gitignore to the server, writes
//! back the merged files it returns, installs the credential that came with
//! the code, and stages everything. It never commits and never pushes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::Env;

const SENT: [&str; 5] = [".claude/settings.json", ".mcp.json", "AGENTS.md", "CLAUDE.md", ".gitignore"];

fn repo_root() -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Only paths the server may write: relative, inside the repo, no `..`.
fn safe(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.split('/').any(|part| part == ".." || part.is_empty())
}

pub fn run(server: &str, code: &str, env: &Env) -> i32 {
    let server = server.trim_end_matches('/');
    if server.is_empty() || code.is_empty() {
        eprintln!("usage: collide-hook setup <server> <code>");
        return 2;
    }
    let root = repo_root();
    let mut files = Map::new();
    for rel in SENT {
        files.insert(rel.into(), std::fs::read_to_string(root.join(rel)).map(Value::String).unwrap_or(Value::Null));
    }
    let answer = match crate::http::post(
        server, "/setup/apply", "", &crate::report::user_agent(), &json!({"code": code, "files": files}),
        Duration::from_secs(30),
    ) {
        Ok(answer) => answer,
        Err(()) => {
            eprintln!("Collide setup: could not reach {server}.");
            return 1;
        }
    };
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        eprintln!("Collide setup: {}", answer.get("error").and_then(Value::as_str).unwrap_or("the server refused"));
        return 1;
    }
    let mut written: Vec<String> = Vec::new();
    for (rel, content) in answer.get("write").and_then(Value::as_object).cloned().unwrap_or_default() {
        let Some(content) = content.as_str() else { continue };
        if !safe(&rel) {
            eprintln!("Collide setup: refused to write {rel}");
            continue;
        }
        let path = root.join(&rel);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, content).is_ok() {
            written.push(rel);
        }
    }
    // the credential that came with the code, merged like add-credentials
    let credential_note = match answer.get("credentials").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())) {
        Some(credentials) => {
            let tmp = std::env::temp_dir().join(format!("collide-credential-{}.json", std::process::id()));
            let ok = std::fs::write(&tmp, credentials.to_string()).is_ok()
                && crate::report::add_credentials(&tmp.to_string_lossy(), env) == 0;
            let _ = std::fs::remove_file(&tmp);
            if ok { "Installed your hook credential in ~/.collide/credentials.json (never committed)." }
            else { "Could not install the hook credential: run setup again." }
        }
        None => "No credential came with this code.",
    };
    let staged = std::process::Command::new("git")
        .current_dir(&root)
        .arg("add")
        .arg("--")
        .args(&written)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!("Collide is set up in {}.", root.display());
    println!("Wrote: {}", written.join(", "));
    println!("{credential_note}");
    if staged {
        println!("Staged. Commit with: {}", answer.get("commit").and_then(Value::as_str).unwrap_or("git commit -m \"Add Collide\""));
    } else {
        println!("Not a git repository, or git add failed: commit these files yourself.");
    }
    let _ = Path::new(".");
    0
}

#[cfg(test)]
mod tests {
    use super::safe;

    #[test]
    fn only_paths_inside_the_repo_are_written() {
        assert!(safe(".claude/settings.json"));
        assert!(safe("AGENTS.md"));
        assert!(!safe("../outside"));
        assert!(!safe("/etc/passwd"));
        assert!(!safe(".collide/../../x"));
        assert!(!safe(""));
    }
}
