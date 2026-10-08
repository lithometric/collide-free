//! Collide on one machine: the free version.
//!
//! `collide-server local` is this same server, run by the hooks on the
//! developer's own machine for every repo on it. Everything that happens
//! between the agents of one machine happens here, for free and with no
//! account: the briefings, the graph, the gate, the deltas, messages,
//! intents, recipes, the savings. What stays on Collide's servers is what
//! needs them: agents on other machines (teammates), and the embedding
//! model. So in local mode:
//!
//! - it listens on 127.0.0.1 only, and answers one credential: a key minted
//!   here at first start and kept in `~/.collide/local/token` (0600);
//! - the database is `~/.collide/local/collide.db`, one for every repo on
//!   the machine, each repo a scope of one local workspace;
//! - the cloud parts are off: the Postgres log, R2 snapshots, the embedding
//!   model, billing, analytics, Firebase, the hook release sync. Nothing
//!   here calls out.
//!
//! The hooks find it through `~/.collide/local/port` and start it when it
//! is not running (see collide-hooks `machine.rs`).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::store::{now, Store};

/// The first port tried; the next few are tried when it is taken.
pub const DEFAULT_PORT: u16 = 47_600;
const PORTS_TRIED: u16 = 20;
/// The local user's uid: one person, this machine's.
pub const LOCAL_UID: &str = "local";

/// Set by `main` before anything reads the environment.
pub fn active() -> bool {
    std::env::var("COLLIDE_LOCAL").map(|v| v == "1").unwrap_or(false)
}

fn home() -> PathBuf {
    for key in ["COLLIDE_HOME", "HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// `~/.collide/local`, or `COLLIDE_LOCAL_DIR`.
pub fn dir() -> PathBuf {
    match std::env::var("COLLIDE_LOCAL_DIR") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => home().join(".collide").join("local"),
    }
}

/// The environment of a local server, set before the runtime starts: the
/// data directory, and every cloud part switched off whatever the shell
/// happened to export.
pub fn prepare_env() {
    let dir = dir();
    let _ = std::fs::create_dir_all(&dir);
    let set = |key: &str, value: &str| std::env::set_var(key, value);
    set("COLLIDE_LOCAL", "1");
    set("DATA_DIR", &dir.to_string_lossy());
    for key in [
        "COLLIDE_PG_URL", "COLLIDE_DB_PATH", "COLLIDE_ENGINE_ID", "R2_ENDPOINT", "R2_BUCKET", "R2_ACCESS_KEY_ID",
        "R2_SECRET_ACCESS_KEY", "R2_PREFIX", "COLLIDE_EMBED_URL", "COLLIDE_EMBED_TOKEN", "COLLIDE_EMBEDDINGS",
        "POSTHOG_API_KEY", "POSTHOG_KEY", "STRIPE_SECRET_KEY", "STRIPE_WEBHOOK_SECRET", "COLLIDE_GITHUB_TOKEN",
        "COLLIDE_HOOK_SYNC", "COLLIDE_TEST_AUTH", "COLLIDE_DASHBOARD_URL", "RESEND_API_KEY",
    ] {
        std::env::remove_var(key);
    }
    if std::env::var("COLLIDE_WORKERS").is_err() {
        set("COLLIDE_WORKERS", "4");
    }
}

/// A free port on 127.0.0.1: the one asked for, else the default, else the
/// next free one after it. `None` when a local Collide already answers on
/// the first one taken (the hooks raced to start two).
pub fn pick_port(asked: Option<u16>) -> Result<u16, String> {
    let first = asked.unwrap_or(DEFAULT_PORT);
    for port in first..first.saturating_add(PORTS_TRIED) {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
        if already_serving(port) {
            return Err(format!("a local Collide already answers on port {port}"));
        }
    }
    Err(format!("no free port from {first} to {}", first + PORTS_TRIED - 1))
}

fn already_serving(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(600)));
    let _ = stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer.contains(&format!("\"local_id\":\"{}\"", instance_id()))
}

