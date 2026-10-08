//! Presence: where an AGENT is looking.
//!
//! The read-side hook fires on every Read, Grep and Bash, so this has to be
//! featherweight — ephemeral markers only, no ledger row, no tree change,
//! gone in minutes. What it buys is that teammates can see where someone is
//! working before they write, which is the whole point of reporting reads at
//! all.
//!
//! Two markers, with deliberately different lifetimes. `focus:` is short (3
//! minutes) because it drives the live "reading now" line and the online dot,
//! and a stale one would lie. `lastact:` lasts a week, so someone who only
//! read files an hour ago is still listed as idle with what they last
//! touched, instead of vanishing when the focus marker expires.
//!
//! The unit is the agent, not the person: `{user}#{session}`, the identity
//! the hot markers already chose (see `report::report_edit`) and the gate
//! already honours. Four agents on one credential were four agents whose
//! markers overwrote each other; now each has its own. And the unit of the
//! ROOM is the workspace, not the repo — presence keys are
//! `focus:{workspace}:{agent_id}` with the repo carried in the record — so
//! the roster of any repo can show everyone in the workspace, filtered
//! through the member's per-repo access at read time.

use serde_json::{json, Map, Value};

use crate::store::{now, Store};

const FOCUS_TTL_S: f64 = 180.0;
const LAST_ACTION_TTL_S: f64 = 7.0 * 86400.0;
const AGENT_TOKENS_TTL_S: f64 = 7.0 * 86400.0;
/// A paused agent stays paused on the Now panel for up to six hours, or
/// until it acts again — the harness's reset times are hours away, and a
/// 3-minute marker would flip a limited agent back to "idle" while its
/// quota is still gone.
const LIMIT_TTL_S: f64 = 6.0 * 3600.0;
const MAX_PATH: usize = 300;
const MAX_NOTE: usize = 200;

pub(crate) fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `{user}#{session}`, or the bare user when no session was reported —
/// exactly the identity the hot markers use, so an MCP-only agent that never
/// names a session is one agent under the person's own name rather than a
/// row that can never be matched.
pub fn agent_id(user: &str, session: &str) -> String {
    if session.is_empty() { user.to_string() } else { format!("{user}#{session}") }
}

/// `("<workspace>", "<repo>")`. The repo half may itself carry colons, so the
/// split is on the FIRST one only — the same `partition(":")` the Python half
/// uses everywhere a scope is taken apart.
pub fn split_scope(scope: &str) -> (&str, &str) {
    scope.split_once(':').unwrap_or((scope, ""))
}

pub fn focus_key(scope: &str, user: &str, session: &str) -> String {
    format!("focus:{}:{}", split_scope(scope).0, agent_id(user, session))
}

pub fn last_action_key(scope: &str, user: &str, session: &str) -> String {
    format!("lastact:{}:{}", split_scope(scope).0, agent_id(user, session))
}

/// A subagent working inside a session: `worker:{ws}:{agent_id}:{worker}`,
/// alive for as long as a focus marker. Counted per session by the roster.
pub const WORKER_TTL_S: f64 = 180.0;

pub fn worker_key(scope: &str, user: &str, session: &str, worker: &str) -> String {
    format!("worker:{}:{}:{}", split_scope(scope).0, agent_id(user, session), worker)
}

/// A hook post named the subagent it came from: refresh that worker's marker.
/// Byte-for-byte what Python's `_note_worker` stores.
pub fn note_worker(store: &Store, scope: &str, user: &str, session: &str, worker: &str, worker_type: &str) {
    if worker.is_empty() || user.is_empty() {
        return;
    }
    let mut record = identity(scope, user, session);
    record.insert("worker".into(), json!(worker));
    record.insert("type".into(), json!(worker_type));
    record.insert("ts".into(), json!(now()));
    let _ = store.eph_set(&worker_key(scope, user, session, worker), &Value::Object(record), Some(WORKER_TTL_S));
}

pub fn tokens_key(scope: &str, user: &str, session: &str) -> String {
    format!("agenttokens:{}:{}", split_scope(scope).0, agent_id(user, session))
}

