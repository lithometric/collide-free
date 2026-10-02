//! `collide-hook land`: get the agent's commits into origin without the model.
//!
//! Study 5 measured where a concurrent agent's session goes once Collide has
//! cut the searching: over half its tool calls were landing. Push, rejected,
//! pull --rebase, rerun the tests, push, rejected again: each lap a message
//! that replays the whole context. This runs that loop as a subprocess and
//! answers once:
//!
//! 1. take the repo's push turn from the server, so concurrent agents queue
//!    instead of racing (fail open: no server, no turn, land anyway);
//! 2. fetch; when teammates' commits came in, rebase onto them. A conflict
//!    aborts cleanly and hands back only the conflicting hunks;
//! 3. adapt the agent's own commits to a teammate's rename that has landed
//!    (the lint's registry names it; git confirms origin no longer uses the
//!    old name), as its own commit;
//! 4. when anything came in, run the repo's tests: the agent tested its own
//!    tree already, not the merge;
//! 5. push; a lost race goes round again, up to four times.
//!
//! The gate turns an agent's plain `git push` into this (`rewrite`), so no
//! agent has to learn a command. Native only: the Python fallback hook never
//! rewrites, and a push there stays a push.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::report::user_agent;

const ATTEMPTS: usize = 4;
const MAX_TURN_WAIT_S: f64 = 240.0;
const TEST_MAX_S: f64 = 600.0;
const CONFLICT_LINES: usize = 40;
const TEST_TAIL_LINES: usize = 40;

/// (ok, stdout, stderr) of one git command, bounded.
fn git(root: &Path, args: &[&str], timeout_s: u64) -> (bool, String, String) {
    let child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else { return (false, String::new(), "git could not start".into()) };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(timeout_s) => {
                let _ = child.kill();
                let _ = child.wait();
                return (false, String::new(), format!("git {} timed out", args.join(" ")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return (false, String::new(), String::new()),
        }
    }
    match child.wait_with_output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(_) => (false, String::new(), String::new()),
    }
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let (ok, out, _) = git(root, args, 60);
    if ok { out.trim().to_string() } else { String::new() }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn names(items: &[String]) -> String {
    let shown: Vec<&str> = items.iter().take(4).map(String::as_str).collect();
    let more = if items.len() > 4 { format!(" and {} more", items.len() - 4) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

struct Server {
    cfg: config::Config,
    session: String,
}

impl Server {
    fn post(&self, payload: Value) -> Option<Value> {
        if !self.cfg.usable() {
            return None;
        }
        let mut body = payload;
        body["repo_id"] = json!(self.cfg.repo_id);
        body["session"] = json!(self.session);
        body["agent"] = json!("collide-hook land");
        http::post(&self.cfg.server, "/land", &self.cfg.token, &user_agent(), &body, Duration::from_secs(5)).ok()
    }
}

/// The repo's test command: `--test`, then `land_test` or `verify` in
/// .collide/config.json, then the runner the repo plainly uses.
pub fn test_command(root: &Path, explicit: &str) -> String {
    if !explicit.trim().is_empty() {
        return explicit.trim().to_string();
    }
    let cfg = config::load_json(&root.join(".collide").join("config.json"));
    for key in ["land_test", "verify"] {
        if let Some(cmd) = cfg.get(key).and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
            return cmd.trim().to_string();
        }
    }
    let exists = |p: &str| root.join(p).exists();
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    if exists("pytest.ini") || exists("conftest.py") || pyproject.contains("[tool.pytest") || (exists("tests") && has_python_tests(&root.join("tests"))) {
        let python = [".venv/bin/python", "venv/bin/python"]
            .iter()
            .map(|c| root.join(c))
            .find(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "python3".to_string());
        return format!("{} -m pytest -q -p no:cacheprovider --no-header", shell_quote(&python));
    }
    if exists("package.json") {
        let manifest = config::load_json(&root.join("package.json"));
        let script = manifest.get("scripts").and_then(|s| s.get("test")).and_then(Value::as_str).unwrap_or("");
        if !script.is_empty() && !script.contains("no test specified") {
            return "npm test --silent".to_string();
        }
    }
    if exists("Cargo.toml") {
        return "cargo test -q".to_string();
    }
    if exists("go.mod") {
        return "go test ./...".to_string();
    }
    String::new()
}

fn has_python_tests(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".py")))
        .unwrap_or(false)
}

