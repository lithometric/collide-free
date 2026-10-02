//! The parts of a report that watch, remember, and warn.
//!
//! None of these change what an edit does. They are advisory by construction:
//! a tripwire fires, a counter moves, an alert lands in the ledger. An edit is
//! never blocked by any of them, because a supervisor that can stop work is
//! one people turn off.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::hashing::file_hash;
use crate::repo::path_key;
use crate::store::{now, Store};

const MAX_REV_HISTORY: usize = 20;
const WATCHDOG_COOLDOWN_S: f64 = 900.0;
const STREAK_TTL_S: f64 = 3600.0;
const MASS_DELETION_LINES: i64 = 200;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn hashes_of(symbols: Option<&Map<String, Value>>) -> BTreeMap<String, String> {
    symbols
        .map(|map| {
            map.iter()
                .filter_map(|(name, symbol)| {
                    Some((name.clone(), symbol.get("hash")?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Per-anchor rewrite counters, at SCOPE level so two workspaces reporting the
/// same content bump nothing.
///
/// Every distinct hash a symbol (or the file) has worn appends a history
/// entry. Anchored memories read the counts to report honest rewrites; the
/// scar miner reads the trajectory to spot attempts that were reverted from
/// any workspace.
pub fn bump_anchor_revs(
    store: &Store, scope: &str, path: &str,
    before: Option<&Map<String, Value>>, after: Option<&Map<String, Value>>,
    stamp: f64, by: &str,
) {
    let before_hashes = hashes_of(before);
    let after_hashes = hashes_of(after);
    let mut changed: Vec<(String, String)> = Vec::new();

    let names: BTreeSet<&String> =
        before_hashes.keys().chain(after_hashes.keys()).collect();
    for name in names {
        let was = before_hashes.get(name);
        let is = after_hashes.get(name).cloned().unwrap_or_default();
        if was.map(|h| h != &is).unwrap_or(true) || before.is_none() {
            changed.push((name.clone(), is));
        }
    }

    let file_before = if before.is_some() { file_hash(&before_hashes) } else { String::new() };
    let file_after = if after.is_some() { file_hash(&after_hashes) } else { String::new() };
    if file_before != file_after {
        changed.push((String::new(), file_after)); // "" is the file-level anchor
    }

    for (name, new_hash) in changed {
        let key = format!("{scope}:{}:{name}", path_key(path));
        let mut record = store
            .kv_get("symrev", &key)
            .unwrap_or_else(|| json!({"count": 0, "last_hash": Value::Null, "history": []}));
        // deduped on the last hash: reporting identical content is not a rewrite
        if record.get("last_hash").and_then(Value::as_str) == Some(new_hash.as_str()) {
            continue;
        }
        let Some(map) = record.as_object_mut() else { continue };
        let count = map.get("count").and_then(Value::as_i64).unwrap_or(0);
        map.insert("count".into(), json!(count + 1));
        map.insert("last_hash".into(), json!(new_hash));
        let mut history: Vec<Value> = map
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        history.push(json!({"ts": stamp, "hash": new_hash, "by": by}));
        if history.len() > MAX_REV_HISTORY {
            history.drain(0..history.len() - MAX_REV_HISTORY);
        }
        map.insert("history".into(), json!(history));
        let _ = store.kv_put("symrev", &key, &record, stamp);
    }
}

/// Dormant tripwires matching this path or symbols fire once.
pub fn fire_tripwires(
    store: &Store, scope: &str, path: &str, symbols: &[String], by: &str, agent: &str, via: &str,
) {
    let stamp = now();
    let touched: BTreeSet<&String> = symbols.iter().collect();
    for (key, mut tripwire) in store.kv_list("tripwire", &format!("{scope}:")) {
        if crate::compat::truthy(tripwire.get("fired")) {
            continue;
        }
        if tripwire.get("expires").and_then(Value::as_f64).unwrap_or(0.0) <= stamp {
            continue;
        }
        let watches_path = tripwire
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| paths.iter().any(|p| p.as_str() == Some(path)))
            .unwrap_or(false);
        let watches_symbol = tripwire
            .get("symbols")
            .and_then(Value::as_array)
            .map(|names| {
                names.iter().filter_map(Value::as_str).any(|name| touched.iter().any(|t| *t == name))
            })
            .unwrap_or(false);
        if !watches_path && !watches_symbol {
            continue;
        }
        if let Some(map) = tripwire.as_object_mut() {
            map.insert("fired".into(), json!({"ts": stamp, "by": by, "path": path}));
        }
        let _ = store.kv_put("tripwire", &key, &tripwire, stamp);
        let payload = json!({
            "tripwire_id": tripwire.get("id").cloned().unwrap_or(Value::Null),
            "note": tripwire.get("note").cloned().unwrap_or(Value::Null),
            "owner": tripwire.get("owner").cloned().unwrap_or(Value::Null),
            "by": by, "agent": agent, "path": path,
        });
        let _ = store.ledger_append(scope, "tripwire_fired", &payload, stamp);
        let mut event = payload;
        if let Some(map) = event.as_object_mut() {
            map.insert("kind".into(), json!("tripwire"));
        }
        crate::events::publish(store, scope, event, via);
    }
}

pub struct WatchdogInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub session: &'a str,
    pub agent: &'a str,
    pub path: &'a str,
    pub parse_status: &'a str,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub covering_intent: &'a str,
    pub live_intents: usize,
    pub via: &'a str,
}

/// Supervisor for unattended runs. Three conservative rules, all advisory: an
/// edit landing outside EVERY intent its author declared, a mass deletion, and
/// three consecutive edits that failed to parse cleanly.
///
/// A fifteen-minute cooldown per rule means a burst alerts once, and every
/// fact comes from the row just recorded rather than being estimated.
pub fn watchdog(store: &Store, input: &WatchdogInput) {
    let mut alerts: Vec<(&str, String)> = Vec::new();

    if input.live_intents > 0 && input.covering_intent.is_empty() {
        alerts.push((
            "outside_intent",
            format!(
                "edit to {} is covered by none of the author's {} declared intent(s)",
                input.path, input.live_intents
            ),
        ));
    }
    if input.lines_removed >= MASS_DELETION_LINES
        && input.lines_removed >= 4 * input.lines_added.max(1)
    {
        alerts.push((
            "mass_deletion",
            format!("-{}/+{} lines in one edit", input.lines_removed, input.lines_added),
        ));
    }

    let bucket = if !input.session.is_empty() {
        input.session
    } else if !input.agent.is_empty() {
        input.agent
    } else {
        "work"
    };
    let streak_key = format!("wdstreak:{}:{}:{bucket}", input.scope, input.user_id);
    if input.parse_status == "partial" {
        let streak = store
            .eph_get(&streak_key)
            .and_then(|record| record.get("n").and_then(Value::as_i64))
            .unwrap_or(0)
            + 1;
        let _ = store.eph_set(&streak_key, &json!({"n": streak}), Some(STREAK_TTL_S));
        if streak == 3 {
            alerts.push((
                "broken_parse_streak",
                "3 consecutive edits failed to parse cleanly".to_string(),
            ));
        }
    } else if input.parse_status == "clean" {
        // only a clean parse ends the streak; "unsupported" (a non-code file)
        // neither counts toward it nor absolves it
        store.eph_delete(&streak_key);
    }

    for (rule, detail) in alerts {
        let cool_key = format!("wdcool:{}:{}:{rule}", input.scope, input.user_id);
        if store.eph_get(&cool_key).is_some() {
            continue;
        }
        let _ = store.eph_set(&cool_key, &json!({"ts": now()}), Some(WATCHDOG_COOLDOWN_S));
        let payload = json!({
            "rule": rule, "user": input.user_id, "session": input.session,
            "agent": input.agent, "path": input.path, "detail": detail,
        });
        let _ = store.ledger_append(input.scope, "watchdog", &payload, now());
        let mut event = payload;
        if let Some(map) = event.as_object_mut() {
            map.insert("kind".into(), json!("watchdog"));
        }
        crate::events::publish(store, input.scope, event, input.via);
    }
}

/// Live intents this user owns in this scope, and whether any covers the path
/// or symbols of the edit just made.
pub fn covering_intent(
    store: &Store, scope: &str, user_id: &str, path: &str, symbols: &BTreeSet<String>,
) -> (usize, String) {
    let stamp = now();
    let mut mine = 0;
    let mut covering = String::new();
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        if text(&intent, "owner") != user_id || text(&intent, "status") != "active" {
            continue;
        }
        if intent.get("expires").and_then(Value::as_f64).map(|at| at <= stamp).unwrap_or(false) {
            continue;
        }
        mine += 1;
        if !covering.is_empty() {
            continue;
        }
        let covers_path = intent
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| paths.iter().any(|p| p.as_str() == Some(path)))
            .unwrap_or(false);
        let covers_symbol = intent
            .get("symbols")
            .and_then(Value::as_array)
            .map(|names| {
                names.iter().filter_map(Value::as_str).any(|name| symbols.contains(name))
            })
            .unwrap_or(false);
        if covers_path || covers_symbol {
            covering = text(&intent, "intent_id");
        }
    }
    (mine, covering)
}
