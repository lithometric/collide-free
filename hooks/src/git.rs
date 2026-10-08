//! Git facts the hooks read cheaply, plus the continuous auto-pull.
//!
//! Branch and origin come straight out of `.git/` with no subprocess — free
//! inside the hook's budget. The sync does shell out, but the network fetch is
//! throttled to once per interval and every call is time-boxed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::load_json;

pub const SYNC_INTERVAL_S: f64 = 15.0;
pub const FETCH_TIMEOUT_S: u64 = 4;

/// The repo's current branch, read straight from `.git/HEAD`.
pub fn branch(root: &Path) -> String {
    // a linked worktree's .git is a file naming its own git dir
    let dot_git = root.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(text) => match text.trim().strip_prefix("gitdir:") {
            Some(dir) => {
                let dir = Path::new(dir.trim());
                if dir.is_absolute() { dir.to_path_buf() } else { root.join(dir) }
            }
            None => dot_git,
        },
        Err(_) => dot_git,
    };
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return String::new();
    };
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(name) => name.chars().take(80).collect(),
        None => head.chars().take(12).collect(),
    }
}

fn origin_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)\[remote "origin"\][^\[]*?\burl\s*=\s*(\S+)"#).expect("origin pattern")
    })
}

/// A git remote URL (or repo id) reduced to host/org/name, lowercased — the
/// shape a Collide repo id takes.
pub fn normalize_repo_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
    }
    s = s.replace(':', "/");
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.to_lowercase()
}

/// The repo id implied by git's `origin`, read from `.git/config`. Gates a
/// server-driven rename: a config is only repointed when this agrees.
pub fn origin_repo_id(root: &Path) -> String {
    // a worktree's .git is a file pointing at the main repository: git
    // itself knows where its config is
    let text = match std::fs::read_to_string(root.join(".git").join("config")) {
        Ok(text) => text,
        Err(_) if root.join(".git").is_file() => {
            let url = std::process::Command::new("git")
                .args(["-C", &root.to_string_lossy(), "config", "--get", "remote.origin.url"])
                .output()
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_default();
            if url.is_empty() {
                return String::new();
            }
            format!("[remote \"origin\"]\n\turl = {url}\n")
        }
        Err(_) => return String::new(),
    };
    origin_url_re()
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .filter(|url| !is_local_origin(url))
        .map(normalize_repo_url)
        .unwrap_or_default()
}

