//! Two sessions in one checkout share one git index. A `git commit` takes
//! whatever is staged, so one session's commit could carry another's
//! half-done work (a staged `git rm`, an edit it has not finished) and, with
//! a push, ship it. The gate catches the commit (and `git add -A`,
//! `git commit -a`) before it runs: when what it would take includes
//! uncommitted work another live session in the SAME folder made, it is
//! held, with the command that commits only this session's own files.
//!
//! Each session's own files are what its hooks saw it write
//! (`written_paths`) and what it staged or removed through git
//! (`staged_paths`); its folder is recorded the first time its gate runs.
//! Separate worktrees are separate folders and never meet here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Env};

/// How recent another session's record must be to count as live work.
const LIVE_WINDOW_S: f64 = 12.0 * 3600.0;
const NAMED: usize = 8;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

fn toplevel(dir: &Path) -> Option<PathBuf> {
    git_lines(dir, &["rev-parse", "--show-toplevel"]).into_iter().next().map(PathBuf::from)
}

/// One simple command's words, quotes honoured (`-m 'a b'` is two words).
fn words(segment: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => word.push(c),
        }
    }
    if started || !word.is_empty() {
        out.push(word);
    }
    out
}

/// The simple commands of a shell line (split on `;`, `&&`, `||`, `|`).
fn segments(command: &str) -> Vec<Vec<String>> {
    crate::shell::shell_segments(command).iter().map(|segment| words(segment)).collect()
}

/// `git <sub> ...` as `(sub, rest)`, skipping `git -C dir` and `-c k=v`.
fn git_call(words: &[String]) -> Option<(String, Vec<String>)> {
    let start = words.iter().position(|w| w == "git")?;
    let mut i = start + 1;
    while i < words.len() && (words[i] == "-C" || words[i] == "-c") {
        i += 2;
    }
    let sub = words.get(i)?.clone();
    Some((sub, words[i + 1..].to_vec()))
}

/// The paths a `git add/rm/mv` names (not `.`, `-A` or a glob: those are
/// sweeps, not claims), repo-relative.
fn named_paths(rest: &[String], cwd: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut after_dashes = false;
    for word in rest {
        if word == "--" {
            after_dashes = true;
            continue;
        }
        if (!after_dashes && word.starts_with('-')) || word == "." || word.contains('*') {
            continue;
        }
        let path = if Path::new(word).is_absolute() { PathBuf::from(word) } else { cwd.join(word) };
        if let Some(rel) = config::rel_path(&config::normpath(&path), root) {
            out.push(rel);
        }
    }
    out
}

