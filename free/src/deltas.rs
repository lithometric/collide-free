//! Live deltas: what other agents changed in the region this session is
//! working in, since its last step, as a few compressed lines.
//!
//! The working set is what the session has read, written, or been briefed
//! on (ephemeral `ws:{scope}:{session}:{path_key}` rows, two hours), and a
//! row remembers whether the session WROTE the file. What is relevant
//! depends on that: in a file the session wrote, every change by someone
//! else is (the two edits overlap); in a file it only read or was briefed
//! on, and through a dependent, only a change to an interface is — a
//! rename, a removal, a new signature. A new function or a body edit there
//! changes nothing the reader relies on, and telling it anyway made agents
//! pull and re-read for nothing (Study 3: the agents at the edges spent
//! 0.3–0.6M tokens more each keeping up with changes that did not touch
//! them). Lines about one file fold into one. The hook asks on
//! every event and prepends the answer to that event's own output, so an
//! agent learns of a colliding edit inside the step it was already taking —
//! never by rereading the graph, never the whole graph.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

const WS_TTL_S: f64 = 7_200.0;
const DEFAULT_BUDGET: usize = 1_500;
const MAX_BUDGET: usize = 6_000;
const MAX_EVENTS_PER_ROW: usize = 4;
const MAX_SYMBOLS_PER_ROW: usize = 10;
const MAX_LOOKBACK_S: f64 = 3_600.0;

fn ws_key(scope: &str, session: &str, path: &str) -> String {
    format!("ws:{scope}:{session}:{}", crate::codegraph::path_key(path))
}

/// A path joins the session's working set. `wrote` marks a file the session
/// wrote; a later read or briefing never takes that back.
pub fn touch(store: &Store, scope: &str, session: &str, paths: &[String], stamp: f64) {
    touch_as(store, scope, session, paths, false, stamp);
}

pub fn touch_as(store: &Store, scope: &str, session: &str, paths: &[String], wrote: bool, stamp: f64) {
    if session.is_empty() {
        return;
    }
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let key = ws_key(scope, session, path);
        let had = store.eph_get(&key).and_then(|v| v.get("wrote").and_then(Value::as_bool)).unwrap_or(false);
        let _ = store.eph_set(&key, &json!({"path": path, "ts": stamp, "wrote": wrote || had}), Some(WS_TTL_S));
    }
}

