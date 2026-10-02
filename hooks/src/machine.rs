//! Collide installed once per machine, for every repo on it.
//!
//! `collide install` (what https://collidemcp.com/install.sh runs) puts the
//! one `collide` program (these hooks and the local server) in
//! `~/.collide/bin`, registers the hooks at the USER level of each agent
//! tool (Claude Code's `~/.claude/settings.json`), and writes
//! `~/.collide/machine.json`. From then on every repo the agents open is
//! covered, with no per-repo setup:
//!
//! - a repo with its own `.collide/config.json` (set up for a team) keeps
//!   using it; the machine's hooks stand down where the repo runs its own;
//! - any other repo is named by its git origin and served by the machine's
//!   mode: "local" (the free version: the local server on 127.0.0.1, nothing
//!   leaves the machine) or "cloud" (a paid plan: Collide's servers, with the
//!   credential `login` saved).
//!
//! A free account, or none, is "local": the hooks never call out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};

pub const DEFAULT_PORT: u16 = 47_600;
/// The free version's one program: these hooks and the local server.
pub const PROGRAM: &str = "collide";

static SERVES_LOCAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called by the `collide` program (collide-rs with the hooks linked in):
/// this binary can also run the local server, as `collide local`.
pub fn set_serves_local() {
    SERVES_LOCAL.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn serves_local() -> bool {
    SERVES_LOCAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether this is the free version's `collide` program.
pub fn serves_local_pub() -> bool {
    serves_local()
}

/// `~/.collide/bin/collide`: where the free version lives on the machine.
pub fn program_path(env: &Env) -> PathBuf {
    bin_dir(env).join(exe(PROGRAM))
}
/// What the machine-level hook commands set, so they can stand down where
/// a repo runs Collide's hooks itself.
pub const MACHINE_FLAG: &str = "COLLIDE_MACHINE";

/// Windows: a program this one starts inherits every inheritable handle it
/// holds, its own stdout pipe included. Something started to outlive the
/// hook (the local server, an index, the tests of a change, an update) then
/// keeps that pipe open, and whatever waits for the hook's output (Claude
/// Code, Codex, Cursor) waits until that program exits: a stalled hook.
/// Called once at start: the standard handles stop being inheritable. A
/// child still gets its own stdin/stdout/stderr, which Rust duplicates for
/// it explicitly; it no longer gets this process's.
#[cfg(windows)]
pub fn no_inherit_std() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE
    for which in [-10i32 as u32, -11i32 as u32, -12i32 as u32] {
        // SAFETY: GetStdHandle takes no pointers; SetHandleInformation only
        // changes a flag on the handle it is given, which may be null or
        // invalid and then fails harmlessly.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn no_inherit_std() {}

pub fn collide_dir(env: &Env) -> PathBuf {
    PathBuf::from(crate::check::home(env)).join(".collide")
}

pub fn bin_dir(env: &Env) -> PathBuf {
    collide_dir(env).join("bin")
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn local_dir(env: &Env) -> PathBuf {
    let given = config::get(env, "COLLIDE_LOCAL_DIR");
    if given.is_empty() { collide_dir(env).join("local") } else { PathBuf::from(given) }
}

/// `~/.collide/machine.json`: `{"mode": "local"|"cloud", "server_url",
/// "workspace", "account": {...}}`. Empty when Collide was never installed
/// for the machine.
pub fn settings(env: &Env) -> Value {
    config::load_json(&collide_dir(env).join("machine.json"))
}

pub fn save_settings(env: &Env, value: &Value) -> bool {
    let path = collide_dir(env).join("machine.json");
    let _ = std::fs::create_dir_all(collide_dir(env));
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(value).unwrap_or_default())).is_ok()
}

pub fn installed(env: &Env) -> bool {
    settings(env).get("mode").and_then(Value::as_str).is_some()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The local server's address and key, as its files say.
pub fn local_endpoint(env: &Env) -> (String, String) {
    let dir = local_dir(env);
    let port = std::fs::read_to_string(dir.join("port"))
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let token = std::fs::read_to_string(dir.join("token")).unwrap_or_default().trim().to_string();
    (format!("http://127.0.0.1:{port}"), token)
}

/// A repo's id when it carries no Collide config: its git origin
/// (`github.com/owner/name`), else `local/<folder>`.
pub fn repo_id_for(root: &Path) -> String {
    let origin = crate::git::origin_repo_id(root);
    if !origin.is_empty() {
        return origin;
    }
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if name.is_empty() { String::new() } else { format!("local/{name}") }
}

/// A repo a team can share: named by a hosted remote, `host.tld/owner/name`
/// (GitHub, GitLab, Bitbucket, a company's own). A folder, a git repo with
/// no remote, or one whose remote is another folder on this machine is not:
/// there is no one else to share it with.
pub fn is_hosted(repo_id: &str) -> bool {
    let parts: Vec<&str> = repo_id.split('/').collect();
    parts.len() >= 3
        && parts[0].contains('.')
        && parts.iter().all(|p| !p.is_empty() && !p.chars().any(char::is_whitespace))
}

/// The config for a repo with none of its own, from the machine's mode.
/// `None` when Collide was not installed for the machine. On a machine with
/// Team, only a repo with a hosted remote goes to the team's workspace; any
/// other git repo stays on this machine, as on the free version.
pub fn repo_config(root: &Path, env: &Env) -> Option<config::Config> {
    let machine = settings(env);
    let mode = text(&machine, "mode");
    if mode.is_empty() {
        return None;
    }
    let repo_id = repo_id_for(root);
    let cloud = mode == "cloud" && is_hosted(&repo_id);
    let machine_local = !cloud;
    let (server, token) = if cloud {
        let server = text(&machine, "server_url").trim_end_matches('/').to_string();
        let creds = config::load_json(&config::credentials_path(env));
        let token = config::resolve_token(&creds, &server, &text(&machine, "workspace"));
        (server, token)
    } else {
        local_endpoint(env)
    };
    Some(config::Config { server, token, repo_id, verify: String::new(), local_parse: true, machine_local })
}

/// True when this repo runs Collide's hooks from its own settings: the
/// machine's hooks then stay silent, or every event would be handled twice.
pub fn repo_runs_its_own(root: &Path) -> bool {
    let settings = std::fs::read_to_string(root.join(".claude").join("settings.json")).unwrap_or_default();
    settings.contains("collide-hook") || settings.contains(".collide/report_hook.py")
}

/// The local server's own id for this data directory: the same hash it
/// reports on /health (see collide-rs `local::instance_id`).
fn instance_id(env: &Env) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(local_dir(env).to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A local Collide answers at `base`, and it is this account's (another
/// person on the same computer runs their own, on another port).
fn healthy(base: &str, env: &Env) -> bool {
    let id = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(400))
        .build()
        .get(&format!("{base}/health"))
        .call()
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .map(|v| text(&v, "local_id"))
        .unwrap_or_default();
    !id.is_empty() && id == instance_id(env)
}

/// Make sure the local server is up, starting it when it is not. Waits at
/// most `wait` for it to answer; returns whether it does.
pub fn ensure_local(env: &Env, wait: Duration) -> bool {
    let (base, _) = local_endpoint(env);
    if healthy(&base, env) {
        return true;
    }
    let server = {
        let installed = program_path(env);
        if installed.exists() {
            installed
        } else if serves_local() {
            match std::env::current_exe() {
                Ok(me) => me,
                Err(_) => return false,
            }
        } else {
            return false;
        }
    };
    let dir = local_dir(env);
    let _ = std::fs::create_dir_all(&dir);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("server.log"));
    let mut command = Command::new(&server);
    command.arg("local").stdin(Stdio::null());
    match log.and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok((out, err)) => {
            command.stdout(out).stderr(err);
        }
        Err(_) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // outlives the hook, and the agent session it serves
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    if command.spawn().is_err() {
        return false;
    }
    let started = Instant::now();
    while started.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(50));
        let (base, _) = local_endpoint(env);
        if healthy(&base, env) {
            return true;
        }
    }
    false
}

