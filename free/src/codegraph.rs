//! The live code graph's storage side.
//!
//! `collide-core` resolves a file's edges; this keeps the per-file records and
//! the reverse index that answers "what depends on this symbol" without
//! walking the whole graph. Both move on every clean report, which is what
//! makes the graph current on the write rather than on a commit.

use std::collections::{BTreeMap, BTreeSet};

use collide_core::graph as core_graph;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::Store;

pub const GRAPH_BUCKET: &str = "graph";
pub const REV_BUCKET: &str = "graphrev";

/// Collide's own installed files (`.collide/report_hook.py`, the gate, the
/// config) are tracked by git in every repo setup touches. They are not the
/// user's code: never in the map's hubs, recency or working sets.
pub fn is_collide_artifact(path: &str) -> bool {
    path.starts_with(".collide/") || path.contains("/.collide/")
}

pub fn path_key(path: &str) -> String {
    path.replace('/', "|")
}

pub(crate) fn paths_fingerprint(known: &BTreeSet<String>) -> String {
    let mut hasher = Sha256::new();
    for path in known {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

type Records = std::sync::Arc<BTreeMap<String, std::sync::Arc<Value>>>;
static RECORDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ((i64, f64), Records, f64)>>> =
    std::sync::OnceLock::new();

/// Every graph record of a repo, path to record, parsed once and kept.
/// Reading and parsing every row per call was the whole-graph cost behind
/// each snapshot check (every hook event's deltas, every briefing): on a
/// 3,000-file repo about half a second. The copy catches up by reading only
/// rows written since it was last in step, and reloads in full only when a
/// row disappeared. Records are shared, so handing them out copies nothing.
pub fn records_shared(store: &Store, scope: &str) -> Records {
    let prefix = format!("{scope}:");
    let stat = store.kv_stat(GRAPH_BUCKET, &prefix);
    let cache = RECORDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = store.cache_key(scope);
    let held = cache.lock().ok().and_then(|mut c| {
        let entry = c.get_mut(&key)?;
        entry.2 = crate::store::now();
        Some((entry.0, entry.1.clone()))
    });
    if let Some((seen, records)) = &held {
        if *seen == stat {
            return records.clone();
        }
    }
    let mut map: BTreeMap<String, std::sync::Arc<Value>> = match &held {
        Some((seen, records)) if stat.0 >= seen.0 => {
            let mut next = (**records).clone();
            for (_key, record, _updated) in store.kv_list_since(GRAPH_BUCKET, &prefix, seen.1) {
                if let Some(path) = record.get("path").and_then(Value::as_str).map(str::to_string) {
                    next.insert(path, std::sync::Arc::new(record));
                }
            }
            next
        }
        _ => BTreeMap::new(),
    };
    if map.len() as i64 != stat.0 {
        // a row was deleted (or this is the first read): the whole set
        map = store
            .kv_list(GRAPH_BUCKET, &prefix)
            .into_iter()
            .filter_map(|(_key, record)| {
                let path = record.get("path").and_then(Value::as_str)?.to_string();
                Some((path, std::sync::Arc::new(record)))
            })
            .collect();
    }
    let records: Records = std::sync::Arc::new(map);
    if let Ok(mut c) = cache.lock() {
        c.insert(key, (stat, records.clone(), crate::store::now()));
    }
    records
}

fn records(store: &Store, scope: &str) -> Records {
    records_shared(store, scope)
}

/// The names one record contributes to the name index, in the order
/// `indexes` adds them (a symbol, then its bare name when that differs).
pub(crate) fn contributions_of(record: &Value) -> Vec<String> {
    contributions(record)
}

fn contributions(record: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(symbols) = record.get("symbols").and_then(Value::as_object) else { return out };
    for name in symbols.keys() {
        out.push(name.clone());
        let bare = name.rsplit("::").next().unwrap_or(name).rsplit('.').next().unwrap_or(name);
        if bare != name {
            out.push(bare.to_string());
        }
    }
    out
}

/// A repo's file set and name index, kept in memory and moved one file at a
/// time. Every write used to re-read and re-parse every record in the repo
/// to learn only these two things (about 120ms a write on a 3,000-file
/// repo). `stat` is the graph rows' count and newest write when the index
/// was last in step: a write from anywhere else (the other half, a deleted
/// repo) moves it, and the index is rebuilt once.
struct GraphIndex {
    stat: (i64, f64),
    last_used: f64,
    known: BTreeSet<String>,
    names: BTreeMap<String, Vec<String>>,
    known_fp: String,
}

impl GraphIndex {
    fn build(store: &Store, scope: &str) -> Self {
        let stat = store.kv_stat(GRAPH_BUCKET, &format!("{scope}:"));
        let all = records(store, scope);
        let (known, names) = indexes(&*all);
        let known_fp = paths_fingerprint(&known);
        Self { stat, last_used: crate::store::now(), known, names, known_fp }
    }

    /// `path` now holds `record` (it held `old`): the same index a rebuild
    /// would produce. Name lists stay in path order, duplicates kept, exactly
    /// as `indexes` builds them from records sorted by path.
    fn put(&mut self, path: &str, old: &Value, record: &Value) {
        for name in contributions(old) {
            if let Some(paths) = self.names.get_mut(&name) {
                paths.retain(|p| p != path);
                if paths.is_empty() {
                    self.names.remove(&name);
                }
            }
        }
        for name in contributions(record) {
            let paths = self.names.entry(name).or_default();
            let at = paths.partition_point(|p| p.as_str() <= path);
            paths.insert(at, path.to_string());
        }
        if self.known.insert(path.to_string()) {
            self.known_fp = paths_fingerprint(&self.known);
        }
    }
}

type IndexHandle = std::sync::Arc<std::sync::Mutex<GraphIndex>>;
static INDEXES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, IndexHandle>>> =
    std::sync::OnceLock::new();

/// This repo's index, current: reused while the graph rows are as it last
/// left them, rebuilt once when anything else moved them.
fn index_for(store: &Store, scope: &str) -> IndexHandle {
    let all = INDEXES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let handle = {
        let mut map = all.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(store.cache_key(scope))
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(GraphIndex::build(store, scope))))
            .clone()
    };
    {
        let mut idx = handle.lock().unwrap_or_else(|e| e.into_inner());
        idx.last_used = crate::store::now();
        let now_stat = store.kv_stat(GRAPH_BUCKET, &format!("{scope}:"));
        if idx.stat != now_stat {
            tracing::debug!("graph index for {scope} rebuilt: {:?} -> {:?}", idx.stat, now_stat);
            *idx = GraphIndex::build(store, scope);
        }
    }
    handle
}