/// The files of the working set this session wrote.
fn written_set(store: &Store, scope: &str, session: &str) -> BTreeSet<String> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter(|(_, v)| v.get("wrote").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|(_, v)| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// A change another agent's code could break on: a rename, a removal, a new
/// signature. A body edit or a new symbol breaks no caller.
fn is_interface_change(kind: &str, signature_before: &str, signature_after: &str) -> bool {
    matches!(kind, "renamed" | "removed")
        || (kind != "added" && !signature_after.is_empty() && signature_after != signature_before)
}

pub(crate) fn working_set(store: &Store, scope: &str, session: &str) -> BTreeSet<String> {
    store
        .eph_scan(&format!("ws:{scope}:{session}:"))
        .into_iter()
        .filter_map(|(_, v)| v.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub(crate) fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7_200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else {
        format!("{}h ago", (s / 3_600.0) as i64)
    }
}

/// What other agents changed in this session's working set, plus any held
/// plan groups that came free for this person: the release lines ride on
/// every answer, deltas or not, because a planner waiting on them may have
/// nothing else changing around it.
pub fn deltas(store: &Store, scope: &str, user: &str, session: &str, since_ts: f64, budget: usize) -> Value {
    let mut releases = crate::traffic::release_lines(store, scope, user, session);
    // a module a teammate is writing that this session imports: once each
    releases.extend(crate::inflight::take_notices(store, scope, session));
    let mut answer = deltas_inner(store, scope, user, session, since_ts, budget);
    if !releases.is_empty() {
        if let Some(map) = answer.as_object_mut() {
            let mut lines: Vec<Value> = map.get("lines").and_then(Value::as_array).cloned().unwrap_or_default();
            lines.extend(releases.into_iter().map(Value::String));
            map.insert("lines".into(), json!(lines));
        }
    }
    answer
}

fn deltas_inner(store: &Store, scope: &str, user: &str, session: &str, since_ts: f64, budget: usize) -> Value {
    let stamp = now();
    if session.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let budget = if budget == 0 { DEFAULT_BUDGET } else { budget.min(MAX_BUDGET) };
    let ws = working_set(store, scope, session);
    let written = written_set(store, scope, session);
    if ws.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let since = if since_ts > 0.0 { since_ts.max(stamp - MAX_LOOKBACK_S) } else { stamp - 120.0 };
    let mut rows = store.ledger_since_kinds(scope, since, &["edit_reported", "rationale"]);
    // the reasons authors gave after the fact, keyed by (path, user, session)
    let reasons: BTreeMap<(String, String, String), String> = rows
        .iter()
        .filter(|row| row.kind == "rationale")
        .map(|row| {
            let p = &row.payload;
            let key = (
                p.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                p.get("user").and_then(Value::as_str).unwrap_or("").to_string(),
                p.get("session").and_then(Value::as_str).unwrap_or("").to_string(),
            );
            (key, p.get("why").and_then(Value::as_str).unwrap_or("").to_string())
        })
        .collect();
    rows.retain(|row| row.kind == "edit_reported" && row.ts > since);
    if rows.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let graph = crate::graphview::snapshot(store, scope);
    // the authors, named the way the reader knows them
    let authors: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.payload.get("user").and_then(Value::as_str).map(str::to_string))
        .filter(|author| author != user)
        .collect();
    let labels = crate::activity::identity_labels(store, user, &authors);
    // latest change per (path, symbol, kind) wins
    let mut seen: BTreeMap<(String, String, String), Value> = BTreeMap::new();
    let mut order: Vec<(String, String, String)> = Vec::new();
    for row in rows {
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        let row_user = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
        let row_session = row.payload.get("session").and_then(Value::as_str).unwrap_or("");
        let same_user = row_user == user;
        if same_user && (row_session.is_empty() || row_session == session) {
            continue; // my own step
        }
        let who = if same_user {
            "another session of yours".to_string()
        } else {
            labels.get(row_user).and_then(|l| l.get("label")).and_then(Value::as_str).unwrap_or(row_user).to_string()
        };
        let events: Vec<Value> = row.payload.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
        let changed: Vec<String> = row
            .payload
            .get("symbols_changed")
            .and_then(Value::as_object)
            .map(|m| m.keys().take(MAX_SYMBOLS_PER_ROW).cloned().collect())
            .unwrap_or_default();
        // relevance: a file this session wrote takes every change; a file it
        // only read, or a dependent of a changed symbol, only an interface change
        let overlaps = written.contains(&path);
        let mut via: Option<String> = None;
        if !overlaps && !ws.contains(&path) {
            'outer: for symbol in &changed {
                for level in crate::graphview::blast_radius(&graph, &format!("{path}::{symbol}"), 1) {
                    for node in level {
                        if let Some(dep) = node.get("path").and_then(Value::as_str) {
                            if ws.contains(dep) {
                                via = Some(dep.to_string());
                                break 'outer;
                            }
                        }
                    }
                }
            }
            if via.is_none() {
                continue;
            }
        }
        let age = age_text(stamp - row.ts);
        // why, when the author said: on the row, or in a rationale row that followed it
        let because = row
            .payload
            .get("why")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|w| !w.is_empty())
            .or_else(|| reasons.get(&(path.clone(), row_user.to_string(), row_session.to_string())).cloned())
            .unwrap_or_default();
        let mut lines_for_row: Vec<(String, String, String, String)> = Vec::new(); // (symbol, kind, text)
        for event in events.iter().take(MAX_EVENTS_PER_ROW) {
            let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let kind = event.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
            let before = event.get("signature_before").and_then(Value::as_str).unwrap_or("");
            let after = event.get("signature_after").and_then(Value::as_str).unwrap_or("");
            if !overlaps && !is_interface_change(&kind, before, after) {
                continue;
            }
            // diffed against another checkout: not a change anyone can rely on
            if kind == "modified" && event.get("confidence").and_then(Value::as_str) == Some(crate::semantics::UNCONFIRMED) {
                continue;
            }
            let new_name = event.get("new_name").and_then(Value::as_str).unwrap_or("");
            // a ledger row keeps kind "modified" for a signature change and
            // carries the new signature alongside; that is the change that matters
            let what = match kind.as_str() {
                "renamed" => format!("renamed → {new_name}"),
                "removed" => "removed".to_string(),
                "added" => if after.is_empty() { "added".to_string() } else { format!("added: {after}") },
                // an unchanged signature is a body edit, not a new interface
                _ if !after.is_empty() && after != before => format!("signature → {after}"),
                _ => "body changed".to_string(),
            };
            lines_for_row.push((symbol, kind, what, String::new()));
        }
        if lines_for_row.is_empty() {
            // a change with no event of its own says only that the file moved:
            // worth a line where the two edits overlap, and only when it names what
            if !overlaps || changed.is_empty() {
                continue;
            }
            lines_for_row.push((String::new(), "changed".to_string(), format!("changed: {}", changed.join(", ")), String::new()));
        }
        for (symbol, kind, what, _) in lines_for_row {
            let key = (path.clone(), symbol.clone(), kind.clone());
            if !seen.contains_key(&key) {
                order.push(key.clone());
            }
            seen.insert(key, json!({"symbol": symbol, "what": what, "who": who, "age": age,
                                    "via": via.clone().unwrap_or_default(), "because": because}));
        }
    }
    // one line per file, newest file first: its changes joined, one tail
    let mut files: Vec<String> = Vec::new();
    let mut by_file: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for key in order.iter().rev() {
        let Some(entry) = seen.get(key) else { continue };
        if !by_file.contains_key(&key.0) {
            files.push(key.0.clone());
        }
        by_file.entry(key.0.clone()).or_default().push(entry.clone());
    }
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    let mut count = 0usize;
    for path in files {
        let entries = &by_file[&path];
        let first = &entries[0];
        let target_of = |e: &Value| {
            let symbol = field(e, "symbol");
            if symbol.is_empty() { path.clone() } else { format!("{path}::{symbol}") }
        };
        let head = if entries.len() == 1 {
            format!("Δ {} {}", target_of(first), field(first, "what"))
        } else {
            let parts: Vec<String> = entries
                .iter()
                .map(|e| {
                    let symbol = field(e, "symbol");
                    if symbol.is_empty() { field(e, "what") } else { format!("{symbol} {}", field(e, "what")) }
                })
                .collect();
            format!("Δ {path}: {}", parts.join("; "))
        };
        let mut who: Vec<String> = Vec::new();
        for e in entries {
            let w = field(e, "who");
            if !who.contains(&w) {
                who.push(w);
            }
        }
        let mut text = format!("{head} — {}, {}", who.join(", "), field(first, "age"));
        if let Some(dep) = entries.iter().map(|e| field(e, "via")).find(|v| !v.is_empty()) {
            text.push_str(&format!(" · you use it from {dep}"));
        }
        if let Some(why) = entries.iter().map(|e| field(e, "because")).find(|v| !v.is_empty()) {
            text.push_str(&format!(" · because: {why}"));
        }
        // their change is in their checkout; whether it can be in yours yet
        if let Some(b) = crate::freshness::behind(store, scope, user, session, &path) {
            text.push_str(if b.pushed {
                " · pushed, not in your copy yet: pull before you edit it"
            } else {
                " · still only in their checkout (not pushed yet)"
            });
        }
        if used + text.len() > budget {
            dropped += entries.len();
            continue;
        }
        used += text.len() + 1;
        count += entries.len();
        lines.push(text);
    }
    let count = count + dropped;
    if lines.is_empty() {
        return json!({"ok": true, "ts": stamp, "lines": [], "count": 0});
    }
    let mut out = vec![format!(
        "Collide deltas since your last step — {count} change(s) by other agents in what you are working on. \
Say so to the user when one changes what you do:"
    )];
    out.extend(lines);
    if dropped > 0 {
        out.push(format!("  … {dropped} more; blast_radius(path, symbol) for any of them"));
    }
    json!({"ok": true, "ts": stamp, "lines": out, "count": count})
}