/// Called before a `gate` or `report` event is handled. A machine-level
/// hook (`COLLIDE_MACHINE=1`) in a repo that runs Collide's hooks itself
/// exits here, silent. A session or a prompt in a repo the local server
/// serves makes sure it is running first.
pub fn before_event(stdin_data: &str, env: &Env) -> Option<i32> {
    let input: Value = crate::harness::normalize(serde_json::from_str(stdin_data).unwrap_or(Value::Null));
    let cwd = {
        let given = text(&input, "cwd");
        if given.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(given) }
    };
    let project = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| PathBuf::from(project));
    let root = config::find_repo_root(&[Some(cwd), project])?;
    if config::get(env, MACHINE_FLAG) == "1" && repo_runs_its_own(&root) {
        return Some(0);
    }
    let event = text(&input, "hook_event_name");
    note_agent_process(&text(&input, "session_id"), env);
    if matches!(event.as_str(), "SessionStart" | "UserPromptSubmit") {
        let repo_cfg = config::load_json(&root.join(".collide").join("config.json"));
        let own = repo_cfg.get("server_url").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        let local = repo_config(&root, env).is_some_and(|c| c.machine_local);
        if !own && local {
            let wait = if event == "SessionStart" { Duration::from_secs(4) } else { Duration::from_millis(1500) };
            if ensure_local(env, wait) && event == "UserPromptSubmit" {
                index_once(&root, env);
            }
        }
    }
    None
}

