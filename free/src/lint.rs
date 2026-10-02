//! The linter whose rules are your teammates' changes.
//!
//! A rename, removal or signature change made by one agent becomes a
//! short-lived rule for everyone else's next report. The scan happens at the
//! only moment source exists — in memory, at report time — and the content is
//! discarded immediately afterwards, the same promise parsing makes.
//!
//! Two deliberate limits keep it from becoming noise an agent learns to
//! ignore: rules expire after seven days, and names shorter than four
//! characters are skipped entirely because `id` or `run` matches everything.
//! Your own changes never lint your own file.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::store::{now, Store};

const RULE_TTL_S: f64 = 7.0 * 86400.0;
const MAX_RULES_PER_BUCKET: usize = 200;
const MAX_FINDINGS: usize = 10;
const MAX_HITS_PER_SYMBOL: usize = 10;
const MIN_LINTABLE_NAME: usize = 4;
const BUCKETS: [&str; 3] = ["renames", "removed", "signatures"];

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Workspace setting: off (compute nothing), findings (report only, the
/// default), propose (findings plus fix instructions), auto (mechanical fixes
/// applied by the hook's applier).
pub fn autofix_mode(store: &Store, workspace_id: &str) -> String {
    let mode = store
        .kv_get("autofix", workspace_id)
        .map(|record| text(&record, "mode"))
        .unwrap_or_default();
    match mode.as_str() {
        "off" | "findings" | "propose" | "auto" => mode,
        _ => "findings".to_string(),
    }
}

/// Line numbers where a name appears as a whole word. Short names are skipped:
/// matching `id` or `run` across a file produces findings nobody can act on.
fn occurrences(lines: &[&str], name: &str) -> Vec<usize> {
    if name.chars().count() < MIN_LINTABLE_NAME {
        return Vec::new();
    }
    let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else {
        return Vec::new();
    };
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| pattern.is_match(line))
        .map(|(index, _)| index + 1)
        .take(MAX_HITS_PER_SYMBOL)
        .collect()
}

/// Flag references to symbols OTHER people renamed, removed or re-signatured
/// recently. Advisory and capped.
pub fn lint_report(store: &Store, scope: &str, user_id: &str, session: &str, content: &str) -> Vec<Value> {
    let lines: Vec<&str> = content.lines().collect();
    lint_with(store, scope, user_id, session, &|name| occurrences(&lines, name))
}

/// The same lint over a hook's local parse: `{name: [lines]}` instead of the
/// file. A dotted rule (`Class.method`) matches by its last segment.
pub fn lint_names(store: &Store, scope: &str, user_id: &str, session: &str, names: &Value) -> Vec<Value> {
    let lookup = |name: &str| -> Vec<usize> {
        if name.chars().count() < MIN_LINTABLE_NAME {
            return Vec::new();
        }
        let key = name.rsplit('.').next().unwrap_or(name);
        names
            .get(key)
            .and_then(Value::as_array)
            .map(|lines| lines.iter().filter_map(Value::as_u64).map(|n| n as usize).take(MAX_HITS_PER_SYMBOL).collect())
            .unwrap_or_default()
    };
    lint_with(store, scope, user_id, session, &lookup)
}

