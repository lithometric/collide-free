//! The smaller agent-facing tools: directed messaging, the reconciliation
//! claim queue, `explain_code`'s ledger-with-why, graph interchange, and the
//! dashboard deep-link builder.
//!
//! Each function here mirrors one `service.py` method (or one MCP tool
//! wrapper, for the parts that are pure formatting and never reach
//! `service.py` at all) closely enough that the two can be read side by
//! side. None of it is wired into `mcp.rs`'s dispatch table yet — that is
//! the next step, not this one — so every function takes the same shape the
//! rest of the port settled on: `&Store` plus an explicit scope/user/etc.,
//! returning the `Value` an MCP tool call would answer with.
//!
//! `move_repo`, `remove_repo` and `switch_workspace` are NOT here. All three
//! turn on machinery this crate has not ported yet and which is larger than
//! "the minimal piece": `move_repo`/`remove_repo` both start from Python's
//! `workspace_for()`, the multi-workspace routing search that resolves which
//! workspace actually owns a repo (main.rs's own module doc calls this out
//! as future work), and both move the repo's *binding* through
//! `register_watch()`, which carries its own GitHub-numeric-id rename
//! reconciliation (auto-relinking a renamed repo via `rename_repo`) — a
//! separate subsystem, not a helper this task's scope covers. `workspaces.rs`
//! landed underneath this while it was being written (see its anchored
//! rationale) and now supplies the membership half (`get`, `list_for`,
//! `set_member_repos`, `org_workspace_for`), so the membership/seat-granting
//! side of a real port is no longer blocked — only the binding-registry side
//! is. `switch_workspace`'s no-argument listing is just `workspaces::list_for`
//! and would be trivial; its re-bind path rewrites the caller's raw bearer
//! token's `oauth_token` KV record, which needs the raw token string that
//! only the HTTP/MCP header layer has — out of reach from a function shaped
//! like the rest of this module.
//!
//! Wired from `mcp.rs`'s dispatch and the `/inbox` route; `inbox_ack` also
//! feeds the `messages_pending` delivery every carrying tool call makes
//! (`advisory::apply_setup_hint`).

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::presence::split_scope;
use crate::store::{now, Store};

// ------------------------------------------------------------- small helpers

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// First 240 Unicode characters — Python's `s[:240]` slices by code point,
/// not byte, and a multi-byte character split mid-sequence would not just
/// mismatch, it would not be valid UTF-8.
fn truncate_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

// ------------------------------------------------------- agent-to-agent mail

/// Queue a directed message for one teammate's agents. Delivered two ways:
/// the awake path (rides the recipient's next tool-call response) and
/// `GET /api/inbox`, which a wake poller reads to start a headless agent on
/// an otherwise idle machine. Mirrors `service.send_agent_message`.
pub fn send_agent_message(
    store: &Store, scope: &str, from_user: &str, to: &str, message: &str, agent: &str,
) -> Value {
    send_agent_message_as(store, scope, from_user, to, message, agent, false)
}

/// `send_agent_message`, saying whether it is one copy of a note to every
/// agent at work in the repo (`broadcast`): news for whoever is working,
/// carried by their next hook, never a reason to start an idle session.
pub fn send_agent_message_as(
    store: &Store, scope: &str, from_user: &str, to: &str, message: &str, agent: &str, broadcast: bool,
) -> Value {
    let to = to.trim();
    let message = message.trim();
    if message.is_empty() || to.is_empty() {
        return json!({"ok": false, "error": "empty recipient or message"});
    }
    let id = crate::compat::new_id();
    let ts = now();
    let truncated = truncate_chars(message, 4000);
    let mut record = json!({
        "id": id, "to": to, "from": from_user, "message": truncated, "agent": agent, "ts": ts,
    });
    if broadcast {
        record["broadcast"] = json!(true);
    }
    let _ = store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &record, ts);
    crate::push::arrived();
    let _ = store.ledger_append(scope, "agent_message", &json!({
        "to": to, "from": from_user, "id": id, "chars": truncated.chars().count(),
    }), ts);
    // the receipt: who it went to, as the anchored form already says
    json!({"ok": true, "message_id": id, "delivered_to": [to]})
}

/// How far back an edit counts as "working on it" when a message is routed
/// by code instead of by name.
pub const ROUTE_WINDOW_S: f64 = 24.0 * 3600.0;
/// A routed message reaches at most this many people.
pub const MAX_ROUTED: usize = 10;

/// The anchor as agents write it: `path::symbol`, `path`, or `dir/`.
pub fn anchor_label(anchor: &crate::memory::Anchor) -> String {
    match anchor {
        crate::memory::Anchor::Symbol { path, symbol } => format!("{path}::{symbol}"),
        crate::memory::Anchor::File { path } => path.clone(),
        crate::memory::Anchor::Dir { path } => format!("{path}/"),
    }
}