/// Remember this session's folder, and the paths it stages or removes
/// through git, from the command its gate is looking at. Quiet and cheap:
/// one small file, written only when something is new.
pub fn note(hook_input: &Value, cwd: &Path, env: &Env) {
    let session = text(hook_input, "session_id");
    if session.is_empty() {
        return;
    }
    let path = crate::check::state_path(&session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut changed = false;
    let command = hook_input.get("tool_input").map(|i| text(i, "command")).unwrap_or_default();
    let is_git = command.contains("git ");
    if !state.contains_key("root") || is_git {
        let Some(root) = toplevel(cwd) else { return };
        if text(&Value::Object(state.clone()), "root") != root.to_string_lossy() {
            state.insert("root".into(), json!(root.to_string_lossy()));
            changed = true;
        }
        if is_git {
            let mut staged: Vec<String> = state.get("staged_paths").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            for words in segments(&command) {
                let Some((sub, rest)) = git_call(&words) else { continue };
                if !matches!(sub.as_str(), "add" | "rm" | "mv" | "restore") {
                    continue;
                }
                for rel in named_paths(&rest, cwd, &root) {
                    if !staged.contains(&rel) {
                        staged.push(rel);
                        changed = true;
                    }
                }
            }
            if staged.len() > 400 {
                staged.drain(..staged.len() - 400);
            }
            state.insert("staged_paths".into(), json!(staged));
        }
    }
    if changed {
        state.insert("seen".into(), json!(now_s()));
        let _ = std::fs::write(&path, Value::Object(state).to_string());
    }
}

fn paths_of(state: &Value) -> BTreeSet<String> {
    ["written_paths", "staged_paths"]
        .iter()
        .flat_map(|key| state.get(*key).and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Another live session's claim on paths in this folder.
struct Other {
    tag: String,
    age_s: f64,
    paths: BTreeSet<String>,
}

fn others(root: &Path, session: &str, env: &Env) -> Vec<Other> {
    let mine = crate::check::state_path(session, env);
    let dir = mine.parent().map(Path::to_path_buf).unwrap_or_default();
    let now = now_s();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path == mine || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let age = entry.metadata().and_then(|m| m.modified()).ok()
            .and_then(|t| t.elapsed().ok()).map(|d| d.as_secs_f64()).unwrap_or(f64::MAX);
        if age > LIVE_WINDOW_S {
            continue;
        }
        let state = config::load_json(&path);
        if text(&state, "root") != root.to_string_lossy() {
            continue;
        }
        let paths = paths_of(&state);
        if !paths.is_empty() {
            let tag = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            out.push(Other { tag, age_s: age.min(now), paths });
        }
    }
    out
}

/// What a sweeping command would take into the commit: `None` when it is
/// not one (a plain command, or a commit limited to named paths).
fn would_take(command: &str, root: &Path) -> Option<BTreeSet<String>> {
    let mut sweep = false;
    let mut all_tracked = false;
    let mut everything = false;
    for words in segments(command) {
        let Some((sub, rest)) = git_call(&words) else { continue };
        match sub.as_str() {
            "commit" => {
                // `git commit -- a b` (or `git commit a b`) takes only those
                let mut named = false;
                let mut flags_done = false;
                let mut i = 0;
                while i < rest.len() {
                    let w = rest[i].as_str();
                    if flags_done || !w.starts_with('-') || w == "-" {
                        if w != "-" {
                            named = true;
                        }
                    } else if w == "--" {
                        flags_done = true;
                    } else if let Some(long) = w.strip_prefix("--") {
                        if long == "all" {
                            all_tracked = true;
                        }
                        let takes = ["message", "file", "author", "date", "reuse-message", "reedit-message", "fixup", "squash", "template", "cleanup", "trailer"];
                        if !long.contains('=') && takes.contains(&long) {
                            i += 1;
                        }
                    } else {
                        let cluster = &w[1..];
                        if cluster.contains('a') {
                            all_tracked = true;
                        }
                        // a cluster ending in a flag that takes a value: the next word is it
                        if cluster.ends_with(['m', 'F', 'C', 'c', 't']) {
                            i += 1;
                        }
                    }
                    i += 1;
                }
                if !named {
                    sweep = true;
                }
            }
            "add" if rest.iter().any(|w| matches!(w.as_str(), "-A" | "--all" | "." | "-u" | "--update")) => {
                everything = true;
                sweep = true;
            }
            _ => {}
        }
    }
    if !sweep {
        return None;
    }
    let mut take: BTreeSet<String> = git_lines(root, &["diff", "--cached", "--name-only"]).into_iter().collect();
    if all_tracked || everything {
        take.extend(git_lines(root, &["diff", "--name-only"]));
    }
    if everything {
        take.extend(git_lines(root, &["ls-files", "--others", "--exclude-standard"]));
    }
    Some(take)
}

fn ago(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s < 90 { "just now".into() } else if s < 5400 { format!("{}m ago", s / 60) } else { format!("{}h ago", s / 3600) }
}

/// The gate's check for a Bash command: `Some(reason)` holds it.
pub fn commit_guard(stdin_data: &str, env: &Env) -> Option<String> {
    if config::get(env, "COLLIDE_SHARED_TREE") == "0" {
        return None;
    }
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if !matches!(text(&input, "tool_name").as_str(), "Bash" | "shell" | "run_terminal_cmd") {
        return None;
    }
    let command = input.get("tool_input").map(|i| text(i, "command")).unwrap_or_default();
    // the user said to commit them together: the agent puts this in front
    if command.contains("COLLIDE_SHARED_TREE=0") {
        return None;
    }
    static GIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    if !GIT.get_or_init(|| Regex::new(r"\bgit\b[^;&|]*\b(commit|add)\b").expect("static regex")).is_match(&command) {
        return None;
    }
    let session = text(&input, "session_id");
    let cwd = PathBuf::from(text(&input, "cwd"));
    let cwd = if cwd.as_os_str().is_empty() { std::env::current_dir().ok()? } else { cwd };
    let root = toplevel(&cwd)?;
    let others = others(&root, &session, env);
    if others.is_empty() {
        return None;
    }
    let take = would_take(&command, &root)?;
    let mine = paths_of(&config::load_json(&crate::check::state_path(&session, env)));
    let mut theirs: Vec<(String, f64, Vec<String>)> = Vec::new();
    for other in &others {
        let hit: Vec<String> = take.iter().filter(|p| other.paths.contains(*p) && !mine.contains(*p)).cloned().collect();
        if !hit.is_empty() {
            theirs.push((other.tag.clone(), other.age_s, hit));
        }
    }
    if theirs.is_empty() {
        return None;
    }
    let foreign: BTreeSet<&String> = theirs.iter().flat_map(|(_, _, h)| h.iter()).collect();
    let own: Vec<&String> = take.iter().filter(|p| !foreign.contains(p)).collect();
    let who: Vec<String> = theirs
        .iter()
        .map(|(tag, age, hit)| {
            let mut named: Vec<String> = hit.iter().take(NAMED).cloned().collect();
            if hit.len() > NAMED {
                named.push(format!("{} more", hit.len() - NAMED));
            }
            format!("session {tag} (active {}): {}", ago(*age), named.join(", "))
        })
        .collect();
    let instead = if own.is_empty() {
        "Nothing staged here is yours: do not commit for them.".to_string()
    } else {
        let list: Vec<String> = own.iter().map(|p| shell_word(p)).collect();
        format!("Commit only your own files instead: git commit -m \"...\" -- {}", list.join(" "))
    };
    Some(format!(
        "Collide held this command: another Claude session working in this same folder ({}) has uncommitted work it would sweep in:\n  {}\n\
That work is theirs, half done or not; committing it ships it under your commit. {instead}\n\
Leave their changes staged and untouched (never unstage, restore or stash them). \
Tell the user in one line which session's files you left out. If the user wants them committed together, \
they will say so; then run the command with COLLIDE_SHARED_TREE=0 in front.",
        root.display(),
        who.join("\n  "),
    ))
}

fn shell_word(path: &str) -> String {
    if path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        assert!(Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null())
            .status().unwrap().success(), "git {args:?}");
    }

    fn input(session: &str, cwd: &Path, command: &str) -> String {
        json!({"tool_name": "Bash", "session_id": session, "cwd": cwd, "tool_input": {"command": command}}).to_string()
    }

    #[test]
    fn a_commit_that_would_sweep_in_another_sessions_staged_removal_is_held() {
        let base = std::env::temp_dir().join(format!("collide-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut env = Env::new();
        env.insert("HOME".into(), base.to_string_lossy().to_string());
        std::fs::create_dir_all(base.join(".collide").join("check")).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        sh(&repo, &["config", "user.name", "t"]);
        for f in ["old_view.js", "app.js", "mine.js"] {
            std::fs::write(repo.join(f), "x\n").unwrap();
        }
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "init"]);
        let repo = config::normpath(&toplevel(&repo).unwrap());

        // session A removes the old view through git; its gate saw the command
        let remove = input("aaaaaaaa-1", &repo, "git rm old_view.js");
        note(&serde_json::from_str(&remove).unwrap(), &repo, &env);
        sh(&repo, &["rm", "-q", "old_view.js"]);
        // session B edits its own file and stages it
        std::fs::write(repo.join("mine.js"), "y\n").unwrap();
        let add = input("bbbbbbbb-2", &repo, "git add mine.js");
        note(&serde_json::from_str(&add).unwrap(), &repo, &env);
        sh(&repo, &["add", "mine.js"]);

        let held = commit_guard(&input("bbbbbbbb-2", &repo, "git commit -m 'Ship it' && git push"), &env).unwrap();
        assert!(held.contains("session aaaaaaaa"), "{held}");
        assert!(held.contains("old_view.js"));
        assert!(held.contains("git commit -m \"...\" -- mine.js"), "{held}");
        // committing only its own paths is fine, and so is A's own commit
        assert!(commit_guard(&input("bbbbbbbb-2", &repo, "git commit -m x -- mine.js"), &env).is_none());
        assert!(commit_guard(&input("aaaaaaaa-1", &repo, "git commit -m 'Remove the old view' -- old_view.js"), &env).is_none());
        // and the user can say so
        assert!(commit_guard(&input("bbbbbbbb-2", &repo, "COLLIDE_SHARED_TREE=0 git commit -m x"), &env).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sweeps_are_told_apart_from_commits_of_named_paths() {
        let dir = std::env::temp_dir();
        assert!(would_take("git commit -m 'a b' -- x.py", &dir).is_none());
        assert!(would_take("git commit -m msg x.py", &dir).is_none());
        assert!(would_take("git status && git diff", &dir).is_none());
        assert!(would_take("git add src/a.py", &dir).is_none());
        assert!(would_take("git commit -m msg", &dir).is_some());
        assert!(would_take("git add -A && git commit -qm wip", &dir).is_some());
        assert!(would_take("cd sub && git commit -am wip", &dir).is_some());
    }
}
