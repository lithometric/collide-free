//! The intent lifecycle: declare, heartbeat, complete, defer.
//!
//! An intent is a lease on the code an agent is about to change. It is the
//! only part of the protocol that speaks about the FUTURE, which is what lets
//! two agents avoid a collision rather than discover one — and why a typed
//! operation is worth so much more than prose here. "rename compute_tax to
//! calculate_tax" can be compared with another intent by table lookup;
//! "refactoring billing" can be compared with nothing.
//!
//! Leases expire on their own. An agent that dies mid-task releases its claim
//! without anyone cleaning up after it, which is the only design that survives
//! unattended runs.

use serde_json::{json, Map, Value};

use crate::compat::python_list;
use crate::operations;
use crate::store::{now, Store};

/// Prose change types, kept only as legacy input. `refactor` lives here and
/// deliberately NOT in the typed operation set: as prose it is merely
/// unhelpful, but as an operation it would be a bucket nothing could be
/// compared against, which poisons the commutativity table.
const CHANGE_TYPES: [&str; 6] =
    ["rename", "signature", "move", "delete", "add", "refactor"];
const MAX_REF: usize = 120;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}


pub struct DeclareInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub repo_id: &'a str,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub change_type: &'a str,
    pub before: &'a str,
    pub after: &'a str,
    pub summary: &'a str,
    pub agent: &'a str,
    pub session: &'a str,
    pub reference: &'a str,
    pub operations: Option<Value>,
    pub idempotency_key: &'a str,
    pub ttl_s: f64,
    pub idle_after_s: f64,
    pub via: &'a str,
}