/// Who is working on the anchored code right now: the owners of open
/// intents over it, and whoever edited it in the last day — on the anchor
/// itself and on the files that call it, because a change to a symbol is
/// news to its callers first. Everyone but the sender, sorted, capped.
/// Python's `_route_recipients`.
pub fn route_recipients(store: &Store, scope: &str, sender: &str, anchor: &crate::memory::Anchor) -> Vec<String> {
    let (dir, mut paths): (Option<String>, BTreeSet<String>) = match anchor {
        crate::memory::Anchor::Dir { path } => (Some(format!("{path}/")), BTreeSet::new()),
        other => (None, BTreeSet::from([other.path().to_string()])),
    };
    if let crate::memory::Anchor::Symbol { path, symbol } = anchor {
        for caller in crate::recipes::callers(store, scope, path, symbol) {
            paths.insert(text(&caller, "path"));
        }
    }
    let covers = |path: &str| paths.contains(path) || dir.as_deref().is_some_and(|d| path.starts_with(d));
    let mut people: BTreeSet<String> = BTreeSet::new();
    for (_, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        if text(&intent, "status") != "active" {
            continue;
        }
        let touched = intent.get("paths").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).any(covers);
        if touched {
            people.insert(text(&intent, "owner"));
        }
    }
    for row in store.ledger_since(scope, now() - ROUTE_WINDOW_S) {
        if row.kind == "edit_reported" && covers(&text(&row.payload, "path")) {
            people.insert(text(&row.payload, "user"));
        }
    }
    let sender = sender.trim().to_lowercase();
    people.into_iter().filter(|p| !p.is_empty() && p.to_lowercase() != sender).take(MAX_ROUTED).collect()
}

/// `send_agent_message` with an anchor: the message points at exact code
/// (the anchor's hash and revision are captured now, so delivery can say
/// whether the code has moved since) and, with no `to`, goes to whoever is
/// working on that code. Messaging limits apply per recipient: on a plan
/// that keeps messages to your own agents, a routed message reaches no one
/// and says why. Python's `send_agent_message` anchor branch.
pub fn send_anchored_message(
    store: &Store, scope: &str, workspace: &str, from_user: &str, to: &str, message: &str, anchor: &str,
    agent: &str, dashboard_url: &str,
) -> Value {
    let message = message.trim();
    if message.is_empty() {
        return json!({"ok": false, "error": "empty recipient or message"});
    }
    let Some(parsed) = crate::memory::parse_anchor(anchor) else {
        return json!({"ok": false, "error": "anchor must be path::symbol, a path, or dir/"});
    };
    let label = anchor_label(&parsed);
    let routed = to.trim().is_empty();
    let recipients: Vec<String> =
        if routed { route_recipients(store, scope, from_user, &parsed) } else { vec![to.trim().to_string()] };
    if recipients.is_empty() {
        return json!({"ok": false, "error": format!(
            "nobody else is working on {label} right now (no open intent or edit in the last 24h on it or its \
callers); name a teammate in `to`")});
    }
    let mut allowed: Vec<String> = Vec::new();
    let mut refusal = String::new();
    for person in &recipients {
        match crate::access::messaging_refusal(store, workspace, from_user, person, dashboard_url) {
            Some(why) => refusal = why,
            None => allowed.push(person.clone()),
        }
    }
    if allowed.is_empty() {
        return json!({"ok": false, "error": refusal});
    }
    let (hash, rev, _) = crate::memory::anchor_state(store, scope, &parsed);
    let ts = now();
    let truncated = truncate_chars(message, 4000);
    let mut ids: Vec<String> = Vec::new();
    for person in &allowed {
        let id = crate::compat::new_id();
        let record = json!({
            "id": id, "to": person, "from": from_user, "message": truncated, "agent": agent, "ts": ts,
            "anchor": label, "anchor_hash": hash, "anchor_rev": rev, "routed": routed,
        });
        let _ = store.kv_put("inbox", &format!("{scope}:{person}:{id}"), &record, ts);
        let _ = store.ledger_append(scope, "agent_message", &json!({
            "to": person, "from": from_user, "id": id, "chars": truncated.chars().count(),
            "anchor": label, "routed": routed,
        }), ts);
        ids.push(id);
    }
    crate::push::arrived();
    let mut out = json!({"ok": true, "message_id": ids[0], "delivered_to": allowed, "anchor": label});
    if allowed.len() < recipients.len() {
        out["withheld"] = json!(recipients.len() - allowed.len());
        out["notice"] = json!(refusal);
    }
    out
}

/// One message as a briefing line: who, what it is about and what became
/// of that code since, then the text.
pub fn message_line(message: &Value) -> String {
    message_line_within(message, 600)
}

/// `message_line` with the message text cut at `limit` characters, saying
/// how much was left out and where to read it whole.
pub fn message_line_within(message: &Value, limit: usize) -> String {
    let mut about = String::new();
    let anchor = text(message, "anchor");
    if !anchor.is_empty() {
        let moved = match message.get("stale").and_then(Value::as_bool) {
            Some(true) => "changed since it was sent",
            Some(false) => "unchanged since",
            None => "its state unknown",
        };
        let o = message.get("outcomes").cloned().unwrap_or(json!({}));
        let n = |k: &str| o.get(k).and_then(Value::as_i64).unwrap_or(0);
        let tests = if n("passes") + n("failures") + n("refixes") == 0 {
            String::new()
        } else {
            format!("; since: {} test run(s) passed, {} failed, {} refix(es)", n("passes"), n("failures"), n("refixes"))
        };
        about = format!(" about {anchor} ({moved}{tests})");
    }
    let said = text(message, "message");
    let total = said.chars().count();
    let rest = if total > limit {
        format!(" [{} more characters: inbox_ack(repo_id) returns the whole message]", total - limit)
    } else {
        String::new()
    };
    format!("- [{}] from {}{about}: {}{rest}", text(message, "id"), text(message, "from"), truncate_chars(&said, limit))
}