/// A session that was already open when Collide was installed missed its
/// session start, the step that maps the repo's code. Its first prompt
/// maps it instead, in the background, once per repo on this machine.
fn index_once(root: &Path, env: &Env) {
    use sha2::{Digest, Sha256};
    let key: String = Sha256::digest(root.to_string_lossy().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let marker = local_dir(env).join("indexed").join(key);
    if marker.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&marker, root.to_string_lossy().as_bytes());
    let Ok(me) = std::env::current_exe() else { return };
    let mut command = Command::new(me);
    command.arg("index").arg(root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// A session start maps the repo itself: the first prompt need not.
pub fn mark_indexed(root: &Path, env: &Env) {
    use sha2::{Digest, Sha256};
    let key: String = Sha256::digest(root.to_string_lossy().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let marker = local_dir(env).join("indexed").join(key);
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&marker, root.to_string_lossy().as_bytes());
}

// ------------------------------------------------------------------ install

fn claude_settings_path(env: &Env) -> PathBuf {
    PathBuf::from(crate::check::home(env)).join(".claude").join("settings.json")
}

/// The machine-level hook commands, one per kind: every event but the gate
/// goes to `report`. They find the binary in `~/.collide/bin`, say nothing
/// and exit 0 when it is missing, and mark themselves so they can stand down
/// in a repo that runs Collide itself.
fn machine_command(sub: &str) -> String {
    machine_command_for(sub, "claude")
}

/// The same for another agent tool, which hands its events over in its own
/// dialect (`--harness codex|cursor`).
fn machine_command_for(sub: &str, harness: &str) -> String {
    let flag = if harness == "claude" { String::new() } else { format!(" --harness {harness}") };
    let run = if sub == "gate" {
        format!("exec env COLLIDE_MACHINE=1 \"$b\" gate{flag}")
    } else {
        format!("env COLLIDE_MACHINE=1 \"$b\" report{flag} || true")
    };
    format!(
        "b=\"$HOME/.collide/bin/collide\"; [ -x \"$b\" ] || b=\"$b.exe\"; [ -x \"$b\" ] || exit 0; {run}; exit 0 # collide-machine"
    )
}

/// Claude Code's events and matchers, as the repo-level settings carry them.
const CLAUDE_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("SessionStart", None, "report"),
    ("UserPromptSubmit", None, "report"),
    ("PreToolUse", Some("Edit|Write|MultiEdit|Bash"), "gate"),
    ("PostToolUse", Some("Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob|Bash"), "report"),
    ("PostToolUseFailure", Some("Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob|Bash"), "report"),
    ("PostToolBatch", None, "report"),
    ("Stop", None, "report"),
    ("SessionEnd", None, "report"),
];

fn is_machine_entry(entry: &Value) -> bool {
    entry.to_string().contains("# collide-machine")
}

/// `~/.claude/settings.json` with Collide's machine hooks in, everything the
/// person had kept as it was, and an older copy of ours replaced.
pub fn merge_claude_settings(existing: &str) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    for (event, matcher, sub) in CLAUDE_EVENTS {
        let mut kept: Vec<Value> = hooks
            .get(*event)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| !is_machine_entry(entry))
            .collect();
        let mut entry = json!({"hooks": [{"type": "command", "command": machine_command(sub), "timeout": 30}]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// The same file with Collide's machine hooks taken out again.
pub fn remove_from_claude_settings(existing: &str) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    if let Some(hooks) = current.get_mut("hooks").and_then(Value::as_object_mut) {
        for entries in hooks.values_mut() {
            if let Some(list) = entries.as_array_mut() {
                list.retain(|entry| !is_machine_entry(entry));
            }
        }
        hooks.retain(|_, entries| entries.as_array().map_or(true, |l| !l.is_empty()));
    }
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// Codex's events, as its per-repo hooks carry them (`apply_patch` is its
/// file edit, `shell` its terminal).
const CODEX_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("SessionStart", None, "report"),
    ("UserPromptSubmit", None, "report"),
    ("PreToolUse", Some("^(apply_patch|Edit|Write|MultiEdit|shell)$"), "gate"),
    ("PostToolUse", Some("^(apply_patch|Edit|Write|MultiEdit|Read|Grep|Glob|Bash|shell)$"), "report"),
    ("Stop", None, "report"),
];

/// `~/.codex/hooks.json` with Collide's machine hooks in: Codex's format is
/// Claude Code's (a `hooks` object of event -> matcher entries).
pub fn merge_codex_hooks(existing: &str) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    for (event, matcher, sub) in CODEX_EVENTS {
        let mut kept: Vec<Value> = hooks.get(*event).and_then(Value::as_array).cloned().unwrap_or_default()
            .into_iter().filter(|entry| !is_machine_entry(entry)).collect();
        let mut entry = json!({"hooks": [{"type": "command", "command": machine_command_for(sub, "codex"), "timeout": 30}]});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

/// Cursor's events (camelCase, one command per entry).
const CURSOR_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("sessionStart", None, "report"),
    ("beforeSubmitPrompt", None, "report"),
    ("preToolUse", Some("^(Write|Edit|MultiEdit|Delete|Shell)$"), "gate"),
    ("postToolUse", Some("^(Write|Edit|MultiEdit|Delete|Read|Glob|Grep|Shell)$"), "report"),
    ("stop", None, "report"),
    ("sessionEnd", None, "report"),
];

/// `~/.cursor/hooks.json` with Collide's machine hooks in.
pub fn merge_cursor_hooks(existing: &str) -> String {
    let mut current: serde_json::Map<String, Value> =
        serde_json::from_str::<Value>(existing).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    current.entry("version").or_insert(json!(1));
    let mut hooks = current.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default();
    for (event, matcher, sub) in CURSOR_EVENTS {
        let mut kept: Vec<Value> = hooks.get(*event).and_then(Value::as_array).cloned().unwrap_or_default()
            .into_iter().filter(|entry| !is_machine_entry(entry)).collect();
        let mut entry = json!({"command": machine_command_for(sub, "cursor"), "timeout": 30});
        if let Some(matcher) = matcher {
            entry["matcher"] = json!(matcher);
        }
        kept.push(entry);
        hooks.insert((*event).to_string(), Value::Array(kept));
    }
    current.insert("hooks".into(), Value::Object(hooks));
    format!("{}\n", serde_json::to_string_pretty(&Value::Object(current)).unwrap_or_default())
}

// ------------------------------------------------------------ AGENTS.md

/// The section a repo's AGENTS.md carries for the agents Collide connects on
/// a machine. Committed and shared, so it names no path on this machine.
pub const AGENTS_SECTION: &str = "<!-- collide:start -->
## Collide: the agents in this repo work as a team

Other agents may be editing this repo at the same time. Collide's hooks keep
you in step with them, with no calls from you:

- A **Collide briefing** arrives at session start and with prompts: the
  signatures, callers and recent changes of the code you are about to touch.
  Build on it instead of re-reading files.
- The **gate** checks every write. When it holds one, it says whose live
  change it protects: adapt to it, then retry.
- Lines starting with **Δ** in a tool result are other agents' changes since
  your last step: adapt to them, never undo them.
- A **lint** finding means another agent renamed, removed or re-signatured
  something you use: fix your side.
- `git push` lands through Collide: it brings in the others' commits and
  pushes once, so a push needs no pull or retry from you.
- To tell the others something, run `collide message \"...\"` (the full
  command is in your session-start note; `--to agent-xxxx` for one agent,
  `--about path.py::symbol` for whoever is on that code). Messages for you
  arrive in your tool results: act on them.
- The `collide` MCP tools answer what a briefing did not: `get_symbol`,
  `blast_radius` before changing something shared, `recap` for what changed
  and why, `remember` for a decision about specific code.
<!-- collide:end -->";

const AGENTS_ASK_EVERY_S: f64 = 24.0 * 3600.0;

/// For a repo Collide covers through this machine's install: when its
/// AGENTS.md has no Collide section, the ask to add one. Told to one session
/// per repo per day, so agents starting together do not all edit the file.
pub fn agents_md_ask(root: &Path, env: &Env) -> Option<String> {
    if root.join(".collide").join("config.json").exists() {
        return None; // a repo set up on its own writes the section itself
    }
    let path = root.join("AGENTS.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.contains("<!-- collide:start -->") {
        return None;
    }
    use sha2::{Digest, Sha256};
    // by repo, not folder: twelve clones of one repo are one ask
    let key: String = Sha256::digest(repo_id_for(root).as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let marker = local_dir(env).join("agents-md").join(key);
    let asked: f64 = std::fs::read_to_string(&marker).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0.0);
    if now_s() - asked < AGENTS_ASK_EVERY_S {
        return None;
    }
    let _ = std::fs::create_dir_all(marker.parent()?);
    let _ = std::fs::write(&marker, format!("{:.0}", now_s()));
    let file = if current.trim().is_empty() { "an AGENTS.md (it has none)" } else { "its AGENTS.md, after what is already there" };
    Some(format!(
        "Collide: this repo's agents have no written note on working alongside each other. When it fits, ask the user \
whether to add Collide's section to {file}; it tells every agent here (Claude Code, Codex, Cursor) how the briefings, the \
gate, messages and landing work. If they say yes, run: {}{} agents-md (it adds the section and changes nothing else), \
then commit the file.",
        home_prefix(env),
        program_path(env).display()
    ))
}

/// `collide agents-md`: add Collide's section to this repo's AGENTS.md,
/// after everything already in it (or replace an older Collide section in
/// place). Nothing else in the file changes.
pub fn agents_md(env: &Env) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = config::find_repo_root(&[Some(cwd.clone())]).unwrap_or(cwd);
    let path = root.join("AGENTS.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = with_agents_section(&current);
    if updated == current {
        println!("Collide: AGENTS.md already has Collide's section.");
        return 0;
    }
    if std::fs::write(&path, &updated).is_err() {
        println!("Collide: could not write {}.", path.display());
        return 1;
    }
    let _ = env;
    println!("Collide: added its section to {}. Nothing else in the file changed; commit it when you are ready.", path.display());
    0
}

/// The file with Collide's section: an existing one replaced in place,
/// otherwise added at the end.
fn with_agents_section(current: &str) -> String {
    const START: &str = "<!-- collide:start -->";
    const END: &str = "<!-- collide:end -->";
    if let (Some(a), Some(b)) = (current.find(START), current.find(END)) {
        if b > a {
            return format!("{}{AGENTS_SECTION}{}", &current[..a], &current[b + END.len()..]);
        }
    }
    let body = current.trim_end();
    if body.is_empty() { format!("{AGENTS_SECTION}\n") } else { format!("{body}\n\n{AGENTS_SECTION}\n") }
}

// ------------------------------------------------------------ MCP tools

const CODEX_MCP_MARK: &str = "# collide-machine: Collide's MCP tools";

/// How an agent tool starts Collide's MCP tools: this program, `mcp`.
fn mcp_entry(env: &Env) -> Value {
    let mut entry = json!({"type": "stdio", "command": program_path(env).to_string_lossy(), "args": ["mcp"]});
    let collide_home = config::get(env, "COLLIDE_HOME");
    if !home_prefix(env).is_empty() {
        entry["env"] = json!({"COLLIDE_HOME": collide_home});
    }
    entry
}

/// A JSON file with an `mcpServers` map (Claude Code's ~/.claude.json,
/// Cursor's ~/.cursor/mcp.json) with Collide's entry set, or removed when
/// `entry` is None. Everything else in the file is kept; a file that is not
/// JSON is left alone (None).
fn merge_mcp_json(existing: &str, entry: Option<&Value>) -> Option<String> {
    let mut current: Value = if existing.trim().is_empty() { json!({}) } else { serde_json::from_str(existing).ok()? };
    let map = current.as_object_mut()?;
    match entry {
        Some(entry) => {
            let servers = map.entry("mcpServers").or_insert_with(|| json!({}));
            servers.as_object_mut()?.insert("collide".into(), entry.clone());
        }
        None => {
            if let Some(servers) = map.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove("collide");
            }
        }
    }
    Some(format!("{}\n", serde_json::to_string_pretty(&current).ok()?))
}

/// Codex's ~/.codex/config.toml with Collide's MCP server, or without it.
fn merge_codex_mcp(existing: &str, entry: Option<&Value>) -> String {
    // our block runs from the mark to the next table or the end
    let mut kept: Vec<&str> = Vec::new();
    let mut ours = false;
    for line in existing.lines() {
        if line.trim() == CODEX_MCP_MARK {
            ours = true;
            continue;
        }
        if ours && line.trim_start().starts_with('[') && line.trim() != "[mcp_servers.collide]" && !line.trim().starts_with("[mcp_servers.collide.") {
            ours = false;
        }
        if !ours {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n").trim_end().to_string();
    if let Some(entry) = entry {
        let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("{CODEX_MCP_MARK}\n[mcp_servers.collide]\ncommand = {}\nargs = [\"mcp\"]\n",
            quote(entry["command"].as_str().unwrap_or(""))));
        if let Some(home) = entry.pointer("/env/COLLIDE_HOME").and_then(Value::as_str) {
            out.push_str(&format!("env = {{ COLLIDE_HOME = {} }}\n", quote(home)));
        }
    } else if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The MCP configs Collide fills, beside the hooks: (name, file, is_toml).
fn mcp_files(env: &Env, with_claude: bool) -> Vec<(&'static str, PathBuf, bool)> {
    let home = PathBuf::from(crate::check::home(env));
    let mut out = Vec::new();
    for (name, _, _) in tools(env) {
        match name {
            "Claude Code" if with_claude => out.push((name, home.join(".claude.json"), false)),
            "Codex" => out.push((name, home.join(".codex").join("config.toml"), true)),
            "Cursor" => out.push((name, home.join(".cursor").join("mcp.json"), false)),
            _ => {}
        }
    }
    out
}

/// Claude Code rewrites ~/.claude.json while it runs, so its own command
/// changes it when it is installed. False when there is no `claude` to ask.
fn claude_mcp(env: &Env, entry: Option<&Value>) -> bool {
    let mut command = Command::new(exe("claude"));
    match entry {
        Some(entry) => command.args(["mcp", "add-json", "-s", "user", "collide", &entry.to_string()]),
        None => command.args(["mcp", "remove", "-s", "user", "collide"]),
    };
    command.env("HOME", crate::check::home(env)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match command.status() {
        Ok(status) => status.success() || entry.is_none(), // removing what was never there fails, and is still done
        Err(_) => false,
    }
}

/// Add (or with `entry` None, take out) Collide's MCP tools for each tool.
fn write_mcp(env: &Env, entry: Option<&Value>, with_claude: bool) {
    if with_claude && config::get(env, "COLLIDE_CLAUDE_CLI") != "0" && entry.is_some() {
        let _ = claude_mcp(env, None);
    }
    for (name, path, toml) in mcp_files(env, with_claude) {
        if name == "Claude Code" && config::get(env, "COLLIDE_CLAUDE_CLI") != "0" && claude_mcp(env, entry) {
            continue;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if entry.is_none() && existing.is_empty() {
            continue;
        }
        let merged = if toml { Some(merge_codex_mcp(&existing, entry)) } else { merge_mcp_json(&existing, entry) };
        if let Some(merged) = merged {
            if merged != existing {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                let _ = std::fs::write(&path, merged);
            }
        }
    }
}

/// The agent tools on this machine whose user-level hooks Collide fills:
/// (name, file, merge). Claude Code always; Codex and Cursor when present.
fn tools(env: &Env) -> Vec<(&'static str, PathBuf, fn(&str) -> String)> {
    let home = PathBuf::from(crate::check::home(env));
    let mut out: Vec<(&'static str, PathBuf, fn(&str) -> String)> =
        vec![("Claude Code", claude_settings_path(env), merge_claude_settings)];
    let on_path = |name: &str| {
        std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(exe(name)).is_file()))
    };
    if home.join(".codex").is_dir() || on_path("codex") {
        out.push(("Codex", home.join(".codex").join("hooks.json"), merge_codex_hooks));
    }
    if home.join(".cursor").is_dir() || on_path("cursor") || on_path("cursor-agent") {
        out.push(("Cursor", home.join(".cursor").join("hooks.json"), merge_cursor_hooks));
    }
    out
}

fn copy_executable(from: &Path, to: &Path) -> Result<(), String> {
    if from == to {
        return Ok(());
    }
    let staged = to.with_extension("new");
    std::fs::copy(from, &staged).map_err(|e| format!("{}: {e}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    // a running copy on Windows cannot be replaced, only renamed aside
    if to.exists() {
        let aside = to.with_extension("old");
        let _ = std::fs::remove_file(&aside);
        let _ = std::fs::rename(to, &aside);
    }
    std::fs::rename(&staged, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// `collide install [--no-settings] [--claim CODE --server URL]`: set
/// Collide up for every repo on this machine. The program copies itself to
/// `~/.collide/bin/collide`: it is both the hooks and the local server.
pub fn install(args: &[String], env: &Env) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    if !serves_local() {
        println!("Collide install: this is the per-repo hook. Install the free version with: curl -fsSL https://collidemcp.com/install.sh | sh");
        return 1;
    }
    let bin = bin_dir(env);
    if std::fs::create_dir_all(&bin).is_err() {
        println!("Collide install: could not create {}.", bin.display());
        return 1;
    }
    let me = std::env::current_exe().unwrap_or_default();
    if let Err(problem) = copy_executable(&me, &program_path(env)) {
        println!("Collide install: could not place the program ({problem}).");
        return 1;
    }
    // an install from before the merge: its two binaries make way
    for old in ["collide-hook", "collide-server"] {
        let _ = std::fs::remove_file(bin.join(exe(old)));
    }

    // the machine's mode: local unless a paid login already made it cloud
    let mut machine = settings(env);
    if machine.as_object().map_or(true, |m| m.is_empty()) {
        machine = json!({"mode": "local"});
    }
    if !save_settings(env, &machine) {
        println!("Collide install: could not write ~/.collide/machine.json.");
        return 1;
    }
    machine_id(env);

    // Claude Code: the user-level settings apply to every project. The
    // plugin brings its own hooks and installs with --no-settings.
    // the plugin brings Claude Code's hooks itself and installs with
    // --no-settings; the other tools are still Collide's to fill
    let quiet = args.iter().any(|a| a == "--no-settings");
    let mut covered: Vec<String> = Vec::new();
    for (name, path, merge) in tools(env) {
        if quiet && name == "Claude Code" {
            continue;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        if std::fs::write(&path, merge(&existing)).is_err() {
            println!("Collide install: could not update {}.", path.display());
            return 1;
        }
        covered.push(format!("{name} ({})", path.display()));
    }

    // the MCP tools (get_symbol, blast_radius, recap, ...) from the local
    // server, for every tool; the plugin brings Claude Code's itself
    write_mcp(env, Some(&mcp_entry(env)), !quiet);

    let running = ensure_local(env, Duration::from_secs(5));
    if quiet {
        return 0;
    }
    let (base, _) = local_endpoint(env);
    println!("Collide is installed for every repo on this machine.");
    println!("  hooks:  {}", covered.join(", "));
    println!("  server: {} ({})", base, if running { "running" } else { "starts with the next session" });
    println!("  data:   {} (stays on this machine)", local_dir(env).display());
    println!("Start or restart your agent sessions; agents in the same repo now see each other's work and messages.");
    // installed from `setup`: the code links this machine to that account
    let code = arg("--claim");
    let server = arg("--server");
    if !code.is_empty() && !server.is_empty() {
        let server = server.trim_end_matches('/').to_string();
        let mut body = machine_body(env);
        body["code"] = json!(code);
        let claimed = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build()
            .post(&format!("{server}/machine/claim"))
            .send_json(body)
            .ok()
            .and_then(|r| r.into_json::<Value>().ok())
            .unwrap_or(Value::Null);
        if claimed.get("ok").and_then(Value::as_bool) == Some(true) {
            return link(&server, &claimed, env);
        }
        let why = text(&claimed, "error");
        println!("Collide is installed, but not linked to your account{}. Link it any time with: ~/.collide/bin/collide login",
            if why.is_empty() { String::new() } else { format!(" ({why})") });
        return 0;
    }
    println!("To bring in your team: ~/.collide/bin/collide upgrade");
    0
}

/// `collide uninstall`: take the machine hooks out. The local data stays.
pub fn uninstall(env: &Env) -> i32 {
    for (_, path, _) in tools(env) {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let _ = std::fs::write(&path, remove_from_claude_settings(&existing));
        }
    }
    write_mcp(env, None, true);
    let path = collide_dir(env).join("machine.json");
    let _ = std::fs::remove_file(path);
    println!("Collide's machine hooks are removed. Your local data is kept in {}.", local_dir(env).display());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_merge_keeps_everything_else_and_replaces_its_own() {
        let theirs = r#"{"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}"#;
        let once = merge_claude_settings(theirs);
        let twice = merge_claude_settings(&once);
        assert_eq!(once, twice, "installing twice changes nothing");
        let parsed: Value = serde_json::from_str(&twice).unwrap();
        assert_eq!(parsed["model"], json!("opus"));
        let stop = parsed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "theirs kept, ours added once: {stop:?}");
        let removed: Value = serde_json::from_str(&remove_from_claude_settings(&twice)).unwrap();
        assert_eq!(removed["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(removed["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn codex_and_cursor_get_the_machine_hooks_in_their_own_dialects() {
        let codex: Value = serde_json::from_str(&merge_codex_hooks(r#"{"hooks": {"Stop": [{"hooks": [{"command": "mine"}]}]}}"#)).unwrap();
        assert_eq!(codex["hooks"]["Stop"].as_array().unwrap().len(), 2, "theirs kept");
        assert!(codex["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("gate --harness codex"));
        assert_eq!(merge_codex_hooks(&merge_codex_hooks("")), merge_codex_hooks(""), "installing twice changes nothing");
        let cursor: Value = serde_json::from_str(&merge_cursor_hooks("")).unwrap();
        assert_eq!(cursor["version"], json!(1));
        assert!(cursor["hooks"]["beforeSubmitPrompt"][0]["command"].as_str().unwrap().contains("report --harness cursor"));
        let removed: Value = serde_json::from_str(&remove_from_claude_settings(&merge_cursor_hooks(""))).unwrap();
        assert!(removed["hooks"].as_object().unwrap().is_empty(), "{removed}");
    }

    #[test]
    fn the_mcp_tools_are_added_beside_everything_else_and_taken_out_cleanly() {
        let entry = json!({"type": "stdio", "command": "/h/.collide/bin/collide", "args": ["mcp"]});
        let theirs = r#"{"numStartups": 9, "mcpServers": {"github": {"command": "gh-mcp"}}}"#;
        let added: Value = serde_json::from_str(&merge_mcp_json(theirs, Some(&entry)).unwrap()).unwrap();
        assert_eq!(added["numStartups"], json!(9));
        assert_eq!(added["mcpServers"]["github"]["command"], json!("gh-mcp"));
        assert_eq!(added["mcpServers"]["collide"]["args"], json!(["mcp"]));
        let removed: Value = serde_json::from_str(&merge_mcp_json(&added.to_string(), None).unwrap()).unwrap();
        assert!(removed["mcpServers"].get("collide").is_none() && removed["mcpServers"].get("github").is_some());
        assert!(merge_mcp_json("not json", Some(&entry)).is_none(), "a file that is not JSON is left alone");

        let toml = "model = \"o4\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let once = merge_codex_mcp(toml, Some(&entry));
        assert_eq!(merge_codex_mcp(&once, Some(&entry)), once, "installing twice changes nothing");
        assert!(once.contains("[mcp_servers.other]") && once.contains("[mcp_servers.collide]\ncommand = \"/h/.collide/bin/collide\""), "{once}");
        assert_eq!(merge_codex_mcp(&once, None).trim(), toml.trim());
    }

    #[test]
    fn agents_md_is_asked_for_once_per_repo_and_never_once_it_is_there() {
        let base = std::env::temp_dir().join(format!("collide-agentsmd-{}", std::process::id()));
        let repo = base.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "# House rules\nUse tabs.\n").unwrap();
        let mut env = Env::new();
        env.insert("COLLIDE_HOME".into(), base.join("home").to_string_lossy().to_string());
        let ask = agents_md_ask(&repo, &env).expect("first session is asked");
        assert!(ask.contains("ask the user") && ask.contains("agents-md"), "{ask}");
        let added = with_agents_section("# House rules\nUse tabs.\n");
        assert!(added.starts_with("# House rules\nUse tabs.\n\n<!-- collide:start -->"), "{added}");
        assert_eq!(with_agents_section(&added), added, "adding twice changes nothing");
        let older = added.replace("work as a team", "work together (old)");
        assert_eq!(with_agents_section(&older), added, "an older section is replaced in place");
        assert!(agents_md_ask(&repo, &env).is_none(), "the next session starting alongside is not");
        let _ = std::fs::remove_dir_all(base.join("home"));
        std::fs::write(repo.join("AGENTS.md"), format!("# House rules\n\n{AGENTS_SECTION}\n")).unwrap();
        assert!(agents_md_ask(&repo, &env).is_none(), "added: never asked again");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_hosted_remote_is_a_repo_a_team_can_share() {
        for shared in ["github.com/acme/api", "gitlab.com/group/sub/app", "git.acme.io/team/app"] {
            assert!(is_hosted(shared), "{shared}");
        }
        for local in ["local/my-app", "my-app", "acme/api", "C:/Users/me/app", ""] {
            assert!(!is_hosted(local), "{local}");
        }
    }

    #[test]
    fn a_message_command_with_settings_in_front_is_still_signed_by_its_agent() {
        let input = |command: &str| json!({"tool_name": "Bash", "session_id": "s-1", "tool_input": {"command": command}}).to_string();
        for command in ["~/.collide/bin/collide message hi", "COLLIDE_HOME=/tmp/h /tmp/h/.collide/bin/collide message hi"] {
            let out = message_rewrite(&input(command)).expect(command);
            assert!(out.contains("COLLIDE_SESSION=s-1"), "{out}");
        }
        for command in [
            "collide message \"renamed amount_due -> checkout_total; switch over\"",
            "collide message 'a | b & c' 2>&1",
            "collide message \\\n  \"hi there\"",
        ] {
            assert!(message_rewrite(&input(command)).is_some(), "{command}");
        }
        assert!(message_rewrite(&input("collide message \"$(cat ~/.ssh/id_rsa)\"")).is_none());
        assert!(message_rewrite(&input("collide message hi > /etc/x")).is_none());
        assert!(message_rewrite(&input("cd x\ncollide message hi")).is_none());
        assert!(message_rewrite(&input("COLLIDE_HOME=/tmp/h ls")).is_none());
        assert!(message_rewrite(&input("collide message hi && rm -rf x")).is_none());
    }

    #[test]
    fn a_repo_without_an_origin_is_named_by_its_folder() {
        let dir = std::env::temp_dir().join(format!("collide-machine-{}", std::process::id())).join("my-app");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(repo_id_for(&dir), "local/my-app");
    }
}

// ------------------------------------------------------------ agent messages

/// The exact command an agent on this machine runs to message the others:
/// this program's own path, with its home when that is not the default (a
/// `~/.collide/bin/collide` that does not exist reads as no way to talk).
pub fn message_command(env: &Env) -> String {
    let program = std::env::current_exe()
        .ok()
        .filter(|me| me.file_stem().and_then(|s| s.to_str()) == Some(PROGRAM))
        .unwrap_or_else(|| program_path(env));
    format!("{}{} message", home_prefix(env), program.display())
}

/// `COLLIDE_HOME=... ` when Collide's home is not the default one, for a
/// command an agent runs in its own shell, which does not carry it.
pub fn home_prefix(env: &Env) -> String {
    let collide_home = config::get(env, "COLLIDE_HOME");
    let home = config::get(env, "HOME");
    if !collide_home.is_empty() && collide_home != home { format!("COLLIDE_HOME={collide_home} ") } else { String::new() }
}

/// PreToolUse (Bash): an agent about to run `collide message ...` gets
/// its session stamped on the command (`COLLIDE_SESSION=...`), so the
/// message is signed by that agent rather than by the person. Only a command
/// that is nothing but the message is rewritten and let through: anything
/// chained onto it (`;`, `&&`, pipes, redirects, substitutions) is left
/// exactly as it was, for the harness's own permission check.
pub fn message_rewrite(stdin_data: &str) -> Option<String> {
    let input: Value = serde_json::from_str(stdin_data).ok()?;
    if input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = input.pointer("/tool_input/command").and_then(Value::as_str)?.trim().to_string();
    let session = text(&input, "session_id");
    if session.is_empty() || command.contains("COLLIDE_SESSION=") {
        return None;
    }
    // a line continuation is a space; `2>&1` only joins its own output
    let command = command.replace("\\\n", " ").replace(" 2>&1", "");
    // `VAR=value` settings may come first; the program is the word after them
    let words: Vec<&str> = command.split_whitespace().collect();
    let at = words.iter().position(|w| {
        let name = w.split('=').next().unwrap_or("");
        !(w.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    });
    let Some(at) = at else { return None };
    let first = words[at];
    let is_ours = ["collide", "collide.exe", "collide-hook", "collide-hook.exe"]
        .iter()
        .any(|name| first == *name || first.ends_with(&format!("/{name}")) || first.ends_with(&format!("\\{name}")));
    if !is_ours || words.get(at + 1) != Some(&"message") {
        return None;
    }
    if chains_anything(&command) {
        return None;
    }
    let safe_session: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let mut updated = input.get("tool_input").cloned().unwrap_or(json!({}));
    updated["command"] = json!(format!("COLLIDE_SESSION={safe_session} {command}"));
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": "Collide: a message to the other agents.",
            "updatedInput": updated,
        }})
        .to_string(),
    )
}

/// Whether a command does anything besides run one program: chaining,
/// pipes, redirects, substitutions or a second line, outside quotes. Text in
/// single quotes is inert; in double quotes only a substitution is live.
fn chains_anything(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if !single => i += 1,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return true,
            '$' if !single && chars.get(i + 1) == Some(&'(') => return true,
            ';' | '&' | '|' | '>' | '<' | '\n' if !single && !double => return true,
            _ => {}
        }
        i += 1;
    }
    single || double
}

/// Where the hooks note which agent session each harness process runs.
fn agents_dir(env: &Env) -> PathBuf {
    local_dir(env).join("agents")
}

/// A process's parent and its program name, or None when unknown.
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // pid (comm) state ppid ...: comm may hold spaces, so split at the last ')'
        let close = stat.rfind(')')?;
        let comm = stat[stat.find('(')? + 1..close].to_string();
        let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
        return Some((ppid, comm));
    }
    let out = Command::new("ps").args(["-o", "ppid=,comm=", "-p", &pid.to_string()]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (ppid, comm) = line.split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, comm.trim().rsplit('/').next().unwrap_or("").to_string()))
}

#[cfg(not(unix))]
fn parent_of(_pid: u32) -> Option<(u32, String)> {
    None
}

fn is_shell(comm: &str) -> bool {
    let name = comm.trim_start_matches('-');
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh")
}

/// How long a session counts as at work in its harness process: one
/// process may host several agents at once (Cursor's chats share its app).
const AGENT_PROCESS_WINDOW_S: f64 = 120.0;

/// A hook runs as a child of the agent's harness (sometimes through a
/// shell). Note when each session last ran in that process, so a `collide
/// message` the agent runs, a descendant of the same process, is signed by
/// the agent even when its command was not stamped with the session.
fn note_agent_process(session: &str, env: &Env) {
    if session.is_empty() || session.contains(char::is_whitespace) {
        return;
    }
    let Some((mut harness, comm)) = parent_of(std::process::id()) else { return };
    if is_shell(&comm) {
        let Some((up, _)) = parent_of(harness) else { return };
        harness = up;
    }
    if harness <= 1 {
        return;
    }
    let path = agents_dir(env).join(harness.to_string());
    let stamp = now_s();
    let mut seen: Vec<(String, f64)> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' ').and_then(|(s, t)| Some((s.to_string(), t.parse().ok()?))))
        .filter(|(s, t)| s != session && stamp - t < AGENT_PROCESS_WINDOW_S)
        .collect();
    seen.push((session.to_string(), stamp));
    let body: String = seen.iter().map(|(s, t)| format!("{s} {t:.0}\n")).collect();
    let _ = std::fs::create_dir_all(agents_dir(env));
    let _ = std::fs::write(&path, body);
}

/// The agent session this command runs under: COLLIDE_SESSION when stamped,
/// else the one session at work in the nearest ancestor process a hook
/// noted. Two at work in one process: no way to tell, so no session.
fn running_session(env: &Env) -> String {
    let stamped = config::get(env, "COLLIDE_SESSION");
    if !stamped.is_empty() {
        return stamped.to_string();
    }
    let stamp = now_s();
    let mut pid = std::process::id();
    for _ in 0..8 {
        let Some((up, _)) = parent_of(pid) else { break };
        if up <= 1 {
            break;
        }
        if let Ok(body) = std::fs::read_to_string(agents_dir(env).join(up.to_string())) {
            let live: Vec<&str> = body
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter(|(_, t)| t.parse::<f64>().is_ok_and(|t| stamp - t < AGENT_PROCESS_WINDOW_S))
                .map(|(s, _)| s)
                .collect();
            return if live.len() == 1 { live[0].to_string() } else { String::new() };
        }
        pid = up;
    }
    String::new()
}

/// `collide message [--to AGENT | --about path.py::symbol] TEXT`: tell
/// the other agents in this repo something. With neither flag it goes to
/// every agent at work in the repo right now.
pub fn message(args: &[String], env: &Env) -> i32 {
    let mut to = String::new();
    let mut about = String::new();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                to = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--about" => {
                about = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            word => {
                words.push(word.to_string());
                i += 1;
            }
        }
    }
    let said = words.join(" ");
    if said.trim().is_empty() || matches!(said.trim(), "--help" | "-h" | "help") {
        println!("usage: collide message [--to AGENT | --about path.py::symbol] \"what to tell them\"");
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else {
        println!("Collide: run this inside a repository Collide covers.");
        return 1;
    };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        println!("Collide: this repository is not connected, so there is no one to tell.");
        return 1;
    }
    let payload = json!({
        "repo_id": cfg.repo_id, "session": running_session(env),
        "text": said, "to": to, "about": about,
    });
    let answer = crate::http::post(&cfg.server, "/message", &cfg.token, &crate::report::user_agent(), &payload, Duration::from_secs(5))
        .unwrap_or(Value::Null);
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = text(&answer, "error");
        println!("Collide: the message was not sent{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
        return 1;
    }
    let recipients: Vec<String> = answer
        .get("sent_to")
        .or_else(|| answer.get("delivered_to"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let note = text(&answer, "note");
    if !note.is_empty() {
        println!("Collide: {note}");
    } else if recipients.is_empty() {
        println!("Collide: sent.");
    } else {
        println!("Collide: sent to {}.", recipients.join(", "));
    }
    0
}

/// A hook response's waiting messages, for the context the hook prints.
pub fn inbox(response: &Value) -> String {
    text(response, "inbox_note")
}

// ------------------------------------------------------------ the summary

/// The least time between two daily summaries.
const SUMMARY_EVERY_S: f64 = 20.0 * 3600.0;

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "1.4M", "82k", "640".
pub fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// What the local server says happened since `since`, or `None`.
pub fn fetch_summary(cfg: &config::Config, since: f64, wait: Duration) -> Option<Value> {
    let answer = crate::http::post(&cfg.server, "/local/summary", &cfg.token, &crate::report::user_agent(), &json!({"since": since}), wait).ok()?;
    (answer.get("ok").and_then(Value::as_bool) == Some(true)).then_some(answer)
}

/// The summary in one line a person reads.
pub fn summary_line(summary: &Value, period: &str) -> String {
    let saved = number(summary, "tokens_saved");
    let usd = summary.get("est_usd_saved").and_then(Value::as_f64).unwrap_or(0.0);
    let skipped = number(summary, "messages_not_sent");
    let (agents, repos, changes, said) =
        (number(summary, "agents"), number(summary, "repos"), number(summary, "changes"), number(summary, "agent_messages"));
    let plural = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let mut parts: Vec<String> = Vec::new();
    if saved > 0 {
        parts.push(format!(
            "your agents skipped {} and about {} tokens (~${usd:.2} at cache-read rates)",
            plural(skipped, "message", "messages"), tokens(saved)
        ));
    }
    if changes > 0 {
        let who = if agents > 1 { format!("{agents} agents") } else { "your agents".to_string() };
        let mut shared = format!("{who} shared {} across {}", plural(changes, "change", "changes"), plural(repos.max(1), "repo", "repos"));
        if said > 0 {
            shared.push_str(&format!(" and sent each other {}", plural(said, "message", "messages")));
        }
        parts.push(shared);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("Collide {period}: {}.", parts.join("; "))
}

/// Once a day, at a session start in a repo the local server covers: what
/// Collide did since the last summary. `None` when it is not time yet, or
/// there is nothing to say.
pub fn daily_summary(cfg: &config::Config, env: &Env) -> Option<String> {
    if !cfg.machine_local {
        return None;
    }
    let path = local_dir(env).join("summary.json");
    let last = config::load_json(&path).get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let now = now_s();
    if now - last < SUMMARY_EVERY_S {
        return None;
    }
    let since = if last > 0.0 { last } else { now - 86_400.0 };
    let summary = fetch_summary(cfg, since, Duration::from_millis(800))?;
    let _ = std::fs::write(&path, json!({"ts": now}).to_string());
    let line = summary_line(&summary, "since yesterday on this machine");
    (!line.is_empty()).then_some(line)
}

/// `collide status`: what this machine runs, and what Collide did on
/// it in the last day and in all.
pub fn status(env: &Env) -> i32 {
    let machine = settings(env);
    let mode = text(&machine, "mode");
    if mode.is_empty() {
        println!("Collide is not installed for this machine. Install it with: curl -fsSL https://collidemcp.com/install.sh | sh");
        return 0;
    }
    if mode == "cloud" {
        println!("Collide: this machine is connected to your team ({}).", text(&machine, "server_url"));
        return 0;
    }
    let running = ensure_local(env, Duration::from_secs(3));
    let (base, token) = local_endpoint(env);
    println!("Collide (free, on this machine): {}", if running { format!("running at {base}") } else { "not running; it starts with the next agent session".to_string() });
    println!("  data: {}", local_dir(env).display());
    if !running {
        return 0;
    }
    let cfg = config::Config { server: base, token, repo_id: String::new(), verify: String::new(), local_parse: true, machine_local: true };
    for (period, since) in [("in the last day", now_s() - 86_400.0), ("since install", 0.0)] {
        if let Some(summary) = fetch_summary(&cfg, since, Duration::from_secs(5)) {
            let line = summary_line(&summary, period);
            println!("  {}", if line.is_empty() { format!("Collide {period}: nothing yet.") } else { line });
        }
    }
    println!("To bring in your team (other machines, the dashboard, prompt matching by meaning): ~/.collide/bin/collide upgrade");
    0
}

// ------------------------------------------------------------ the account

const CLOUD: &str = "https://mcp.collidemcp.com";
/// How often a signed-in free machine asks whether its workspace is paid
/// yet: soon after someone pays, the machine has moved.
const PLAN_CHECK_EVERY_S: f64 = 15.0 * 60.0;

/// Where this machine's binaries update from: its account's server, else
/// Collide's.
pub fn update_server(env: &Env) -> String {
    let from_account = settings(env).get("account").map(|a| text(a, "server_url")).unwrap_or_default();
    if from_account.is_empty() { CLOUD.to_string() } else { from_account }
}

/// This machine's id (made at install, kept) and its name.
pub fn machine_id(env: &Env) -> String {
    let mut machine = settings(env);
    let known = text(&machine, "machine_id");
    if !known.is_empty() {
        return known;
    }
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}{}{}", std::time::SystemTime::now(), std::process::id(), crate::check::home(env));
    let id: String = Sha256::digest(seed.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    if let Some(map) = machine.as_object_mut() {
        map.insert("machine_id".into(), json!(id));
        save_settings(env, &machine);
    }
    id
}

fn machine_name(env: &Env) -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        let value = config::get(env, key);
        if !value.is_empty() {
            return value.to_string();
        }
    }
    Command::new("hostname")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn local_cfg(env: &Env) -> config::Config {
    let (server, token) = local_endpoint(env);
    config::Config { server, token, repo_id: String::new(), verify: String::new(), local_parse: true, machine_local: true }
}

/// What this machine has done locally, for its record on the account.
fn local_stats(env: &Env) -> Value {
    ensure_local(env, Duration::from_secs(3));
    fetch_summary(&local_cfg(env), 0.0, Duration::from_secs(5))
        .map(|s| {
            json!({"tokens_saved": s["tokens_saved"], "est_usd_saved": s["est_usd_saved"], "agents": s["agents"],
                   "repos": s["repos"], "changes": s["changes"], "agent_messages": s["agent_messages"]})
        })
        .unwrap_or_else(|| json!({}))
}

fn machine_body(env: &Env) -> Value {
    json!({"machine": machine_id(env), "name": machine_name(env), "os": std::env::consts::OS, "stats": local_stats(env)})
}

/// A paid workspace: from now on this machine's hooks use Collide's servers.
fn switch_to_cloud(env: &Env, account: &Value) {
    let mut machine = settings(env);
    if let Some(map) = machine.as_object_mut() {
        map.insert("mode".into(), json!("cloud"));
        map.insert("server_url".into(), json!(text(account, "server_url")));
        map.insert("workspace".into(), json!(text(account, "workspace")));
    }
    save_settings(env, &machine);
}

/// `collide login` on a machine with Collide installed: sign in, record
/// the machine on the account, and either stay local (a Free workspace) or
/// move to Collide's servers and sync (a paid one).
pub fn login(args: &[String], env: &Env) -> i32 {
    login_as(args, env, "Collide CLI: login")
}

fn login_as(args: &[String], env: &Env, client_name: &str) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let server = {
        let given = arg("--server");
        let from_env = config::get(env, "COLLIDE_SERVER_URL").to_string();
        (if !given.is_empty() { given } else if !from_env.is_empty() { from_env } else { CLOUD.to_string() })
            .trim_end_matches('/')
            .to_string()
    };
    let access = match crate::login::sign_in_as(&server, env, client_name) {
        Ok(access) => access,
        Err(problem) => {
            println!("Collide login: {problem}.");
            return 1;
        }
    };
    let connected = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .post(&format!("{server}/machine/connect"))
        .set("Authorization", &format!("Bearer {access}"))
        .send_json(machine_body(env))
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .unwrap_or(Value::Null);
    if connected.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = text(&connected, "error");
        println!("Collide login: this machine could not be connected{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
        return 1;
    }
    link(&server, &connected, env)
}

/// What a sign-in or a claim answered, kept: the credential merged with any
/// others this machine holds, the account in machine.json, and the mode the
/// workspace's plan decides (local on Free, the team's servers once paid,
/// with the history synced).
fn link(server: &str, connected: &Value, env: &Env) -> i32 {
    // the credential, merged with any others this machine holds
    let fragment: serde_json::Map<String, Value> = connected
        .get("credentials")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter(|(k, _)| k.starts_with(&server)).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().join(format!("collide-machine-login-{}.json", std::process::id()));
    let saved = std::fs::write(&scratch, Value::Object(fragment).to_string()).is_ok()
        && crate::report::add_credentials(&scratch.to_string_lossy(), env) == 0;
    let _ = std::fs::remove_file(&scratch);
    if !saved {
        println!("Collide login: signed in, but the credential could not be saved to ~/.collide/credentials.json.");
        return 1;
    }
    let account = json!({
        "user": text(&connected, "user"), "workspace": text(&connected, "workspace"),
        "workspace_name": text(&connected, "workspace_name"), "server_url": text(&connected, "server_url"),
        "dashboard_url": text(&connected, "dashboard_url"), "checked": now_s(),
    });
    let mut machine = settings(env);
    if machine.as_object().map_or(true, |m| m.is_empty()) {
        machine = json!({"mode": "local"});
    }
    machine["account"] = account.clone();
    save_settings(env, &machine);
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { text(&account, "workspace") } else { named }
    };
    let who = text(&account, "user");
    if connected.get("paid").and_then(Value::as_bool) == Some(true) {
        switch_to_cloud(env, &account);
        println!("Signed in as {who}. This machine now works with your team in {name}; syncing its local history...");
        return sync(env);
    }
    println!(
        "Signed in as {who}. {name} is on Free, so this machine stays local: nothing it does leaves it. \
When {name} moves to Team (from the dashboard), this machine switches over and syncs its history by itself."
    );
    0
}

/// `collide sync`: send this machine's local record to its paid
/// workspace, batch by batch, from where the last sync stopped.
pub fn sync(env: &Env) -> i32 {
    let machine = settings(env);
    let account = machine.get("account").cloned().unwrap_or(Value::Null);
    let server = text(&account, "server_url");
    if server.is_empty() {
        println!("Collide sync: sign in first with: ~/.collide/bin/collide login");
        return 1;
    }
    let creds = config::load_json(&config::credentials_path(env));
    let token = config::resolve_token(&creds, &server, &text(&account, "workspace"));
    if !ensure_local(env, Duration::from_secs(5)) {
        println!("Collide sync: the local server did not start, so there is nothing to read from.");
        return 1;
    }
    let local = local_cfg(env);
    let cursor_path = local_dir(env).join("sync.json");
    let mut cursor = config::load_json(&cursor_path).get("cursor").cloned().unwrap_or_else(|| json!({}));
    let id = machine_id(env);
    let (mut rows, mut notes) = (0u64, 0u64);
    loop {
        let batch = crate::http::post(&local.server, "/local/export", &local.token, &crate::report::user_agent(),
            &json!({"cursor": cursor}), Duration::from_secs(30))
            .unwrap_or(Value::Null);
        if batch.get("ok").and_then(Value::as_bool) != Some(true) {
            println!("Collide sync: could not read the local record.");
            return 1;
        }
        let more = batch.get("more").and_then(Value::as_bool).unwrap_or(false);
        let body = json!({"machine": id, "repos": batch["repos"], "final": !more, "all_repos": batch["all_repos"]});
        let answer = crate::http::post(&server, "/machine/import", &token, &crate::report::user_agent(), &body, Duration::from_secs(60))
            .unwrap_or(Value::Null);
        if answer.get("ok").and_then(Value::as_bool) != Some(true) {
            let why = text(&answer, "error");
            println!("Collide sync: stopped{}. Run ~/.collide/bin/collide sync to continue.", if why.is_empty() { String::new() } else { format!(" ({why})") });
            return 1;
        }
        rows += answer.get("rows").and_then(Value::as_u64).unwrap_or(0);
        notes += answer.get("notes").and_then(Value::as_u64).unwrap_or(0);
        cursor = batch.get("cursor").cloned().unwrap_or(cursor);
        let _ = std::fs::write(&cursor_path, json!({"cursor": cursor, "ts": now_s()}).to_string());
        if !more {
            break;
        }
    }
    println!("Collide sync: {rows} entries of history and {notes} notes from this machine are in your workspace now.");
    0
}

/// At a session start on a signed-in free machine, now and then: is the
/// workspace paid yet? When it is, the machine moves to Collide's servers
/// and syncs in the background, with nothing for the person to do. Returns
/// the line to tell them, once.
pub fn plan_check(env: &Env) -> Option<String> {
    let mut machine = settings(env);
    if text(&machine, "mode") != "local" {
        return None;
    }
    let account = machine.get("account").cloned()?;
    let checked = account.get("checked").and_then(Value::as_f64).unwrap_or(0.0);
    if now_s() - checked < PLAN_CHECK_EVERY_S {
        return None;
    }
    machine["account"]["checked"] = json!(now_s());
    save_settings(env, &machine);
    let server = text(&account, "server_url");
    let creds = config::load_json(&config::credentials_path(env));
    let token = config::resolve_token(&creds, &server, &text(&account, "workspace"));
    if server.is_empty() || token.is_empty() {
        return None;
    }
    let answer = crate::http::post(&server, "/machine/status", &token, &crate::report::user_agent(), &machine_body(env), Duration::from_millis(1500))
        .ok()?;
    if answer.get("paid").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    switch_to_cloud(env, &account);
    // the history goes over in the background; the session does not wait
    if let Ok(exe) = std::env::current_exe() {
        let mut command = Command::new(exe);
        command.arg("sync").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _ = command.spawn();
    }
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { "your workspace".to_string() } else { named }
    };
    Some(format!(
        "Collide: {name} is on Team now, so this machine has switched to your team's workspace and is syncing its local history there. \
From this session on, agents on your teammates' machines see this one's work too. Tell the user this in one line."
    ))
}