pub fn declare(store: &Store, input: &DeclareInput) -> Value {
    let mut symbols = input.symbols.clone();
    let mut change_type = input.change_type.to_string();
    let mut typed_ops: Vec<Value> = Vec::new();

    if let Some(raw) = input.operations.as_ref().filter(|v| !v.is_null()) {
        match operations::validate_operations(raw) {
            Err(problem) => return json!({"ok": false, "error": problem}),
            Ok(ops) => {
                // the operations define the symbol set: derived, not asserted,
                // so an agent cannot claim a lease wider than what it declared
                let mut names: std::collections::BTreeSet<String> =
                    symbols.iter().cloned().collect();
                for op in &ops {
                    names.extend(operations::names_touched(op));
                }
                symbols = names.into_iter().collect();
                change_type = text(&ops[0], "op");
                typed_ops = ops;
            }
        }
    } else if !CHANGE_TYPES.contains(&change_type.as_str()) {
        let mut known = CHANGE_TYPES;
        known.sort_unstable();
        return json!({
            "ok": false,
            "error": format!(
                "change_type must be one of {}, or pass typed `operations` (preferred)",
                python_list(&known)),
        });
    }

    if !input.idempotency_key.is_empty() {
        let key = format!(
            "idem:{}:{}:declare:{}", input.scope, input.user_id, input.idempotency_key);
        if let Some(existing) = store.eph_get(&key) {
            let mut response = existing;
            if let Some(map) = response.as_object_mut() {
                map.insert("deduplicated".into(), json!(true));
            }
            return response;
        }
    }

    let stamp = now();
    let id = crate::compat::new_id();
    let reference: String = input.reference.trim().chars().take(MAX_REF).collect();

    let mut intent = Map::new();
    intent.insert("intent_id".into(), json!(id));
    intent.insert("scope".into(), json!(input.scope));
    intent.insert("owner".into(), json!(input.user_id));
    // the agent, not just the person: two sessions of one user are two claims
    intent.insert("session".into(), json!(input.session));
    intent.insert("repo_id".into(), json!(input.repo_id));
    intent.insert("paths".into(), json!(input.paths));
    intent.insert("symbols".into(), json!(symbols));
    intent.insert("change_type".into(), json!(change_type));
    intent.insert("before".into(), json!(input.before));
    intent.insert("after".into(), json!(input.after));
    intent.insert("summary".into(), json!(input.summary));
    intent.insert("created".into(), json!(stamp));
    intent.insert("expires_at".into(), json!(stamp + input.ttl_s));
    intent.insert("status".into(), json!("active"));
    intent.insert("agent".into(), json!(input.agent));
    if !typed_ops.is_empty() {
        intent.insert("operations".into(), json!(typed_ops));
    }
    if !reference.is_empty() {
        intent.insert("ref".into(), json!(reference));
    }
    let intent = Value::Object(intent);

    let _ = store.eph_set(
        &format!("intent:{}:{id}", input.scope), &intent, Some(input.ttl_s));

    let mut row = Map::new();
    row.insert("intent_id".into(), json!(id));
    row.insert("owner".into(), json!(input.user_id));
    row.insert("session".into(), json!(input.session));
    row.insert("agent".into(), json!(input.agent));
    row.insert("paths".into(), json!(input.paths));
    row.insert("symbols".into(), intent.get("symbols").cloned().unwrap_or(json!([])));
    row.insert("change_type".into(), json!(change_type));
    row.insert("before".into(), json!(input.before));
    row.insert("after".into(), json!(input.after));
    row.insert("summary".into(), json!(input.summary));
    if !typed_ops.is_empty() {
        row.insert("operations".into(), json!(typed_ops));
    }
    if !reference.is_empty() {
        row.insert("ref".into(), json!(reference));
    }
    let _ = store.ledger_append(input.scope, "intent_declared", &Value::Object(row), stamp);
    crate::events::publish(
        store,
        input.scope,
        json!({"kind": "intent", "action": "declared", "intent": intent}),
        input.via,
    );

    let mut response = json!({"ok": true, "intent_id": id, "ttl_s": input.ttl_s});
    if !typed_ops.is_empty() {
        let others: Vec<Value> =
            crate::collisions::active_intents(store, input.scope, input.idle_after_s)
                .into_iter()
                .filter(|other| {
                    text(other, "intent_id") != id && text(other, "owner") != input.user_id
                })
                .collect();
        let predicted = operations::intent_conflicts(&typed_ops, &others);
        if !predicted.is_empty() {
            if let Some(map) = response.as_object_mut() {
                map.insert("intent_conflicts".into(), json!(predicted));
            }
        }

        // Re-litigation. Would this intent reverse one that was already
        // settled? Then the recorded rationale comes back as a blocking
        // notice — "this was decided, and here is why" — rather than sitting
        // in an index nobody queries.
        let mut settled_hits: Vec<Value> = Vec::new();
        for (_key, settled) in store.kv_list("settled", &format!("{}:", input.scope)) {
            let prior_ops =
                settled.get("operations").and_then(Value::as_array).cloned().unwrap_or_default();
            let reversals: Vec<Value> = typed_ops
                .iter()
                .flat_map(|mine| {
                    prior_ops
                        .iter()
                        .filter(|prior| operations::reverses(mine, prior))
                        .map(move |prior| json!({"mine": mine, "settled_op": prior}))
                })
                .collect();
            if !reversals.is_empty() {
                settled_hits.push(json!({
                    "intent_id": text(&settled, "intent_id"),
                    "owner": text(&settled, "owner"),
                    "rationale": text(&settled, "rationale"),
                    "settled_at": settled.get("ts").cloned().unwrap_or(json!(0)),
                    "reversals": reversals,
                    "level": "blocking",
                    "notice": "this was settled; reversing it re-litigates a decision",
                }));
            }
        }
        if !settled_hits.is_empty() {
            if let Some(map) = response.as_object_mut() {
                map.insert("settled".into(), json!(settled_hits));
            }
        }
    }

    if !input.idempotency_key.is_empty() {
        let key = format!(
            "idem:{}:{}:declare:{}", input.scope, input.user_id, input.idempotency_key);
        let _ = store.eph_set(&key, &response, Some(3600.0));
    }
    crate::events::with_deltas(store, input.scope, input.user_id, response)
}

// ---------------------------------------------------------------- heartbeat

