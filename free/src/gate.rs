//! The write gate.
//!
//! PreToolUse asks this whether a write may proceed. It blocks only on
//! confidence-certain collisions — a rename, a removal or a signature change
//! by someone else — touching the exact path being written, and it fails open
//! on anything else at all. A gate that wrongly blocks is worse than no gate:
//! agents learn to route around it.
//!
//! An enforce-mode check IS a check before write, performed mechanically on
//! every Edit. Recording only the blocks left the ledger unable to tell "the
//! gate cleared this path" from "nobody looked", which is what made the
//! strictest repos score worst on check-before-write. So the clear is
//! recorded too.

use serde_json::{json, Value};

use crate::repo::path_key;
use crate::store::{now, Store};

const BLOCKING_KINDS: [&str; 3] = ["renamed", "removed", "signature"];
const CERTAIN: &str = "certain";
const BLOCKED_TTL_S: f64 = 600.0;
/// How long the gate's "editing" presence marker stands. PreToolUse fires
/// before EVERY Edit/Write/MultiEdit/apply_patch/Bash, so an agent that is
/// actually writing re-arms this many times a minute; 60s is long enough to
/// bridge one slow tool call (a big MultiEdit, a long build) without the line
/// flickering, and short enough that an agent which stopped writing — or
/// whose write was blocked and abandoned — stops claiming to edit within a
/// minute. A stale "editing" line is worse than a missing one, and
/// report_edit deletes this marker the moment the write actually lands.
const EDITING_TTL_S: f64 = 60.0;
const MAX_PATH: usize = 300;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Collisions, blocks and overrides leave scar tissue on a path: a running
/// score in the shared KV, so nothing has to scan the ledger on the request
/// path.
fn bump_salience(store: &Store, scope: &str, path: &str, field: &str) {
    let key = format!("{scope}:{}", path_key(path));
    let mut record = store.kv_get("salience", &key).unwrap_or_else(|| {
        json!({"path": path, "collisions": 0, "overrides": 0, "blocks": 0})
    });
    let Some(map) = record.as_object_mut() else { return };
    let current = map.get(field).and_then(Value::as_i64).unwrap_or(0);
    map.insert(field.to_string(), json!(current + 1));
    let read = |name: &str| map.get(name).and_then(Value::as_i64).unwrap_or(0);
    let score = 3 * read("collisions") + 4 * read("overrides") + 2 * read("blocks");
    map.insert("score".into(), json!(score));
    let stamp = now();
    map.insert("updated".into(), json!(stamp));
    let _ = store.kv_put("salience", &key, &record, stamp);
}

/// The pre-write gate IS presence. It fires before EVERY Edit/Write, which
/// makes it the earliest honest moment the dashboard can say "editing
/// <path>" — earlier than any report, which only arrives once the write has
/// already landed. (The other source of that line is the `draft:` marker, and
/// no hook has ever sent draft=true.) `report_edit` deletes this marker when
/// the write lands, flipping the row from "editing <path>" to the last-action
/// line with no extra bookkeeping.
///
/// Written DIRECTLY, not through `touch_presence`: that one returns early
/// when any focus marker is under 60s old, so a read half a minute earlier
/// would silently swallow this.
///
/// Per agent: the gate carries the session, so the marker is THIS agent's
/// and it inherits from THIS agent's standing marker — a second agent of the
/// same person editing elsewhere neither overwrites this line nor lends it
/// its model.
fn mark_editing(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, via: &str) {
    let key = crate::presence::focus_key(scope, user_id, session);
    let existing = store.eph_get(&key).unwrap_or_else(|| json!({}));
    // the gate payload carries only repo_id, path and session on purpose:
    // agent/model/branch would mean reading the transcript tail, which the
    // hook's fail-open budget cannot afford. So they are INHERITED from the
    // marker already standing — an empty model BLANKS the dashboard's agent
    // chip, because model resolution takes whichever of focus/lastedit is
    // newer and returns its model verbatim.
    let agent = text(&existing, "agent");
    let model = text(&existing, "model");
    let shown = crate::presence::clip(path, MAX_PATH);
    let agent_id = crate::presence::agent_id(user_id, session);
    let _ = store.eph_set(
        &key,
        &json!({"user": user_id, "session": session, "agent_id": &agent_id,
                "repo_id": crate::presence::split_scope(scope).1,
                "path": &shown, "action": "editing",
                "agent": &agent, "model": &model, "branch": text(&existing, "branch"),
                "ts": now()}),
        Some(EDITING_TTL_S),
    );
    // liveness: the dashboard polls every 10s anyway, but any event refreshes
    // it immediately. NOTE for the dashboard: the Activity feed renders a
    // focus event that is not running/searching as "read <path>", so it needs
    // an "editing" case of its own.
    let mut event = json!({"kind": "focus", "user": user_id, "session": session,
                           "agent_id": &agent_id, "path": &shown,
                           "action": "editing", "agent": &agent});
    if !model.is_empty() {
        if let Some(map) = event.as_object_mut() {
            map.insert("model".into(), json!(model));
        }
    }
    crate::events::publish(store, scope, event, via);
    crate::presence::nudge_workspace(store, scope);
}