/// What happened to the anchored code since the message was sent: whether
/// it moved (its hash no longer the one captured at send time — `null` when
/// either side is unknown), and the verified test runs and refixes on its
/// file since — the same outcomes an anchored note is scored by.
fn annotate_anchor(store: &Store, scope: &str, record: &mut Value) {
    let Some(parsed) = record.get("anchor").and_then(Value::as_str).and_then(crate::memory::parse_anchor) else {
        return;
    };
    let (current, _, _) = crate::memory::anchor_state(store, scope, &parsed);
    let sent = record.get("anchor_hash").and_then(Value::as_str).map(str::to_string);
    let stale = match (sent, current) {
        (Some(sent), Some(current)) => json!(sent != current),
        _ => Value::Null,
    };
    let since = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let outcome = if matches!(parsed, crate::memory::Anchor::Dir { .. }) {
        crate::memory::Outcomes::default()
    } else {
        crate::memory::outcomes(store, scope, parsed.path(), since, now())
    };
    record["stale"] = stale;
    record["outcomes"] = json!({"passes": outcome.passes, "failures": outcome.failures, "refixes": outcome.refixes});
}

/// Pending directed messages for the calling user; `ack` clears the ids the
/// caller has already acted on. Unacked messages redeliver on every call —
/// there is no other retry signal a wake-polled headless agent could give.
///
/// A message is filed under the SENDER's scope (`send_agent_message`), and
/// the recipient may well be working in another repo of the same workspace —
/// so a read of the recipient's own scope alone made a message to the other
/// repo undeliverable. The read fans across `visible`, the caller's allowlist
/// from `access::visible_scopes` (non-optional: a caller that forgot it does
/// not compile), plus the queried scope; a message filed under a repo the
/// recipient cannot see stays undelivered, because it names paths and work
/// in that repo. An ack matches by id wherever the message was filed, so an
/// ack from the recipient's repo clears a message written under the
/// sender's. The queried repo's messages come first, then the rest of the
/// workspace, oldest first within each; foreign rows carry `from_repo`, the
/// recipe convention. Mirrors `service._inbox`.
pub fn inbox_ack(
    store: &Store, scope: &str, user_id: &str, visible: &BTreeSet<String>, ack: &[String],
) -> Value {
    let mut scopes: Vec<&str> = vec![scope];
    scopes.extend(visible.iter().map(String::as_str).filter(|other| *other != scope));

    let mut pending: Vec<Value> = Vec::new();
    for in_scope in scopes {
        for (key, mut record) in store.kv_list("inbox", &format!("{in_scope}:{user_id}:")) {
            // the id is the key's last segment by construction; matching on
            // the key rather than the record survives a record without one
            let id = key.rsplit_once(':').map(|(_, id)| id).unwrap_or("");
            if ack.iter().any(|acked| acked == id) {
                let _ = store.kv_delete("inbox", &key);
                continue;
            }
            if in_scope != scope {
                if let Some(map) = record.as_object_mut() {
                    map.insert("from_repo".into(), json!(split_scope(in_scope).1));
                }
            }
            annotate_anchor(store, in_scope, &mut record);
            pending.push(record);
        }
    }
    pending.sort_by(|a, b| {
        let rank = |record: &Value| {
            (record.get("from_repo").is_some(), record.get("ts").and_then(Value::as_f64).unwrap_or(0.0))
        };
        let (fa, ta) = rank(a);
        let (fb, tb) = rank(b);
        fa.cmp(&fb).then(ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal))
    });
    json!({"ok": true, "messages": pending})
}

// ------------------------------------------------------- reconciliation queue

/// The mandate text handed back with every freshly claimed reconciliation —
/// verbatim from `service.claim_reconciliations`, because an agent reads
/// this sentence to decide how to resolve a collision and a reworded one is
/// different guidance.
const RECONCILE_MANDATE: &str = "ADDITIVE RESOLUTION: build out what BOTH agents were building. \
Never resolve by deleting either side's work — synthesize the union (e.g. keep the new feature \
AND adapt it to the new interface). Verify with tests and differential_check, report the merged \
edit normally, then call pending_reconciliations again with resolve=[this id] and complete any \
covering intent with a rationale describing the synthesis.";

