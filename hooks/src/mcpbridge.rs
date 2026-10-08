//! `collide mcp`: Collide's MCP tools for the agents on this machine, over
//! stdio. The local server already serves MCP at `/mcp`; agent tools start
//! MCP servers as programs, so this relays each line the agent writes to
//! that endpoint and writes back what it answers. The server is started when
//! it is not running, and a server that restarted (an update replaces it)
//! gets a fresh session, opened the way the agent opened the first.
//!
//! In a repo that works with a team the same relay talks to Collide's cloud
//! instead, with the credential the machine already holds: one sign-in
//! (`collide login`, or the code `setup` hands out) covers hooks and tools.
//!
//! A relay, not a request/response loop: every message the agent sends is
//! posted at once on its own thread, and every event the server streams
//! back is written the moment it arrives. A server may ask the client
//! something in the middle of a call and wait for the answer; holding its
//! stream until it closed left that question unread and the call waiting.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Env;
use crate::machine::{ensure_local, local_endpoint};

/// The longest one call may take before the agent is told it failed.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// The server closes an MCP session left idle for five minutes; a call sent
/// as it closes is never answered, so a session idle this long is reopened.
const REOPEN_AFTER: Duration = Duration::from_secs(240);

enum Sent {
    /// the server took it (and, for a request, answered it)
    Done,
    /// no server, or it no longer knows this session
    Gone,
    Failed(String),
}

struct Relay {
    env: Env,
    /// Collide's cloud, with this machine's own credential, when the repo
    /// (or the machine) works with a team; None: the local server
    cloud: Option<(String, String)>,
    out: Mutex<std::io::Stdout>,
    session: Mutex<String>,
    /// how the agent opened its session, replayed when it is reopened
    opening: Mutex<Vec<String>>,
    last_sent: Mutex<Instant>,
    /// one reopen at a time
    reopening: Mutex<()>,
}

/// Each server-sent event's `data`, handed to `emit` as the event ends;
/// stops early when `emit` says the answer is in (the stream may stay open).
fn read_events(reader: impl BufRead, mut emit: impl FnMut(&str) -> bool) {
    let mut data: Vec<String> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            let done = emit(&data.join("\n"));
            data.clear();
            if done {
                return;
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    emit(&data.join("\n"));
}

impl Relay {
    fn write(&self, line: &str) {
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{line}").and_then(|_| out.flush());
        }
    }

    fn session(&self) -> String {
        self.session.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Post one message. Each event the server streams back is written as
    /// it arrives (unless `quiet`); `id` is the answer to wait for.
    fn send(&self, body: &str, id: Option<&Value>, quiet: bool) -> Sent {
        let (base, token) = self.cloud.clone().unwrap_or_else(|| local_endpoint(&self.env));
        let session = self.session();
        let mut request = ureq::AgentBuilder::new()
            .timeout(ANSWER_WITHIN)
            .build()
            .post(&format!("{base}/mcp"))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Authorization", &format!("Bearer {token}"));
        if !session.is_empty() {
            request = request.set("Mcp-Session-Id", &session);
        }
        let response = match request.send_string(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) | Err(ureq::Error::Transport(_)) => return Sent::Gone,
            Err(ureq::Error::Status(code, response)) => {
                return Sent::Failed(format!("{code} {}", response.into_string().unwrap_or_default().trim()))
            }
        };
        if let Some(new) = response.header("mcp-session-id") {
            if let Ok(mut s) = self.session.lock() {
                *s = new.to_string();
            }
        }
        let streamed = response.content_type().contains("event-stream");
        let mut answered = false;
        let emit = |payload: &str, answered: &mut bool| {
            let payload = payload.trim();
            if payload.is_empty() {
                return;
            }
            if let (Some(id), Ok(message)) = (id, serde_json::from_str::<Value>(payload)) {
                if message.get("id") == Some(id) && (message.get("result").is_some() || message.get("error").is_some()) {
                    *answered = true;
                }
            }
            if !quiet {
                self.write(payload);
            }
        };
        let reader = BufReader::new(response.into_reader());
        if streamed {
            read_events(reader, |payload| {
                emit(payload, &mut answered);
                answered
            });
        } else {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                emit(&line, &mut answered);
            }
        }
        if id.is_some() && !answered {
            return Sent::Failed("no answer in time".into());
        }
        Sent::Done
    }

    /// Open a new session the way the agent opened its first.
    fn reopen(&self, stale: &str) {
        let Ok(_one) = self.reopening.lock() else { return };
        if self.session() != stale {
            return; // another thread already reopened it
        }
        if let Ok(mut s) = self.session.lock() {
            s.clear();
        }
        if self.cloud.is_none() {
            ensure_local(&self.env, Duration::from_secs(8));
        }
        let opening = self.opening.lock().map(|o| o.clone()).unwrap_or_default();
        for earlier in opening {
            let id = serde_json::from_str::<Value>(&earlier).ok().and_then(|m| m.get("id").cloned());
            let _ = self.send(&earlier, id.as_ref(), true);
        }
    }

    fn handle(&self, line: &str, message: &Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        // a reply from the agent to the server's own question has an id and no method
        let wanted = if method.is_empty() { None } else { message.get("id") };
        if !matches!(method, "initialize" | "notifications/initialized") {
            let idle = self.last_sent.lock().map(|t| t.elapsed() > REOPEN_AFTER).unwrap_or(false);
            let stale = self.session();
            if idle && !stale.is_empty() {
                self.reopen(&stale);
            }
        }
        if let Ok(mut t) = self.last_sent.lock() {
            *t = Instant::now();
        }
        let mut sent = self.send(line, wanted, false);
        if matches!(sent, Sent::Gone) && method != "notifications/initialized" {
            if method == "initialize" {
                if self.cloud.is_none() {
                    ensure_local(&self.env, Duration::from_secs(8));
                }
            } else {
                let stale = self.session();
                self.reopen(&stale);
            }
            sent = self.send(line, wanted, false);
        }
        let why = match sent {
            Sent::Done => return,
            Sent::Gone => "Collide's local server is not running".to_string(),
            Sent::Failed(why) => format!("Collide's local server: {why}"),
        };
        if wanted.is_some() {
            self.write(&failure(message, &why));
        }
    }
}