fn lint_with(store: &Store, scope: &str, user_id: &str, session: &str, occurrences_of: &dyn Fn(&str) -> Vec<usize>) -> Vec<Value> {
    let Some(registry) = store.kv_get("semlint", scope) else { return Vec::new() };
    let cutoff = now() - RULE_TTL_S;

    let bucket = |name: &str| -> Map<String, Value> {
        registry.get(name).and_then(Value::as_object).cloned().unwrap_or_default()
    };
    // this agent's own change is not a finding against its own file; another
    // agent's is, the same person's included (one person runs many agents,
    // and each one's rename breaks the others' code the same way)
    let applies = |info: &Value| -> bool {
        let rule_session = text(info, "session");
        let own = text(info, "by") == user_id && (rule_session.is_empty() || session.is_empty() || rule_session == session);
        !own && info.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= cutoff
    };

    let mut findings: Vec<Value> = Vec::new();

    for (old_name, info) in bucket("renames") {
        if !applies(&info) {
            continue;
        }
        let hits = occurrences_of(&old_name);
        if hits.is_empty() {
            continue;
        }
        let new_name = text(&info, "new");
        findings.push(json!({
            "rule": "stale_name",
            "change_ts": info.get("ts").cloned().unwrap_or(json!(0)),
            "symbol": old_name, "renamed_to": new_name,
            "by": text(&info, "by"), "lines": hits,
            "fix": {"class": "mechanical", "kind": "rename",
                    "from": old_name, "to": new_name},
        }));
    }

    for (name, info) in bucket("removed") {
        if !applies(&info) {
            continue;
        }
        let hits = occurrences_of(&name);
        if hits.is_empty() {
            continue;
        }
        let by = text(&info, "by");
        findings.push(json!({
            "rule": "removed_symbol_use",
            "change_ts": info.get("ts").cloned().unwrap_or(json!(0)),
            "symbol": name, "by": by, "lines": hits,
            "fix": {"class": "judgment", "prompt": format!(
                "`{name}` was removed by {by} but this file still references it (lines {hits:?}). \
Find the replacement (check_collisions / get_symbol) and adapt each usage.")},
        }));
    }

    for (name, info) in bucket("signatures") {
        if !applies(&info) {
            continue;
        }
        let hits = occurrences_of(&name);
        if hits.is_empty() {
            continue;
        }
        let by = text(&info, "by");
        let signature = text(&info, "signature");
        let to = if signature.is_empty() { String::new() } else { format!(" to `{signature}`") };
        let mut finding = json!({
            "rule": "signature_drift",
            "change_ts": info.get("ts").cloned().unwrap_or(json!(0)),
            "symbol": name, "by": by, "lines": hits,
            "fix": {"class": "judgment", "prompt": format!(
                "`{name}`'s signature was changed by {by}{to}; verify each call site in this file \
(lines {hits:?}) matches the new signature and adapt.")},
        });
        if !signature.is_empty() {
            if let Some(map) = finding.as_object_mut() {
                map.insert("signature".into(), json!(signature));
            }
        }
        findings.push(finding);
    }

    findings.truncate(MAX_FINDINGS);
    findings
}

/// Certain renames, removals and signature changes become lint rules for
/// everyone else's future reports.
pub fn update_registry(store: &Store, scope: &str, user_id: &str, session: &str, hot: &[Value], stamp: f64) {
    if hot.is_empty() {
        return;
    }
    let mut registry = store.kv_get("semlint", scope).unwrap_or_else(|| json!({}));
    let Some(root) = registry.as_object_mut() else { return };

    for event in hot {
        let symbol = text(event, "symbol");
        if symbol.is_empty() {
            continue;
        }
        let detail = event.get("detail").cloned().unwrap_or(Value::Null);
        let kind = text(event, "kind");
        let mut base = json!({"by": user_id, "ts": stamp});
        if !session.is_empty() {
            base["session"] = json!(session);
        }

        if kind == "renamed" && !text(&detail, "new_name").is_empty() {
            let mut entry = base.clone();
            if let Some(map) = entry.as_object_mut() {
                map.insert("new".into(), json!(text(&detail, "new_name")));
            }
            root.entry("renames").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("renames").and_then(Value::as_object_mut) {
                map.insert(symbol.clone(), entry);
            }
            // a rename is not a removal; clear any stale removal rule
            if let Some(map) = root.get_mut("removed").and_then(Value::as_object_mut) {
                map.remove(&symbol);
            }
        } else if kind == "removed" {
            root.entry("removed").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("removed").and_then(Value::as_object_mut) {
                map.insert(symbol.clone(), base);
            }
        } else {
            let mut entry = base.clone();
            if let Some(map) = entry.as_object_mut() {
                map.insert(
                    "signature".into(),
                    json!(clip(&text(&detail, "signature"), 200)),
                );
            }
            root.entry("signatures").or_insert_with(|| json!({}));
            if let Some(map) = root.get_mut("signatures").and_then(Value::as_object_mut) {
                map.insert(symbol.clone(), entry);
            }
        }
    }

    // expire old rules and cap each bucket, newest first
    let cutoff = stamp - RULE_TTL_S;
    for name in BUCKETS {
        let Some(entries) = root.get(name).and_then(Value::as_object).cloned() else { continue };
        let mut kept: Vec<(String, Value)> = entries
            .into_iter()
            .filter(|(_, value)| value.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= cutoff)
            .collect();
        kept.sort_by(|a, b| {
            let ts = |entry: &Value| entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            ts(&b.1).partial_cmp(&ts(&a.1)).unwrap_or(std::cmp::Ordering::Equal)
        });
        kept.truncate(MAX_RULES_PER_BUCKET);
        let bucket: BTreeMap<String, Value> = kept.into_iter().collect();
        root.insert(name.to_string(), json!(bucket));
    }
    let _ = store.kv_put("semlint", scope, &registry, stamp);
}