/// Renew a lease. The one call an agent makes to say "still working on it".
///
/// Ownership is checked rather than assumed: a lease belongs to whoever
/// declared it, and letting anyone else push its expiry out would turn the
/// TTL from a guarantee into a suggestion.
pub fn heartbeat(store: &Store, scope: &str, user_id: &str, intent_id: &str, ttl_s: f64) -> Value {
    let key = format!("intent:{scope}:{intent_id}");
    let Some(mut intent) = store.eph_get(&key) else {
        return json!({"ok": false, "error": "intent not found or expired; re-declare"});
    };
    if text(&intent, "owner") != user_id {
        return json!({"ok": false, "error": "intent not found or expired; re-declare"});
    }
    if let Some(map) = intent.as_object_mut() {
        map.insert("expires_at".into(), json!(now() + ttl_s));
    }
    let _ = store.eph_set(&key, &intent, Some(ttl_s));
    json!({"ok": true, "intent_id": intent_id, "ttl_s": ttl_s})
}

// ----------------------------------------------------------------- complete

const MAX_RATIONALE: usize = 2000;
/// One anchored note per symbol, and only for the first path: a rationale is
/// a decision, not a broadcast, and fanning it across every touched file
/// would bury the code in copies of itself.
const RATIONALE_PATHS: usize = 1;
const RATIONALE_SYMBOLS: usize = 5;

pub struct CompleteInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub intent_id: &'a str,
    pub session: &'a str,
    pub rationale: &'a str,
    pub via: &'a str,
}

/// Release a lease, and — if a rationale is given — turn it into memory that
/// fires on re-litigation.
///
/// The rationale is the valuable half. It is stored as a settled record keyed
/// by scope and intent, which `declare` reads to decide whether an incoming
/// intent would REVERSE a decision already made; and it is anchored to each
/// changed symbol so locality surfaces it to whoever touches that code next.
/// Memory that triggers on the relevant edit, rather than memory that waits
/// in an index for someone to think to search it.
pub fn complete(store: &Store, input: &CompleteInput) -> Value {
    let key = format!("intent:{}:{}", input.scope, input.intent_id);
    let Some(intent) = store.eph_get(&key) else {
        return json!({"ok": false, "error": "intent not found or expired"});
    };
    if text(&intent, "owner") != input.user_id {
        return json!({"ok": false, "error": "intent not found or expired"});
    }
    store.eph_delete(&key);

    let rationale: String = input.rationale.trim().chars().take(MAX_RATIONALE).collect();
    let stamp = now();
    let operations = intent.get("operations").cloned().unwrap_or_else(|| json!([]));
    let symbols: Vec<String> = intent
        .get("symbols")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let paths: Vec<String> = intent
        .get("paths")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    if !rationale.is_empty() {
        let settled = json!({
            "intent_id": input.intent_id,
            "owner": input.user_id,
            "rationale": rationale,
            "operations": operations,
            "symbols": symbols,
            "paths": paths,
            "summary": text(&intent, "summary"),
            "ts": stamp,
        });
        let _ = store.kv_put(
            "settled", &format!("{}:{}", input.scope, input.intent_id), &settled, stamp);

        let anchor_paths: Vec<&String> =
            if paths.is_empty() { Vec::new() } else { paths.iter().take(RATIONALE_PATHS).collect() };
        for path in anchor_paths {
            for symbol in symbols.iter().take(RATIONALE_SYMBOLS) {
                if path.is_empty() || symbol.is_empty() {
                    continue;
                }
                crate::memory::save(
                    store,
                    &crate::memory::SaveInput {
                        scope: input.scope,
                        user_id: input.user_id,
                        fact: &format!("[settled {}] {rationale}", input.intent_id),
                        tags: &["rationale"],
                        agent: &text(&intent, "agent"),
                        anchor: &format!("{path}::{symbol}"),
                        auto: "rationale",
                        supersedes: "",
                    },
                );
            }
        }
    }

    let mut row = Map::new();
    row.insert("intent_id".into(), json!(input.intent_id));
    row.insert("owner".into(), json!(input.user_id));
    row.insert("session".into(), json!(input.session));
    row.insert("outcome".into(), json!("completed"));
    if !rationale.is_empty() {
        row.insert("rationale".into(), json!(rationale));
    }
    if operations.as_array().map(|ops| !ops.is_empty()).unwrap_or(false) {
        row.insert("operations".into(), operations);
    }
    let _ = store.ledger_append(input.scope, "intent_completed", &Value::Object(row), stamp);
    crate::events::publish(
        store,
        input.scope,
        json!({
            "kind": "intent", "action": "completed",
            "intent": {"intent_id": input.intent_id, "owner": input.user_id},
        }),
        input.via,
    );
    // a claim ended: every held group in the scope is checked again, and
    // the ones that came free are released to their planners
    crate::traffic::release_after(store, input.scope, input.user_id, input.session);
    json!({"ok": true, "intent_id": input.intent_id})
}

