//! Behavioural breakage, with zero model tokens.
//!
//! A certain interface change (rename, removal, signature change) comes back
//! from `/report` with a `verify` plan: the files that depend on the changed
//! symbol and the test files that reach them. Those tests run DETACHED — the
//! agent never waits, never reads a line — and the verdict goes to `/verify`,
//! where the server grades the change: clear when every dependent was covered
//! and every test passed, partial when some dependent has no test, failing
//! when a test failed. The whole thing is a subprocess and one HTTP post.
//! Mirrors the `_verify_tests_command` half of report_hook.py, byte for byte
//! in what it posts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::report::user_agent;

pub const VERIFY_MAX_S: f64 = 240.0;

/// POSIX single-quote quoting, the same bytes as Python's `_shell_quote`.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The closest directory at or above `start` (repo-relative) holding `name`,
/// as a repo-relative directory ("" for the root).
fn nearest_up(root: &Path, start: &str, name: &str) -> Option<String> {
    let dir = Path::new(start).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    loop {
        let rel = parts.join("/");
        let probe = if rel.is_empty() { root.join(name) } else { root.join(&rel).join(name) };
        if probe.exists() {
            return Some(rel);
        }
        if parts.pop().is_none() {
            return None;
        }
    }
}

struct Run {
    label: &'static str,
    command: String,
    files: Vec<String>,
}

fn js_like(path: &str) -> bool {
    [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].iter().any(|ext| path.ends_with(ext))
}

