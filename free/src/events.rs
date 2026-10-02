//! The near-realtime channel, and the awareness it carries.
//!
//! MCP cannot push mid-turn, so the calls the protocol already mandates —
//! report_edit, check_collisions, heartbeat — double as the delivery
//! mechanism. Every response carries what teammates did since this agent's
//! previous delta-carrying call, which is how an agent adapts to a rename
//! within one tool call of it happening rather than at its next voluntary
//! check.
//!
//! One caveat worth stating plainly: this writes the durable ring, which is
//! what deltas and the dashboard's polling read. It does NOT reach the Python
//! half's in-process websocket subscribers. While both servers run, an
//! endpoint must be served by exactly one of them.

use serde_json::{json, Map, Value};

use crate::store::{now, Store};

const RING_LIMIT: usize = 100;
const RING_TTL_S: f64 = 86400.0;
const CURSOR_TTL_S: f64 = 7.0 * 86400.0;
const MAX_DELTAS: usize = 15;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Append to the scope's recent-events ring.
///
/// Reads are chatty, so a run of focus events from one user coalesces into a
/// single slot — browsing a dozen files must not flush the edits out of the
/// Activity feed.
pub fn publish(store: &Store, scope: &str, event: Value, via: &str) {
    store.broadcast(scope, &event);
    let (workspace, repo_id) = scope.split_once(':').unwrap_or((scope, ""));
    let mut event = event;
    if let Some(map) = event.as_object_mut() {
        map.insert("ts".into(), json!(now()));
        map.insert("repo_id".into(), json!(repo_id));
        map.insert("workspace".into(), json!(workspace));
        if !via.is_empty() && !map.contains_key("via") {
            map.insert("via".into(), crate::presence::via_view(via));
        }
    }

    let key = format!("recent:{scope}");
    let mut ring = store.eph_get(&key).unwrap_or_else(|| json!({"events": []}));
    let Some(events) = ring.get_mut("events").and_then(Value::as_array_mut) else { return };
    let coalesce = event.get("kind").and_then(Value::as_str) == Some("focus")
        && events
            .last()
            .map(|last| {
                last.get("kind").and_then(Value::as_str) == Some("focus")
                    && last.get("user") == event.get("user")
            })
            .unwrap_or(false);
    if coalesce {
        let last = events.len() - 1;
        events[last] = event;
    } else {
        events.push(event);
        if events.len() > RING_LIMIT {
            let excess = events.len() - RING_LIMIT;
            events.drain(0..excess);
        }
    }
    let _ = store.eph_set(&key, &ring, Some(RING_TTL_S));
}

fn event_actor(event: &Value) -> String {
    if event.get("kind").and_then(Value::as_str) == Some("intent") {
        return event
            .get("intent")
            .map(|intent| text(intent, "owner"))
            .unwrap_or_default();
    }
    let user = text(event, "user");
    if user.is_empty() { text(event, "checker") } else { user }
}

/// Compact one-line rendering of a bus event. `None` drops it — `root_changed`
/// is redundant with the edit that caused it.
fn delta_view(event: &Value) -> Option<Value> {
    let kind = event.get("kind").and_then(Value::as_str)?;
    let copy = |keys: &[&str]| -> Map<String, Value> {
        keys.iter()
            .map(|key| (key.to_string(), event.get(*key).cloned().unwrap_or(Value::Null)))
            .collect()
    };
    match kind {
        "edit" => {
            let mut view = Map::new();
            view.insert("kind".into(), json!("edit"));
            view.insert("user".into(), event.get("user").cloned().unwrap_or(Value::Null));
            view.insert("path".into(), event.get("path").cloned().unwrap_or(Value::Null));
            if event.get("stage").and_then(Value::as_str) == Some("draft") {
                view.insert("stage".into(), json!("draft"));
                if let Some(changed) = event.get("symbols_changed").and_then(Value::as_object) {
                    let names: Vec<&String> = changed.keys().take(10).collect();
                    view.insert("symbols".into(), json!(names));
                }
            }
            if let Some(hunks) = event.get("hunks").and_then(Value::as_array) {
                let rendered: Vec<Value> = hunks
                    .iter()
                    .filter(|hunk| hunk.get("op").and_then(Value::as_str) != Some("truncated"))
                    .take(3)
                    .map(|hunk| {
                        let start = hunk.get("new_start").cloned().unwrap_or(Value::Null);
                        let end = hunk.get("new_end").cloned().unwrap_or(Value::Null);
                        let code: Vec<Value> = hunk
                            .get("lines")
                            .and_then(Value::as_array)
                            .map(|lines| lines.iter().take(1).cloned().collect())
                            .unwrap_or_else(|| vec![json!("")]);
                        json!({"at": format!("{start}-{end}"), "code": code})
                    })
                    .collect();
                if !rendered.is_empty() {
                    view.insert("hunks".into(), json!(rendered));
                }
            }
            Some(Value::Object(view))
        }
        "symbol_event" => {
            let change = event.get("event").cloned().unwrap_or(Value::Null);
            Some(json!({
                "kind": "symbol",
                "user": event.get("user").cloned().unwrap_or(Value::Null),
                "path": event.get("path").cloned().unwrap_or(Value::Null),
                "symbol": change.get("symbol").cloned().unwrap_or(Value::Null),
                "change": change.get("kind").cloned().unwrap_or(Value::Null),
                "confidence": change.get("confidence").cloned().unwrap_or(Value::Null),
                // the new name and signature: what an adapting agent needs
                "detail": change.get("detail").cloned().unwrap_or(Value::Null),
            }))
        }
        "intent" => {
            let intent = event.get("intent").cloned().unwrap_or(Value::Null);
            let paths: Vec<Value> = intent
                .get("paths")
                .and_then(Value::as_array)
                .map(|items| items.iter().take(10).cloned().collect())
                .unwrap_or_default();
            Some(json!({
                "kind": "intent",
                "action": event.get("action").cloned().unwrap_or(Value::Null),
                "owner": intent.get("owner").cloned().unwrap_or(Value::Null),
                "change_type": intent.get("change_type").cloned().unwrap_or(json!("")),
                "summary": intent.get("summary").cloned().unwrap_or(json!("")),
                "paths": paths,
            }))
        }
        "collision" => Some(json!({
            "kind": "collision",
            "checker": event.get("checker").cloned().unwrap_or(Value::Null),
            "count": event.get("items").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        })),
        "tripwire" => Some(Value::Object(copy(&["note", "by", "path"]).into_iter().chain(
            [("kind".to_string(), json!("tripwire"))]).collect())),
        "gate" | "override" => Some(json!({
            "kind": kind,
            "user": event.get("user").cloned().unwrap_or(Value::Null),
            "path": event.get("path").cloned().unwrap_or(Value::Null),
            "action": event.get("action").cloned().unwrap_or(json!("")),
        })),
        "restart_required" => Some(json!({
            "kind": "restart_required",
            "user": event.get("user").cloned().unwrap_or(Value::Null),
            "note": "hooks installed but not running in this session — restart Claude Code",
        })),
        "agent_limit" => {
            let user = text(event, "user");
            let note = text(event, "note");
            Some(json!({
                "kind": "agent_limit", "user": user,
                "agent": event.get("agent").cloned().unwrap_or(json!("")),
                "note": format!(
                    "{user}'s agent hit its usage limit ({note}) — it is paused, not ignoring \
you; don't wait on its in-flight work"),
            }))
        }
        _ => None,
    }
}