// -------------------------------------------------------------------- defer

const MAX_NOTE: usize = 500;

/// Prospective memory: a dormant note that fires when a FUTURE edit touches
/// the given paths or symbols.
///
/// This is the shape of handoff that survives the agent that wrote it. An
/// intent expires because a lease should; a tripwire persists for days
/// because "the Stripe migration here is half-done" stays true long after
/// whoever noticed it has stopped running. `supervise::fire_tripwires`
/// matches them on every report.
pub fn defer(
    store: &Store, scope: &str, user_id: &str, agent: &str,
    paths: &[String], symbols: &[String], note: &str, expires_days: f64,
) -> Value {
    let note: String = note.trim().chars().take(MAX_NOTE).collect();
    if note.is_empty() || (paths.is_empty() && symbols.is_empty()) {
        return json!({"ok": false, "error": "note plus at least one path or symbol required"});
    }
    let stamp = now();
    let id = crate::compat::new_id();
    let expires = stamp + expires_days.max(0.01) * 86_400.0;
    let tripwire = json!({
        "id": id,
        "owner": user_id,
        "agent": agent,
        "note": note,
        "paths": paths,
        "symbols": symbols,
        "created": stamp,
        "expires": expires,
        "fired": Value::Null,
    });
    let _ = store.kv_put("tripwire", &format!("{scope}:{id}"), &tripwire, stamp);
    let _ = store.ledger_append(
        scope, "tripwire_set",
        &json!({"tripwire_id": id, "owner": user_id, "agent": agent, "note": note}),
        stamp,
    );
    json!({"ok": true, "tripwire_id": id, "expires": expires})
}

/// How long a turn's summary may run in a note: the gist, not the essay.
const TURN_NOTE_CHARS: usize = 400;

/// The note a finished turn leaves, with no tool call: the hooks send the
/// agent's closing message when its turn ends, and if that turn changed
/// code it becomes one note, anchored at the first symbol the turn
/// changed and naming the rest. This is what the `[settled]` notes did
/// while agents declared intents; the hooks stopped them declaring, and the
/// notes stopped with them. One note per turn, never one per symbol.
pub fn settle_turn(store: &Store, scope: &str, user_id: &str, session: &str, summary: &str, agent: &str) -> Value {
    let stamp = now();
    let key = format!("settledturn:{scope}:{session}");
    let since = store
        .eph_get(&key)
        .and_then(|v| v.get("ts").and_then(Value::as_f64))
        .unwrap_or(stamp - 6.0 * 3600.0);
    let _ = store.eph_set(&key, &json!({"ts": stamp}), Some(86_400.0));
    let gist = turn_gist(summary);
    if session.is_empty() || gist.chars().count() < 40 {
        return json!({"ok": true, "saved": 0});
    }
    let mut changed: Vec<(String, String)> = Vec::new();
    for row in store.ledger_since_kinds(scope, since, &["edit_reported"]) {
        let payload = &row.payload;
        if text(payload, "session") != session || text(payload, "user") != user_id {
            continue;
        }
        let path = text(payload, "path");
        let mut names: Vec<String> = payload
            .get("symbols_changed")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for event in payload.get("events").and_then(Value::as_array).into_iter().flatten() {
            names.push(text(event, "symbol"));
        }
        for name in names.into_iter().filter(|n| !n.is_empty()) {
            if !changed.iter().any(|(p, n)| *p == path && *n == name) {
                changed.push((path.clone(), name));
            }
        }
    }
    let Some((path, symbol)) = changed.first().cloned() else {
        return json!({"ok": true, "saved": 0});
    };
    let others: Vec<String> = changed.iter().skip(1).take(6).map(|(_, n)| n.clone()).collect();
    let fact = if others.is_empty() { gist } else { format!("{gist} (also changed: {})", others.join(", ")) };
    let saved = crate::memory::save(
        store,
        &crate::memory::SaveInput {
            scope,
            user_id,
            fact: &fact,
            tags: &["rationale", "turn"],
            agent,
            anchor: &format!("{path}::{symbol}"),
            auto: "rationale",
            supersedes: "",
        },
    );
    json!({"ok": true, "saved": 1, "note": saved.get("memory_id").cloned().unwrap_or(Value::Null)})
}