/// `(runs, skipped)`: each run is a label, a shell command and the test files
/// it covers; skipped are test files no runner is mapped for. Files are
/// sorted so the two hooks build identical commands. Python's `_test_runners`.
fn test_runners(root: &Path, tests: &[String], env: &Env) -> (Vec<Run>, Vec<String>) {
    let mut runs: Vec<Run> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut sorted: Vec<String> = tests.to_vec();
    sorted.sort();

    let py: Vec<String> = sorted.iter().filter(|t| t.ends_with(".py")).cloned().collect();
    if !py.is_empty() {
        let override_python = config::get(env, "COLLIDE_PYTEST_PYTHON");
        let python: String = if !override_python.is_empty() {
            override_python.to_string()
        } else {
            [".venv/bin/python", "venv/bin/python"]
                .iter()
                .map(|c| root.join(c))
                .find(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "python3".to_string())
        };
        let mut parts = vec![shell_quote(&python), "-m".into(), "pytest".into(), "-q".into(), "-p".into(), "no:cacheprovider".into(), "--no-header".into()];
        parts.extend(py.iter().map(|t| shell_quote(t)));
        runs.push(Run { label: "pytest", command: parts.join(" "), files: py });
    }
    for t in sorted.iter().filter(|t| t.ends_with(".rs")) {
        let crate_dir = nearest_up(root, t, "Cargo.toml");
        let segments: Vec<&str> = t.split('/').collect();
        let in_tests_dir = segments.len() >= 2 && segments[segments.len() - 2] == "tests";
        let Some(crate_dir) = crate_dir.filter(|_| in_tests_dir) else {
            skipped.push(t.clone()); // inline #[cfg(test)] modules are not test files the graph can name
            continue;
        };
        let stem = Path::new(t).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let manifest = if crate_dir.is_empty() { "Cargo.toml".to_string() } else { format!("{crate_dir}/Cargo.toml") };
        runs.push(Run {
            label: "cargo test",
            command: ["cargo", "test", "-q", "--manifest-path", &shell_quote(&manifest), "--test", &shell_quote(&stem)].join(" "),
            files: vec![t.clone()],
        });
    }
    let js: Vec<String> = sorted.iter().filter(|t| js_like(t)).cloned().collect();
    if !js.is_empty() {
        let mut by_pkg: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in &js {
            match nearest_up(root, t, "package.json") {
                Some(pkg) => by_pkg.entry(pkg).or_default().push(t.clone()),
                None => skipped.push(t.clone()),
            }
        }
        for (pkg, files) in by_pkg {
            let manifest = if pkg.is_empty() { root.join("package.json") } else { root.join(&pkg).join("package.json") };
            let manifest = config::load_json(&manifest);
            let has = |dep: &str| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|k| manifest.get(k).and_then(Value::as_object).is_some_and(|m| m.contains_key(dep)))
            };
            let runner = if has("vitest") { "vitest" } else if has("jest") { "jest" } else { "" };
            if runner.is_empty() {
                skipped.extend(files);
                continue;
            }
            let rel: Vec<String> = files
                .iter()
                .map(|f| if pkg.is_empty() { f.clone() } else { f.strip_prefix(&format!("{pkg}/")).unwrap_or(f).to_string() })
                .collect();
            let prefix = if pkg.is_empty() { String::new() } else { format!("cd {} && ", shell_quote(&pkg)) };
            let run_word = if runner == "vitest" { "run " } else { "" };
            let command = format!("{prefix}npx {runner} {run_word}{}", rel.iter().map(|r| shell_quote(r)).collect::<Vec<_>>().join(" "));
            runs.push(Run { label: if runner == "vitest" { "vitest" } else { "jest" }, command, files });
        }
    }
    let go: Vec<String> = sorted.iter().filter(|t| t.ends_with(".go")).cloned().collect();
    if !go.is_empty() {
        let mut dirs: Vec<String> = go
            .iter()
            .map(|t| {
                let dir = Path::new(t).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                if dir.is_empty() { ".".to_string() } else { format!("./{dir}") }
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        let mut parts = vec!["go".to_string(), "test".to_string()];
        parts.extend(dirs.iter().map(|d| shell_quote(d)));
        runs.push(Run { label: "go test", command: parts.join(" "), files: go });
    }
    for t in &sorted {
        let known = t.ends_with(".py") || t.ends_with(".rs") || js_like(t) || t.ends_with(".go");
        if !known && !skipped.contains(t) {
            skipped.push(t.clone());
        }
    }
    skipped.sort();
    skipped.dedup();
    (runs, skipped)
}

fn pytest_failed_re() -> Regex {
    Regex::new(r"(?m)^(?:FAILED|ERROR) (\S+?)(?:::|\s|$)").expect("pytest failure pattern")
}

fn js_failed_re() -> Regex {
    Regex::new(r"(?m)^\s*(?:FAIL|✗|×)\s+(\S+)").expect("js failure pattern")
}

/// Which of a run's files failed, from its output; when the output names
/// none, every file of the run did (a collection or compile error).
fn failed_files(label: &str, output: &str, files: &[String]) -> Vec<String> {
    let named: Vec<String> = match label {
        "pytest" => pytest_failed_re().captures_iter(output).map(|c| c[1].trim_start_matches("./").to_string()).collect(),
        "vitest" | "jest" => js_failed_re().captures_iter(output).map(|c| c[1].trim_start_matches("./").to_string()).collect(),
        _ => Vec::new(),
    };
    let hit: Vec<String> = files
        .iter()
        .filter(|f| named.iter().any(|n| n == *f || f.ends_with(&format!("/{n}")) || n.ends_with(&format!("/{f}"))))
        .cloned()
        .collect();
    if hit.is_empty() { files.to_vec() } else { hit }
}

pub fn spawn(root: &Path, session: &str, plan: &Value) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = Command::new(exe);
    command
        .arg("verify-tests")
        .arg(root)
        .arg(session)
        .arg(plan.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// `verify-tests <root> <session> <plan-json>`: the detached run.
pub fn run(root: &str, session: &str, plan_json: &str, env: &Env) -> i32 {
    if root.is_empty() {
        return 0;
    }
    let root = PathBuf::from(root);
    let Ok(plan) = serde_json::from_str::<Value>(plan_json) else { return 0 };
    let strings = |key: &str| -> Vec<String> {
        plan.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
    };
    let tests = strings("tests");
    if tests.is_empty() {
        return 0;
    }
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let (runs, skipped) = test_runners(&root, &tests, env);
    let mut passed: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut labels: Vec<&str> = Vec::new();
    let mut timed_out = false;
    let started = Instant::now();
    for run in &runs {
        let remaining = VERIFY_MAX_S - started.elapsed().as_secs_f64();
        if remaining <= 0.0 {
            timed_out = true;
            break;
        }
        labels.push(run.label);
        let (ok, output, _elapsed, to) = crate::check::run_verify(&root, &run.command, remaining);
        if to {
            timed_out = true;
            break;
        }
        if ok {
            passed.extend(run.files.iter().cloned());
        } else {
            let bad = failed_files(run.label, &output, &run.files);
            passed.extend(run.files.iter().filter(|f| !bad.contains(f)).cloned());
            failed.extend(bad);
        }
    }
    passed.sort();
    passed.dedup();
    failed.sort();
    failed.dedup();
    let _ = http::post(
        &cfg.server,
        "/verify",
        &cfg.token,
        &user_agent(),
        &json!({
            "repo_id": cfg.repo_id, "path": plan.get("path").and_then(Value::as_str).unwrap_or(""), "session": session,
            "symbols": strings("symbols"), "tests": tests, "passed": passed, "failed": failed,
            "uncovered": strings("uncovered"), "skipped": skipped,
            "command": labels.join(" && "), "elapsed_ms": started.elapsed().as_millis() as i64,
            "timed_out": timed_out,
        }),
        Duration::from_secs(10),
    );
    0
}