/// The identity one agent session reports as: `agent-3f2a91c0`, from
/// its session id. Without a session (the person themselves, a command run
/// by hand) it is the person.
pub fn agent_identity(person: &str, session: &str) -> String {
    let short: String = session.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    if short.is_empty() { person.to_string() } else { format!("agent-{}", short.to_lowercase()) }
}

/// Which machine account's data this server holds: a hash of its data
/// directory. `/health` carries it and the hooks compare it with their own,
/// so on a computer shared by two people neither uses the other's server.
pub fn instance_id() -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(dir().to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Written once the port is bound: how the hooks find this server.
pub fn write_port(port: u16) {
    let _ = std::fs::write(dir().join("port"), format!("{port}\n"));
}

fn git_email() -> String {
    std::process::Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|email| email.contains('@'))
        .unwrap_or_default()
}

fn write_private(path: &std::path::Path, body: &str) {
    let _ = std::fs::write(path, body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// The records a local server needs, made on first start and kept: the
/// local person, their one workspace (every repo on the machine is a repo
/// of it, added on first sight), the credential the hooks present, and a
/// plan record with no limits. Returns the workspace id.
pub fn bootstrap(store: &Store) -> String {
    let dir = dir();
    let meta_path = dir.join("local.json");
    let mut meta = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut workspace = meta.get("workspace").and_then(Value::as_str).unwrap_or("").to_string();
    let email = {
        let known = meta.get("email").and_then(Value::as_str).unwrap_or("").to_string();
        if !known.is_empty() { known } else { git_email() }
    };
    let label = if email.is_empty() { "you".to_string() } else { email.clone() };
    let principal = crate::workspaces::Principal {
        uid: LOCAL_UID.to_string(),
        email: email.clone(),
        name: label.split('@').next().unwrap_or("you").to_string(),
    };
    if workspace.is_empty() || crate::workspaces::get(store, &workspace).is_none() {
        match crate::workspaces::create(store, &principal, "This machine", "custom", "") {
            Ok(ws) => workspace = ws.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            Err(error) => {
                tracing::warn!("local: could not create the workspace: {error}");
                return workspace;
            }
        }
    }
    let stamp = now();
    let _ = store.kv_put("profile", LOCAL_UID, &json!({"username": principal.name, "email": email}), stamp);
    // no limits: everything that happens on one machine is free
    let _ = store.kv_put(
        crate::billing::BILLING_BUCKET, &workspace,
        &json!({"plan": "business", "status": "active", "seats": null, "interval": null, "local": true}), stamp,
    );
    let _ = store.kv_put(crate::watch::NEW_REPOS_BUCKET, &workspace, &json!({"mode": "add"}), stamp);

    // the credential: kept while its file exists, minted again when it does not
    let token_path = dir.join("token");
    let mut token = std::fs::read_to_string(&token_path).unwrap_or_default().trim().to_string();
    if token.is_empty() {
        token = crate::blocks::random_bytes(32).iter().map(|b| format!("{b:02x}")).collect();
        write_private(&token_path, &format!("{token}\n"));
    }
    let _ = store.kv_put(
        crate::auth::TOKEN_BUCKET,
        &crate::auth::token_key(&token),
        &json!({
            "kind": "access", "uid": LOCAL_UID, "email": email, "name": principal.name,
            "workspace": workspace, "workspaces": [workspace], "client_id": "collide-local",
            "created": stamp, "expires": stamp + 3650.0 * 86_400.0,
        }),
        stamp,
    );
    if let Some(map) = meta.as_object_mut() {
        map.insert("workspace".into(), json!(workspace));
        map.insert("email".into(), json!(email));
        map.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    }
    write_private(&meta_path, &format!("{}\n", serde_json::to_string_pretty(&meta).unwrap_or_default()));
    workspace
}

// ------------------------------------------------------------ agent messages

/// Messages waiting for this agent, as one note for the hook to print, and
/// taken off its queue: an agent on this machine is told once. `None` when
/// nothing is waiting.
pub fn inbox_note(store: &Store, scope: &str, user_id: &str) -> Option<String> {
    inbox_note_as(store, scope, user_id, "Messages for you from the other agents on this machine. Act on them, and say so to the user; to answer one, use \
the collide message command from your session's start, with --to <sender>.")
}

/// The inbox key for one session of a person. On a team a message goes to a
/// person, whichever session reads first; a person's own sessions have no
/// other way to tell each other apart.
pub fn session_inbox(user: &str, session: &str) -> String {
    format!("{user}#{session}")
}

fn inbox_note_as(store: &Store, scope: &str, user_id: &str, heading: &str) -> Option<String> {
    let none = std::collections::BTreeSet::new();
    let inbox = crate::agenttools::inbox_ack(store, scope, user_id, &none, &[]);
    let messages: Vec<Value> = inbox.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return None;
    }
    let ids: Vec<String> = messages.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string)).collect();
    let lines: Vec<String> = messages.iter().map(crate::agenttools::message_line).collect();
    let _ = crate::agenttools::inbox_ack(store, scope, user_id, &none, &ids);
    Some(format!("{heading}\n{}", lines.join("\n")))
}