/// The inline reconciler entry point behind `pending_reconciliations`: fetch
/// open collisions, claim the unclaimed ones (15-minute TTL so a stalled
/// claimant releases), and optionally mark ids resolved. Mirrors
/// `service.claim_reconciliations`; the CREATION side
/// (`service._track_reconciliations`, called from the lint pipeline on every
/// report) is a separate, larger piece and not ported here — this only
/// reads and updates records that already exist in the `reconcile:{scope}`
/// ephemeral blob.
///
/// Iteration order: Python walks its `records` dict in insertion order — the
/// order reconciliations were first opened. `serde_json::Map` here is a
/// `BTreeMap` (this crate does not enable the `preserve_order` feature), so
/// this instead sorts by each record's own `opened` timestamp. That recovers
/// the same order exactly, EXCEPT after a resolve-then-reopen of the same
/// (path, symbol, rule, user): Python's dict keeps the record at its
/// ORIGINAL position while `_track_reconciliations` overwrites `opened` to
/// the reopen time, so the two orders can disagree in that one case.
pub fn claim_reconciliations(
    store: &Store, scope: &str, user_id: &str, resolve: &[String], idle_after_s: f64,
) -> Value {
    let key = format!("reconcile:{scope}");
    let mut records: serde_json::Map<String, Value> =
        store.eph_get(&key).and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let stamp = now();
    let mut changed = false;

    for rid in resolve {
        let Some(record) = records.get_mut(rid) else { continue };
        if record.get("status").and_then(Value::as_str) != Some("open") {
            continue;
        }
        let symbol = record.get("symbol").cloned().unwrap_or(Value::Null);
        let rule = record.get("rule").cloned().unwrap_or(Value::Null);
        if let Some(map) = record.as_object_mut() {
            map.insert("status".into(), json!("resolved"));
            map.insert("resolved_by".into(), json!(user_id));
            map.insert("resolved_ts".into(), json!(stamp));
        }
        changed = true;
        let _ = store.ledger_append(scope, "reconciliation_resolved", &json!({
            "id": rid, "symbol": symbol, "user": user_id, "rule": rule,
        }), stamp);
    }

    let mut order: Vec<String> = records.keys().cloned().collect();
    order.sort_by(|a, b| {
        let opened = |rid: &str| records[rid].get("opened").and_then(Value::as_f64).unwrap_or(0.0);
        opened(a).partial_cmp(&opened(b)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out: Vec<Value> = Vec::new();
    for rid in &order {
        let Some(record) = records.get_mut(rid) else { continue };
        if record.get("status").and_then(Value::as_str) != Some("open") {
            continue;
        }
        let claimant = text(record, "claimed_by");
        let claim_ts = record.get("claim_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let claim_fresh = stamp - claim_ts < 900.0;
        if !claimant.is_empty() && claimant != user_id && claim_fresh {
            out.push(json!({
                "id": record.get("id").cloned().unwrap_or_else(|| json!(rid)),
                "symbol": record.get("symbol").cloned().unwrap_or(Value::Null),
                "rule": record.get("rule").cloned().unwrap_or(Value::Null),
                "status": "claimed", "claimed_by": claimant,
            }));
            continue;
        }
        if let Some(map) = record.as_object_mut() {
            map.insert("claimed_by".into(), json!(user_id));
            map.insert("claim_ts".into(), json!(stamp));
        }
        changed = true;

        let record_path = text(record, "path");
        let record_symbol = text(record, "symbol");
        let mut entry = record.clone();
        if let Some(map) = entry.as_object_mut() {
            map.remove("claim_ts");
            map.insert("status".into(), json!("claimed_by_you"));
        }

        let intents = crate::collisions::active_intents(store, scope, idle_after_s);
        let involved: Vec<Value> = intents
            .iter()
            .filter(|intent| {
                let in_symbols = intent.get("symbols").and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|s| s.as_str() == Some(record_symbol.as_str())));
                let in_paths = intent.get("paths").and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|p| p.as_str() == Some(record_path.as_str())));
                in_symbols || in_paths
            })
            .take(4)
            .map(|intent| json!({
                "intent_id": intent.get("intent_id").cloned().unwrap_or(Value::Null),
                "owner": intent.get("owner").cloned().unwrap_or(Value::Null),
                "summary": intent.get("summary").cloned().unwrap_or(Value::Null),
                "operations": intent.get("operations").cloned().unwrap_or(Value::Null),
            }))
            .collect();
        let notes = crate::memory::locality_memories(
            store, scope, &[record_path.clone()], &[record_symbol.clone()], 10);

        if let Some(map) = entry.as_object_mut() {
            map.insert("involved_intents".into(), json!(involved));
            if !notes.is_empty() {
                map.insert("anchored_memories".into(), json!(notes));
            }
            map.insert("mandate".into(), json!(RECONCILE_MANDATE));
        }
        out.push(entry);
    }

    let open_count =
        records.values().filter(|r| r.get("status").and_then(Value::as_str) == Some("open")).count() as i64;
    if changed {
        let _ = store.eph_set(&key, &Value::Object(records), Some(6.0 * 3600.0));
    }
    json!({"reconciliations": out, "open": open_count})
}

// --------------------------------------------------------------- explain_code