/// An origin that is a folder on this machine (a clone of a local repo, a
/// bare repo beside it) names no hosted repo: it is never a rename.
pub fn is_local_origin(url: &str) -> bool {
    let url = url.trim();
    let b = url.as_bytes();
    url.starts_with('/') || url.starts_with("./") || url.starts_with("../") || url.starts_with('~')
        || url.starts_with("file://") || url.starts_with('\\')
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn sha1_hex12(text: &str) -> String {
    // The stamp path only has to be stable and collision-free per checkout;
    // FNV-1a keyed on the full path is plenty and keeps the dependency list
    // at zero. Distinct from the Python stamp on purpose: the two
    // implementations must not share a throttle file.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:012x}")
}

/// Per-checkout scratch file for notes that must not repeat on every edit.
pub fn stamp_path(root: &Path, kind: &str) -> PathBuf {
    let digest = sha1_hex12(&root.to_string_lossy());
    std::env::temp_dir().join(format!("collide-rs-{kind}-{digest}.json"))
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn git(root: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = SystemTime::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_string(&mut out);
                }
                return if status.success() { Some(out) } else { None };
            }
            Ok(None) => {
                if SystemTime::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// Whether git IGNORES this path.
///
/// The session-start index walks `git ls-files`, so it never sees an ignored
/// file. The per-edit report had no such check: whatever the agent wrote, it
/// posted, and a gitignored scratch file entered the map for good. On this
/// repo that was nine phantom Python files out of 113, every one under a
/// throwaway harness directory.
///
/// `check-ignore` without `--no-index` says nothing about TRACKED files, which
/// is exactly right: a tracked file that happens to match a pattern is still
/// the user's code. Only 0 means ignored; 1 is "not ignored", 128 is an error,
/// and both of those fail toward reporting — being one file too generous is a
/// far smaller problem than a real edit going unobserved.
pub fn is_ignored(root: &Path, rel: &str) -> bool {
    let Ok(mut child) = Command::new("git")
        .args(["check-ignore", "-q", "--", rel])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = SystemTime::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code() == Some(0),
            Ok(None) => {
                if SystemTime::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return false,
        }
    }
}

/// Whether this checkout asked Collide to fast-forward it: `"auto_pull":
/// true` in the committed `.collide/config.json`, or `COLLIDE_AUTO_PULL=1`.
/// Off by default — changing someone's working tree is theirs to switch on.
pub fn auto_pull_enabled(root: &Path, env_value: &str) -> bool {
    if env_value.trim() == "1" {
        return true;
    }
    std::fs::read_to_string(root.join(".collide").join("config.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|config| config.get("auto_pull").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Files the incoming commits change that this checkout changed too: in its
/// working tree (uncommitted or untracked) or in commits not yet pushed.
fn overlap(root: &Path) -> Vec<String> {
    let names = |args: &[&str]| -> Vec<String> {
        git(root, args, Duration::from_secs(2))
            .map(|out| out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let incoming = names(&["diff", "--name-only", "HEAD...@{u}"]);
    if incoming.is_empty() {
        return Vec::new();
    }
    let mut mine: Vec<String> = names(&["diff", "--name-only", "@{u}...HEAD"]);
    mine.extend(names(&["diff", "--name-only", "HEAD"]));
    mine.extend(names(&["ls-files", "--others", "--exclude-standard"]));
    incoming.into_iter().filter(|path| mine.contains(path)).collect()
}

/// Git sync. Once per interval the network fetch runs synchronously
/// (bounded) so this hook fire sees teammates' just-pushed commits. By
/// default Collide only TELLS the agent the checkout is behind, once per
/// behind-count; the working tree is never touched. With auto-pull on it
/// fast-forwards — `--ff-only` is the safety: a dirty tree or a diverged
/// branch aborts untouched and downgrades to advising the agent.
pub fn sync_note(root: &Path, auto_pull: bool) -> String {
    let stamp = stamp_path(root, "sync");
    let mut state = load_json(&stamp);
    let now = now_secs();
    let last = state.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    if now - last >= SYNC_INTERVAL_S {
        if let Some(map) = state.as_object_mut() {
            map.insert("ts".into(), json!(now));
        }
        let _ = std::fs::write(&stamp, state.to_string());
        let _ = git(root, &["fetch", "origin", "--quiet"], Duration::from_secs(FETCH_TIMEOUT_S));
    }

    let behind: i64 = git(root, &["rev-list", "--count", "HEAD..@{u}"], Duration::from_secs(2))
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0);
    if behind <= 0 {
        return String::new();
    }

    // who pushed the incoming commits, captured BEFORE the ff-merge while the
    // range still names exactly the pulled commits
    let mut authors = String::new();
    if let Some(log) = git(
        root,
        &["log", "HEAD..@{u}", "--format=%an", "-n", "30"],
        Duration::from_secs(2),
    ) {
        let mut names: Vec<String> = Vec::new();
        for name in log.lines() {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        if !names.is_empty() {
            let shown = names.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            let ellipsis = if names.len() > 4 { "…" } else { "" };
            authors = format!(" from {shown}{ellipsis}");
        }
    }

    if !auto_pull {
        // only when the incoming commits change a file this checkout changed:
        // anything else the push's rebase takes in, and a teammate's change
        // to code the agent reads arrives as a delta. Study 5's agents pulled
        // on every nudge before they had edited anything.
        let shared = overlap(root);
        if shared.is_empty() {
            return String::new();
        }
        if state.get("told").and_then(Value::as_i64) == Some(behind) {
            return String::new(); // told about this state already
        }
        if let Some(map) = state.as_object_mut() {
            map.insert("told".into(), json!(behind));
        }
        let _ = std::fs::write(&stamp, state.to_string());
        let ellipsis = if shared.len() > 3 { "…" } else { "" };
        let files = shared.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        return format!(
            "Collide git-sync: origin is {behind} commit(s) ahead{authors}, changing {files}{ellipsis} \
that you changed here too. Commit, `git pull --rebase`, and re-read those files before you \
go on; or set \"auto_pull\": true in .collide/config.json to have Collide fast-forward this \
checkout for you."
        );
    }
    if git(root, &["merge", "--ff-only", "@{u}"], Duration::from_secs(5)).is_some() {
        return format!(
            "Collide git-sync: auto-pulled {behind} commit(s){authors} into this checkout. \
Files may have changed on disk — re-read anything you loaded before this point instead of \
editing from memory, and check since_your_last_call/get_briefing for who landed what (each \
carries a resolved identity: nickname / @username / email)."
        );
    }

    if state.get("warned").and_then(Value::as_i64) == Some(behind) {
        return String::new(); // same stuck state: one warning, not one per tool call
    }
    if let Some(map) = state.as_object_mut() {
        map.insert("warned".into(), json!(behind));
    }
    let _ = std::fs::write(&stamp, state.to_string());
    format!(
        "Collide git-sync: origin is {behind} commit(s) ahead but auto-pull could not \
fast-forward (uncommitted overlapping changes or a diverged branch). Reconcile at your next \
safe moment: commit or stash, then `git pull --ff-only`."
    )
}

#[cfg(test)]
mod ignore_tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(root)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn ignored_untracked_and_tracked_paths_are_told_apart() {
        let root = std::env::temp_dir().join(format!("collide-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        sh(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "scratch/\n").unwrap();
        std::fs::write(root.join("kept.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("new.py"), "y = 2\n").unwrap();
        std::fs::write(root.join("scratch").join("junk.py"), "z = 3\n").unwrap();
        sh(&root, &["add", ".gitignore", "kept.py"]);

        assert!(is_ignored(&root, "scratch/junk.py"), "a gitignored path is ignored");
        assert!(!is_ignored(&root, "kept.py"), "a tracked path is never ignored");
        assert!(!is_ignored(&root, "new.py"), "untracked but not ignored is still the user's code");
        assert!(!is_ignored(&root, "does/not/exist.py"), "an absent path fails toward reporting");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn origin_ahead_is_told_only_when_it_touches_what_this_checkout_changed() {
        let base = std::env::temp_dir().join(format!("collide-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "seed"]);
        let seed = base.join("seed");
        sh(&seed, &["config", "user.email", "t@t"]);
        sh(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-qm", "one"]);
        sh(&seed, &["push", "-q", "-u", "origin", "HEAD"]);
        sh(&base, &["clone", "-q", "origin.git", "clone"]);
        let clone = base.join("clone");
        std::fs::write(seed.join("a.txt"), "two\n").unwrap();
        sh(&seed, &["commit", "-qam", "two"]);
        sh(&seed, &["push", "-q", "origin", "HEAD"]);
        sh(&clone, &["fetch", "-q", "origin"]);
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));

        std::fs::write(clone.join("b.txt"), "mine\n").unwrap();
        assert_eq!(sync_note(&clone, false), "", "nothing incoming touches what this checkout changed");
        std::fs::write(clone.join("a.txt"), "one, edited here\n").unwrap();
        let note = sync_note(&clone, false);
        assert!(note.contains("origin is 1 commit(s) ahead") && note.contains("changing a.txt"), "{note}");
        assert_eq!(sync_note(&clone, false), "", "told once");
        let _ = std::fs::remove_file(stamp_path(&clone, "sync"));
        let _ = std::fs::remove_dir_all(&base);
    }
}

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The repo's local branch names, for the dashboard's branch switcher.
pub fn local_branches(root: &Path) -> Vec<String> {
    git_lines(root, &["for-each-ref", "--count=200", "--format=%(refname:short)", "refs/heads"])
        .into_iter()
        .map(|b| b.trim().chars().take(80).collect::<String>())
        .filter(|b| !b.is_empty())
        .collect()
}

/// The repo's worktrees by their folders' names (never the paths above
/// them), for the dashboard's worktree switcher.
pub fn worktree_names(root: &Path) -> Vec<String> {
    git_lines(root, &["worktree", "list", "--porcelain"])
        .into_iter()
        .filter_map(|line| line.strip_prefix("worktree ").map(|p| p.trim().to_string()))
        .filter_map(|p| Path::new(&p).file_name().and_then(|n| n.to_str()).map(|n| n.chars().take(120).collect::<String>()))
        .take(100)
        .collect()
}
