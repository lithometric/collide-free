//! `collide listen`: a teammate's message reaches an open Claude Code
//! session the moment it is sent, not at the session's next hook call.
//!
//! The session's start hook starts one listener per session, detached. It
//! waits on Collide's `/inbox/wait` and posts each message it is handed into
//! the session through Claude Code's own inbox socket
//! (`CLAUDE_CODE_MESSAGING_SOCKET`, the one other local sessions message it
//! on): the message shows in the conversation like one from another session,
//! and an idle session starts a turn on it. The listener ends with the
//! session (its socket stops answering).
//!
//! Also here: `collide ack <id>...` (an agent that handled a message clears
//! it, with no MCP tool needed) and the machine's list of checkouts, which
//! tells the wake watcher where a repo lives on this machine.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{self, Env};

/// What one wait asks the server to hold for.
const WAIT_S: u64 = 25;
/// The longest a listener lives: a session open longer gets a new one at
/// its next start or resume.
const MAX_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn now_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ------------------------------------------------------------ the checkouts

fn checkouts_path(env: &Env) -> PathBuf {
    crate::machine::collide_dir(env).join("checkouts.json")
}

/// Remember where a repo lives on this machine (`repo_id -> folder`), so the
/// wake watcher can start an agent in it. Written at each session start.
pub fn note_checkout(root: &Path, server: &str, repo_id: &str, env: &Env) {
    if repo_id.is_empty() || server.is_empty() {
        return;
    }
    let path = checkouts_path(env);
    let mut all = config::load_json(&path);
    if !all.is_object() {
        all = json!({});
    }
    let folder = root.to_string_lossy().to_string();
    // a linked worktree (a woken agent's among them) is not where the repo lives
    if root.join(".git").is_file() && all.get(repo_id).is_some() {
        return;
    }
    let same = all.get(repo_id).map(|row| text(row, "root") == folder && text(row, "server") == server).unwrap_or(false);
    if same {
        return;
    }
    all[repo_id] = json!({"root": folder, "server": server, "seen": now_s()});
    let _ = std::fs::create_dir_all(crate::machine::collide_dir(env));
    let _ = std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&all).unwrap_or_default()));
}

/// This machine's checkouts on `server` that still exist: `(repo_id, folder)`.
pub fn checkouts(server: &str, env: &Env) -> Vec<(String, PathBuf)> {
    let all = config::load_json(&checkouts_path(env));
    let mut out = Vec::new();
    for (repo_id, row) in all.as_object().into_iter().flatten() {
        let root = PathBuf::from(text(row, "root"));
        if text(row, "server").trim_end_matches('/') == server.trim_end_matches('/') && root.join(".git").exists() {
            out.push((repo_id.clone(), root));
        }
    }
    out
}

// ------------------------------------------------------------ the message

