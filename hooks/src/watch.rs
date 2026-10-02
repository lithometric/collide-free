//! collide-watch: the disk-confirmed reporter.
//!
//! The only Collide component that reads files off disk on its own, and it
//! runs on the developer's machine by their choice. It covers the case hooks
//! cannot: a human editing in an editor, with no agent in the loop. Edits it
//! reports carry the `collide-watch` identity, which the server treats as
//! disk-confirmed rather than agent-claimed.
//!
//! Polls mtime and size, re-hashes only what moved, and posts the changed
//! files to /report.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::config::{self, Env};
use crate::http;

const LANGUAGE_ALLOWLIST: [&str; 3] = [".py", ".ts", ".tsx"];
const SKIP_DIRS: [&str; 9] = [
    ".git", "node_modules", "__pycache__", ".venv", "venv", "dist", "build", ".mypy_cache",
    ".ruff_cache",
];
const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Options {
    pub root: PathBuf,
    pub repo_id: String,
    pub server: String,
    pub token: String,
    pub interval: Duration,
    pub once: bool,
    pub state_file: Option<PathBuf>,
}

fn wanted(path: &str) -> bool {
    LANGUAGE_ALLOWLIST.iter().any(|ext| path.ends_with(ext))
}

/// `git ls-files` (tracked plus untracked-but-unignored); falls back to
/// walking the tree when git is absent or this is not a checkout.
fn discover_files(root: &Path) -> Vec<String> {
    if let Ok(output) = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
    {
        if output.status.success() {
            let mut found: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.is_empty() && wanted(line))
                .filter(|line| root.join(line).is_file())
                .map(str::to_string)
                .collect();
            found.sort();
            found.dedup();
            return found;
        }
    }
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, found);
            }
        } else if wanted(&name) {
            if let Ok(rel) = path.strip_prefix(root) {
                found.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn content_fingerprint(bytes: &[u8]) -> String {
    // The watcher only needs a stable change detector, and pulling a hashing
    // crate in for it would be the only reason this binary has one. FNV-1a
    // over the content is enough to answer "did this file move since last
    // time", which is the only question asked of it.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[derive(Default)]
struct Entry {
    mtime: f64,
    size: u64,
    sha: String,
}

type State = BTreeMap<String, Entry>;

fn load_state(path: &Path) -> State {
    let value = config::load_json(path);
    let mut state = State::new();
    if let Some(map) = value.as_object() {
        for (key, entry) in map {
            state.insert(
                key.clone(),
                Entry {
                    mtime: entry.get("mtime").and_then(Value::as_f64).unwrap_or(0.0),
                    size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
                    sha: entry.get("sha").and_then(Value::as_str).unwrap_or("").to_string(),
                },
            );
        }
    }
    state
}

fn save_state(path: &Path, state: &State) {
    let mut map = Map::new();
    for (key, entry) in state {
        map.insert(
            key.clone(),
            json!({"mtime": entry.mtime, "size": entry.size, "sha": entry.sha}),
        );
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, Value::Object(map).to_string());
}

/// Files whose content actually changed. The mtime and size check
/// short-circuits re-hashing everything that did not move.
fn find_changes(root: &Path, state: &mut State) -> Vec<(String, String)> {
    let mut changed = Vec::new();
    for rel in discover_files(root) {
        let full = root.join(&rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let size = meta.len();
        if let Some(previous) = state.get(&rel) {
            if previous.mtime == mtime && previous.size == size {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&full) else { continue };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let sha = content_fingerprint(content.as_bytes());
        let unchanged = state.get(&rel).map(|p| p.sha == sha).unwrap_or(false);
        state.insert(rel.clone(), Entry { mtime, size, sha });
        if !unchanged {
            changed.push((rel, content));
        }
    }
    changed
}

fn default_state_file(root: &Path, repo_id: &str) -> PathBuf {
    let key = content_fingerprint(format!("{}|{repo_id}", root.display()).as_bytes());
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache").join("collide-watch").join(format!("{key}.json"))
}

fn report(options: &Options, changes: &[(String, String)]) -> usize {
    let mut reported = 0;
    // the same local-parse switch the hooks read: the repo's config, or
    // COLLIDE_LOCAL_PARSE for this process
    let local = crate::config::config(Some(&options.root), &crate::config::env_map()).local_parse;
    for (path, content) in changes {
        let mut payload = json!({
            "repo_id": options.repo_id,
            "path": path,
            // this identity is what marks the edit disk-confirmed rather than
            // agent-claimed, which the dashboard shows as the anchor
            "agent": "collide-watch",
        });
        if let Some(map) = payload.as_object_mut() {
            if local {
                map.extend(collide_core::local::fields(path, content));
            } else {
                map.insert("content".into(), json!(content));
            }
        }
        match http::post(
            &options.server,
            "/report",
            &options.token,
            "collide-watch/1",
            &payload,
            REPORT_TIMEOUT,
        ) {
            Ok(_) => {
                reported += 1;
                println!("collide-watch: reported {path}");
            }
            Err(_) => eprintln!("collide-watch: report failed for {path}"),
        }
    }
    reported
}

pub fn run(options: Options, env: &Env) -> i32 {
    let state_file = options
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_file(&options.root, &options.repo_id));
    let _ = env;
    loop {
        let mut state = load_state(&state_file);
        let changes = find_changes(&options.root, &mut state);
        if !changes.is_empty() {
            report(&options, &changes);
        }
        save_state(&state_file, &state);
        if options.once {
            return 0;
        }
        std::thread::sleep(options.interval);
    }
}

pub fn parse_args(args: &[String], env: &Env) -> Result<Options, String> {
    let mut root = std::env::current_dir().unwrap_or_default();
    let mut repo_id = config::get(env, "COLLIDE_REPO_ID").to_string();
    let mut server = config::get(env, "COLLIDE_SERVER_URL").to_string();
    let mut token = config::get(env, "COLLIDE_TOKEN").to_string();
    let mut interval = Duration::from_secs(5);
    let mut once = false;
    let mut state_file = None;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || {
            index += 1;
            args.get(index).cloned().unwrap_or_default()
        };
        match flag {
            "--root" => root = PathBuf::from(value()),
            "--repo-id" => repo_id = value(),
            "--server" => server = value(),
            "--token" => token = value(),
            "--state-file" => state_file = Some(PathBuf::from(value())),
            "--interval" => {
                interval = Duration::from_secs_f64(value().parse().unwrap_or(5.0));
            }
            "--once" => once = true,
            other => return Err(format!("unknown flag {other}")),
        }
        index += 1;
    }

    // anything not given on the command line comes from the repo's committed
    // config and this machine's credential, exactly like the hooks
    if server.is_empty() || token.is_empty() || repo_id.is_empty() {
        let discovered = config::find_repo_root(&[Some(root.clone())]).unwrap_or(root.clone());
        let cfg = config::config(Some(&discovered), env);
        if server.is_empty() {
            server = cfg.server;
        }
        if token.is_empty() {
            token = cfg.token;
        }
        if repo_id.is_empty() {
            repo_id = cfg.repo_id;
        }
    }
    if server.is_empty() || token.is_empty() || repo_id.is_empty() {
        return Err("need --server, --token and --repo-id (or a .collide/config.json)".into());
    }
    let server = server.trim_end_matches('/').to_string();
    Ok(Options { root, repo_id, server, token, interval, once, state_file })
}