/// The identity every presence record carries. `repo_id` is what tells a
/// per-agent record from one written before this change — never the key's
/// arity, because a scope can hold more than one colon.
fn identity(scope: &str, user: &str, session: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("user".into(), json!(user));
    map.insert("session".into(), json!(session));
    map.insert("agent_id".into(), json!(agent_id(user, session)));
    map.insert("repo_id".into(), json!(split_scope(scope).1));
    map
}

/// Cumulative per-agent token counter, deduped by turn: many tool calls share
/// one assistant turn, so only the FIRST report carrying a new turn id adds
/// that turn's tokens.
///
/// The record also keeps the context the agent was last carrying (`context`,
/// with `model`): the size a message it did not send would have replayed,
/// which is what a saving reported without its own usage is priced against.
pub fn bump_tokens(
    store: &Store, scope: &str, user: &str, session: &str, tokens: i64, turn_id: &str, context: i64, model: &str,
) -> i64 {
    let key = tokens_key(scope, user, session);
    let mut record = store
        .eph_get(&key)
        .unwrap_or_else(|| json!({"total": 0, "last_turn": ""}));
    let last_turn = record.get("last_turn").and_then(Value::as_str).unwrap_or("").to_string();
    let total = record.get("total").and_then(Value::as_i64).unwrap_or(0);
    let mut result = total;
    let mut changed = false;
    if let Some(map) = record.as_object_mut() {
        if tokens > 0 && !turn_id.is_empty() && turn_id != last_turn {
            result = total + tokens;
            map.insert("total".into(), json!(result));
            map.insert("last_turn".into(), json!(turn_id));
            changed = true;
        }
        if context > 0 {
            map.insert("context".into(), json!(context));
            if !model.is_empty() {
                map.insert("model".into(), json!(model));
            }
            changed = true;
        }
        if changed {
            map.extend(identity(scope, user, session));
        }
    }
    if changed {
        let _ = store.eph_set(&key, &record, Some(AGENT_TOKENS_TTL_S));
    }
    result
}

/// The `via` tag as Python stores it — `{host, path, mcp}` — from the
/// "host path" string the routes build (`via_tag`). Python's `via_tag`, byte
/// for byte: the first host of a comma list, lowercased; the path
/// normalised to one leading slash ("/" when empty); `mcp` when the path is
/// the root or ends in "/mcp". A bare word with no host ("mcp", the MCP
/// server's tag) is a path. The dashboard's `viaLabel` reads the object and
/// shows nothing for a string, so this is what makes the chip render.
pub fn via_view(via: &str) -> Value {
    let via = via.trim();
    if via.is_empty() {
        return Value::Null;
    }
    let (host, path) = match via.split_once(' ') {
        Some((host, path)) => (host, path),
        None if via.starts_with('/') => ("", via),
        None if via.contains('.') || via.contains(':') => (via, ""),
        None => ("", via),
    };
    let host = host.split(',').next().unwrap_or("").trim().to_lowercase();
    let trimmed = path.trim().trim_matches('/');
    let path = if trimmed.is_empty() { "/".to_string() } else { format!("/{trimmed}") };
    let mcp = path == "/" || path.trim_end_matches('/').ends_with("/mcp");
    json!({"host": host, "path": path, "mcp": mcp})
}

/// The one line the Now panel shows under an agent: the last thing it did.
pub fn note_last_action(
    store: &Store, scope: &str, user: &str, session: &str, kind: &str, path: &str,
    agent: &str, model: &str, branch: &str, note: &str, via: &str,
) {
    let mut record = identity(scope, user, session);
    record.insert("kind".into(), json!(kind));
    record.insert("path".into(), json!(clip(path, MAX_PATH)));
    record.insert("ts".into(), json!(now()));
    record.insert("agent".into(), json!(agent));
    record.insert("model".into(), json!(model));
    record.insert("branch".into(), json!(branch));
    record.insert("note".into(), json!(clip(note, MAX_NOTE)));
    record.insert("via".into(), via_view(via));
    // the line it replaces joins the agent's recent history (the dashboard
    // shows it above the current bubble), unless it said the same thing
    let key = last_action_key(scope, user, session);
    if let Some(previous) = store.eph_get(&key) {
        let same = previous.get("kind") == record.get("kind") && previous.get("path") == record.get("path");
        if !same {
            push_history(store, scope, user, session, &previous);
        }
    }
    let _ = store.eph_set(&key, &Value::Object(record), Some(LAST_ACTION_TTL_S));
}