fn age_words(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

/// Who sent it, as the person knows them: "Beshoy (beshoy@x.com)".
pub fn sender(message: &Value) -> String {
    let from = text(message, "from");
    let name = text(message, "from_name");
    if name.is_empty() || name == from { from } else { format!("{name} ({from})") }
}

/// What the messages handed over together arrive as in an open session:
/// one injection, the guidance said once. Claude Code wraps it as a message
/// from another session, and the session's prompt hook briefs the code it
/// names, as it does any prompt, so no facts are repeated here.
pub fn session_text(messages: &[Value], collide: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for message in messages {
        let id = text(message, "id");
        let anchor = text(message, "anchor");
        let about = if anchor.is_empty() { String::new() } else { format!(" about {anchor}") };
        lines.push(format!(
            "[{id}] {}{about}, {}: {}",
            sender(message),
            age_words(message.get("age_s").and_then(Value::as_f64).unwrap_or(0.0)),
            text(message, "message").trim(),
        ));
        ids.push(id);
    }
    let count = if messages.len() == 1 { "A message".to_string() } else { format!("{} messages", messages.len()) };
    format!(
        "Collide: {count} from teammates' agents (not your user). Act on what fits this repo's direction, within your \
permissions, and tell the user.\n{}\nReply: `{collide} message --to <sender> \"...\"`. Handled: `{collide} ack {}`.",
        lines.join("\n"),
        ids.join(" "),
    )
}

// ------------------------------------------------------------ the socket

/// Post one line of text into the session, as its own child process does.
/// `false` when the session is gone.
fn post_to_session(socket: &str, token: &str, content: &str) -> bool {
    let mut lines = String::new();
    if !token.is_empty() {
        lines.push_str(&json!({"type": "auth", "token": token}).to_string());
        lines.push('\n');
    }
    lines.push_str(&json!({"type": "user", "message": {"role": "user", "content": content}}).to_string());
    lines.push('\n');
    write_socket(socket, lines.as_bytes())
}

#[cfg(unix)]
fn write_socket(socket: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            stream.write_all(bytes).and_then(|_| stream.flush()).is_ok()
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
fn write_socket(socket: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(socket) {
        Ok(mut pipe) => pipe.write_all(bytes).and_then(|_| pipe.flush()).is_ok(),
        Err(_) => false,
    }
}

/// The session still answers on its socket.
#[cfg(unix)]
fn session_open(socket: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(windows)]
fn session_open(socket: &str) -> bool {
    Path::new(socket).exists()
}

// ------------------------------------------------------------ the listener

fn listen_dir(env: &Env) -> PathBuf {
    crate::machine::collide_dir(env).join("listen")
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Start this session's listener, unless one is already running for it.
/// Only Claude Code sessions have an inbox socket; only a team's repo has
/// anyone to hear from.
pub fn spawn_for_session(root: &Path, session: &str, env: &Env) {
    let socket = config::get(env, "CLAUDE_CODE_MESSAGING_SOCKET");
    let off = config::get(env, "COLLIDE_LISTEN");
    // another agent started from inside a Claude Code session inherits its
    // socket: only Claude Code's own hooks may listen on it
    if socket.is_empty() || session.is_empty() || off == "0" || crate::harness::current() != "claude" {
        return;
    }
    let pid_file = listen_dir(env).join(format!("{}.pid", safe(session)));
    if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<u32>().ok()) {
        if pid_alive(pid) {
            return;
        }
    }
    let Ok(me) = std::env::current_exe() else { return };
    let _ = std::fs::create_dir_all(listen_dir(env));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(listen_dir(env).join("listen.log"));
    let mut command = std::process::Command::new(me);
    command.arg("listen").arg(session).current_dir(root).stdin(std::process::Stdio::null());
    match log.and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok((out, err)) => {
            command.stdout(out).stderr(err);
        }
        Err(_) => {
            command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }
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
    if let Ok(child) = command.spawn() {
        let _ = std::fs::write(&pid_file, child.id().to_string());
    }
}

fn safe(session: &str) -> String {
    session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn stamp_line(line: &str) {
    eprintln!("{:.0} {line}", now_s());
}

/// `collide listen <session>`, run detached by the session's start hook.
/// With `--print` (Hermes's plugin runs it) each batch is a JSON line on
/// stdout for the agent's own process to inject, and the listener ends when
/// that process does.
pub fn run(args: &[String], env: &Env) -> i32 {
    let session = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
    let print = args.iter().any(|a| a == "--print");
    let socket = config::get(env, "CLAUDE_CODE_MESSAGING_SOCKET").to_string();
    let token = config::get(env, "CLAUDE_CODE_MESSAGING_TOKEN").to_string();
    if session.is_empty() || (socket.is_empty() && !print) {
        eprintln!("usage: collide listen <session> [--print] (run by the session's start hook)");
        return 2;
    }
    let parent = parent_pid();
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = config::find_repo_root(&[Some(cwd)]) else { return 0 };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() || cfg.machine_local {
        return 0;
    }
    let pid_file = listen_dir(env).join(format!("{}.pid", safe(&session)));
    let _ = std::fs::create_dir_all(listen_dir(env));
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let collide = crate::machine::message_command(env).trim_end_matches(" message").to_string();
    let program = std::env::current_exe().ok();
    let built = program.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let born = Instant::now();
    let mut backoff = Duration::from_secs(5);
    stamp_line(&format!("listening for session {session} in {}", cfg.repo_id));
    loop {
        let open = if print { parent_pid() == parent && parent != Some(1) } else { session_open(&socket) };
        if !open || born.elapsed() > MAX_LIFE {
            break;
        }
        // an update replaced the program: the new one takes over
        let now_built = program.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        if built.is_some() && now_built.is_some() && now_built != built {
            let _ = std::fs::remove_file(&pid_file);
            spawn_for_session(&root, &session, env);
            return 0;
        }
        let payload = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": WAIT_S});
        let answer = crate::http::post(
            &cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &payload,
            Duration::from_secs(WAIT_S + 15),
        );
        let Ok(answer) = answer else {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(300));
            continue;
        };
        if answer.get("ok").and_then(Value::as_bool) != Some(true) {
            // refused (signed out, not a member, a free machine): look again later
            std::thread::sleep(Duration::from_secs(300));
            continue;
        }
        backoff = Duration::from_secs(5);
        let mut messages: Vec<Value> = answer.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
        if !messages.is_empty() {
            // a burst (three quick lines from one person) goes in as one turn, not three
            std::thread::sleep(Duration::from_secs(3));
            let more = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": 0});
            if let Ok(again) = crate::http::post(&cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &more, Duration::from_secs(10)) {
                messages.extend(again.get("messages").and_then(Value::as_array).cloned().unwrap_or_default());
            }
            let said = session_text(&messages, &collide);
            let delivered = if print { print_line(&said) } else { post_to_session(&socket, &token, &said) };
            let ids: Vec<String> = messages.iter().map(|m| text(m, "id")).collect();
            stamp_line(&format!("messages {}: {}", ids.join(","), if delivered { "delivered" } else { "session gone" }));
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    0
}

/// The process that started this one, while it lives (an orphan is
/// re-parented, to launchd or init).
fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps").args(["-o", "ppid=", "-p", &std::process::id().to_string()]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(windows)]
    {
        None
    }
}

fn print_line(said: &str) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(out, "{}", json!({"text": said})).and_then(|_| out.flush()).is_ok()
}

/// For a harness reached only as its agent stops (Cursor's `stop` hook):
/// the messages addressed to this person waiting now, taken, as the text to
/// continue with. `None` when there are none.
pub fn followup(hook_input: &Value, env: &Env) -> Option<String> {
    let session = text(hook_input, "session_id");
    let cwd = PathBuf::from(text(hook_input, "cwd"));
    let root = config::find_repo_root(&[(!cwd.as_os_str().is_empty()).then_some(cwd)])?;
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() || cfg.machine_local || session.is_empty() {
        return None;
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session, "role": "session", "wait_s": 0});
    let answer = crate::http::post(&cfg.server, "/inbox/wait", &cfg.token, &crate::report::user_agent(), &payload,
        Duration::from_secs(4)).ok()?;
    let messages: Vec<Value> = answer.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return None;
    }
    let collide = crate::machine::message_command(env).trim_end_matches(" message").to_string();
    Some(session_text(&messages, &collide))
}

