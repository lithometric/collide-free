//! Replay: what was known at a moment, rebuilt from the ledger.
//!
//! Git stores results; the ledger stores beliefs. For any past instant this
//! reconstructs what an agent was looking at: which symbols existed and with
//! which hashes, which intents were open, which reasons had been written,
//! which notes existed and whether the ground under each had already moved.
//! When something breaks, the state a call was made from says whether the
//! information was wrong or the judgement was, which is what outcome scoring
//! feeds on. Python's `insights.replay`, same shape, same order.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::Store;

/// How far back a replay looks for the rows that built the state.
pub const REPLAY_WINDOW_S: f64 = 30.0 * 86400.0;
const MAX_RATIONALES: usize = 30;
const MAX_RECENT: usize = 20;
const MAX_NOTES: usize = 50;
/// An intent with no end row is treated as open for this long after it was declared.
const INTENT_LIFE_S: f64 = 7200.0;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

pub fn replay(store: &Store, scope: &str, at: f64) -> Value {
    let since = at - REPLAY_WINDOW_S;
    let mut rows = store.ledger_since(scope, since);
    rows.retain(|row| row.ts <= at);
    // the symbol table as it stood: per path, the hash each symbol last had
    let mut symbols: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut opened: BTreeMap<String, Value> = BTreeMap::new();
    let mut closed: BTreeSet<String> = BTreeSet::new();
    let mut rationales: Vec<Value> = Vec::new();
    let (mut checks, mut collisions, mut blocks) = (0i64, 0i64, 0i64);
    for row in &rows {
        let p = &row.payload;
        match row.kind.as_str() {
            "edit_reported" => {
                let path = text(p, "path");
                if path.is_empty() {
                    continue;
                }
                let table = symbols.entry(path.clone()).or_default();
                // a row records {before, after} per symbol; the hash that stood
                // after the write is the belief, and no `after` is a removal
                if let Some(changed) = p.get("symbols_changed").and_then(Value::as_object) {
                    for (symbol, hash) in changed {
                        let after = hash
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| hash.get("after").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_default();
                        if after.is_empty() {
                            table.remove(symbol);
                        } else {
                            table.insert(symbol.clone(), after);
                        }
                    }
                }
                for removed in p.get("symbols_removed").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    table.remove(removed);
                }
                let why = text(p, "why");
                if !why.is_empty() {
                    rationales.push(json!({"ts": row.ts, "user": text(p, "user"), "path": path, "why": why}));
                }
            }
            "rationale" => rationales.push(json!({"ts": row.ts, "user": text(p, "user"), "path": text(p, "path"), "why": text(p, "why")})),
            "intent_declared" => {
                let id = text(p, "intent_id");
                if !id.is_empty() {
                    opened.insert(id.clone(), json!({
                        "intent_id": id, "owner": text(p, "owner"), "session": text(p, "session"),
                        "paths": p.get("paths").cloned().unwrap_or(json!([])), "symbols": p.get("symbols").cloned().unwrap_or(json!([])),
                        "summary": text(p, "summary"), "declared_at": row.ts,
                    }));
                }
            }
            "intent_completed" | "intent_abandoned" => {
                closed.insert(text(p, "intent_id"));
            }
            "check_performed" => checks += 1,
            "collision_returned" => collisions += 1,
            "gate_block" => blocks += 1,
            _ => {}
        }
    }
    let open_intents: Vec<Value> = opened
        .into_values()
        .filter(|i| !closed.contains(&text(i, "intent_id")))
        .filter(|i| at - i.get("declared_at").and_then(Value::as_f64).unwrap_or(at) <= INTENT_LIFE_S)
        .collect();
    rationales.reverse();
    rationales.truncate(MAX_RATIONALES);
    // the notes that existed, and whether the ground under each had moved
    let mut notes: Vec<Value> = store
        .kv_list("memory", &format!("{scope}:"))
        .into_iter()
        .map(|(_k, m)| m)
        .filter(|m| m.get("created").and_then(Value::as_f64).unwrap_or(f64::MAX) <= at)
        .map(|m| {
            let anchor = m.get("anchor").cloned().unwrap_or(Value::Null);
            let path = text(&anchor, "path");
            let symbol = text(&anchor, "symbol");
            let anchored_hash = text(&m, "anchor_hash");
            let hash_at = if !path.is_empty() && !symbol.is_empty() {
                symbols.get(&path).and_then(|t| t.get(&symbol)).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            let drifted = !anchored_hash.is_empty() && !hash_at.is_empty() && anchored_hash != hash_at;
            json!({"id": text(&m, "id"), "fact": text(&m, "fact"), "owner": text(&m, "owner"), "created": m.get("created").cloned().unwrap_or(Value::Null),
                   "path": path, "symbol": symbol, "anchored_hash": anchored_hash, "hash_at": hash_at, "drifted": drifted})
        })
        .collect();
    notes.sort_by(|a, b| {
        b.get("created").and_then(Value::as_f64).unwrap_or(0.0)
            .partial_cmp(&a.get("created").and_then(Value::as_f64).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| text(a, "id").cmp(&text(b, "id")))
    });
    notes.truncate(MAX_NOTES);
    let recent: Vec<Value> = rows
        .iter()
        .rev()
        .take(MAX_RECENT)
        .map(|row| json!({"seq": row.seq, "ts": row.ts, "kind": row.kind, "user": text(&row.payload, "user"),
                          "path": text(&row.payload, "path"), "summary": text(&row.payload, "summary")}))
        .collect();
    let symbol_count: usize = symbols.values().map(|t| t.len()).sum();
    let files: Vec<Value> = symbols
        .iter()
        .map(|(path, table)| json!({"path": path, "symbols": table.len(), "hashes": table}))
        .collect();
    json!({
        "at": at, "window_s": REPLAY_WINDOW_S, "rows": rows.len(),
        "files": files.len(), "symbols": symbol_count, "symbol_table": files,
        "open_intents": open_intents, "rationales": rationales, "notes": notes,
        "checks": checks, "collisions": collisions, "blocks": blocks, "recent": recent,
        "how": "What was known at this moment, rebuilt from the ledger: the symbols that existed and their hashes, the intents open, the reasons written, the notes and whether the ground under each had already moved. Git stores results; this stores beliefs.",
    })
}