/// Notes store anchors as `{"kind","path","symbol"}` objects, or (older
/// notes) as a bare `"path::symbol"` string — normalize both to (path,
/// symbol), matching `insights._note_anchor`.
fn note_anchor(note: &Value) -> (String, String) {
    match note.get("anchor") {
        Some(Value::Object(map)) => (
            map.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            map.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        Some(Value::String(raw)) => match raw.split_once("::") {
            Some((path, symbol)) => (path.to_string(), symbol.to_string()),
            None => (raw.clone(), String::new()),
        },
        _ => (String::new(), String::new()),
    }
}

fn note_text(note: &Value) -> String {
    let t = text(note, "text");
    if !t.is_empty() { t } else { text(note, "fact") }
}

fn note_matches_anchor(note: &Value, path: &str, symbol: &str) -> bool {
    let (a_path, a_symbol) = note_anchor(note);
    if a_path.is_empty() && a_symbol.is_empty() {
        return false;
    }
    if !symbol.is_empty() && !a_symbol.is_empty() && a_symbol != symbol {
        return false;
    }
    if path.is_empty() {
        return !symbol.is_empty() && a_symbol == symbol;
    }
    if a_path.ends_with('/') {
        return path.starts_with(&a_path);
    }
    a_path == path
}

fn edit_row_touches(payload: &Value, path: &str, symbol: &str) -> bool {
    if !path.is_empty() && text(payload, "path") != path {
        return false;
    }
    if !symbol.is_empty() {
        let in_changed = payload.get("symbols_changed").and_then(Value::as_object)
            .is_some_and(|m| m.contains_key(symbol));
        let in_events = payload.get("events").and_then(Value::as_array)
            .is_some_and(|events| events.iter().any(|e| e.get("symbol").and_then(Value::as_str) == Some(symbol)));
        if !in_changed && !in_events {
            return false;
        }
    }
    true
}

/// `[settled <intent-id>]`, anchored at the START of the text — the prefix
/// `intents::complete` mints for the anchored twin of a completed intent's
/// rationale (see `intents.rs`'s `[settled {id}] {rationale}`). Matches
/// Python's `re.match(r"\[settled ([0-9a-f]+)\]", text)`.
fn settled_intent_id(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("[settled ")?;
    let end = rest.find(']')?;
    let candidate = &rest[..end];
    let is_hex = !candidate.is_empty()
        && candidate.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    is_hex.then_some(candidate)
}

/// Hard bound on ledger rows walked, matching `insights::MAX_ROWS` /
/// `service._walk_ledger`'s `MAX_ROWS`: on a very busy scope this answers
/// from the most RECENT rows in history rather than growing without bound.
const WHY_MAX_ROWS: usize = 4000;

/// The chain behind one path or symbol, oldest first: edits, the rationales
/// behind them, scars (attempts that were reverted), and anchored notes.
/// `git blame` says who and when; this says what was tried, what was
/// settled, and why. Mirrors `insights.why`.
///
/// `"seq"` (the ledger row's sequence number) is NOT in a chain entry here,
/// unlike Python's: `store::LedgerRow` deliberately does not carry it (every
/// existing caller only needs ordering, which the row order already answers
/// — see its doc comment), and adding it back just for this one caller was
/// not worth reopening that decision. An agent reading `explain_code` loses
/// the ledger deep-link number; everything else in the chain is unaffected.
/// `since_ts` is the plan's history window: rows before it are not read
/// (0 = all of history).
pub fn why(store: &Store, scope: &str, path: &str, symbol: &str, limit: usize, since_ts: f64) -> Value {
    let mut rows = store.ledger_since(scope, since_ts);
    if rows.len() > WHY_MAX_ROWS {
        rows = rows.split_off(rows.len() - WHY_MAX_ROWS);
    }
    let considered_rows = rows.len();

    let mut intent_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // (ts, entry); a final stable sort by ts reproduces Python's
    // `chain.sort(key=lambda e: e.get("ts") or 0)` over the same entries.
    let mut chain: Vec<(f64, Value)> = Vec::new();

    for row in &rows {
        if row.kind != "edit_reported" || !edit_row_touches(&row.payload, path, symbol) {
            continue;
        }
        let payload = &row.payload;
        let mut changed_symbols: Vec<String> = payload.get("symbols_changed").and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect()).unwrap_or_default();
        changed_symbols.sort();
        changed_symbols.truncate(8);
        let intent_id = text(payload, "intent_id");
        let mut entry = json!({
            "type": "edit", "ts": row.ts, "seq": row.seq,
            "by": text(payload, "user"), "agent": text(payload, "agent"),
            "path": text(payload, "path"), "symbols": changed_symbols,
            "lines_added": payload.get("lines_added").and_then(Value::as_i64).unwrap_or(0),
            "lines_removed": payload.get("lines_removed").and_then(Value::as_i64).unwrap_or(0),
        });
        if !intent_id.is_empty() {
            entry["intent_id"] = json!(intent_id);
            intent_ids.insert(intent_id);
        }
        chain.push((row.ts, entry));
    }

    let mut rationale_intents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in &rows {
        if row.kind != "intent_completed" {
            continue;
        }
        let payload = &row.payload;
        let this_intent = text(payload, "intent_id");
        let mut related = !this_intent.is_empty() && intent_ids.contains(&this_intent);
        if !related && !symbol.is_empty() {
            related = payload.get("operations").and_then(Value::as_array)
                .is_some_and(|ops| ops.iter().any(|op| op.get("symbol").and_then(Value::as_str) == Some(symbol)));
        }
        let rationale = text(payload, "rationale");
        if related && !rationale.is_empty() {
            if !this_intent.is_empty() {
                rationale_intents.insert(this_intent.clone());
            }
            let entry = json!({
                "type": "rationale", "ts": row.ts, "seq": row.seq,
                "by": text(payload, "owner"), "intent_id": this_intent, "text": rationale,
            });
            chain.push((row.ts, entry));
        }
    }

    // anchored memory: notes, auto-rationales, and scars pinned to this code
    for (_key, note) in store.kv_list("memory", &format!("{scope}:")) {
        if !note_matches_anchor(&note, path, symbol) {
            continue;
        }
        let auto = note.get("auto").and_then(Value::as_str).unwrap_or("");
        if auto == "rationale" {
            // complete_intent mints an anchored twin of its rationale; if the
            // ledger row is already in the chain, the twin is a duplicate
            if let Some(settled) = settled_intent_id(&note_text(&note)) {
                if rationale_intents.contains(settled) {
                    continue;
                }
            }
        }
        let (a_path, a_symbol) = note_anchor(&note);
        let entry_type = match auto {
            "scar" => "scar",
            "rationale" => "rationale",
            _ => "note",
        };
        let created = note.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let anchor = if !a_symbol.is_empty() { format!("{a_path}::{a_symbol}") } else { a_path };
        let entry = json!({
            "type": entry_type, "ts": created, "by": text(&note, "by"),
            "text": truncate_chars(&note_text(&note), 500), "anchor": anchor,
        });
        chain.push((created, entry));
    }

    chain.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut entries: Vec<Value> = chain.into_iter().map(|(_, entry)| entry).collect();
    if entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    json!({"path": path, "symbol": symbol, "chain": entries, "considered_rows": considered_rows})
}

