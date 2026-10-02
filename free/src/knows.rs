//! What a player's agents already KNOW of their teammates' recent changes.
//!
//! Every delta-carrying call (`since_your_last_call`) moves a cursor per
//! scope and person (`deltacursor:{scope}:{user}` — or, when the cursor is
//! kept per agent, `deltacursor:{scope}:{user}#{session}`). Read against the
//! ledger's last day of change events by OTHER people, that cursor says how
//! current the person's agents are: `seen` of `total`, and the `unseen` rows
//! still waiting for their next call. The dashboard's members panel shows
//! it beside each player. Python's `knows_*` in service.py, row for row.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::store::{now, LedgerRow, Store};

/// The window `total` counts over.
pub const WINDOW_S: f64 = 86_400.0;
/// Hard bound on ledger rows walked, the same as insights'.
const MAX_ROWS: usize = 4000;
const MAX_UNSEEN: usize = 50;
/// The ledger kinds that are a CHANGE a teammate's agent should hear about:
/// reported edits, symbol events (renames and removals) and declared intents.
const KINDS: [&str; 3] = ["edit_reported", "symbol_event", "intent_declared"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// One ledger row as a change event: `ts`, `by` (the identity
/// `recent_events[].user` carries), `kind`, `path` and `symbols` when the
/// row names any. `None` for a row that is not a change.
fn event_of(row: &LedgerRow) -> Option<Value> {
    if !KINDS.contains(&row.kind.as_str()) {
        return None;
    }
    let payload = &row.payload;
    let mut by = text(payload, "user");
    if by.is_empty() {
        by = text(payload, "owner");
    }
    if by.is_empty() {
        return None;
    }
    let mut path = text(payload, "path");
    if path.is_empty() {
        let paths: Vec<String> = payload
            .get("paths")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        path = paths.join(", ");
    }
    let mut symbols: BTreeSet<String> = BTreeSet::new();
    match row.kind.as_str() {
        "edit_reported" => {
            if let Some(changed) = payload.get("symbols_changed").and_then(Value::as_object) {
                symbols.extend(changed.keys().cloned());
            }
            for event in payload.get("events").and_then(Value::as_array).into_iter().flatten() {
                let symbol = text(event, "symbol");
                if !symbol.is_empty() {
                    symbols.insert(symbol);
                }
            }
        }
        "intent_declared" => {
            for symbol in payload.get("symbols").and_then(Value::as_array).into_iter().flatten() {
                if let Some(name) = symbol.as_str() {
                    if !name.is_empty() {
                        symbols.insert(name.to_string());
                    }
                }
            }
        }
        _ => {
            let symbol = payload.get("event").map(|event| text(event, "symbol")).unwrap_or_default();
            if !symbol.is_empty() {
                symbols.insert(symbol);
            }
        }
    }
    let mut out = json!({"ts": row.ts, "by": by, "kind": row.kind, "path": path});
    if !symbols.is_empty() {
        out["symbols"] = json!(symbols.into_iter().collect::<Vec<String>>());
    }
    Some(out)
}

/// The scope's change events of the last [`WINDOW_S`], by anyone, newest
/// first — from the ledger (bounded the way insights walks it), never the
/// volatile recent ring.
pub fn events(store: &Store, scope: &str) -> Vec<Value> {
    let mut rows = store.ledger_since(scope, now() - WINDOW_S);
    if rows.len() > MAX_ROWS {
        rows = rows.split_off(rows.len() - MAX_ROWS);
    }
    rows.sort_by(|a, b| {
        b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal).then(b.seq.cmp(&a.seq))
    });
    rows.iter().filter_map(event_of).collect()
}

/// This person's latest delivery cursor in the scope — the newest of their
/// per-agent delta cursors — or `None` when none of their agents has ever
/// been handed `since_your_last_call` here.
pub fn cursor(store: &Store, scope: &str, user_id: &str) -> Option<f64> {
    if user_id.is_empty() {
        return None;
    }
    let exact = format!("deltacursor:{scope}:{user_id}");
    let per_agent = format!("{exact}#");
    let mut best: Option<f64> = None;
    for (key, value) in store.eph_scan(&exact) {
        if key != exact && !key.starts_with(&per_agent) {
            continue;
        }
        if let Some(ts) = value.get("ts").and_then(Value::as_f64) {
            if best.map_or(true, |b| ts > b) {
                best = Some(ts);
            }
        }
    }
    best
}

/// `seen`/`total` over the events by people other than `own` (the member's
/// own identities: email and uid), with the unseen rows newest first, capped.
pub fn of(events: &[Value], own: &[String], cursor: Option<f64>) -> Value {
    let others: Vec<&Value> = events.iter().filter(|event| !own.contains(&text(event, "by"))).collect();
    let ts_of = |event: &Value| event.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let seen = others.iter().filter(|event| cursor.is_some_and(|at| ts_of(event) <= at)).count();
    let unseen: Vec<Value> = others
        .iter()
        .filter(|event| cursor.map_or(true, |at| ts_of(event) > at))
        .take(MAX_UNSEEN)
        .map(|event| (*event).clone())
        .collect();
    json!({
        "seen": seen,
        "total": others.len(),
        "window_s": WINDOW_S as i64,
        "cursor_ts": cursor,
        "unseen": unseen,
    })
}