/// Drop every repo whose records and index went unused for `idle_s`: an
/// idle repo costs no memory, and its next use rebuilds them. Returns how
/// many repos were dropped.
pub fn evict_idle(idle_s: f64) -> usize {
    let cutoff = crate::store::now() - idle_s;
    let mut dropped = 0;
    if let Some(cache) = RECORDS.get() {
        if let Ok(mut c) = cache.lock() {
            let before = c.len();
            c.retain(|_, entry| entry.2 >= cutoff);
            dropped += before - c.len();
        }
    }
    if let Some(all) = INDEXES.get() {
        if let Ok(mut map) = all.lock() {
            map.retain(|_, handle| handle.lock().map(|idx| idx.last_used >= cutoff).unwrap_or(false));
        }
    }
    dropped
}

/// The repo's current file set, for callers that resolve imports.
pub fn with_known<R>(store: &Store, scope: &str, f: impl FnOnce(&BTreeSet<String>) -> R) -> R {
    let handle = index_for(store, scope);
    let idx = handle.lock().unwrap_or_else(|e| e.into_inner());
    f(&idx.known)
}

/// The symbols the graph holds for one file (name to kind, signature, …).
pub fn symbols_of(store: &Store, scope: &str, path: &str) -> serde_json::Map<String, Value> {
    record_of(store, scope, path).get("symbols").and_then(Value::as_object).cloned().unwrap_or_default()
}

fn record_of(store: &Store, scope: &str, path: &str) -> Value {
    store.kv_get(GRAPH_BUCKET, &format!("{scope}:{}", path_key(path))).unwrap_or(Value::Null)
}