/// `explain_code`'s entry point — `insights.why` with the tool's fixed
/// 40-entry limit.
pub fn explain_code(store: &Store, scope: &str, path: &str, symbol: &str, since_ts: f64) -> Value {
    why(store, scope, path, symbol, 40, since_ts)
}

// ------------------------------------------------------------ graph export

/// Hand the graph to other tools: Graphify's `graph.json` shape, GraphML for
/// Gephi/yEd, or Cypher for Neo4j/FalkorDB. Mirrors `service.graph_export`.
pub fn graph_export(store: &Store, scope: &str, repo_id: &str, format: &str, idle_after_s: f64) -> Value {
    // `fmt = (fmt or "graphify").strip().lower()`: the default only kicks in
    // on a genuinely EMPTY input, before trimming — "  " stays "  ", trims to
    // "", and falls through to the format-not-recognized error below, same
    // as Python.
    let raw = if format.is_empty() { "graphify" } else { format };
    let normalized = raw.trim().to_lowercase();
    if !matches!(normalized.as_str(), "graphify" | "graphml" | "cypher") {
        return json!({"ok": false, "error": "format must be graphify, graphml, or cypher"});
    }

    let graph = crate::graphview::snapshot(store, scope);
    let overlays = crate::graphview::overlays_for(store, scope, &graph, idle_after_s, 30.0, true, true);
    let comms = crate::graphview::communities_cached(&graph, scope, &overlays.co_change);

    if normalized == "graphify" {
        return json!({
            "ok": true, "format": "graphify",
            "graph": crate::graphview::to_graphify(&graph, comms.as_slice(), &overlays, repo_id),
        });
    }
    let (content, filename) = if normalized == "graphml" {
        (crate::graphview::to_graphml(&graph, comms.as_slice()), "collide-graph.graphml")
    } else {
        (crate::graphview::to_cypher(&graph, comms.as_slice()), "collide-graph.cypher")
    };
    json!({"ok": true, "format": normalized, "content": content, "filename": filename})
}

fn text_field(value: &Value, key: &str) -> String {
    text(value, key)
}

/// `(path, symbol)` from a Graphify node: `source_file` + `label`, or an id
/// of the form `path::symbol` / `path:symbol` / `path#symbol`. Mirrors
/// `graph._graphify_node_parts`.
fn graphify_node_parts(node: &Value) -> (String, String) {
    let mut path = {
        let a = text_field(node, "source_file");
        if !a.is_empty() { a } else {
            let b = text_field(node, "file");
            if !b.is_empty() { b } else { text_field(node, "path") }
        }
    };
    let mut label = {
        let a = text_field(node, "label");
        if !a.is_empty() { a } else { text_field(node, "name") }
    };
    let nid = text_field(node, "id");
    if path.is_empty() {
        for sep in ["::", "#", ":"] {
            if let Some(pos) = nid.find(sep) {
                let (before, after) = (nid[..pos].to_string(), nid[pos + sep.len()..].to_string());
                path = before;
                if label.is_empty() {
                    label = after;
                }
                break;
            }
        }
    }
    if label.is_empty() && !nid.is_empty() && nid != path {
        label = nid;
    }
    if label == path {
        label = String::new();
    }
    (path, label)
}