/// The conflicting hunks, marker to marker with a little context, capped.
fn conflict_excerpt(root: &Path, files: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for file in files.iter().take(3) {
        let Ok(body) = std::fs::read_to_string(root.join(file)) else { continue };
        let all: Vec<&str> = body.lines().collect();
        let mut i = 0;
        while i < all.len() && out.len() < CONFLICT_LINES {
            if all[i].starts_with("<<<<<<<") {
                out.push(format!("{file}:{}", i + 1));
                while i < all.len() && out.len() < CONFLICT_LINES {
                    out.push(format!("  {}", all[i]));
                    if all[i].starts_with(">>>>>>>") {
                        break;
                    }
                    i += 1;
                }
            }
            i += 1;
        }
    }
    out.join("\n")
}

/// Rewrite the agent's own commits for teammates' renames that have landed:
/// the new name is on origin and nothing on origin still uses the old one,
/// but a file this branch changed does. Returns (renames applied, files).
fn adapt_renames(root: &Path, server: &Server, upstream: &str) -> (Vec<String>, Vec<String>) {
    let Some(answer) = server.post(json!({"action": "renames"})) else { return (Vec::new(), Vec::new()) };
    let Some(renames) = answer.get("renames").and_then(Value::as_object) else { return (Vec::new(), Vec::new()) };
    adapt_to(root, renames, upstream)
}

fn adapt_to(root: &Path, renames: &serde_json::Map<String, Value>, upstream: &str) -> (Vec<String>, Vec<String>) {
    let range = format!("{upstream}..HEAD");
    let mine: Vec<String> = lines(&git_out(root, &["diff", "--name-only", "--diff-filter=AMR", &range]));
    let mut applied: Vec<String> = Vec::new();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for (old, new) in renames {
        let Some(new) = new.as_str() else { continue };
        let ident = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static regex");
        if !ident.is_match(old) || !ident.is_match(new) {
            continue;
        }
        let landed = git(root, &["grep", "-q", "-w", "-e", new, upstream, "--"], 30).0;
        let old_on_origin = git(root, &["grep", "-q", "-w", "-e", old, upstream, "--"], 30).0;
        if !landed || old_on_origin {
            continue;
        }
        let word = Regex::new(&format!(r"\b{}\b", regex::escape(old))).expect("escaped regex");
        let mut hit = false;
        for file in &mine {
            let path = root.join(file);
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            if !word.is_match(&body) {
                continue;
            }
            if std::fs::write(&path, word.replace_all(&body, new).as_ref()).is_ok() {
                touched.insert(file.clone());
                hit = true;
            }
        }
        if hit {
            applied.push(format!("{old} → {new}"));
        }
    }
    if !touched.is_empty() {
        let files: Vec<&str> = touched.iter().map(String::as_str).collect();
        let mut add = vec!["add", "--"];
        add.extend(files.iter());
        git(root, &add, 30);
        let message = format!("Adapt to teammates' renames: {}", applied.join(", "));
        git(root, &["commit", "-q", "-m", &message], 60);
    }
    (applied, touched.into_iter().collect())
}

struct Args {
    session: String,
    test: String,
}