pub struct Decision {
    pub body: Value,
}

/// One ephemeral scan, then a verdict. Any internal failure allows the write.
/// Whether any agent holds a hot marker on this path: the cheap test before a
/// read is parsed for `acknowledge_read`.
pub fn has_blocking_marker(store: &Store, scope: &str, path: &str) -> bool {
    !store.eph_scan(&format!("hot:{scope}:{}:", path_key(path))).is_empty()
}

/// The key that records one agent having one blocking change in its copy.
fn ack_key(scope: &str, user_id: &str, session: &str, path: &str, marker: &Value, event: &Value) -> String {
    // fixed decimals: Python and Rust print a bare float differently
    let what = format!(
        "{}|{}|{}|{}|{:.6}",
        text(marker, "user"), text(marker, "session"), text(event, "kind"), text(event, "symbol"),
        marker.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
    );
    format!("gateack:{scope}:{}:{}:{}", crate::presence::agent_id(user_id, session), path_key(path), crate::hashing::sha256_hex(&what).get(..16).unwrap_or(""))
}

/// A read of `path` by this agent: `symbols` is what ITS copy holds, name to
/// signature. Each certain change another agent made there that the copy
/// already reflects is acknowledged, so the gate stops blocking it. Without
/// this a block lasted until the marker expired, whatever the agent did: in
/// the staging demo an agent pulled the rename, re-read the file, and was
/// still blocked twelve times, then fixed the bug somewhere worse.
pub fn acknowledge_read(
    store: &Store, scope: &str, user_id: &str, session: &str, path: &str,
    symbols: &std::collections::BTreeMap<String, String>,
) -> usize {
    let mut acked = 0;
    for (_key, marker) in store.eph_scan(&format!("hot:{scope}:{}:", path_key(path))) {
        if text(&marker, "path") != path {
            continue;
        }
        let same_user = text(&marker, "user") == user_id;
        let marker_session = text(&marker, "session");
        if same_user && (session.is_empty() || marker_session.is_empty() || marker_session == session) {
            continue;
        }
        for event in marker.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let symbol = text(event, "symbol");
            let adapted = match text(event, "kind").as_str() {
                // the copy has the new name: it has the teammate's change. The
                // old name may rightly still be there when the "rename" was two
                // different symbols; demanding it gone held an agent that had
                // pulled everything, five times (Study 5, medium)
                "renamed" => {
                    let new_name = text(event, "new_name");
                    !new_name.is_empty() && symbols.contains_key(&new_name)
                }
                "removed" => !symbols.contains_key(&symbol),
                "signature" => {
                    let after = text(event, "signature_after");
                    !after.is_empty() && symbols.get(&symbol).map(String::as_str) == Some(after.as_str())
                }
                _ => false,
            };
            if adapted {
                let _ = store.eph_set(
                    &ack_key(scope, user_id, session, path, &marker, event),
                    &json!({"ts": now()}),
                    Some(BLOCKED_TTL_S.max(7_200.0)),
                );
                acked += 1;
            }
        }
    }
    acked
}