const GRAPHIFY_KIND_MAP: &[(&str, &str)] = &[
    ("calls", "calls"), ("call", "calls"),
    ("imports", "references"), ("import", "references"),
    ("inherits", "inherits"), ("extends", "inherits"), ("implements", "inherits"), ("mixes_in", "inherits"),
    ("uses", "uses_type"), ("uses_type", "uses_type"),
    ("references", "references"), ("depends_on", "references"),
];

fn map_graphify_kind(relation: &str) -> &'static str {
    GRAPHIFY_KIND_MAP.iter().find(|(k, _)| *k == relation).map(|(_, v)| *v).unwrap_or("references")
}

/// Bootstrap the graph from a Graphify `graph.json`: files Collide has not
/// parsed itself (a language it has no grammar for yet, or code nobody has
/// edited) get nodes and edges straight from the import. A file Collide
/// already parses is left alone — the live parse is the truth for it.
/// Mirrors `graph.import_graphify`.
pub fn graph_import(store: &Store, scope: &str, data: &Value, ts: f64) -> Value {
    let Some(nodes) = data.get("nodes").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return json!({"ok": false, "error": "expected a graph.json object with a 'nodes' array"});
    };
    let edges_in: Vec<Value> = data.get("edges").and_then(Value::as_array).cloned()
        .or_else(|| data.get("links").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    // existing per-file records, keyed by path (same shape `codegraph`'s
    // internal `records()` builds), so a file Collide already parses itself
    // is left untouched
    let existing: std::collections::BTreeMap<String, Value> = store
        .kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:"))
        .into_iter()
        .filter_map(|(_key, record)| {
            let path = record.get("path").and_then(Value::as_str)?.to_string();
            Some((path, record))
        })
        .collect();

    let mut by_id: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    let mut per_file: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    let mut skipped_parsed: i64 = 0;

    for node in nodes {
        let (path, label) = graphify_node_parts(node);
        if path.is_empty() {
            continue;
        }
        let id_field = text_field(node, "id");
        let key = if !id_field.is_empty() { id_field } else { crate::graphview::node_id(&path, &label) };
        by_id.insert(key, (path.clone(), label.clone()));

        if let Some(existing_rec) = existing.get(&path) {
            if !crate::compat::truthy(existing_rec.get("imported")) {
                skipped_parsed += 1;
                continue;
            }
        }
        let rec = per_file.entry(path.clone()).or_insert_with(|| {
            let language = { let l = text_field(node, "language"); if l.is_empty() { "graphify".to_string() } else { l } };
            json!({"path": path, "language": language, "symbols": {}, "imports": [], "edges": [],
                   "imported": true, "ts": ts})
        });
        if !label.is_empty() {
            let kind = { let t = text_field(node, "type"); if t.is_empty() { "definition".to_string() } else { t } };
            if let Some(symbols) = rec.get_mut("symbols").and_then(Value::as_object_mut) {
                symbols.insert(label.clone(), json!({"kind": kind, "edges": []}));
            }
        }
    }

    let mut edge_count: i64 = 0;
    for edge in &edges_in {
        let src_id = { let a = text_field(edge, "source"); if !a.is_empty() { a } else { text_field(edge, "from") } };
        let dst_id = { let a = text_field(edge, "target"); if !a.is_empty() { a } else { text_field(edge, "to") } };
        let (Some((src_path, src_sym)), Some((dst_path, dst_sym))) =
            (by_id.get(&src_id), by_id.get(&dst_id))
        else {
            continue;
        };
        let from_id = crate::graphview::node_id(src_path, src_sym);
        let to_id = crate::graphview::node_id(dst_path, dst_sym);
        let relation = {
            let r = text_field(edge, "relation");
            let r = if !r.is_empty() { r } else { text_field(edge, "type") };
            let r = if !r.is_empty() { r } else { text_field(edge, "kind") };
            if r.is_empty() { "references".to_string() } else { r }
        }.to_lowercase();
        let confidence = { let c = text_field(edge, "confidence"); if c.is_empty() { "inferred".to_string() } else { c } }
            .to_lowercase();
        let kind = map_graphify_kind(&relation);
        let Some(rec) = per_file.get_mut(src_path) else { continue };
        if let Some(edges_arr) = rec.get_mut("edges").and_then(Value::as_array_mut) {
            edges_arr.push(json!({"from": from_id, "to": to_id, "kind": kind, "confidence": confidence}));
        }
        edge_count += 1;
    }

    for (path, rec) in &per_file {
        let _ = store.kv_put(
            crate::codegraph::GRAPH_BUCKET, &format!("{scope}:{}", crate::codegraph::path_key(path)), rec, ts);
        if let Some(edges_arr) = rec.get("edges").and_then(Value::as_array) {
            for edge in edges_arr {
                let to = text(edge, "to");
                let from = text(edge, "from");
                let kind = text(edge, "kind");
                let rev_key = format!("{scope}:{to}");
                let mut doc = store.kv_get(crate::codegraph::REV_BUCKET, &rev_key)
                    .unwrap_or_else(|| json!({"dependents": {}}));
                if let Some(map) = doc.as_object_mut() {
                    if let Some(dependents) = map.get_mut("dependents").and_then(Value::as_object_mut) {
                        dependents.insert(from, json!(kind));
                    }
                    map.insert("ts".into(), json!(ts));
                }
                let _ = store.kv_put(crate::codegraph::REV_BUCKET, &rev_key, &doc, ts);
            }
        }
    }
    crate::graphview::invalidate(store, scope);

    json!({
        "ok": true, "files_imported": per_file.len() as i64, "edges_imported": edge_count,
        "files_skipped_already_parsed": skipped_parsed,
    })
}