fn indexes<V: std::ops::Deref<Target = Value>>(records: &BTreeMap<String, V>) -> (BTreeSet<String>, BTreeMap<String, Vec<String>>) {
    let known: BTreeSet<String> = records.keys().cloned().collect();
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, record) in records {
        let Some(symbols) = record.get("symbols").and_then(Value::as_object) else { continue };
        for name in symbols.keys() {
            names.entry(name.clone()).or_default().push(path.clone());
            let bare = name.rsplit("::").next().unwrap_or(name).rsplit('.').next().unwrap_or(name);
            if bare != name {
                names.entry(bare.to_string()).or_default().push(path.clone());
            }
        }
    }
    (known, names)
}

/// The per-file graph record: names, kinds, raw name-level edges and imports.
pub fn file_record(
    path: &str, language: &str, symbols: &BTreeMap<String, Value>, imports: &[Value], now: f64,
    doc: &str,
) -> Value {
    let mut entries = serde_json::Map::new();
    for (name, symbol) in symbols {
        entries.insert(
            name.clone(),
            json!({
                "kind": symbol.get("kind").and_then(Value::as_str).unwrap_or("definition"),
                "edges": symbol.get("edges").cloned().unwrap_or(json!([])),
                "signature": symbol.get("signature").cloned().unwrap_or(json!("")),
                "hash": symbol.get("hash").cloned().unwrap_or(json!("")),
                "span": symbol.get("span").cloned().unwrap_or(json!([0, 0])),
                "params": symbol.get("params").cloned().unwrap_or(json!([])),
                "sites": symbol.get("sites").cloned().unwrap_or(json!([])),
                "doc": symbol.get("doc").cloned().unwrap_or(json!("")),
            }),
        );
    }
    json!({
        "path": path,
        "language": language,
        "symbols": entries,
        "imports": imports,
        "ts": now,
        "doc": doc,
    })
}

