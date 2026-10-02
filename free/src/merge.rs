//! Offline imagination: what would happen if this landed?
//!
//! Both answers here are counterfactual. `simulate_merge` asks which symbols
//! would disagree if several workspaces landed together right now, before any
//! real merge exists. `blind_spots` asks the harder question — where is this
//! map unreliable — and answers it about itself.
//!
//! Precision over recall in both. A one-sided add is normal in-flight work,
//! not a conflict, and reporting it as one would teach agents to ignore the
//! output. Only the same symbol, held by two workspaces, at two different
//! hashes, counts.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::compat::python_round;
use crate::semantics;
use crate::store::{now, Store};

pub fn simulate_merge(store: &Store, scope: &str, repo_id: &str, users: &[String]) -> Value {
    let stamp = now();
    let all_users: Vec<String> = store
        .list_workspaces(scope)
        .into_iter()
        .filter_map(|workspace| {
            workspace.get("user").and_then(Value::as_str).map(str::to_string)
        })
        .collect();
    let targets: Vec<String> = if users.is_empty() {
        all_users.clone()
    } else {
        users.iter().filter(|user| all_users.contains(user)).cloned().collect()
    };

    // user -> path -> symbols, from each workspace's freshest servable parse
    let mut tables: BTreeMap<String, BTreeMap<String, serde_json::Map<String, Value>>> =
        BTreeMap::new();
    for user in &targets {
        let mut per_path = BTreeMap::new();
        for (path, record) in store.list_files(scope, user) {
            if let Some(served) = semantics::serve(Some(&record), stamp) {
                per_path.insert(path, served.symbols);
            }
        }
        tables.insert(user.clone(), per_path);
    }

    let paths: BTreeSet<&String> = tables.values().flat_map(|table| table.keys()).collect();
    let mut conflicts: Vec<Value> = Vec::new();
    for path in &paths {
        let holders: Vec<(&String, &serde_json::Map<String, Value>)> = tables
            .iter()
            .filter_map(|(user, table)| table.get(*path).map(|symbols| (user, symbols)))
            .collect();
        if holders.len() < 2 {
            continue; // only one workspace has this file: nothing to disagree about
        }
        let names: BTreeSet<&String> =
            holders.iter().flat_map(|(_user, symbols)| symbols.keys()).collect();
        for symbol in names {
            let versions: Vec<(&String, &Value)> = holders
                .iter()
                .filter_map(|(user, symbols)| symbols.get(symbol).map(|entry| (*user, entry)))
                .collect();
            if versions.len() < 2 {
                continue; // a one-sided add is in-flight work, not a conflict
            }
            let hashes: BTreeSet<&str> = versions
                .iter()
                .map(|(_user, entry)| entry.get("hash").and_then(Value::as_str).unwrap_or(""))
                .collect();
            if hashes.len() < 2 {
                continue;
            }
            let signatures: serde_json::Map<String, Value> = versions
                .iter()
                .map(|(user, entry)| {
                    (
                        (*user).clone(),
                        json!(entry.get("signature").and_then(Value::as_str).unwrap_or("")),
                    )
                })
                .collect();
            let distinct: BTreeSet<&str> =
                signatures.values().filter_map(Value::as_str).collect();
            conflicts.push(json!({
                "path": path,
                "symbol": symbol,
                // a differing signature is a contract break; a differing body
                // at the same signature is only an implementation clash
                "kind": if distinct.len() > 1 { "signature" } else { "implementation" },
                "signatures": signatures,
            }));
        }
    }

    // Counterfactuals over MEMORY, not just code. If these workspaces landed,
    // which anchored notes go stale, and which settled rationales would be
    // re-litigated? The second is the more valuable half: it surfaces a
    // decision before someone unknowingly reverses it.
    let mut notes_affected: Vec<Value> = Vec::new();
    let mut rationales_triggered: Vec<Value> = Vec::new();
    if !conflicts.is_empty() {
        let keys: BTreeSet<(String, String)> = conflicts
            .iter()
            .map(|conflict| {
                (
                    conflict.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                    conflict.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
                )
            })
            .collect();
        let conflict_paths: BTreeSet<String> = keys.iter().map(|(path, _)| path.clone()).collect();

        for (_key, memory) in store.kv_list("memory", &format!("{scope}:")) {
            let Some(anchor) = memory.get("anchor").filter(|a| !a.is_null()) else { continue };
            if crate::compat::truthy(memory.get("superseded_by")) {
                continue;
            }
            let kind = anchor.get("kind").and_then(Value::as_str).unwrap_or("");
            let path = anchor.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let symbol = anchor.get("symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let hit = (kind == "symbol" && keys.contains(&(path.clone(), symbol)))
                || (kind == "file" && conflict_paths.contains(&path));
            if !hit {
                continue;
            }
            let auto = memory.get("auto").and_then(Value::as_str).unwrap_or("");
            let entry = json!({
                "memory_id": memory.get("id").cloned().unwrap_or(Value::Null),
                "fact": memory.get("fact").and_then(Value::as_str).unwrap_or(""),
                "anchor": anchor.clone(),
                "auto": auto,
            });
            if auto == "rationale" {
                rationales_triggered.push(entry);
            } else {
                notes_affected.push(entry);
            }
        }
    }

    json!({
        "repo_id": repo_id,
        "users": targets,
        "paths_compared": paths.len(),
        "conflicts": conflicts,
        "clean": conflicts.is_empty(),
        "notes_affected": notes_affected,
        "rationales_triggered": rationales_triggered,
    })
}

const BLIND_SPOT_CAP: usize = 50;

/// Metacognition: where this map is unreliable.
///
/// Files that never parsed cleanly, files serving state old enough to be
/// wrong, workspaces that have gone quiet. A system that answers confidently
/// everywhere is lying somewhere, and the honest move is to name the places
/// rather than let an agent discover them by being wrong.
pub fn blind_spots(
    store: &Store, scope: &str, repo_id: &str, idle_after_s: f64, hot_ttl_s: f64,
) -> Value {
    let stamp = now();
    let mut never_clean: Vec<Value> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    let mut quiet: Vec<Value> = Vec::new();

    for workspace in store.list_workspaces(scope) {
        let user = workspace.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        let silent_s = stamp - workspace.get("updated").and_then(Value::as_f64).unwrap_or(stamp);
        if silent_s > idle_after_s {
            quiet.push(json!({"user": user, "silent_s": python_round(silent_s, 1)}));
        }
        for (path, record) in store.list_files(scope, &user) {
            let Some(last_clean) = record.get("last_clean").filter(|v| !v.is_null()) else {
                never_clean.push(json!({"user": user, "path": path}));
                continue;
            };
            let ts = last_clean.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            if record.get("status").and_then(Value::as_str) == Some("partial")
                && stamp - ts > hot_ttl_s
            {
                stale.push(json!({
                    "user": user, "path": path,
                    "age_s": python_round(stamp - ts, 1),
                }));
            }
        }
    }

    // Neither listing orders its rows, and both of these are truncated — so
    // unsorted, the same repo reported not just a different order but a
    // different SET of blind spots each call. Sorted, the cap always keeps
    // the same fifty.
    let by_user_path = |entry: &Value| {
        (
            entry.get("user").and_then(Value::as_str).unwrap_or("").to_string(),
            entry.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    };
    never_clean.sort_by_key(by_user_path);
    stale.sort_by_key(by_user_path);
    quiet.sort_by_key(|entry| {
        entry.get("user").and_then(Value::as_str).unwrap_or("").to_string()
    });
    never_clean.truncate(BLIND_SPOT_CAP);
    stale.truncate(BLIND_SPOT_CAP);

    json!({
        "repo_id": repo_id,
        "never_clean": never_clean,
        "stale": stale,
        "quiet_workspaces": quiet,
        "note": "Collide is silent about these; treat its answers there as unreliable.",
    })
}

// ------------------------------------------------------- differential check

/// The narrow differential oracle.
///
/// The question is never "is this code correct" — nothing here could answer
/// that, because the server keeps no source and cannot run a typechecker or a
/// test. The question it CAN answer is narrower and still worth asking: does
/// my diff preserve the assumptions the other workspace's code relies on,
/// judged at the interface level from two trees and the parsed reference
/// graph.
///
/// The verdicts are deliberately unbalanced. FAIL needs a signature that
/// diverges on a symbol someone actually references. Implementation-only
/// divergence is not a failure of THIS oracle and is reported as nothing at
/// all rather than as a pass it did not earn. Everything else — a signature
/// nobody is known to reference, a symbol present on one side only — is
/// UNKNOWN, because reliance outside the parsed fragment is unknowable from
/// here. The response says exactly what was and was not checked and never
/// claims past it.
pub fn differential_check(store: &Store, scope: &str, mine_user: &str, other_user: &str) -> Value {
    let stamp = now();
    let table_of = |user: &str| -> BTreeMap<String, serde_json::Map<String, Value>> {
        let mut per_path = BTreeMap::new();
        for (path, record) in store.list_files(scope, user) {
            if let Some(served) = semantics::serve(Some(&record), stamp) {
                per_path.insert(path, served.symbols);
            }
        }
        per_path
    };
    let mine = table_of(mine_user);
    let theirs = table_of(other_user);
    if theirs.is_empty() {
        return json!({
            "ok": false,
            "error": format!("no workspace state for {}", crate::compat::python_repr(other_user)),
        });
    }

    /// Everything in `table` that names `symbol` in its extracted references.
    /// A symbol never counts as referencing itself.
    fn dependents_of(
        table: &BTreeMap<String, serde_json::Map<String, Value>>, name: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (path, symbols) in table {
            for (symbol, entry) in symbols {
                if symbol == name {
                    continue;
                }
                let refs = entry.get("refs").and_then(Value::as_array);
                let names = refs.map(|refs| {
                    refs.iter().any(|reference| reference.as_str() == Some(name))
                });
                if names.unwrap_or(false) {
                    out.push(format!("{path}::{symbol}"));
                }
            }
        }
        out.sort();
        out
    }

    let empty = serde_json::Map::new();
    let paths: BTreeSet<&String> = mine.keys().chain(theirs.keys()).collect();
    let mut findings: Vec<Value> = Vec::new();

    for path in paths {
        let my_syms = mine.get(path).unwrap_or(&empty);
        let their_syms = theirs.get(path).unwrap_or(&empty);
        let names: BTreeSet<&String> = my_syms.keys().chain(their_syms.keys()).collect();

        for symbol in names {
            let my_version = my_syms.get(symbol);
            let their_version = their_syms.get(symbol);

            let (Some(my_version), Some(their_version)) = (my_version, their_version) else {
                // one side only: a removal this oracle cannot confirm. It is
                // only worth reporting when the OTHER side's code still names
                // it, which is the shape that would actually break.
                let (present, absent) = if my_version.is_none() {
                    ("theirs", "mine")
                } else {
                    ("mine", "theirs")
                };
                let refs = dependents_of(if my_version.is_none() { &theirs } else { &mine }, symbol);
                if !refs.is_empty() {
                    findings.push(json!({
                        "verdict": "UNKNOWN", "path": path, "symbol": symbol,
                        "reason": format!(
                            "present in {present} only; removal unconfirmed, \
                             but referenced by {absent}'s code"),
                        "referenced_by": refs,
                    }));
                }
                continue;
            };

            if my_version.get("hash") == their_version.get("hash") {
                continue;
            }
            let sig_mine = my_version.get("signature").and_then(Value::as_str).unwrap_or("");
            let sig_theirs = their_version.get("signature").and_then(Value::as_str).unwrap_or("");
            if sig_mine == sig_theirs {
                continue; // implementation-only: this oracle has no claim
            }

            let their_deps = dependents_of(&theirs, symbol);
            let my_deps = dependents_of(&mine, symbol);
            if !their_deps.is_empty() || !my_deps.is_empty() {
                findings.push(json!({
                    "verdict": "FAIL", "path": path, "symbol": symbol,
                    "reason": "signature diverges on a symbol the other side's code references",
                    "signature_mine": sig_mine, "signature_theirs": sig_theirs,
                    "referenced_by_theirs": their_deps, "referenced_by_mine": my_deps,
                }));
            } else {
                findings.push(json!({
                    "verdict": "UNKNOWN", "path": path, "symbol": symbol,
                    "reason": "signature diverges but no parsed code references it; \
                               reliance outside the fragment is unknowable here",
                    "signature_mine": sig_mine, "signature_theirs": sig_theirs,
                }));
            }
        }
    }

    let verdicts: BTreeSet<&str> =
        findings.iter().filter_map(|f| f.get("verdict").and_then(Value::as_str)).collect();
    let result = if verdicts.contains("FAIL") {
        "FAIL"
    } else if verdicts.contains("UNKNOWN") {
        "UNKNOWN"
    } else {
        "PASS"
    };

    json!({
        "ok": true,
        "result": result,
        "oracle": "interface",
        "checked": "signature preservation of symbols referenced by the other workspace's parsed code",
        "not_checked": ["behavior", "types beyond signatures", "tests",
                        "references from unparsed or unreported files"],
        "users": [mine_user, other_user],
        "findings": findings,
    })
}