/// How long a "running" that has started may stand with nothing after it:
/// long enough for a slow test suite or build, not forever for a killed one.
const STARTED_TTL_S: f64 = 1800.0;

/// A shell command has STARTED (PreToolUse; the hook hands it off so the
/// command is not held): the agent is running it now. Only the focus marker
/// moves, flagged `started`; the command's end (PostToolUse) overwrites it
/// and writes the last action, which is when the dashboard says "ran".
pub fn note_started(store: &Store, scope: &str, user: &str, session: &str, command: &str, branch: &str, model: &str) {
    let mut marker = identity(scope, user, session);
    if !model.is_empty() {
        marker.insert("model".into(), json!(model));
    }
    marker.insert("path".into(), json!(clip(command, MAX_PATH)));
    marker.insert("action".into(), json!("running"));
    marker.insert("started".into(), json!(true));
    marker.insert("branch".into(), json!(branch));
    marker.insert("ts".into(), json!(now()));
    let _ = store.eph_set(&focus_key(scope, user, session), &Value::Object(marker), Some(STARTED_TTL_S));
}

/// How many earlier lines an agent keeps.
const HISTORY_LEN: usize = 30;

pub fn history_key(scope: &str, user: &str, session: &str) -> String {
    format!("lasthist:{}:{}", split_scope(scope).0, agent_id(user, session))
}

fn push_history(store: &Store, scope: &str, user: &str, session: &str, previous: &Value) {
    let key = history_key(scope, user, session);
    let mut items: Vec<Value> = store.eph_get(&key).and_then(|v| v.get("items").and_then(Value::as_array).cloned()).unwrap_or_default();
    let pick = |k: &str| previous.get(k).cloned().unwrap_or(Value::Null);
    items.insert(0, json!({"kind": pick("kind"), "path": pick("path"), "note": pick("note"), "ts": pick("ts")}));
    items.truncate(HISTORY_LEN);
    let _ = store.eph_set(&key, &json!({"items": items}), Some(LAST_ACTION_TTL_S));
}