fn edge_tuples(entry: &Value) -> Vec<(String, String, String)> {
    entry
        .get("edges")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|edge| {
                    let edge = edge.as_array()?;
                    Some((
                        edge.first()?.as_str()?.to_string(),
                        edge.get(1)?.as_str()?.to_string(),
                        edge.get(2)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fold an OBSERVATION in — a file the agent read, or one the session-start
/// index swept — rather than one it wrote. The map is what blast_radius and
/// the repo map answer from, and until observations existed it filled only
/// where someone had written, so a symbol's callers stayed invisible unless
/// an earlier agent happened to edit them. Two rules keep a reader from
/// corrupting an editor's map: an observation never displaces a record a
/// write produced inside `hot_ttl_s` (a stale checkout must not roll the map
/// back under someone mid-change), and identical symbols are a no-op.
/// Returns what happened: `indexed`, `unchanged` or `deferred`.
pub fn observe_file(
    store: &Store, scope: &str, path: &str, record: Value, now: f64, hot_ttl_s: f64,
) -> &'static str {
    observe_files(store, scope, vec![(path.to_string(), record)], now, hot_ttl_s)
        .pop()
        .unwrap_or("unchanged")
}

/// Fold one clean parse into the graph: store the record, resolve its edges
/// against the current file set, and move the reverse index.
/// Whether the graph knows this file: indexed from a checkout, or written.
pub fn has_file(store: &Store, scope: &str, path: &str) -> bool {
    store.kv_get(GRAPH_BUCKET, &format!("{scope}:{}", path_key(path))).is_some()
}

pub fn update_file(store: &Store, scope: &str, path: &str, record: Value, now: f64) {
    let old = record_of(store, scope, path);
    let handle = index_for(store, scope);
    {
        let mut idx = handle.lock().unwrap_or_else(|e| e.into_inner());
        idx.put(path, &old, &record);
        fold_file(store, scope, path, record, &old, &idx.known, &idx.names, &idx.known_fp, now);
        idx.stat = store.kv_stat(GRAPH_BUCKET, &format!("{scope}:"));
    }
    // the assembled graph and its partition are now stale
    crate::graphview::invalidate(store, scope);
}

/// Index many observed files at once — a checkout's first index, a batch of
/// reads. The graph is read ONCE and its name index built ONCE for the whole
/// batch; folding files one at a time re-read and re-parsed every record in
/// the repo per file, so indexing N files cost N² record reads (3,216 files:
/// 5.5 seconds per batch of 50, growing with every batch). Resolving against
/// the batch's own files also lets imports between them resolve.
/// Returns each file's outcome, in order: indexed, unchanged or deferred.
pub fn observe_files(
    store: &Store, scope: &str, batch: Vec<(String, Value)>, now: f64, hot_ttl_s: f64,
) -> Vec<&'static str> {
    let mut outcomes = Vec::with_capacity(batch.len());
    let mut changed: Vec<(String, Value, Value)> = Vec::new(); // (path, record, old)
    let mut seen: BTreeMap<String, Value> = BTreeMap::new(); // a path twice in one batch
    for (path, mut record) in batch {
        let current = seen.get(&path).cloned().unwrap_or_else(|| record_of(store, scope, &path));
        if let Some(old) = Some(&current).filter(|v| !v.is_null()) {
            // a record without a source came from a write
            let written = old.get("source").and_then(Value::as_str).is_none();
            let age = now - old.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            if written && age < hot_ttl_s {
                outcomes.push("deferred");
                continue;
            }
            if old.get("symbols") == record.get("symbols") && old.get("imports") == record.get("imports") {
                outcomes.push("unchanged");
                continue;
            }
        }
        if let Some(map) = record.as_object_mut() {
            map.insert("source".into(), json!("observe"));
        }
        seen.insert(path.clone(), record.clone());
        changed.push((path, record, current));
        outcomes.push("indexed");
    }
    if !changed.is_empty() {
        let handle = index_for(store, scope);
        let mut idx = handle.lock().unwrap_or_else(|e| e.into_inner());
        // the whole batch joins the index first, so imports between its
        // files resolve; then each file is folded against it
        for (path, record, old) in &changed {
            idx.put(path, old, record);
        }
        for (path, record, old) in changed {
            fold_file(store, scope, &path, record, &old, &idx.known, &idx.names, &idx.known_fp, now);
        }
        idx.stat = store.kv_stat(GRAPH_BUCKET, &format!("{scope}:"));
        drop(idx);
        crate::graphview::invalidate(store, scope);
    }
    outcomes
}

/// Resolve one file's edges against a known file set and name index, store
/// it, and move the reverse index.
#[allow(clippy::too_many_arguments)]
fn fold_file(
    store: &Store, scope: &str, path: &str, record: Value, old: &Value,
    known: &BTreeSet<String>, names: &BTreeMap<String, Vec<String>>, known_fp: &str, now: f64,
) {
    let language = record.get("language").and_then(Value::as_str).unwrap_or("").to_string();
    let symbols_map = record.get("symbols").and_then(Value::as_object).cloned().unwrap_or_default();
    let raw: Vec<(String, Vec<(String, String, String)>)> =
        symbols_map.iter().map(|(name, entry)| (name.clone(), edge_tuples(entry))).collect();
    let symbols: Vec<core_graph::SymbolEdges> =
        raw.iter().map(|(name, edges)| core_graph::SymbolEdges { name, edges }).collect();

    let imports: Vec<core_graph::ImportSpec> = record
        .get("imports")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| core_graph::ImportSpec {
                    module: item.get("module").and_then(Value::as_str).unwrap_or("").to_string(),
                    names: item
                        .get("names")
                        .and_then(Value::as_array)
                        .map(|pairs| {
                            pairs
                                .iter()
                                .filter_map(|pair| {
                                    let pair = pair.as_array()?;
                                    Some((
                                        pair.first()?.as_str()?.to_string(),
                                        pair.get(1)?.as_str()?.to_string(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let edges = core_graph::resolve_edges(path, &language, &symbols, &imports, known, names);
    let edge_values: Vec<Value> = edges
        .iter()
        .map(|edge| {
            json!({"from": edge.from, "to": edge.to, "kind": edge.kind,
                   "confidence": edge.confidence})
        })
        .collect();

    let mut stored = record;
    if let Some(map) = stored.as_object_mut() {
        map.insert("edges".into(), json!(edge_values));
        // the file set these edges were resolved against: while it is
        // unchanged a snapshot reuses them instead of re-resolving every file
        map.insert("known_fp".into(), json!(known_fp));
    }
    // the record and every reverse-index row it moves go down as one write
    let mut writes: Vec<(String, String, Value)> =
        vec![(GRAPH_BUCKET.to_string(), format!("{scope}:{}", path_key(path)), stored)];

    // move the reverse index: drop edges this write removed, add the new ones
    let previous: BTreeSet<(String, String)> = old
        .get("edges")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|edge| {
                    let to = edge.get("to")?.as_str()?;
                    if to.starts_with("ext:") {
                        return None;
                    }
                    Some((edge.get("from")?.as_str()?.to_string(), to.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let current: BTreeMap<(String, String), String> = edges
        .iter()
        .filter(|edge| !edge.to.starts_with("ext:"))
        .map(|edge| ((edge.from.clone(), edge.to.clone()), edge.kind.clone()))
        .collect();

    let mut touched: BTreeMap<String, Value> = BTreeMap::new();
    let load = |target: &str, touched: &mut BTreeMap<String, Value>| {
        touched.entry(target.to_string()).or_insert_with(|| {
            store
                .kv_get(REV_BUCKET, &format!("{scope}:{target}"))
                .unwrap_or_else(|| json!({"dependents": {}}))
        });
    };

    for (from, to) in &previous {
        if !current.contains_key(&(from.clone(), to.clone())) {
            load(to, &mut touched);
            if let Some(entry) = touched.get_mut(to) {
                if let Some(dependents) = entry.get_mut("dependents").and_then(Value::as_object_mut) {
                    dependents.remove(from);
                }
            }
        }
    }
    for ((from, to), kind) in &current {
        load(to, &mut touched);
        if let Some(entry) = touched.get_mut(to) {
            if let Some(dependents) = entry.get_mut("dependents").and_then(Value::as_object_mut) {
                dependents.insert(from.clone(), json!(kind));
            }
        }
    }
    for (target, mut entry) in touched {
        if let Some(map) = entry.as_object_mut() {
            map.insert("ts".into(), json!(now));
        }
        writes.push((REV_BUCKET.to_string(), format!("{scope}:{target}"), entry));
    }
    let _ = store.kv_put_many(writes, now);
}

#[cfg(test)]
mod index_tests {
    use super::*;

    fn rec(path: &str, names: &[&str]) -> Value {
        let symbols: serde_json::Map<String, Value> =
            names.iter().map(|n| (n.to_string(), json!({"kind": "function"}))).collect();
        json!({"path": path, "language": "python", "symbols": symbols, "imports": []})
    }

    #[test]
    fn the_index_moved_file_by_file_equals_a_rebuild() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:r";
        let writes = [
            ("pkg/b.py", vec!["run", "Model.run", "helper"]),
            ("pkg/a.py", vec!["run", "main"]),
            ("pkg/c.py", vec!["Other.run", "run"]),
            ("pkg/a.py", vec!["main2"]),            // a rewrite drops `run`, adds `main2`
            ("pkg/b.py", vec!["helper", "Model.run"]),
            ("pkg/d.py", vec![]),
        ];
        for (i, (path, names)) in writes.iter().enumerate() {
            update_file(&db, scope, path, rec(path, names), 1_000.0 + i as f64);
            let handle = index_for(&db, scope);
            let live = handle.lock().unwrap();
            let fresh = GraphIndex::build(&db, scope);
            assert_eq!(live.known, fresh.known, "after write {i}");
            assert_eq!(live.names, fresh.names, "after write {i}");
            assert_eq!(live.known_fp, fresh.known_fp, "after write {i}");
        }
        // an idle repo leaves memory, and comes back whole on next use
        assert!(evict_idle(-1.0) >= 1);
        assert!(with_known(&db, scope, |known| known.contains("pkg/a.py") && known.contains("pkg/d.py")));
        // a write from elsewhere (another half) moves the rows: rebuilt, not stale
        let _ = db.kv_put(GRAPH_BUCKET, &format!("{scope}:{}", path_key("pkg/e.py")), &rec("pkg/e.py", &["zed"]), 2_000.0);
        assert!(with_known(&db, scope, |known| known.contains("pkg/e.py")));
    }
}