/// `collide upgrade`: from a free machine to Team. Signs in first when the
/// machine is not linked yet, then opens checkout for its workspace; once
/// Team starts, the machine joins it by itself (the plan check) and brings
/// its history.
pub fn upgrade(args: &[String], env: &Env) -> i32 {
    if settings(env).get("account").is_none() {
        let code = login_as(args, env, "Collide CLI: upgrade");
        if code != 0 {
            return code;
        }
    }
    let machine = settings(env);
    let account = machine.get("account").cloned().unwrap_or(Value::Null);
    let name = {
        let named = text(&account, "workspace_name");
        if named.is_empty() { "your workspace".to_string() } else { named }
    };
    if text(&machine, "mode") == "cloud" {
        println!("This machine already works with your team in {name}.");
        return 0;
    }
    let site = {
        let given = text(&account, "dashboard_url");
        if given.is_empty() { "https://collidemcp.com".to_string() } else { given.trim_end_matches('/').to_string() }
    };
    let url = format!("{site}/dashboard/billing?workspace={}", text(&account, "workspace"));
    println!(
        "Opening checkout for {name}: {url}\nOnce Team starts (14 days free, no card), this machine joins it by itself within 15 minutes and brings its history."
    );
    if config::get(env, "COLLIDE_NO_BROWSER") != "1" {
        crate::login::open_browser(&url);
    }
    // the next session start checks right away, not in 15 minutes
    let mut machine = settings(env);
    if machine.get("account").is_some() {
        machine["account"]["checked"] = json!(0.0);
        save_settings(env, &machine);
    }
    0
}