fn parse_args(args: &[String]) -> Args {
    let mut out = Args { session: String::new(), test: String::new() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                out.session = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--test" => {
                out.test = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

pub fn run(raw_args: &[String], env: &Env) -> i32 {
    run_in(&std::env::current_dir().unwrap_or_default(), raw_args, env)
}

pub fn run_in(cwd: &Path, raw_args: &[String], env: &Env) -> i32 {
    let started = Instant::now();
    let args = parse_args(raw_args);
    let top = git_out(cwd, &["rev-parse", "--show-toplevel"]);
    if top.is_empty() {
        println!("Collide land: this is not a git repository.");
        return 1;
    }
    let root = PathBuf::from(top);
    let server = Server { cfg: config::config(Some(&root), env), session: args.session.clone() };

    let dirty = lines(&git_out(&root, &["status", "--porcelain", "--untracked-files=no"]));
    if !dirty.is_empty() {
        let files: Vec<String> = dirty.iter().map(|l| l.get(3..).unwrap_or(l).to_string()).collect();
        println!("Collide land: commit your changes first, then land again. Not committed: {}.", names(&files));
        return 1;
    }
    let branch = git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let upstream = git_out(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    if upstream.is_empty() {
        // no upstream to rebase onto: a plain first push, told as it is
        let (ok, out, err) = git(&root, &["push", "-u", "origin", "HEAD"], 120);
        print!("{out}{err}");
        return if ok { 0 } else { 1 };
    }
    let remote = upstream.split('/').next().unwrap_or("origin").to_string();
    git(&root, &["fetch", &remote, "--quiet"], 60);
    let ahead: usize = git_out(&root, &["rev-list", "--count", &format!("{upstream}..HEAD")]).parse().unwrap_or(0);
    if ahead == 0 {
        println!("Collide land: nothing to push; {branch} is already on {upstream}.");
        return 0;
    }

    // the push turn: queue behind whoever is landing now
    let mut waited_for = String::new();
    let turn_started = Instant::now();
    loop {
        let Some(answer) = server.post(json!({"action": "acquire"})) else { break };
        if answer.get("held").and_then(Value::as_bool).unwrap_or(true) {
            break;
        }
        waited_for = answer.get("holder").and_then(Value::as_str).unwrap_or("a teammate").to_string();
        if turn_started.elapsed().as_secs_f64() > MAX_TURN_WAIT_S {
            break; // a stuck turn never stops a push
        }
        let wait = answer.get("wait_s").and_then(Value::as_f64).unwrap_or(2.0).clamp(0.5, 2.0);
        std::thread::sleep(Duration::from_secs_f64(wait));
    }
    let waited_s = turn_started.elapsed().as_secs_f64();
    let release = |outcome: &str, extra: Value| {
        let mut body = json!({"action": "release", "outcome": outcome, "branch": branch});
        if let (Some(map), Some(more)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                map.insert(k.clone(), v.clone());
            }
        }
        server.post(body);
    };

    let test_cmd = test_command(&root, &args.test);
    let mut rebases = 0u64;
    let mut over = 0usize;
    let mut authors: Vec<String> = Vec::new();
    let mut renamed: Vec<String> = Vec::new();
    let mut adapted: BTreeSet<String> = BTreeSet::new();
    let mut tested: Option<bool> = None;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            git(&root, &["fetch", &remote, "--quiet"], 60);
        }
        let incoming: usize = git_out(&root, &["rev-list", "--count", &format!("HEAD..{upstream}")]).parse().unwrap_or(0);
        if incoming > 0 {
            for name in lines(&git_out(&root, &["log", &format!("HEAD..{upstream}"), "--format=%an"])) {
                if !authors.contains(&name) {
                    authors.push(name);
                }
            }
            over += incoming;
            let (ok, _, _) = git(&root, &["rebase", &upstream], 120);
            if !ok {
                let files = lines(&git_out(&root, &["diff", "--name-only", "--diff-filter=U"]));
                let excerpt = conflict_excerpt(&root, &files);
                git(&root, &["rebase", "--abort"], 60);
                release("conflict", json!({}));
                println!(
                    "Collide land: your commits conflict with {incoming} new commit(s) on {upstream} from {}, in {}. \
Nothing was pushed and your branch is as it was. Run `git pull --rebase`, keep both sides' work in those files, \
`git rebase --continue`, then push again.\n{excerpt}",
                    names(&authors), names(&files)
                );
                return 1;
            }
            rebases += 1;
            let (applied, files) = adapt_renames(&root, &server, &upstream);
            renamed.extend(applied);
            adapted.extend(files);
            if !test_cmd.is_empty() {
                let (ok, output, _, timed_out) = crate::check::run_verify(&root, &test_cmd, TEST_MAX_S);
                tested = Some(ok);
                if !ok {
                    release("tests_failed", json!({}));
                    let tail: Vec<&str> = output.lines().rev().take(TEST_TAIL_LINES).collect::<Vec<_>>().into_iter().rev().collect();
                    println!(
                        "Collide land: rebased onto {over} new commit(s) from {}, then `{test_cmd}` {}. Nothing was pushed; \
your commits now sit on top of theirs, so fix it here, commit, and push again.\n{}",
                        names(&authors), if timed_out { "timed out" } else { "failed" }, tail.join("\n")
                    );
                    return 1;
                }
            }
        }
        let (ok, _, err) = git(&root, &["push", "--quiet", &remote, &format!("HEAD:{}", upstream.split_once('/').map(|x| x.1).unwrap_or(&branch))], 120);
        if ok {
            let commit = git_out(&root, &["rev-parse", "--short", "HEAD"]);
            let adapted_list: Vec<String> = adapted.iter().cloned().collect();
            release("landed", json!({
                "commit": commit, "commits": ahead, "rebases": rebases, "over": over, "authors": authors,
                "renames_applied": renamed, "adapted": adapted_list,
                "tests": test_cmd, "tests_ok": tested, "seconds": started.elapsed().as_secs_f64(),
            }));
            let mut parts = vec![format!("Collide landed {ahead} commit(s) on {upstream} ({commit}).")];
            if waited_s >= 1.0 && !waited_for.is_empty() {
                parts.push(format!("Waited {}s for {waited_for}'s push.", waited_s.round() as i64));
            }
            if over > 0 {
                parts.push(format!("Rebased onto {over} new commit(s) from {}.", names(&authors)));
            }
            if !renamed.is_empty() {
                parts.push(format!("Adapted to renames that landed ({}) in {}, as its own commit.", renamed.join(", "), names(&adapted_list)));
            }
            match tested {
                Some(true) => parts.push(format!("`{test_cmd}` passed on the rebased tree.")),
                None if over > 0 => parts.push("No test command found, so the rebased tree was not tested.".to_string()),
                _ => {}
            }
            parts.push("It is pushed; nothing else to do.".to_string());
            println!("{}", parts.join(" "));
            return 0;
        }
        let lost_race = ["rejected", "non-fast-forward", "fetch first", "stale info"].iter().any(|w| err.contains(w));
        if !lost_race {
            release("failed", json!({}));
            println!("Collide land: git push failed:\n{}", err.trim());
            return 1;
        }
    }
    release("gave_up", json!({}));
    println!("Collide land: origin kept moving; {ATTEMPTS} pushes lost the race. Your commits are rebased onto the latest; push again.");
    1
}

