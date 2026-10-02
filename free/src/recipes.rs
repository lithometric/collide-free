//! Recipes: a verified transformation, kept so the next agent replays it.
//!
//! When `collide-hook apply` lands a batch of typed ops and the repo's check
//! passes, the hook may save the batch as a recipe: the ops with the
//! primary symbol abstracted to `$target` and its direct callers to
//! `$caller`. A later agent whose prompt names the same symbol sees the
//! recipe in its briefing with the one command that replays it against its
//! own target. That is the only thing that makes agent four cheaper than
//! agent one for the same shape of task: the work, not just the facts, is
//! reused.
//!
//! Recipes are workspace-wide: the same shape of task recurs across a team's
//! repos, and a transformation verified in one is worth offering in another.
//! Every cross-repo read goes through the caller's `visible` allowlist
//! (`access::visible_scopes`), a non-optional argument, and the local repo's
//! own recipes always rank first so a busier sibling repo cannot evict them
//! from the three slots a briefing shows.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::store::Store;

pub const BUCKET: &str = "recipe";
const MAX_LISTED: usize = 3;

pub fn save(store: &Store, scope: &str, user: &str, session: &str, body: &Value, now: f64) -> Value {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let target_symbol = body.get("target_symbol").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let ops = body.get("ops").cloned().unwrap_or(Value::Null);
    if name.is_empty() || target_symbol.is_empty() || !ops.is_array() {
        return json!({"ok": false, "reason": "name, target_symbol and ops (a list) are required"});
    }
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update(name.as_bytes());
    hasher.update(ops.to_string().as_bytes());
    let id = format!("R-{}", &format!("{:x}", hasher.finalize())[..6]);
    let record = json!({
        "id": id,
        "name": name,
        "target_symbol": target_symbol,
        "ops": ops,
        "files": body.get("files").cloned().unwrap_or(json!(0)),
        "verified": body.get("verified").cloned().unwrap_or(json!(true)),
        "by": user,
        "session": session,
        "applied_at": body.get("applied_at").cloned().unwrap_or(json!("")),
        "ts": now,
        "uses": 0,
    });
    let _ = store.kv_put(BUCKET, &format!("{scope}:{id}"), &record, now);
    let _ = store.ledger_append(scope, "recipe_saved", &json!({"user": user, "session": session, "recipe": id, "name": record["name"], "target_symbol": target_symbol}), now);
    json!({"ok": true, "id": id, "replay": format!("collide-hook apply --recipe {id} --at <path>::{target_symbol}")})
}

/// The scopes a federated recipe read may touch: the caller's own first —
/// it was authorised for that one before any of this ran, and a repo with no
/// tree yet is in no scope table to be listed from — then the visible rest.
pub(crate) fn readable_scopes(scope: &str, visible: &BTreeSet<String>) -> Vec<String> {
    let mut scopes = vec![scope.to_string()];
    scopes.extend(visible.iter().filter(|s| s.as_str() != scope).cloned());
    scopes
}

/// Fetch a recipe by id for replay. A recipe listed in a briefing may live in
/// a sibling repo of the workspace, so the lookup walks the same visible
/// scopes the listing did — local repo first — or `--recipe` would name
/// something this repo cannot fetch.
pub fn get(
    store: &Store, scope: &str, visible: &BTreeSet<String>, id: &str, now: f64, viewer: &str, share_knowledge: bool,
) -> Value {
    for owner in readable_scopes(scope, visible) {
        let key = format!("{owner}:{id}");
        let Some(mut record) = store.kv_get(BUCKET, &key) else { continue };
        // a teammate's recipe is knowledge, and knowledge is Team
        if !share_knowledge && !crate::access::own_knowledge(&record, viewer) {
            continue;
        }
        if let Some(map) = record.as_object_mut() {
            let uses = map.get("uses").and_then(Value::as_i64).unwrap_or(0) + 1;
            map.insert("uses".into(), json!(uses));
            let _ = store.kv_put(BUCKET, &key, &Value::Object(map.clone()), now);
            map.insert("ok".into(), json!(true));
            if owner != scope {
                map.insert("from_repo".into(), json!(owner.split_once(':').map(|(_, r)| r).unwrap_or("")));
            }
            return Value::Object(map.clone());
        }
        return record;
    }
    json!({"ok": false, "reason": "no such recipe"})
}

