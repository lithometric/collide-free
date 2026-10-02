//! Reports the server never got, sent again.
//!
//! A write's report can fail: the network drops, the server is between
//! deploys, a request times out. The agent must never wait on it, so the
//! hook moves on — and before this, that edit was simply missing from the
//! map, the teammates' deltas and the ledger. Now the path is remembered
//! here, and the next hook run in the repo sends it again first.
//!
//! What is sent again is the file as it is NOW, not the content that
//! failed: a stale copy arriving after a newer one would roll the map back.
//! And a path is remembered once, however many times it failed, so the
//! spool stays small. A file that is gone by then is dropped.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::{self, Env};

/// At most this many paths are kept; the oldest go first.
const MAX_PATHS: usize = 500;
/// A path older than this is dropped rather than sent: its session is long
/// over and the next session's index covers the file.
const MAX_AGE_S: f64 = 86_400.0;
/// At most this many resends per hook run, and at most this share of the
/// run's budget: the agent's own report comes first.
const MAX_RESENDS: usize = 20;

fn spool_path(env: &Env, server: &str, repo_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{server}\n{repo_id}").as_bytes());
    let tag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    PathBuf::from(crate::check::home(env)).join(".collide").join("spool").join(format!("{tag}.json"))
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn load(path: &Path) -> Map<String, Value> {
    config::load_json(path).as_object().cloned().unwrap_or_default()
}

fn save(path: &Path, entries: &Map<String, Value>) {
    if entries.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    crate::report::save_json(path, &Value::Object(entries.clone()));
}

/// A report for `rel` did not reach the server: remember the path and the
/// session fields it was sent with.
pub fn remember(env: &Env, server: &str, repo_id: &str, rel: &str, meta: &Map<String, Value>) {
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    entries.insert(rel.to_string(), json!({"ts": now(), "meta": meta}));
    if entries.len() > MAX_PATHS {
        let mut by_age: Vec<(String, f64)> =
            entries.iter().map(|(k, v)| (k.clone(), v.get("ts").and_then(Value::as_f64).unwrap_or(0.0))).collect();
        by_age.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (k, _) in by_age.into_iter().take(entries.len() - MAX_PATHS) {
            entries.remove(&k);
        }
    }
    save(&path, &entries);
}

/// A report for `rel` reached the server: nothing about it is owed.
pub fn forget(env: &Env, server: &str, repo_id: &str, rel: &str) {
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    if entries.remove(rel).is_some() {
        save(&path, &entries);
    }
}

/// Send what is owed, oldest first, within `budget`. `build` makes the
/// payload for a path from its current content and the remembered fields
/// (None when the file is gone or not reportable). Stops at the first
/// failure: the server is still unreachable. Returns how many were sent.
pub fn resend(
    env: &Env, server: &str, repo_id: &str, token: &str, user_agent: &str, root: &Path, budget: Duration,
    build: &dyn Fn(&str, &str, &Map<String, Value>) -> Option<Value>,
) -> usize {
    let path = spool_path(env, server, repo_id);
    let mut entries = load(&path);
    if entries.is_empty() {
        return 0;
    }
    let started = Instant::now();
    let mut order: Vec<(String, f64)> =
        entries.iter().map(|(k, v)| (k.clone(), v.get("ts").and_then(Value::as_f64).unwrap_or(0.0))).collect();
    order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut sent = 0;
    for (rel, ts) in order.into_iter().take(MAX_RESENDS) {
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        let meta = entries.get(&rel).and_then(|e| e.get("meta")).and_then(Value::as_object).cloned().unwrap_or_default();
        let full = root.join(&rel);
        let content = match std::fs::read(&full) {
            Ok(bytes) if now() - ts <= MAX_AGE_S => String::from_utf8_lossy(&bytes).into_owned(),
            // gone, or too old to matter: owed no longer
            _ => {
                entries.remove(&rel);
                continue;
            }
        };
        let Some(payload) = build(&rel, &content, &meta) else {
            entries.remove(&rel);
            continue;
        };
        match crate::http::post(server, "/report", token, user_agent, &payload, left) {
            Ok(_) => {
                entries.remove(&rel);
                sent += 1;
            }
            Err(()) => break,
        }
    }
    save(&path, &entries);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path) -> Env {
        let mut e = Env::default();
        e.insert("HOME".into(), home.to_string_lossy().to_string());
        e
    }

    #[test]
    fn a_failed_report_is_sent_again_with_the_files_current_content() {
        let home = std::env::temp_dir().join(format!("spool-{}", std::process::id()));
        let root = home.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let env = env(&home);
        std::fs::write(root.join("a.py"), "v1").unwrap();
        let mut meta = Map::new();
        meta.insert("session".into(), json!("s1"));
        let server = "http://127.0.0.1:9"; // nothing listens: every send fails
        remember(&env, server, "r", "gone.py", &meta); // oldest: visited first
        std::thread::sleep(Duration::from_millis(5));
        remember(&env, server, "r", "a.py", &meta);
        remember(&env, server, "r", "a.py", &meta); // once, however often it failed
        assert_eq!(load(&spool_path(&env, server, "r")).len(), 2);
        assert!(load(&spool_path(&env, "http://other", "r")).is_empty(), "another server has its own spool");
        // the file moved on before the resend: the NEW content goes
        std::fs::write(root.join("a.py"), "v2").unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        let build = |rel: &str, content: &str, meta: &Map<String, Value>| {
            seen.borrow_mut().push((rel.to_string(), content.to_string(), meta.get("session").cloned()));
            Some(json!({"path": rel, "content": content}))
        };
        let sent = resend(&env, server, "r", "t", "ua", &root, Duration::from_millis(500), &build);
        assert_eq!(sent, 0);
        assert_eq!(seen.borrow()[0], ("a.py".to_string(), "v2".to_string(), Some(json!("s1"))));
        // still owed after the failure; the gone file is not
        let left = load(&spool_path(&env, server, "r"));
        assert!(left.contains_key("a.py") && !left.contains_key("gone.py"), "{left:?}");
        forget(&env, server, "r", "a.py");
        assert!(!spool_path(&env, server, "r").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