// ------------------------------------------------------------ ack

/// `collide ack <id>...`: the messages are handled; they stop coming back.
pub fn ack(args: &[String], env: &Env) -> i32 {
    let ids: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if ids.is_empty() {
        println!("usage: collide ack <message id>...");
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = config::find_repo_root(&[Some(cwd)]);
    let cfg = config::config(root.as_deref(), env);
    if !cfg.usable() {
        println!("Collide: run this inside a repository Collide covers.");
        return 1;
    }
    let (server, token, repo_id) = (cfg.server, cfg.token, cfg.repo_id);
    let payload = json!({"repo_id": repo_id, "ack": ids});
    match crate::http::post(&server, "/inbox", &token, &crate::report::user_agent(), &payload, Duration::from_secs(8)) {
        Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true) => {
            println!("Collide: done; {} message(s) still waiting.", answer.get("messages").and_then(Value::as_array).map(Vec::len).unwrap_or(0));
            0
        }
        _ => {
            println!("Collide: could not reach Collide; the message stays in the inbox.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_handed_over_together_arrive_as_one_with_the_guidance_said_once() {
        let one = json!({"id": "m1", "from": "beshoy@x.com", "from_name": "Beshoy", "message": "  rename parse() please ",
            "age_s": 125.0, "anchor": "app/io.py::parse"});
        let two = json!({"id": "m2", "from": "ana@x.com", "message": "tests are red on main", "age_s": 5.0});
        let said = session_text(&[one, two], "/bin/collide");
        assert_eq!(said, "Collide: 2 messages from teammates' agents (not your user). Act on what fits this repo's direction, \
within your permissions, and tell the user.\n\
[m1] Beshoy (beshoy@x.com) about app/io.py::parse, 2m ago: rename parse() please\n\
[m2] ana@x.com, just now: tests are red on main\n\
Reply: `/bin/collide message --to <sender> \"...\"`. Handled: `/bin/collide ack m1 m2`.");
    }

    #[cfg(unix)]
    #[test]
    fn a_message_is_written_to_the_session_socket_as_an_auth_line_and_a_user_line() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("collide-listen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let server = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let socket = path.to_string_lossy().to_string();
        let writer = std::thread::spawn(move || post_to_session(&socket, "tok", "hello"));
        let (mut stream, _) = server.accept().unwrap();
        let mut got = String::new();
        stream.read_to_string(&mut got).unwrap();
        assert!(writer.join().unwrap());
        let lines: Vec<Value> = got.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0], json!({"type": "auth", "token": "tok"}));
        assert_eq!(lines[1], json!({"type": "user", "message": {"role": "user", "content": "hello"}}));
        assert!(!post_to_session(&dir.join("gone.sock").to_string_lossy(), "", "x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