/// Recipes whose target symbol is one of `symbols`, from every scope of the
/// workspace the caller may read: the local repo's first, then newest first,
/// capped at three. Same-scope-first and the workspace-wide read land
/// together on purpose — without the ranking, the slots would go to
/// whichever repo wrote most recently and the local repo's own recipes would
/// be evicted by a busier sibling.
pub fn matching(
    store: &Store, scope: &str, visible: &BTreeSet<String>, symbols: &[String], viewer: &str, share_knowledge: bool,
) -> Vec<Value> {
    let wanted: Vec<String> = symbols.iter().map(|s| s.to_lowercase()).collect();
    let workspace = scope.split_once(':').map(|(ws, _)| ws).unwrap_or(scope);
    let readable: BTreeSet<String> = readable_scopes(scope, visible).into_iter().collect();
    let mut found: Vec<(bool, Value)> = store
        .kv_list(BUCKET, &format!("{workspace}:"))
        .into_iter()
        .filter_map(|(key, mut record)| {
            // key is "<ws>:<repo>:<id>"; ids carry no colon, so the scope is
            // everything before the last one — exactly as memory_recall reads it
            let owner = key.rsplit_once(':').map(|(owner, _)| owner).unwrap_or("");
            if !readable.contains(owner) {
                return None;
            }
            // a teammate's recipe is knowledge, and knowledge is Team
            if !share_knowledge && !crate::access::own_knowledge(&record, viewer) {
                return None;
            }
            let target = record.get("target_symbol").and_then(Value::as_str).unwrap_or("").to_lowercase();
            if target.is_empty() || !wanted.contains(&target) {
                return None;
            }
            let local = owner == scope;
            if !local {
                // the briefing says where a borrowed recipe came from
                if let Some(map) = record.as_object_mut() {
                    map.insert("from_repo".into(), json!(owner.split_once(':').map(|(_, r)| r).unwrap_or("")));
                }
            }
            Some((local, record))
        })
        .collect();
    found.sort_by(|(a_local, a), (b_local, b)| {
        b_local.cmp(a_local).then_with(|| {
            b.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
                .partial_cmp(&a.get("ts").and_then(Value::as_f64).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    found.truncate(MAX_LISTED);
    found.into_iter().map(|(_, record)| record).collect()
}

/// The lines a briefing adds for matching recipes.
pub fn brief_lines(recipes: &[Value], now: f64) -> Vec<String> {
    if recipes.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["Recipes — verified transformations another agent already made; replay one against your target in ONE command instead of editing by hand, and tell the user whose recipe you replayed:".to_string()];
    for r in recipes {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        let name = r.get("name").and_then(Value::as_str).unwrap_or("");
        let target = r.get("target_symbol").and_then(Value::as_str).unwrap_or("");
        let files = r.get("files").and_then(Value::as_i64).unwrap_or(0);
        let by = r.get("by").and_then(Value::as_str).unwrap_or("");
        let age = now - r.get("ts").and_then(Value::as_f64).unwrap_or(now);
        let uses = r.get("uses").and_then(Value::as_i64).unwrap_or(0);
        let applied_at = r.get("applied_at").and_then(Value::as_str).unwrap_or("");
        let from = match r.get("from_repo").and_then(Value::as_str) {
            Some(repo) if !repo.is_empty() => format!(" in {repo}"),
            _ => String::new(),
        };
        out.push(format!(
            "  {id} \"{name}\" — applied at {applied_at}{from}, {files} file(s), check passed, by {by} {}, replayed {uses}x: `collide-hook apply --recipe {id} --at <path>::{target}` (it computes the batch for YOUR target and its callers, writes, verifies, restores on failure, reports)",
            age_text(age)
        ));
    }
    out
}

fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7_200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 172_800.0 {
        format!("{}h ago", (s / 3_600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

/// Direct callers of `path::symbol` from the resolved graph: (path, symbol).
pub fn callers(store: &Store, scope: &str, path: &str, symbol: &str) -> Vec<Value> {
    let graph = crate::graphview::snapshot(store, scope);
    let mut out: Vec<Value> = Vec::new();
    for level in crate::graphview::blast_radius(&graph, &format!("{path}::{symbol}"), 1) {
        for node in level {
            let dep_path = node.get("path").and_then(Value::as_str).unwrap_or("");
            let dep_symbol = node.get("symbol").and_then(Value::as_str).unwrap_or("");
            if dep_path.is_empty() || dep_symbol.is_empty() || dep_path == path {
                continue;
            }
            out.push(json!({"path": dep_path, "symbol": dep_symbol}));
        }
    }
    out
}

#[allow(dead_code)]
fn _unused(_: &Map<String, Value>) {}