// --------------------------------------------------------------- dashboard

const DASH_ROUTES: &[(&str, &str)] = &[
    ("overview", "/dashboard"), ("dashboard", "/dashboard"), ("home", "/dashboard"), ("settings", "/dashboard"),
    ("activity", "/dashboard/activity"), ("feed", "/dashboard/activity"),
    ("change", "/dashboard/change"), ("edit", "/dashboard/change"),
    ("why", "/dashboard/why"), ("history", "/dashboard/why"),
    ("worklog", "/dashboard/worklog"),
    ("journal", "/dashboard/journal"), ("sessions", "/dashboard/journal"),
    ("billing", "/dashboard/billing"), ("invoices", "/dashboard/billing"), ("checkout", "/dashboard/billing"),
    ("pricing", "/pricing"), ("plans", "/pricing"), ("seats", "/pricing"),
    ("upgrade", "/pricing"), ("downgrade", "/pricing"),
];

fn dash_route(section: &str) -> &'static str {
    DASH_ROUTES.iter().find(|(k, _)| *k == section).map(|(_, v)| *v).unwrap_or("/dashboard")
}

/// A human-clickable deep link into one dashboard view. Relative (no origin)
/// when no dashboard URL is configured — the dev-server case. Mirrors
/// `mcp_server._dashboard_link`.
fn dashboard_link(
    dashboard_url: &str, section: &str, path: &str, seq: i64, seats: i64, interval: &str, plan: &str,
) -> String {
    let origin = dashboard_url.trim_end_matches('/');
    let normalized = {
        let s = if section.is_empty() { "overview" } else { section };
        let s = s.trim().to_lowercase();
        if s.is_empty() { "overview".to_string() } else { s }
    };
    let route = dash_route(&normalized);
    let mut params: Vec<(&str, String)> = Vec::new();
    if route.ends_with("/change") && seq != 0 {
        params.push(("seq", seq.to_string()));
    }
    if route.ends_with("/why") && !path.is_empty() {
        params.push(("path", path.to_string()));
    }
    if route == "/pricing" || route == "/dashboard/billing" {
        if !plan.is_empty() {
            params.push(("plan", plan.to_string()));
        }
        if seats != 0 {
            params.push(("seats", seats.to_string()));
        }
        if !interval.is_empty() {
            params.push(("interval", interval.to_string()));
        }
    }
    let query = if params.is_empty() {
        String::new()
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &params {
            ser.append_pair(k, v);
        }
        format!("?{}", ser.finish())
    };
    format!("{origin}{route}{query}")
}

/// A human-clickable deep link into the web dashboard. Call this whenever
/// the person wants to SEE something rather than just hear it, and put the
/// returned `url` at the end of the reply. Mirrors the `open_dashboard` MCP
/// tool wrapper directly — there is no `service.py` method behind it, it is
/// pure link formatting, so this is the whole port rather than a stand-in
/// for a Python function of the same name. The caller (not reproduced here)
/// is responsible for the membership gate Python's wrapper performs via
/// `auth_of`/`workspace_for` before ever reaching this point.
pub fn open_dashboard(
    dashboard_url: &str, section: &str, path: &str, seq: i64, seats: i64, interval: &str, plan: &str,
) -> Value {
    let sec = {
        let s = if section.is_empty() { "overview" } else { section };
        let s = s.trim().to_lowercase();
        if s.is_empty() { "overview".to_string() } else { s }
    };
    let url = dashboard_link(dashboard_url, &sec, path, seq, seats, interval, plan);
    let label = match sec.as_str() {
        "activity" => "Live activity in Collide".to_string(),
        "change" => if seq != 0 { format!("Change #{seq} in Collide") } else { "A change in Collide".to_string() },
        "why" => if !path.is_empty() { format!("Why {path} is like this") } else { "Code history in Collide".to_string() },
        "worklog" => "The Collide worklog".to_string(),
        "journal" => "Agent sessions & cost".to_string(),
        "billing" => "Plan, seats & invoices".to_string(),
        "pricing" => "Plans & pricing".to_string(),
        "seats" => "Add or change seats".to_string(),
        "upgrade" => "Upgrade your plan".to_string(),
        "downgrade" => "Change your plan".to_string(),
        _ => "The Collide dashboard".to_string(),
    };
    json!({
        "section": sec,
        "url": url,
        "label": label,
        "quick_links": {
            "overview": dashboard_link(dashboard_url, "overview", "", 0, 0, "", ""),
            "activity": dashboard_link(dashboard_url, "activity", "", 0, 0, "", ""),
            "billing": dashboard_link(dashboard_url, "billing", "", 0, 0, "", ""),
            "pricing": dashboard_link(dashboard_url, "pricing", "", 0, 0, "", ""),
        },
        "hint": "Put this url at the end of your reply — it opens the exact view in the human's browser.",
    })
}
