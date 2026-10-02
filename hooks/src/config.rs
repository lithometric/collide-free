//! Locating the repo and resolving the credential — shared by every hook.
//!
//! A faithful port of the resolution order in `collide_client/report_hook.py`
//! and `gate_hook.py`; the differential test drives both implementations with
//! the same filesystem and asserts they pick the same server, token and repo.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

pub type Env = BTreeMap<String, String>;

pub fn env_map() -> Env {
    std::env::vars().collect()
}

pub fn get<'a>(env: &'a Env, key: &str) -> &'a str {
    env.get(key).map(String::as_str).unwrap_or("")
}

pub fn load_json(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => value,
            _ => Value::Object(Default::default()),
        },
        Err(_) => Value::Object(Default::default()),
    }
}

fn str_field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// `os.path.normpath`: collapse `.` and `..` lexically, never touching disk.
pub fn normpath(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normpath(path)
    } else {
        normpath(&std::env::current_dir().unwrap_or_default().join(path))
    }
}

/// The nearest ancestor holding `.collide/config.json`, searched upward from
/// each candidate in turn — what makes reporting survive cwd drift.
pub fn find_repo_root(candidates: &[Option<PathBuf>]) -> Option<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for start in candidates.iter().flatten() {
        let mut dir = absolute(start);
        loop {
            if seen.contains(&dir) {
                break;
            }
            seen.push(dir.clone());
            if dir.join(".collide").join("config.json").is_file() {
                return Some(dir);
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    // no repo set up for Collide: with Collide installed for the machine,
    // every git repo is covered, rooted where its .git is
    if !crate::machine::installed(&env_map()) {
        return None;
    }
    for start in candidates.iter().flatten() {
        let mut dir = absolute(start);
        loop {
            if dir.join(".git").exists() {
                return Some(dir);
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    None
}

/// Where this machine's hook credential lives. HOME is honoured first and
/// explicitly: on Windows `expanduser` consults USERPROFILE and ignores HOME,
/// but the hooks run under Git Bash, whose HOME is the one the install path
/// means — and an un-overridable HOME once let a test rewrite a developer's
/// real credentials file.
pub fn credentials_path(env: &Env) -> PathBuf {
    let home = if !get(env, "COLLIDE_HOME").is_empty() {
        get(env, "COLLIDE_HOME").to_string()
    } else if !get(env, "HOME").is_empty() {
        get(env, "HOME").to_string()
    } else {
        get(env, "USERPROFILE").to_string()
    };
    PathBuf::from(home).join(".collide").join("credentials.json")
}

/// The same Collide server answers on more than one hostname (the classic
/// `api.*` and the `mcp.*` root); a config that moved between them must not
/// strand a token stored under the old key.
pub fn host_aliases(server: &str) -> Vec<String> {
    let mut out = vec![server.to_string()];
    for (old, new) in [("://api.", "://mcp."), ("://mcp.", "://api.")] {
        if server.contains(old) {
            out.push(server.replacen(old, new, 1));
        }
    }
    out
}

/// Most specific first: exact server+workspace, the same workspace under a
/// sibling or any hostname (tokens are workspace-bound, not host-bound), the
/// bare server key and its sibling, then the legacy plain token.
pub fn resolve_token(creds: &Value, server: &str, workspace: &str) -> String {
    let Some(map) = creds.as_object() else { return String::new() };
    let aliases = host_aliases(server);
    if !workspace.is_empty() {
        for alias in &aliases {
            let key = format!("{alias}#{workspace}");
            if let Some(value) = map.get(&key) {
                let text = as_text(value);
                if !text.is_empty() {
                    return text;
                }
            }
        }
        let suffix = format!("#{workspace}");
        for (key, value) in map {
            if key.ends_with(&suffix) {
                let text = as_text(value);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    for alias in &aliases {
        if let Some(value) = map.get(alias) {
            let text = as_text(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    map.get("token").map(as_text).unwrap_or_default()
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(false) => String::new(),
        other => other.to_string(),
    }
}

pub struct Config {
    pub server: String,
    pub token: String,
    pub repo_id: String,
    /// The repo's own check — `python -m tests.check`, `cargo check`, `npm
    /// test` — from `.collide/config.json` `verify`. Empty means none.
    pub verify: String,
    /// Parse on this machine and send the structure, never the file
    /// (`"local_parse": true` in `.collide/config.json`, which setup writes
    /// for new repos; `COLLIDE_LOCAL_PARSE=1`/`0` overrides either way).
    pub local_parse: bool,
    /// Served by this machine's local server (the free version): the config
    /// came from the machine's install, not from the repo.
    pub machine_local: bool,
}

impl Config {
    /// What a report or a read sends about one file: its content, or — with
    /// local parsing on — the structure the server keeps, with no body.
    /// A file with no parser sends neither: path-only, like a binary.
    pub fn source_fields(&self, path: &str, content: &str) -> serde_json::Map<String, Value> {
        if self.local_parse {
            return collide_core::local::fields(path, content);
        }
        let mut out = serde_json::Map::new();
        out.insert("content".into(), Value::String(content.to_string()));
        out
    }

    pub fn usable(&self) -> bool {
        !self.server.is_empty() && !self.token.is_empty() && !self.repo_id.is_empty()
    }
}

pub fn config(root: Option<&Path>, env: &Env) -> Config {
    let repo_cfg = root
        .map(|r| load_json(&r.join(".collide").join("config.json")))
        .unwrap_or(Value::Object(Default::default()));

    let mut server = get(env, "COLLIDE_SERVER_URL").trim_end_matches('/').to_string();
    if server.is_empty() {
        server = str_field(&repo_cfg, "server_url").trim_end_matches('/').to_string();
    }
    // no Collide config in the repo: the machine's install covers it
    if server.is_empty() && get(env, "COLLIDE_REPO_ID").is_empty() {
        if let Some(machine) = root.and_then(|r| crate::machine::repo_config(r, env)) {
            return machine;
        }
    }
    let mut repo_id = get(env, "COLLIDE_REPO_ID").to_string();
    if repo_id.is_empty() {
        repo_id = str_field(&repo_cfg, "repo_id");
    }
    let mut token = get(env, "COLLIDE_TOKEN").to_string();
    if token.is_empty() && !server.is_empty() {
        let creds = load_json(&credentials_path(env));
        token = resolve_token(&creds, &server, &str_field(&repo_cfg, "workspace"));
    }
    let verify = str_field(&repo_cfg, "verify");
    let local_parse = match get(env, "COLLIDE_LOCAL_PARSE").trim() {
        "1" => true,
        "0" => false,
        _ => repo_cfg.get("local_parse").and_then(Value::as_bool).unwrap_or(false),
    };
    Config { server, token, repo_id, verify, local_parse, machine_local: false }
}

/// Repo-relative, forward-slashed; `None` when the path sits outside the repo
/// (not ours to report or gate).
pub fn rel_path(file_path: &Path, root: &Path) -> Option<String> {
    let file = absolute(file_path);
    let root = absolute(root);
    let rel = file.strip_prefix(&root).ok()?;
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.starts_with("..") {
        return None;
    }
    Some(if text.is_empty() { ".".to_string() } else { text })
}