/// Where this session's MCP tools live: a repo (or a machine) that works
/// with a team talks to Collide's cloud with the credential the machine
/// already holds, so connecting the tools needs no sign-in of its own;
/// everything else talks to the local server.
fn cloud_target(env: &Env) -> Option<(String, String)> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let project = crate::config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project.is_empty()).then(|| std::path::PathBuf::from(project));
    if let Some(root) = crate::config::find_repo_root(&[Some(cwd), project]) {
        let cfg = crate::config::config(Some(&root), env);
        if cfg.usable() && !cfg.machine_local {
            return Some((cfg.server.trim_end_matches('/').to_string(), cfg.token));
        }
        return None;
    }
    // outside any repo: the machine's own mode
    crate::machine::cloud_account(env)
}

pub fn run(env: &Env) -> i32 {
    let cloud = cloud_target(env);
    if cloud.is_none() {
        ensure_local(env, Duration::from_secs(8));
    }
    let relay = Arc::new(Relay {
        env: env.clone(),
        cloud,
        out: Mutex::new(std::io::stdout()),
        session: Mutex::new(String::new()),
        opening: Mutex::new(Vec::new()),
        last_sent: Mutex::new(Instant::now()),
        reopening: Mutex::new(()),
    });
    let mut running: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut mapped = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim().to_string();
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if matches!(method.as_str(), "initialize" | "notifications/initialized") {
            if let Ok(mut opening) = relay.opening.lock() {
                if method == "initialize" {
                    opening.clear();
                }
                opening.push(line.clone());
            }
            if method == "initialize" {
                if let Ok(mut s) = relay.session.lock() {
                    s.clear();
                }
            }
            // the session must exist before anything else is sent on it
            relay.handle(&line, &message);
            continue;
        }
        // the first tool call maps the repo it is made from, as the first
        // hook call does: an agent that reaches Collide only through these
        // tools still gets a graph, once per repo and server on this machine
        if !mapped && method == "tools/call" {
            mapped = true;
            crate::machine::index_here(env);
        }
        running.retain(|t| !t.is_finished());
        let relay = Arc::clone(&relay);
        running.push(std::thread::spawn(move || relay.handle(&line, &message)));
    }
    for thread in running {
        let _ = thread.join();
    }
    0
}

fn failure(message: &Value, why: &str) -> String {
    json!({"jsonrpc": "2.0", "id": message.get("id").cloned().unwrap_or(Value::Null),
           "error": {"code": -32603, "message": why}})
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_handed_on_as_they_end_and_reading_stops_at_the_answer() {
        let stream = "id: 0/0\nretry: 3000\ndata:\n\ndata: {\"method\":\"elicitation/create\",\"id\":7}\n\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\ndata: {\"never\":\"read\"}\n\n";
        let mut seen: Vec<String> = Vec::new();
        read_events(stream.as_bytes(), |payload| {
            if !payload.trim().is_empty() {
                seen.push(payload.to_string());
            }
            payload.contains("\"result\"")
        });
        assert_eq!(seen, vec![r#"{"method":"elicitation/create","id":7}"#.to_string(), r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string()]);
    }
}