/// The agent's earlier lines, newest first (the current one not among them).
pub fn history(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    store.eph_get(&history_key(scope, user, session))
        .and_then(|v| v.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Writing supersedes reading: `report_edit` calls this so the Now line stops
/// saying "reading X" once THIS agent has moved on — another agent of the
/// same person keeps its own line.
pub fn clear_focus(store: &Store, scope: &str, user: &str, session: &str) {
    store.eph_delete(&focus_key(scope, user, session));
}

/// A roster is workspace-wide but a live feed is per repo, so a presence
/// change in one repo has to reach the dashboards watching every other repo
/// of the workspace. What crosses is a content-free nudge — no user, no path,
/// not even which repo — because the subscriber's per-repo access is unknown
/// here; the dashboard answers it by re-reading the activity endpoint, which
/// filters. Live subscribers only: the nudge never lands in a repo's event
/// ring, where it would be mistaken for history.
pub fn nudge_workspace(store: &Store, scope: &str) {
    nudge(store, scope, false);
}

/// This repo's own feed normally hears the event itself; `include_self` is
/// for a change that publishes none.
fn nudge(store: &Store, scope: &str, include_self: bool) {
    let (workspace, _repo) = split_scope(scope);
    let nudge = json!({"kind": "presence_nudge", "workspace": workspace, "ts": now()});
    for other in store.list_scopes(&format!("{workspace}:")) {
        if include_self || other != scope {
            store.broadcast(&other, &nudge);
        }
    }
}

/// Removing a repo from a workspace takes its agents' presence with it. The
/// keys no longer carry the repo, so this reads the record's `repo_id` —
/// the same field the roster reads — rather than a prefix.
pub fn forget_scope(store: &Store, scope: &str) {
    let (workspace, repo) = split_scope(scope);
    for bucket in ["focus", "lastact", "agenttokens"] {
        for (key, record) in store.eph_scan(&format!("{bucket}:{workspace}:")) {
            if record.get("repo_id").and_then(Value::as_str) == Some(repo) {
                store.eph_delete(&key);
            }
        }
    }
}

/// Presence from any protocol call — a briefing, an intent, a check — so an
/// agent appears on the dashboard the moment it does anything. Never clobbers
/// a richer, fresher read or edit marker from the hooks.
pub fn touch_presence(store: &Store, scope: &str, user: &str, session: &str, agent: &str, via: &str) {
    let key = focus_key(scope, user, session);
    let stamp = now();
    if let Some(existing) = store.eph_get(&key) {
        if stamp - existing.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < 60.0 {
            return;
        }
    }
    let mut marker = identity(scope, user, session);
    marker.insert("path".into(), json!(""));
    marker.insert("action".into(), json!("working"));
    marker.insert("agent".into(), json!(agent));
    marker.insert("ts".into(), json!(stamp));
    let _ = store.eph_set(&key, &Value::Object(marker), Some(FOCUS_TTL_S));
    // the week-long row too, exactly as Python's _touch_presence does: an
    // agent that only ever called the protocol still has a last action, and
    // without this it vanished from the Now panel after three minutes
    note_last_action(store, scope, user, session, "working", "", agent, "", "", "", via);
    nudge_workspace(store, scope);
}

pub struct PresenceInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    /// Which conversation is looking. The hook has always sent it; the
    /// server used to drop it on the floor and fold every agent of a person
    /// into one marker.
    pub session: &'a str,
    pub path: &'a str,
    pub action: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    pub branch: &'a str,
    pub tokens: i64,
    pub turn_id: &'a str,
    /// input + cache read + cache creation of the turn: what a message replays
    pub context: i64,
    pub via: &'a str,
}

/// The agent's harness hit a usage limit (the Stop hook read it off the
/// transcript); `path` carries the harness's own words. Python's
/// `_report_limit`, step for step: the Now panel shows the agent PAUSED with
/// that note for up to six hours or until it acts again — replacing whatever
/// read marker was standing, so a "reading …" pill never floats over a paused
/// agent; the ledger keeps a row; the live bus and since_your_last_call carry
/// it to teammates' agents, so nobody waits on a collaborator that silently
/// went dark. One row per distinct note: repeating the same note refreshes
/// the marker and says so, and writes nothing durable.
fn report_limit(store: &Store, input: &PresenceInput) -> Value {
    let stamp = now();
    let note = clip(if input.path.is_empty() { "usage limit reached" } else { input.path }, MAX_NOTE);
    let key = focus_key(input.scope, input.user_id, input.session);
    let existing = store.eph_get(&key).unwrap_or_else(|| json!({}));
    // the Stop hook has no transcript tail to read a model out of, so the
    // marker keeps the model the agent's last read reported
    let model = if input.model.is_empty() { text(&existing, "model") } else { input.model.to_string() };
    let mut marker = identity(input.scope, input.user_id, input.session);
    marker.insert("path".into(), json!(note));
    marker.insert("action".into(), json!("limit"));
    marker.insert("agent".into(), json!(input.agent));
    marker.insert("model".into(), json!(model));
    marker.insert("branch".into(), json!(input.branch));
    marker.insert("ts".into(), json!(stamp));
    let _ = store.eph_set(&key, &Value::Object(marker), Some(LIMIT_TTL_S));
    // the note travels in `note`, not `path`: the last-action line reads
    // "paused · <note>", and there is no file to name
    note_last_action(
        store, input.scope, input.user_id, input.session, "limit", "",
        input.agent, input.model, input.branch, &note, input.via,
    );
    if text(&existing, "action") == "limit" && text(&existing, "path") == note {
        return json!({"ok": true, "noted": "limit", "repeat": true});
    }
    let mut row = json!({"user": input.user_id, "agent": input.agent, "note": note});
    // Python's record() stamps the caller's address on every row it knows
    // it for; this row is the only one the Rust half writes on this path
    if !input.via.is_empty() {
        row["via"] = via_view(input.via);
    }
    let _ = store.ledger_append(input.scope, "agent_limit", &row, stamp);
    crate::events::publish(
        store,
        input.scope,
        json!({"kind": "agent_limit", "user": input.user_id, "agent": input.agent, "note": note}),
        input.via,
    );
    json!({"ok": true, "noted": "limit"})
}

/// The session is over — the hook's SessionEnd. The focus marker goes at
/// once (a "reading x" pill on an agent that has exited would lie) and the
/// last-action line becomes "ended", which the roster reports as `ended` so
/// the dashboard stops counting the agent while its spend still adds up:
/// the token counter is left alone, what it spent is spent. No ledger row
/// and nothing on the event ring — an exit is not history anyone replays —
/// but every dashboard of the workspace, this repo's included, is nudged to
/// re-read the roster. Python's `_report_ended`.
fn report_ended(store: &Store, input: &PresenceInput) -> Value {
    store.eph_delete(&focus_key(input.scope, input.user_id, input.session));
    note_last_action(
        store, input.scope, input.user_id, input.session, "ended", "",
        input.agent, input.model, input.branch, "", input.via,
    );
    nudge(store, input.scope, true);
    json!({"ok": true, "noted": "ended"})
}

pub fn report_presence(store: &Store, input: &PresenceInput) -> Value {
    // "limit" is a different thing entirely — the agent stopped because it
    // ran out of quota — with its own marker lifetime and a ledger row
    if input.action == "limit" {
        return report_limit(store, input);
    }
    if input.action == "ended" {
        return report_ended(store, input);
    }
    let action = match input.action {
        "reading" | "searching" | "running" | "thinking" => input.action,
        _ => "reading",
    };
    bump_tokens(store, input.scope, input.user_id, input.session, input.tokens, input.turn_id, input.context, input.model);

    let shown = clip(input.path, MAX_PATH);
    let mut marker = identity(input.scope, input.user_id, input.session);
    marker.insert("path".into(), json!(shown));
    marker.insert("action".into(), json!(action));
    marker.insert("agent".into(), json!(input.agent));
    marker.insert("model".into(), json!(input.model));
    marker.insert("branch".into(), json!(input.branch));
    marker.insert("ts".into(), json!(now()));
    let _ = store.eph_set(
        &focus_key(input.scope, input.user_id, input.session),
        &Value::Object(marker),
        Some(FOCUS_TTL_S),
    );
    note_last_action(
        store, input.scope, input.user_id, input.session, action, input.path,
        input.agent, input.model, input.branch, "", input.via,
    );

    let mut event = json!({
        "kind": "focus", "user": input.user_id, "session": input.session,
        "agent_id": agent_id(input.user_id, input.session),
        "path": shown, "action": action, "agent": input.agent,
    });
    if let Some(map) = event.as_object_mut() {
        if input.tokens > 0 {
            map.insert("tokens".into(), json!(input.tokens));
        }
        if !input.model.is_empty() {
            map.insert("model".into(), json!(input.model));
        }
        if !input.branch.is_empty() {
            map.insert("branch".into(), json!(input.branch));
        }
    }
    crate::events::publish(store, input.scope, event, input.via);
    nudge_workspace(store, input.scope);
    json!({"ok": true})
}

#[cfg(test)]
mod tests {
    use super::via_view;
    use serde_json::{json, Value};

    #[test]
    fn via_view_builds_the_object_python_stores() {
        // the routes' "host path" string becomes {host, path, mcp}, exactly
        // as Python's via_tag(host, path) shapes it
        assert_eq!(
            via_view("api.collidemcp.com /presence"),
            json!({"host": "api.collidemcp.com", "path": "/presence", "mcp": false})
        );
        assert_eq!(
            via_view("MCP.collidemcp.com,proxy /"),
            json!({"host": "mcp.collidemcp.com", "path": "/", "mcp": true})
        );
        assert_eq!(
            via_view("api.collidemcp.com mcp/"),
            json!({"host": "api.collidemcp.com", "path": "/mcp", "mcp": true})
        );
        // the MCP server's bare tag is a path with no host
        assert_eq!(via_view("mcp"), json!({"host": "", "path": "/mcp", "mcp": true}));
        // unknown stays unknown, as Python's None does
        assert_eq!(via_view(""), Value::Null);
    }
}