/// Put this agent's waiting messages on a hook response, so an agent deep
/// in a long task hears them on its next edit or read, not only its next
/// prompt.
pub fn attach_inbox(store: &Store, scope: &str, user_id: &str, session: &str, response: &mut Value) {
    let note = if active() {
        inbox_note(store, scope, user_id)
    } else if !session.is_empty() {
        // on a team: what this person's other sessions sent this one
        inbox_note_as(store, scope, &session_inbox(user_id, session), "Messages for you from your other sessions in this repo. \
Act on them, and say so to the user; to answer one, run the collide message command with --to <sender>.")
    } else {
        None
    };
    if let (Some(note), Some(map)) = (note, response.as_object_mut()) {
        map.insert("inbox_note".into(), json!(note));
    }
}

/// `POST /message {repo_id, session, text, to?, about?}`: an agent tells
/// the others something. `to` names one agent (as its messages and deltas
/// name it); `about` (`path.py::symbol`, a path or `dir/`) goes to whoever
/// is working on that code; neither goes to every agent live in the repo.
pub fn send(store: &Store, scope: &str, repo_id: &str, workspace: &str, from: &str, body: &Value, idle_after_s: f64) -> Value {
    let text = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let message = text("text");
    if message.is_empty() {
        return json!({"ok": false, "error": "nothing to say: pass the message text"});
    }
    let (to, about) = (text("to"), text("about"));
    if !about.is_empty() {
        return crate::agenttools::send_anchored_message(store, scope, workspace, from, &to, &message, &about, "collide-hook", "");
    }
    if !to.is_empty() {
        return crate::agenttools::send_agent_message(store, scope, from, &to, &message, "collide-hook");
    }
    let visible = std::collections::BTreeSet::new();
    let view = crate::activity::list_activity(store, scope, repo_id, idle_after_s, &visible);
    let mut sent_to: Vec<String> = Vec::new();
    // on a team the sender's own other sessions count too, each addressed on
    // its own (here every session already is its own agent)
    let own_session = text("session");
    let signed = if !active() && !own_session.is_empty() { session_inbox(from, &own_session) } else { from.to_string() };
    for row in view.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        let user = row.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        let session = row.get("session").and_then(Value::as_str).unwrap_or("").to_string();
        let online = row.get("online").and_then(Value::as_bool).unwrap_or(false);
        let to = if user == from {
            if active() || session.is_empty() || session == own_session {
                continue;
            }
            session_inbox(&user, &session)
        } else {
            user
        };
        if online && !to.is_empty() && !sent_to.contains(&to) {
            crate::agenttools::send_agent_message_as(store, scope, &signed, &to, &message, "collide-hook", true);
            sent_to.push(to);
        }
    }
    if sent_to.is_empty() {
        return json!({"ok": true, "sent_to": [], "note": "no other agent is at work in this repo right now; nothing was sent"});
    }
    json!({"ok": true, "sent_to": sent_to})
}

// ------------------------------------------------------------ the summary