/// Full display identity for agent-facing output: the caller's own nickname
/// for a person wins, then that person's claimed username, then the email's
/// local part. Nicknames are private to the caller.
fn identity_labels(store: &Store, caller: &str, users: &[String]) -> Map<String, Value> {
    let nicks = store.kv_get("nicknames", caller).unwrap_or(Value::Null);
    let mut out = Map::new();
    for user in users {
        let username = store
            .kv_get("username_by_email", user)
            .map(|claim| text(&claim, "username").trim().to_string())
            .unwrap_or_default();
        let nick = nicks
            .get(user)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let fallback =
            if user.contains('@') { user.split('@').next().unwrap_or(user) } else { user };
        let who = if !nick.is_empty() {
            nick.clone()
        } else if !username.is_empty() {
            username.clone()
        } else {
            fallback.to_string()
        };
        let handle = if username.is_empty() { String::new() } else { format!("@{username}") };
        let label = if !nick.is_empty() {
            let inside: Vec<&str> =
                [handle.as_str(), user.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
            if inside.is_empty() { nick.clone() } else { format!("{nick} ({})", inside.join(", ")) }
        } else if !handle.is_empty() {
            format!("{handle} ({user})")
        } else {
            user.clone()
        };
        out.insert(user.clone(), json!({"who": who, "username": username, "label": label}));
    }
    out
}

/// Cross-agent events since this user's previous delta-carrying call. The
/// first call initialises the cursor and returns nothing; failures return
/// nothing rather than disturbing the call they ride on.
pub fn deltas_for(store: &Store, scope: &str, user_id: &str) -> Option<Vec<Value>> {
    let key = format!("deltacursor:{scope}:{user_id}");
    let stamp = now();
    let cursor = store.eph_get(&key);
    let _ = store.eph_set(&key, &json!({"ts": stamp}), Some(CURSOR_TTL_S));
    let since = cursor?.get("ts").and_then(Value::as_f64).unwrap_or(stamp);

    let ring = store.eph_get(&format!("recent:{scope}"))?;
    let events = ring.get("events").and_then(Value::as_array)?;
    let mut out: Vec<Value> = Vec::new();
    for event in events {
        if event.get("ts").and_then(Value::as_f64).unwrap_or(0.0) <= since {
            continue;
        }
        if event_actor(event) == user_id {
            continue; // your own work is not news to you
        }
        if let Some(mut view) = delta_view(event) {
            if let Some(map) = view.as_object_mut() {
                map.insert("ts".into(), event.get("ts").cloned().unwrap_or(Value::Null));
            }
            out.push(view);
        }
    }
    if out.len() > MAX_DELTAS {
        out.drain(0..out.len() - MAX_DELTAS);
    }
    if out.is_empty() {
        return None;
    }

    // decorate with who did it, so an agent reads a name rather than an email
    let users: Vec<String> = {
        let mut seen: Vec<String> = out
            .iter()
            .filter_map(|delta| delta.get("user").and_then(Value::as_str).map(str::to_string))
            .collect();
        seen.sort();
        seen.dedup();
        seen
    };
    let labels = identity_labels(store, user_id, &users);
    for delta in out.iter_mut() {
        let Some(user) = delta.get("user").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let Some(info) = labels.get(&user) else { continue };
        if let Some(map) = delta.as_object_mut() {
            map.insert("who".into(), info.get("who").cloned().unwrap_or(Value::Null));
            map.insert("by".into(), info.get("label").cloned().unwrap_or(Value::Null));
        }
    }
    Some(out)
}

/// Attach `since_your_last_call` to a response, exactly as the Python half does.
pub fn with_deltas(store: &Store, scope: &str, user_id: &str, mut response: Value) -> Value {
    if let Some(deltas) = deltas_for(store, scope, user_id) {
        if let Some(map) = response.as_object_mut() {
            map.insert("since_your_last_call".into(), json!(deltas));
        }
    }
    response
}