pub fn gate(
    store: &Store, scope: &str, user_id: &str, session: &str, path: &str, dashboard_url: &str, via: &str,
) -> Decision {
    let prefix = format!("hot:{scope}:{}:", path_key(path));
    let mut blocking: Vec<Value> = Vec::new();
    for (_key, marker) in store.eph_scan(&prefix) {
        if text(&marker, "path") != path {
            continue;
        }
        // my own marker is mine only when it is my session's: another
        // session of the same user is another agent
        let same_user = text(&marker, "user") == user_id;
        let marker_session = text(&marker, "session");
        if same_user && (session.is_empty() || marker_session.is_empty() || marker_session == session) {
            continue;
        }
        let author = if same_user { "another session of yours".to_string() } else { text(&marker, "user") };
        for event in marker.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let kind = text(event, "kind");
            if !BLOCKING_KINDS.contains(&kind.as_str()) || text(event, "confidence") != CERTAIN {
                continue;
            }
            // this agent already has the change in its copy (it pulled and
            // read the file): the conflict the block prevents is gone
            if store.eph_get(&ack_key(scope, user_id, session, path, &marker, event)).is_some() {
                continue;
            }
            let mut annotated = event.clone();
            if let Some(map) = annotated.as_object_mut() {
                map.insert("author".into(), json!(author));
            }
            blocking.push(annotated);
        }
    }

    // the clear is evidence too: this is what lets compliance tell a gated
    // write from an unobserved one
    store.ledger_append_later(
        scope,
        "check_performed",
        &json!({
            "user": user_id, "session": session, "agent": "collide-gate-hook",
            "paths": [path], "symbols": [], "auto": true,
        }),
        now(),
    );
    mark_editing(store, scope, user_id, session, path, via);

    if blocking.is_empty() {
        // no live rename/removal here, but the copy may still be behind a
        // teammate's change to this very file: said once, before the write
        let who = crate::freshness::behind(store, scope, user_id, session, path).map(|b| {
            if b.user == user_id {
                "Another session of yours".to_string()
            } else {
                let users: std::collections::BTreeSet<String> = [b.user.clone()].into_iter().collect();
                crate::activity::identity_labels(store, user_id, &users)
                    .get(&b.user)
                    .and_then(|l| l.get("label"))
                    .and_then(Value::as_str)
                    .unwrap_or(&b.user)
                    .to_string()
            }
        });
        if let Some(reason) = who.and_then(|who| crate::freshness::gate_note(store, scope, user_id, session, path, &who)) {
            crate::events::publish(store, scope, json!({"kind": "gate", "action": "block", "user": user_id, "path": path, "reason": reason}), via);
            return Decision { body: json!({"allow": false, "path": path, "reason": reason, "collisions": [], "stale_copy": true}) };
        }
        return Decision { body: json!({"allow": true, "path": path}) };
    }

    let first = &blocking[0];
    let detail = match first.get("detail") {
        Some(Value::Object(map)) => format!(
            "now `{}` — {}",
            map.get("new_name").and_then(Value::as_str).unwrap_or(""),
            map.get("signature").and_then(Value::as_str).unwrap_or("")
        ),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let see = if dashboard_url.is_empty() {
        String::new()
    } else {
        format!(
            " Collide just prevented a blind conflict — the human can see it at {dashboard_url}/dashboard."
        )
    };
    let reason = format!(
        "{} {} `{}` in {path} ({detail}). Pull that change and re-read {path}: once your copy has it, this write goes through.{see}",
        text(first, "author"),
        text(first, "kind"),
        text(first, "symbol"),
    );

    let stamp = now();
    // repo-scoped still — a block is about this repo's code — but keyed by
    // AGENT, so two agents of one person blocked on the same path no longer
    // share (and overwrite) one marker
    let _ = store.eph_set(
        &format!("blocked:{scope}:{}:{}", crate::presence::agent_id(user_id, session), path_key(path)),
        &json!({"path": path, "ts": stamp, "reason": reason}),
        Some(BLOCKED_TTL_S),
    );
    bump_salience(store, scope, path, "blocks");
    let _ = store.ledger_append(
        scope,
        "gate_block",
        &json!({"user": user_id, "path": path, "events": blocking}),
        stamp,
    );

    crate::events::publish(
        store,
        scope,
        json!({"kind": "gate", "action": "block", "user": user_id,
               "path": path, "reason": reason}),
        via,
    );

    Decision {
        body: json!({
            "allow": false, "path": path, "reason": reason, "collisions": blocking,
        }),
    }
}