/// `collide add`: this repo joins the team's workspace, the person's yes to
/// the question their agent asked. Only a repo on GitHub (or another git
/// host) can: no one else could be working in any other.
pub fn add(env: &Env) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else {
        println!("Collide: run this inside the git repository to add.");
        return 1;
    };
    let repo_id = repo_id_for(&root);
    if !is_hosted(&repo_id) {
        println!(
            "Collide: this repository has no remote on GitHub (or another git host), so it cannot be shared with a team. \
Push it to GitHub first; until then Collide keeps working for it on this machine."
        );
        return 1;
    }
    let cfg = config::config(Some(&root), env);
    if cfg.machine_local {
        println!(
            "Collide: on the free version every repository on this machine is already covered. \
To share {repo_id} with a team: ~/.collide/bin/collide upgrade"
        );
        return 0;
    }
    if !cfg.usable() {
        println!("Collide: this machine is not signed in. Run: ~/.collide/bin/collide login");
        return 1;
    }
    let answer = crate::http::post(&cfg.server, "/repo/add", &cfg.token, &crate::report::user_agent(),
        &json!({"repo_id": cfg.repo_id, "session": config::get(env, "COLLIDE_SESSION")}), Duration::from_secs(10))
        .unwrap_or(Value::Null);
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        let name = text(&answer, "workspace_name");
        println!("Collide: {} is in {} now; agents working in it share with the team.", cfg.repo_id,
            if name.is_empty() { "your workspace".to_string() } else { name });
        return 0;
    }
    let why = text(&answer, "error");
    println!("Collide: {} was not added{}.", cfg.repo_id, if why.is_empty() { String::new() } else { format!(" ({why})") });
    1
}