/// The first paragraph of a closing message, cut at a sentence end near
/// TURN_NOTE_CHARS, with markdown emphasis and headings dropped.
fn turn_gist(summary: &str) -> String {
    let first = summary
        .split("\n\n")
        .map(|p| p.trim().trim_start_matches('#').trim())
        .find(|p| !p.is_empty() && !p.starts_with("```"))
        .unwrap_or("");
    let plain: String = first.replace("**", "").replace('`', "").split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.chars().count() <= TURN_NOTE_CHARS {
        return plain;
    }
    let cut: String = plain.chars().take(TURN_NOTE_CHARS).collect();
    match cut.rfind(". ") {
        Some(end) if end > TURN_NOTE_CHARS / 2 => cut[..=end].to_string(),
        _ => format!("{}...", cut.trim_end()),
    }
}

#[cfg(test)]
mod settle_tests {
    use super::*;

    #[test]
    fn a_turn_that_changed_code_leaves_one_note_and_a_chat_leaves_none() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let edit = json!({"user": "a@x", "session": "s1", "path": "calc.py",
            "symbols_changed": {"sum_prices": {"before": "h1", "after": "h2"}}, "events": [{"symbol": "checkout"}]});
        store.ledger_append("w:r", "edit_reported", &edit, now()).unwrap();
        let summary = "I renamed total to sum_prices so the name says what it sums, and updated checkout to match.\n\nDetails follow.";
        let saved = settle_turn(&store, "w:r", "a@x", "s1", summary, "claude");
        assert_eq!(saved["saved"], json!(1), "{saved}");
        let notes = store.ledger_since_kinds("w:r", 0.0, &["memory_saved"]);
        assert_eq!(notes.len(), 1, "one note per turn, not one per symbol");
        let id = notes[0].payload["memory_id"].as_str().unwrap().to_string();
        let row = store.kv_get("memory", &format!("w:r:{id}")).unwrap().to_string();
        assert!(row.contains("so the name says what it sums") && row.contains("also changed: checkout"), "{row}");
        assert!(!row.contains("Details follow"), "only the first paragraph");
        // the next turn, nothing new written: no note
        assert_eq!(settle_turn(&store, "w:r", "a@x", "s1", summary, "claude")["saved"], json!(0));
        // another session's turn that wrote nothing: no note
        assert_eq!(settle_turn(&store, "w:r", "a@x", "s2", summary, "claude")["saved"], json!(0));
    }

    #[test]
    fn the_gist_is_the_first_paragraph_cut_at_a_sentence() {
        let long = format!("**Done.** {} End.", "This sentence is twenty-nine chars. ".repeat(20));
        let gist = turn_gist(&long);
        assert!(gist.starts_with("Done.") && gist.ends_with('.') && gist.chars().count() <= TURN_NOTE_CHARS, "{gist}");
    }
}