fn push_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            // whatever runs first (a cd, git add, a commit with a heredoc
            // message, the tests), then the push as the LAST step
            r#"^(?P<pre>[\s\S]*(?:&&|;|\n)\s*)?git\s+push(?P<args>(?:[ \t]+[^\s;&|<>]+)*?)[ \t]*(?:2>&1)?[ \t]*(?:\|[ \t]*(?:tail|head)(?:[ \t]+-n)?[ \t]+-?\d+)?\s*$"#,
        )
        .expect("static regex")
    })
}

/// A plain `git push` of the current branch, as a PreToolUse hook sees it,
/// becomes `collide-hook land` for this session, also when it ends a chain
/// (`git add … && git commit … && git push`: what runs before it is kept).
/// A force, a tag, another branch, or a push that is not the last step is
/// left alone. Claude Code only: it is the
/// harness whose hooks can replace a command.
pub fn rewrite(stdin_data: &str, env: &Env) -> Option<String> {
    if crate::harness::current() != "claude" || config::get(env, "COLLIDE_LAND") == "0" {
        return None;
    }
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let tool_input = input.get("tool_input")?.clone();
    let command = tool_input.get("command").and_then(Value::as_str)?.to_string();
    let caps = push_re().captures(&command)?;
    let pre = caps.name("pre").map(|m| m.as_str()).unwrap_or("");
    let push_args: Vec<&str> = caps.name("args").map(|m| m.as_str().split_whitespace().collect()).unwrap_or_default();

    let cwd = input.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let dir = {
        static CD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let cd = CD.get_or_init(|| Regex::new(r#"^\s*cd\s+("[^"]*"|'[^']*'|[^\s;&|]+)\s*(?:&&|;)"#).expect("static regex"));
        let target = cd.captures(pre).and_then(|c| c.get(1)).map(|m| m.as_str().trim_matches(|c| c == '"' || c == '\'')).unwrap_or("");
        if target.is_empty() { cwd.clone() } else { cwd.join(target) }
    };
    let root = PathBuf::from(git_out(&dir, &["rev-parse", "--show-toplevel"]));
    if root.as_os_str().is_empty() {
        return None;
    }
    // a repo set up with its own config, or any repo on a machine with
    // Collide installed for every repo (the free version lands too)
    let own = root.join(".collide").join("config.json").exists();
    if !own && crate::machine::repo_config(&root, env).is_none() {
        return None;
    }
    if own && config::load_json(&root.join(".collide").join("config.json")).get("land").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let branch = git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let upstream = git_out(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    let (remote, remote_branch) = upstream.split_once('/')?;
    // only a push of this branch to its own upstream
    let mut positional: Vec<&str> = Vec::new();
    for arg in &push_args {
        match *arg {
            "-q" | "--quiet" => {}
            a if a.starts_with('-') => return None,
            a => positional.push(a),
        }
    }
    match positional.as_slice() {
        [] => {}
        [r] if *r == remote => {}
        [r, b] if *r == remote && (*b == branch || *b == remote_branch || *b == "HEAD" || *b == format!("HEAD:{remote_branch}")) => {}
        _ => return None,
    }

    let exe = std::env::current_exe().ok()?;
    let session = input.get("session_id").and_then(Value::as_str).unwrap_or("");
    let home = if own { String::new() } else { crate::machine::home_prefix(env) };
    let landed = format!("{pre}{home}{} land --session {}", shell_quote(&exe.to_string_lossy()), shell_quote(session));
    let mut updated = tool_input;
    updated["command"] = json!(landed);
    // the push was the agent's to make; in a session that asks before
    // running commands, it still asks, now about the landing
    let decision = if input.get("permission_mode").and_then(Value::as_str) == Some("bypassPermissions") { "allow" } else { "ask" };
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": "Collide lands this push: it rebases onto teammates' new commits, runs the tests and pushes, in one step.",
            "updatedInput": updated,
        }})
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) {
        let (ok, _, err) = git(root, args, 60);
        assert!(ok, "git {args:?}: {err}");
    }

    fn repo(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("collide-land-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        sh(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        sh(&base, &["clone", "-q", "origin.git", "a"]);
        let a = base.join("a");
        for (k, v) in [("user.email", "a@x"), ("user.name", "alice")] {
            sh(&a, &["config", k, v]);
        }
        std::fs::write(a.join("lib.py"), "def amount_due(x):\n    return x\n").unwrap();
        std::fs::write(a.join("notes.txt"), "one\n").unwrap();
        sh(&a, &["add", "."]);
        sh(&a, &["commit", "-qm", "seed"]);
        sh(&a, &["push", "-q", "-u", "origin", "main"]);
        sh(&base, &["clone", "-q", "origin.git", "b"]);
        let b = base.join("b");
        for (k, v) in [("user.email", "b@x"), ("user.name", "bob")] {
            sh(&b, &["config", k, v]);
        }
        (base, a, b)
    }

    fn land_in(dir: &Path) -> i32 {
        run_in(dir, &["--test".into(), "true".into()], &Env::new())
    }

    #[test]
    fn it_rebases_onto_a_teammate_and_pushes() {
        let (base, a, b) = repo("rebase");
        // a teammate lands first
        std::fs::write(a.join("notes.txt"), "one\ntwo\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        // this agent committed on the old tip: a plain push would be rejected
        std::fs::write(b.join("extra.py"), "def extra():\n    return 1\n").unwrap();
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "mine"]);
        assert_eq!(land_in(&b), 0);
        sh(&a, &["pull", "-q"]);
        assert!(a.join("extra.py").exists() && std::fs::read_to_string(a.join("notes.txt")).unwrap().contains("two"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_conflict_pushes_nothing_and_leaves_the_branch_as_it_was() {
        let (base, a, b) = repo("conflict");
        std::fs::write(a.join("notes.txt"), "theirs\n").unwrap();
        sh(&a, &["commit", "-qam", "teammate"]);
        sh(&a, &["push", "-q"]);
        std::fs::write(b.join("notes.txt"), "mine\n").unwrap();
        sh(&b, &["commit", "-qam", "mine"]);
        let before = git_out(&b, &["rev-parse", "HEAD"]);
        assert_eq!(land_in(&b), 1);
        assert_eq!(git_out(&b, &["rev-parse", "HEAD"]), before, "rebase aborted");
        assert!(git_out(&b, &["status", "--porcelain"]).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_rename_that_landed_is_applied_to_this_branch_only_once_it_is_on_origin() {
        let (base, a, b) = repo("renames");
        // this agent calls amount_due in a new test
        std::fs::write(b.join("test_x.py"), "from lib import amount_due\n\n\ndef test_x():\n    assert amount_due(1) == 1\n").unwrap();
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "mine"]);
        let renames: serde_json::Map<String, Value> = [("amount_due".to_string(), json!("checkout_total"))].into_iter().collect();
        // not on origin yet: nothing is touched
        sh(&b, &["fetch", "-q"]);
        assert!(adapt_to(&b, &renames, "origin/main").0.is_empty());
        // the teammate's rename lands; this branch rebases onto it
        std::fs::write(a.join("lib.py"), "def checkout_total(x):\n    return x\n").unwrap();
        sh(&a, &["commit", "-qam", "rename"]);
        sh(&a, &["push", "-q"]);
        sh(&b, &["fetch", "-q"]);
        sh(&b, &["rebase", "-q", "origin/main"]);
        let (applied, files) = adapt_to(&b, &renames, "origin/main");
        assert_eq!(applied, vec!["amount_due → checkout_total".to_string()]);
        assert_eq!(files, vec!["test_x.py".to_string()]);
        let body = std::fs::read_to_string(b.join("test_x.py")).unwrap();
        assert!(body.contains("import checkout_total") && !body.contains("amount_due"), "{body}");
        assert!(git_out(&b, &["status", "--porcelain"]).is_empty(), "committed");
        assert!(git_out(&b, &["log", "-1", "--format=%s"]).starts_with("Adapt to teammates' renames"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_push_lands_in_any_repo_on_a_machine_with_collide_installed() {
        let (base, _a, b) = repo("machine");
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "git push"}, "cwd": b.to_string_lossy(),
                           "session_id": "s1", "permission_mode": "bypassPermissions"}).to_string();
        let home = base.join("home");
        let mut env = Env::new();
        env.insert("HOME".into(), base.join("elsewhere").to_string_lossy().to_string());
        env.insert("COLLIDE_HOME".into(), home.to_string_lossy().to_string());
        assert!(rewrite(&input, &env).is_none(), "not installed: the push is left alone");
        assert!(crate::machine::save_settings(&env, &json!({"mode": "local"})));
        let out: Value = serde_json::from_str(&rewrite(&input, &env).expect("installed: the push lands")).unwrap();
        let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap();
        assert!(rewritten.starts_with(&format!("COLLIDE_HOME={} ", home.to_string_lossy())), "{rewritten}");
        assert!(rewritten.ends_with(" land --session 's1'"), "{rewritten}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn uncommitted_work_is_refused() {
        let (base, _a, b) = repo("dirty");
        std::fs::write(b.join("notes.txt"), "half done\n").unwrap();
        assert_eq!(land_in(&b), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_plain_push_of_this_branch_is_rewritten() {
        let (base, _a, b) = repo("rewrite");
        std::fs::create_dir_all(b.join(".collide")).unwrap();
        std::fs::write(b.join(".collide").join("config.json"), "{}").unwrap();
        let ask = |command: &str, mode: &str| {
            let input = json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": b.to_string_lossy(),
                               "session_id": "s1", "permission_mode": mode});
            rewrite(&input.to_string(), &Env::new())
        };
        for command in ["git push", "git push origin main", "git push origin main 2>&1", "git push -q origin HEAD",
                        "git push origin main 2>&1 | tail -5"] {
            let out: Value = serde_json::from_str(&ask(command, "bypassPermissions").unwrap_or_else(|| panic!("{command}"))).unwrap();
            let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap().to_string();
            assert!(rewritten.contains(" land --session 's1'"), "{rewritten}");
            assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "allow");
        }
        let cd = format!("cd {} && git push origin main", b.to_string_lossy());
        assert!(ask(&cd, "bypassPermissions").unwrap().contains("land --session"));
        // a push at the end of a chain lands; what runs before it stays as it was
        for chained in [
            "git add a.py && git commit -m 'Fix it' && git push origin main",
            "git add -A && git commit -q -F - <<'EOF'\nFix it\n\nWith a body && more\nEOF\ngit push -q origin main",
            "python -m pytest -q && git commit -am wip; git push",
        ] {
            let out: Value = serde_json::from_str(&ask(chained, "bypassPermissions").unwrap_or_else(|| panic!("{chained}"))).unwrap();
            let rewritten = out["hookSpecificOutput"]["updatedInput"]["command"].as_str().unwrap().to_string();
            let kept = &chained[..chained.rfind("git push").unwrap()];
            assert!(rewritten.starts_with(kept) && rewritten.ends_with(" land --session 's1'"), "{rewritten}");
        }
        for command in ["git push --force", "git push origin other", "git push origin --tags", "git push && echo hi",
                        "git commit -m x && git push --force",
                        "git pull --rebase", "git push upstream main"] {
            assert!(ask(command, "bypassPermissions").is_none(), "{command}");
        }
        // a session that asks before commands still asks
        let asked: Value = serde_json::from_str(&ask("git push", "default").unwrap()).unwrap();
        assert_eq!(asked["hookSpecificOutput"]["permissionDecision"], "ask");
        // a repo that turned landing off keeps its push
        std::fs::write(b.join(".collide").join("config.json"), r#"{"land": false}"#).unwrap();
        assert!(ask("git push", "bypassPermissions").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
