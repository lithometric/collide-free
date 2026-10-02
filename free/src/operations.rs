//! The typed-operation algebra.
//!
//! An intent that says "rename compute_tax to calculate_tax" can be compared
//! with another intent mechanically. An intent that says "refactoring the
//! billing code" cannot be compared with anything. So the operation set is
//! CLOSED: there is no `refactor` bucket, and an op outside the set is
//! rejected rather than accepted as prose.
//!
//! Every verdict here is provable inside the fragment. Where it is not — an
//! `extract`, whose range semantics are not modelled, or a legacy prose
//! intent — the answer is UNKNOWN. Never a guess: a predicted conflict that
//! turns out to be wrong teaches agents to ignore the prediction.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

pub const COMMUTE: &str = "COMMUTE";
pub const CONFLICT: &str = "CONFLICT";
pub const UNKNOWN: &str = "UNKNOWN";

/// op -> (required operands, optional operands). Closed by design.
const OPERATIONS: &[(&str, &[&str], &[&str])] = &[
    ("rename", &["symbol", "new_name"], &[]),
    ("add_param", &["symbol", "name"], &["type", "default"]),
    ("remove_param", &["symbol", "name"], &[]),
    ("change_return", &["symbol", "type"], &[]),
    ("extract", &["symbol", "new_name"], &["range"]),
    ("move", &["symbol", "new_path"], &[]),
    ("delete", &["symbol"], &[]),
    ("add", &["symbol"], &["signature"]),
    // a body edit with the interface preserved — the most common change of
    // all. Without it, agents fall back to untyped prose and poison the table.
    ("modify", &["symbol"], &[]),
];

fn schema(op: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    OPERATIONS.iter().find(|(name, _, _)| *name == op).map(|(_, req, opt)| (*req, *opt))
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Normalise and validate a typed operation list.
pub fn validate_operations(operations: &Value) -> Result<Vec<Value>, String> {
    let Some(items) = operations.as_array().filter(|items| !items.is_empty()) else {
        return Err("operations must be a non-empty list".into());
    };
    let known: Vec<&str> = {
        let mut names: Vec<&str> = OPERATIONS.iter().map(|(name, _, _)| *name).collect();
        names.sort_unstable();
        names
    };
    let mut normalized = Vec::with_capacity(items.len());
    for (index, raw) in items.iter().enumerate() {
        if !raw.is_object() {
            return Err(format!("operations[{index}] must be an object"));
        }
        let op_name = text(raw, "op");
        let Some((required, optional)) = schema(&op_name) else {
            return Err(format!(
                "operations[{index}].op '{op_name}' is not in the closed set {}; \
there is no 'refactor' bucket",
                crate::compat::python_list(&known)
            ));
        };
        let mut op = Map::new();
        op.insert("op".into(), json!(op_name));
        for field in required {
            let value = text(raw, field);
            if value.is_empty() {
                return Err(format!("operations[{index}] ({op_name}) requires '{field}'"));
            }
            op.insert((*field).to_string(), json!(value));
        }
        for field in optional {
            if raw.get(*field).map(|v| !v.is_null()).unwrap_or(false) {
                op.insert((*field).to_string(), json!(text(raw, field)));
            }
        }
        normalized.push(Value::Object(op));
    }
    Ok(normalized)
}

/// The symbol names an operation claims: its target plus any name it
/// introduces.
pub fn names_touched(op: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let symbol = text(op, "symbol");
    if !symbol.is_empty() {
        names.insert(symbol);
    }
    let kind = text(op, "op");
    if kind == "rename" || kind == "extract" {
        let new_name = text(op, "new_name");
        if !new_name.is_empty() {
            names.insert(new_name);
        }
    }
    names
}

/// The commutativity table.
pub fn commutes(a: &Value, b: &Value) -> &'static str {
    if schema(&text(a, "op")).is_none() || schema(&text(b, "op")).is_none() {
        return UNKNOWN;
    }
    if names_touched(a).intersection(&names_touched(b)).next().is_none() {
        return COMMUTE; // disjoint names: interface-independent
    }
    if text(a, "op") == "extract" || text(b, "op") == "extract" {
        return UNKNOWN; // range semantics are not modelled
    }
    CONFLICT
}

/// Table-lookup collision prediction between my typed operations and every
/// other active intent. Legacy prose intents surface as UNKNOWN when their
/// declared symbols overlap mine — visible, but not vouched for.
pub fn intent_conflicts(my_ops: &[Value], others: &[Value]) -> Vec<Value> {
    let mut findings = Vec::new();
    let mut my_names: BTreeSet<String> = BTreeSet::new();
    for op in my_ops {
        my_names.extend(names_touched(op));
    }
    for intent in others {
        let their_ops: Vec<Value> =
            intent.get("operations").and_then(Value::as_array).cloned().unwrap_or_default();
        if !their_ops.is_empty() {
            for mine in my_ops {
                for theirs in &their_ops {
                    let verdict = commutes(mine, theirs);
                    if verdict != COMMUTE {
                        findings.push(json!({
                            "intent_id": text(intent, "intent_id"),
                            "owner": text(intent, "owner"),
                            "mine": mine, "theirs": theirs, "result": verdict,
                        }));
                    }
                }
            }
            continue;
        }
        let their_symbols: BTreeSet<String> = intent
            .get("symbols")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let overlap: Vec<&String> = my_names.intersection(&their_symbols).collect();
        if !overlap.is_empty() {
            findings.push(json!({
                "intent_id": text(intent, "intent_id"),
                "owner": text(intent, "owner"),
                "mine": overlap,
                "theirs": text(intent, "change_type"),
                "result": UNKNOWN,
                "why": "legacy prose intent; nothing provable about it",
            }));
        }
    }
    findings
}

/// The operation that would undo this one, where one exists in the fragment.
/// Param and return edits carry no old value, so they have no clean inverse.
fn inverse_of(op: &Value) -> Option<Value> {
    match text(op, "op").as_str() {
        "rename" => Some(json!({
            "op": "rename", "symbol": text(op, "new_name"), "new_name": text(op, "symbol"),
        })),
        "delete" => Some(json!({"op": "add", "symbol": text(op, "symbol")})),
        "add" => Some(json!({"op": "delete", "symbol": text(op, "symbol")})),
        // path-insensitive: any move back re-litigates the placement
        "move" => Some(json!({"op": "move", "symbol": text(op, "symbol"), "new_path": "?"})),
        _ => None,
    }
}

/// Does `candidate` undo `settled`?
pub fn reverses(candidate: &Value, settled: &Value) -> bool {
    let Some(inverse) = inverse_of(settled) else { return false };
    if text(candidate, "op") != text(&inverse, "op") {
        return false;
    }
    if text(candidate, "op") == "move" {
        return text(candidate, "symbol") == text(&inverse, "symbol");
    }
    inverse
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(_, value)| value.as_str() != Some("?"))
                .all(|(key, value)| candidate.get(key) == Some(value))
        })
        .unwrap_or(false)
}
