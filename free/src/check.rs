//! The repo check, run by the hook after each batch of writes.
//!
//! Two small services behind it. `/verified` stamps the caller's latest
//! write records with the command that ran and whether it passed, so the
//! briefing and `get_symbol` can say "verified by …" (or "check failed")
//! for every module, not only those written through the typed-op path.
//! `/attribute` answers the two questions a failure raises that grep
//! cannot: who last wrote the files the traceback names (another agent's
//! write a minute ago is not the caller's bug to fix), and what the symbols
//! it names actually look like (the exact signature and docstring, so the
//! moment the agent learns it did not know enough is the moment it gets
//! the fact). Hook-only, like `/observe` and `/brief`.

use serde_json::{json, Map, Value};

use crate::store::Store;

const ATTRIBUTE_WINDOW_S: f64 = 86_400.0;
const MAX_PATHS: usize = 40;
/// A passing baseline stamps the whole indexed tree.
const MAX_STAMP_PATHS: usize = 600;
const MAX_SYMBOLS: usize = 20;

/// Stamp `provenance.verified` on the caller's latest write record for
/// each path; a ledger row records the run for the dashboard.
pub fn stamp_verified(
    store: &Store, scope: &str, user: &str, agent: &str, paths: &[String], command: &str, ok: bool,
    now: f64,
) -> Value {
    let mark = json!({"command": command, "ok": ok, "ts": now});
    let mut stamped = 0usize;
    for path in paths.iter().take(MAX_STAMP_PATHS) {
        let Some(mut record) = store.get_file(scope, user, path) else {
            // indexed from the tree, never written by this user: the mark goes
            // on the graph record, which get_symbol serves as "observed"
            let key = format!("{scope}:{}", crate::codegraph::path_key(path));
            let Some(mut graph_record) = store.kv_get(crate::codegraph::GRAPH_BUCKET, &key) else { continue };
            let Some(map) = graph_record.as_object_mut() else { continue };
            map.insert("verified".into(), mark.clone());
            if store.kv_put(crate::codegraph::GRAPH_BUCKET, &key, &graph_record, now).is_ok() {
                stamped += 1;
            }
            continue;
        };
        let Some(map) = record.as_object_mut() else { continue };
        let prov = map.entry("provenance").or_insert_with(|| json!({}));
        if let Some(prov) = prov.as_object_mut() {
            prov.insert("verified".into(), mark.clone());
        }
        if store.put_file(scope, user, path, &record, now).is_ok() {
            stamped += 1;
        }
    }
    let _ = store.ledger_append(
        scope,
        "check_run",
        &json!({"user": user, "agent": agent, "paths": paths.iter().take(MAX_PATHS).collect::<Vec<_>>(), "count": paths.len(), "command": command, "ok": ok}),
        now,
    );
    json!({"ok": true, "stamped": stamped})
}

/// Who last wrote each path (from the ledger, newest first) and the exact
/// facts for each symbol (from the graph records).
pub fn attribute(
    store: &Store, scope: &str, caller: &str, session: &str, paths: &[String], symbols: &[String], now: f64,
) -> Value {
    let mut files = Map::new();
    if !paths.is_empty() {
        let wanted: std::collections::HashSet<&str> =
            paths.iter().take(MAX_PATHS).map(String::as_str).collect();
        let mut rows = store.ledger_since_kinds(scope, now - ATTRIBUTE_WINDOW_S, &["edit_reported"]);
        rows.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
        for row in rows {
            let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
            if !wanted.contains(path) || files.contains_key(path) {
                continue;
            }
            let user = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
            let row_session = row.payload.get("session").and_then(Value::as_str).unwrap_or("");
            // four agents on one credential are four agents: a write from
            // another session is not the caller's, even under the same user
            let same_user = user == caller;
            let mine = same_user && (session.is_empty() || row_session.is_empty() || row_session == session);
            files.insert(
                path.to_string(),
                json!({
                    "user": user,
                    "agent": row.payload.get("agent").and_then(Value::as_str).unwrap_or(""),
                    "age_s": crate::compat::python_round((now - row.ts).max(0.0), 1),
                    "mine": mine,
                    "same_user": same_user,
                }),
            );
            if files.len() == wanted.len() {
                break;
            }
        }
    }
    let mut facts = Map::new();
    if !symbols.is_empty() {
        let graph = crate::graphview::snapshot(store, scope);
        let wanted: Vec<&str> = symbols.iter().take(MAX_SYMBOLS).map(String::as_str).collect();
        for (_key, record) in store.kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:")) {
            let path = record.get("path").and_then(Value::as_str).unwrap_or("");
            let Some(entries) = record.get("symbols").and_then(Value::as_object) else { continue };
            for name in &wanted {
                if facts.contains_key(*name) {
                    continue;
                }
                if let Some(entry) = entries.get(*name) {
                    // callers from the resolved snapshot, so aliased imports count
                    let callers: Vec<String> = crate::graphview::blast_radius(&graph, &format!("{path}::{name}"), 1)
                        .into_iter()
                        .flatten()
                        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_string))
                        .take(12)
                        .collect();
                    facts.insert(
                        name.to_string(),
                        json!({
                            "path": path,
                            "callers": callers,
                            "kind": entry.get("kind").cloned().unwrap_or(json!("")),
                            "signature": entry.get("signature").cloned().unwrap_or(json!("")),
                            "params": entry.get("params").cloned().unwrap_or(json!([])),
                            "doc": entry.get("doc").cloned().unwrap_or(json!("")),
                        }),
                    );
                }
            }
            if facts.len() == wanted.len() {
                break;
            }
        }
    }
    json!({"ok": true, "files": files, "facts": facts})
}