/// What Collide did on this machine since `since`, across every repo: the
/// savings and the spend, how many agents worked and in how many repos, the
/// changes they told each other about and the messages they sent. The
/// hooks print it once a day; `collide-hook status` prints it on demand.
pub fn summary(store: &Store, workspace: &str, since: f64) -> Value {
    let scopes: Vec<String> = store.list_scopes(&format!("{workspace}:"));
    let days = ((now() - since) / 86_400.0).max(0.02);
    let saved = crate::briefstat::savings_across(store, &scopes, days);
    let spent = crate::insights::costs_across(store, &scopes, days);
    let (mut changes, mut messages) = (0u64, 0u64);
    let mut agents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut repos: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for scope in &scopes {
        for row in store.ledger_since(scope, since) {
            let who = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
            match row.kind.as_str() {
                "edit_reported" => {
                    changes += 1;
                    if who.starts_with("agent-") {
                        agents.insert(who.to_string());
                    }
                    repos.insert(scope.split_once(':').map(|(_, r)| r).unwrap_or(scope).to_string());
                }
                "agent_message" => messages += 1,
                _ => {}
            }
        }
    }
    let totals = &saved["totals"];
    json!({
        "since": since,
        "tokens_saved": totals["tokens_saved"], "est_usd_saved": totals["est_usd"], "messages_not_sent": totals["messages"],
        "tokens_spent": spent["total_tokens"], "est_usd_spent": spent["est_usd"],
        "agents": agents.len(), "repos": repos.len(), "changes": changes, "agent_messages": messages,
        "by_repo": saved["by_repo"],
    })
}

// ------------------------------------------------------------ the sync

/// One batch of this machine's record for the cloud: per repo, the ledger
/// rows after the cursor (`{scope: seq}`) of the kinds a sync carries, and
/// every note on the first batch. `more` says another batch is waiting.
pub fn export(store: &Store, workspace: &str, cursor: &Value, limit: usize) -> Value {
    let prefix = format!("{workspace}:");
    let mut repos: Vec<Value> = Vec::new();
    let mut next = cursor.as_object().cloned().unwrap_or_default();
    let mut budget = limit;
    let mut more = false;
    for scope in store.list_scopes(&prefix) {
        let repo_id = scope[prefix.len()..].to_string();
        let after = cursor.get(&scope).and_then(Value::as_i64).unwrap_or(0);
        let first = after == 0;
        let mut rows: Vec<Value> = Vec::new();
        let mut last = after;
        for row in store.ledger_since(&scope, 0.0) {
            if row.seq <= after || !crate::machines::SYNCED_KINDS.contains(&row.kind.as_str()) {
                continue;
            }
            if budget == 0 {
                more = true;
                break;
            }
            rows.push(json!({"seq": row.seq, "kind": row.kind, "ts": row.ts, "payload": row.payload}));
            last = last.max(row.seq);
            budget -= 1;
        }
        let notes: Vec<Value> = if first {
            store.kv_list("memory", &format!("{scope}:")).into_iter().map(|(_, v)| v).collect()
        } else {
            Vec::new()
        };
        next.insert(scope.clone(), json!(last));
        if !rows.is_empty() || !notes.is_empty() {
            repos.push(json!({"repo_id": repo_id, "rows": rows, "notes": notes}));
        }
        if more {
            break;
        }
    }
    let all: Vec<String> = store.list_scopes(&prefix).into_iter().map(|s| s[prefix.len()..].to_string()).collect();
    json!({"ok": true, "repos": repos, "cursor": next, "more": more, "all_repos": all})
}

/// A local server whose binary was replaced (an update) writes what it
/// holds and exits; the hooks start the new one on the next agent event.
pub fn watch_for_update(store: std::sync::Arc<Store>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let stamp = |path: &std::path::Path| std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()));
    let first = stamp(&exe);
    let _ = std::thread::Builder::new().name("collide-local-update".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let now = stamp(&exe);
        if now.is_some() && now != first {
            tracing::info!("collide local: the binary was updated; writing state and exiting for the new one");
            store.flush();
            store.flush_trees(true);
            std::process::exit(0);
        }
    });
}