fn reconciliation_id(path: &str, symbol: &str, rule: &str, b_user: &str) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(format!("{path}|{symbol}|{rule}|{b_user}").as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

const RECONCILE_TTL_S: f64 = 6.0 * 3600.0;
const MAX_RECONCILIATIONS: usize = 100;

/// Judgment-class findings ARE active collisions: one agent's interface change
/// against another agent's code. Each becomes a claimable record, redelivered
/// until resolved.
///
/// Resolution is evidence-based and differs per rule. For a stale name or a
/// removed symbol, a fresh report of the path that no longer flags it proves
/// the reference is gone. For signature drift the NAME legitimately survives
/// an adapted call, so occurrence proves nothing — instead a re-report after
/// the record was claimed counts as action.
///
/// A resolved record also SUPPRESSES redelivery of the same finding while the
/// underlying change is unchanged. A file that already survived a collision
/// must not be nagged about it again.
pub fn track_reconciliations(
    store: &Store, scope: &str, user_id: &str, path: &str, findings: Vec<Value>,
) -> Vec<Value> {
    let key = format!("reconcile:{scope}");
    let mut records = store
        .eph_get(&key)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let stamp = now();
    let mut delivered: Vec<Value> = Vec::new();
    let mut open_symbols: Vec<String> = Vec::new();
    let mut changed = false;

    for finding in findings {
        let fix_class = finding.get("fix").map(|fix| text(fix, "class")).unwrap_or_default();
        if fix_class != "judgment" {
            delivered.push(finding);
            continue;
        }
        let symbol = text(&finding, "symbol");
        let rule = text(&finding, "rule");
        let rid = reconciliation_id(path, &symbol, &rule, user_id);
        let change_ts = finding.get("change_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let existing = records.get(&rid).cloned();

        if let Some(record) = &existing {
            let resolved = text(record, "status") == "resolved";
            let same_change =
                record.get("change_ts").and_then(Value::as_f64).unwrap_or(-1.0) == change_ts;
            if resolved && same_change {
                continue; // already reconciled against this exact change: quiet
            }
        }
        open_symbols.push(symbol.clone());
        delivered.push(finding.clone());

        let fresh = existing
            .as_ref()
            .map(|record| text(record, "status") == "resolved")
            .unwrap_or(true);
        if fresh {
            let mut record = json!({
                "id": rid, "rule": rule, "symbol": symbol,
                "a_user": text(&finding, "by"), "b_user": user_id,
                "path": path,
                "lines": finding.get("lines").cloned().unwrap_or(json!([])),
                "change_ts": change_ts,
                "opened": stamp, "status": "open", "claimed_by": "", "claim_ts": 0.0,
            });
            if let Some(map) = record.as_object_mut() {
                for carried in ["signature", "renamed_to"] {
                    if let Some(value) = finding.get(carried) {
                        if !value.as_str().unwrap_or("").is_empty() {
                            map.insert(carried.into(), value.clone());
                        }
                    }
                }
            }
            records.insert(rid, record);
        } else if let Some(record) = records.get_mut(&rid) {
            if let Some(map) = record.as_object_mut() {
                map.insert("lines".into(), finding.get("lines").cloned().unwrap_or(json!([])));
            }
        }
        changed = true;
    }

    // anything still open for this user on this path that no longer appears
    let ids: Vec<String> = records.keys().cloned().collect();
    for rid in ids {
        let Some(record) = records.get(&rid).cloned() else { continue };
        if text(&record, "status") != "open"
            || text(&record, "path") != path
            || text(&record, "b_user") != user_id
        {
            continue;
        }
        let symbol = text(&record, "symbol");
        let gone = !open_symbols.contains(&symbol);
        let acted = text(&record, "rule") == "signature_drift"
            && !text(&record, "claimed_by").is_empty()
            && stamp > record.get("claim_ts").and_then(Value::as_f64).unwrap_or(0.0);
        if !(gone || acted) {
            continue;
        }
        if let Some(entry) = records.get_mut(&rid).and_then(Value::as_object_mut) {
            entry.insert("status".into(), json!("resolved"));
            entry.insert("resolved_by".into(), json!(user_id));
            entry.insert("resolved_ts".into(), json!(stamp));
        }
        changed = true;
        let _ = store.ledger_append(
            scope,
            "reconciliation_resolved",
            &json!({"id": rid, "symbol": symbol, "user": user_id,
                    "rule": text(&record, "rule")}),
            stamp,
        );
    }

    if changed {
        let kept: Map<String, Value> = records
            .into_iter()
            .filter(|(_, record)| {
                text(record, "status") == "open"
                    || stamp - record.get("resolved_ts").and_then(Value::as_f64).unwrap_or(stamp)
                        < RECONCILE_TTL_S
            })
            .collect();
        let capped: Map<String, Value> = if kept.len() > MAX_RECONCILIATIONS {
            kept.into_iter().take(MAX_RECONCILIATIONS).collect()
        } else {
            kept
        };
        let _ = store.eph_set(&key, &Value::Object(capped), Some(RECONCILE_TTL_S));
    }
    delivered
}

/// Open collisions in this scope, for the `reconciliations_pending` notice.
pub fn open_reconciliations(store: &Store, scope: &str) -> Vec<Value> {
    store
        .eph_get(&format!("reconcile:{scope}"))
        .and_then(|value| value.as_object().cloned())
        .map(|records| {
            records
                .into_values()
                .filter(|record| text(record, "status") == "open")
                .collect()
        })
        .unwrap_or_default()
}

/// An edit landing on a path this AGENT was just blocked on is an override:
/// it saw the collision and wrote anyway. Recorded, never prevented. Keyed by
/// agent, like the block itself: another session of the same person writing
/// the path never saw the block, so its write is not an override. A block
/// that named no session belongs to the person, though — a gate reached
/// without one cannot say which agent it stopped — so any of the person's
/// agents writing that path overrides it.
pub fn note_override(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, path_key: &str) -> bool {
    let own = format!("blocked:{scope}:{}:{path_key}", crate::presence::agent_id(user_id, session));
    let person = format!("blocked:{scope}:{user_id}:{path_key}");
    let key = if store.eph_get(&own).is_some() {
        own
    } else if !session.is_empty() && store.eph_get(&person).is_some() {
        person
    } else {
        return false;
    };
    store.eph_delete(&key);
    let stamp = now();
    let salience_key = format!("{scope}:{path_key}");
    let mut record = store.kv_get("salience", &salience_key).unwrap_or_else(|| {
        json!({"path": path, "collisions": 0, "overrides": 0, "blocks": 0})
    });
    if let Some(map) = record.as_object_mut() {
        let read = |map: &Map<String, Value>, name: &str| {
            map.get(name).and_then(Value::as_i64).unwrap_or(0)
        };
        let overrides = read(map, "overrides") + 1;
        map.insert("overrides".into(), json!(overrides));
        let score = 3 * read(map, "collisions") + 4 * overrides + 2 * read(map, "blocks");
        map.insert("score".into(), json!(score));
        map.insert("updated".into(), json!(stamp));
    }
    let _ = store.kv_put("salience", &salience_key, &record, stamp);
    let _ = store.ledger_append(
        scope,
        "override",
        &json!({"user": user_id, "session": session, "path": path}),
        stamp,
    );
    true
}
