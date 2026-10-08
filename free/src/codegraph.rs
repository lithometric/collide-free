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

/// One file's graph record as the graph reads it: the parts graph assembly,
/// the name index and a blast radius use, typed. The stored row also holds
/// docstrings, signatures, hashes and the edges resolved at write time;
/// none of that is read from here (briefings read the row itself), and held
/// as JSON objects per file it was most of what a big repo cost in memory.
#[derive(Default)]
pub struct FileRecord {
    pub language: String,
    pub ts: f64,
    /// brought in by graph_import: keeps the edges it came with
    pub imported: bool,
    pub imported_edges: Vec<(String, String, String, String)>,
    pub imports: Vec<core_graph::ImportSpec>,
    /// every import's local name, string or not paired, for a file's scope
    pub import_locals: Vec<String>,
    /// in the row's own order
    pub symbols: Vec<SymbolRecord>,
}

pub struct SymbolRecord {
    pub name: String,
    pub kind: String,
    /// raw edges: (target name, member accessed on it, edge kind)
    pub edges: Vec<(String, String, String)>,
    pub span: Value,
    pub params: Value,
    pub sites: Value,
}

impl FileRecord {
    pub fn from_value(record: &Value) -> Self {
        let symbols = record
            .get("symbols")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(name, entry)| SymbolRecord {
                        name: name.clone(),
                        kind: entry.get("kind").and_then(Value::as_str).unwrap_or("definition").to_string(),
                        edges: edge_tuples_raw(entry),
                        span: entry.get("span").cloned().unwrap_or(json!([0, 0])),
                        params: entry.get("params").cloned().unwrap_or(json!([])),
                        sites: entry.get("sites").cloned().unwrap_or(json!([])),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let imported = crate::compat::truthy(record.get("imported"));
        let imported_edges = if imported {
            record
                .get("edges")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|edge| {
                            Some((
                                edge.get("from")?.as_str()?.to_string(),
                                edge.get("to")?.as_str()?.to_string(),
                                edge.get("kind")?.as_str()?.to_string(),
                                edge.get("confidence")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut import_locals = Vec::new();
        for import in record.get("imports").and_then(Value::as_array).into_iter().flatten() {
            for pair in import.get("names").and_then(Value::as_array).into_iter().flatten() {
                if let Some(local) = pair.get(0).and_then(Value::as_str) {
                    import_locals.push(local.to_string());
                }
            }
        }
        FileRecord {
            language: record.get("language").and_then(Value::as_str).unwrap_or("").to_string(),
            ts: record.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
            imported,
            imported_edges,
            imports: import_specs_of(record),
            import_locals,
            symbols,
        }
    }

    pub fn has_symbol(&self, name: &str) -> bool {
        self.symbols.iter().any(|s| s.name == name)
    }

    pub fn symbol(&self, name: &str) -> Option<&SymbolRecord> {
        self.symbols.iter().find(|s| s.name == name)
    }

    /// Roughly what this record holds in memory, for the cache budget.
    pub fn approx_bytes(&self) -> usize {
        let mut n = 160 + self.language.len();
        for (a, b, c, d) in &self.imported_edges {
            n += 96 + a.len() + b.len() + c.len() + d.len();
        }
        for spec in &self.imports {
            n += 48 + spec.module.len();
            for (a, b) in &spec.names {
                n += 48 + a.len() + b.len();
            }
        }
        for local in &self.import_locals {
            n += 24 + local.len();
        }
        for symbol in &self.symbols {
            n += 200 + symbol.name.len() + symbol.kind.len()
                + value_bytes(&symbol.span) + value_bytes(&symbol.params) + value_bytes(&symbol.sites);
            for (a, b, c) in &symbol.edges {
                n += 72 + a.len() + b.len() + c.len();
            }
        }
        n
    }
}

/// Roughly what a JSON value holds in memory.
pub fn value_bytes(value: &Value) -> usize {
    match value {
        Value::String(s) => 32 + s.len(),
        Value::Array(items) => 32 + items.iter().map(value_bytes).sum::<usize>(),
        Value::Object(map) => 64 + map.iter().map(|(k, v)| 48 + k.len() + value_bytes(v)).sum::<usize>(),
        _ => 32,
    }
}

fn edge_tuples_raw(entry: &Value) -> Vec<(String, String, String)> {
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

fn import_specs_of(record: &Value) -> Vec<core_graph::ImportSpec> {
    record
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
                                    Some((pair.first()?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub type Records = std::sync::Arc<BTreeMap<String, std::sync::Arc<FileRecord>>>;

struct HeldRecords {
    stat: (i64, f64),
    records: Records,
    bytes: usize,
}

static RECORDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, HeldRecords>>> =
    std::sync::OnceLock::new();

fn records_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, HeldRecords>> {
    RECORDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Past this many files a repo's map stays on disk except for its working
/// set: the files most recently written or read, up to this many. The rest
/// are still in the name index (imports resolve against the whole repo) and
/// in the reverse index on disk; they join the working set the moment an
/// agent touches them. `COLLIDE_RESIDENT_FILES` tunes it.
pub fn resident_limit() -> usize {
    #[cfg(test)]
    if let Some(limit) = TEST_LIMIT.with(|l| l.get()) {
        return limit;
    }
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("COLLIDE_RESIDENT_FILES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8_000)
    })
}

#[cfg(test)]
thread_local! {
    static TEST_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn set_test_resident_limit(limit: Option<usize>) {
    TEST_LIMIT.with(|l| l.set(limit));
}

/// How many files this repo's map holds on disk, and whether only a working
/// set of them is kept in memory.
pub fn file_count(store: &Store, scope: &str) -> (usize, bool) {
    let total = store.kv_stat(GRAPH_BUCKET, &format!("{scope}:")).0.max(0) as usize;
    (total, total > resident_limit())
}

fn path_of(record: &Value) -> Option<String> {
    record.get("path").and_then(Value::as_str).map(str::to_string)
}

/// Every graph record of a repo (or, past [`resident_limit`], its working
/// set), path to record, parsed once and kept. The copy catches up by
/// reading only rows written since it was last in step, and reloads only
/// when a row disappeared. Records are shared, so handing them out copies
/// nothing.
pub fn records_shared(store: &Store, scope: &str) -> Records {
    let prefix = format!("{scope}:");
    let stat = store.kv_stat(GRAPH_BUCKET, &prefix);
    let key = store.cache_key(scope);
    crate::membudget::touch(&key);
    let held = records_cache().lock().ok().and_then(|c| c.get(&key).map(|h| (h.stat, h.records.clone(), h.bytes)));
    if let Some((seen, records, _)) = &held {
        if *seen == stat {
            return records.clone();
        }
    }
    let partial = stat.0.max(0) as usize > resident_limit();
    let mut bytes = held.as_ref().map(|h| h.2).unwrap_or(0);
    let mut map: BTreeMap<String, std::sync::Arc<FileRecord>> = match held {
        // caught up in place when this is the only holder: no second map
        Some((seen, records, _)) if stat.0 >= seen.0 => {
            let mut next = std::sync::Arc::try_unwrap(records).unwrap_or_else(|shared| (*shared).clone());
            for (_key, row, _updated) in store.kv_list_since(GRAPH_BUCKET, &prefix, seen.1) {
                if let Some(path) = path_of(&row) {
                    let record = FileRecord::from_value(&row);
                    bytes += record.approx_bytes() + path.len() + 64;
                    if let Some(old) = next.insert(path, std::sync::Arc::new(record)) {
                        bytes = bytes.saturating_sub(old.approx_bytes());
                    }
                }
            }
            next
        }
        _ => BTreeMap::new(),
    };
    if partial {
        if map.is_empty() || stat.0 < held_count(&key) {
            // first read, or a row went: the working set, read again
            map.clear();
            bytes = 0;
            for (_key, row) in store.kv_list_recent(GRAPH_BUCKET, &prefix, resident_limit()) {
                if let Some(path) = path_of(&row) {
                    let record = FileRecord::from_value(&row);
                    bytes += record.approx_bytes() + path.len() + 64;
                    map.insert(path, std::sync::Arc::new(record));
                }
            }
        }
        // past the limit: the least recently written leave
        if map.len() > resident_limit() {
            let mut by_age: Vec<(f64, String)> = map.iter().map(|(p, r)| (r.ts, p.clone())).collect();
            by_age.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            for (_, path) in by_age.into_iter().take(map.len() - resident_limit()) {
                if let Some(old) = map.remove(&path) {
                    bytes = bytes.saturating_sub(old.approx_bytes() + path.len() + 64);
                }
            }
        }
    } else if map.len() as i64 != stat.0 {
        // a row was deleted (or this is the first read): the whole set
        map.clear();
        bytes = 0;
        store.kv_for_each(GRAPH_BUCKET, &prefix, |_key, row| {
            if let Some(path) = path_of(&row) {
                let record = FileRecord::from_value(&row);
                bytes += record.approx_bytes() + path.len() + 64;
                map.insert(path, std::sync::Arc::new(record));
            }
        });
    }
    let records: Records = std::sync::Arc::new(map);
    if let Ok(mut c) = records_cache().lock() {
        c.insert(key.clone(), HeldRecords { stat, records: records.clone(), bytes });
    }
    crate::membudget::report(&key, crate::membudget::Part::Records, bytes);
    records
}

/// The row count the held records were last in step with.
fn held_count(key: &str) -> i64 {
    records_cache().lock().ok().and_then(|c| c.get(key).map(|h| h.stat.0)).unwrap_or(0)
}

fn records(store: &Store, scope: &str) -> Records {
    records_shared(store, scope)
}

/// The names one record contributes to the name index, in the order
/// `indexes` adds them (a symbol, then its bare name when that differs).
pub(crate) fn contributions_of(record: &FileRecord) -> Vec<String> {
    let mut out = Vec::new();
    for symbol in &record.symbols {
        push_names(&symbol.name, &mut out);
    }
    out
}

fn push_names(name: &str, out: &mut Vec<String>) {
    out.push(name.to_string());
    let bare = name.rsplit("::").next().unwrap_or(name).rsplit('.').next().unwrap_or(name);
    if bare != name {
        out.push(bare.to_string());
    }
}

/// The names a stored row contributes, read straight from the row.
fn contributions(record: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(symbols) = record.get("symbols").and_then(Value::as_object) else { return out };
    for name in symbols.keys() {
        push_names(name, &mut out);
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
    /// Built from the stored rows one at a time: the names are all it
    /// needs, and a big repo's records never have to be held at once.
    fn build(store: &Store, scope: &str) -> Self {
        let prefix = format!("{scope}:");
        let stat = store.kv_stat(GRAPH_BUCKET, &prefix);
        let mut known: BTreeSet<String> = BTreeSet::new();
        let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut rows: Vec<(String, Vec<String>)> = Vec::new();
        store.kv_for_each(GRAPH_BUCKET, &prefix, |_key, row| {
            if let Some(path) = path_of(&row) {
                rows.push((path, contributions(&row)));
            }
        });
        // in path order, as a rebuild from records sorted by path adds them
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, contributed) in rows {
            for name in contributed {
                names.entry(name).or_default().push(path.clone());
            }
            known.insert(path);
        }
        let known_fp = paths_fingerprint(&known);
        Self { stat, last_used: crate::store::now(), known, names, known_fp }
    }

    fn approx_bytes(&self) -> usize {
        self.known.iter().map(|p| 48 + p.len()).sum::<usize>()
            + self.names.iter().map(|(n, ps)| 72 + n.len() + ps.iter().map(|p| 24 + p.len()).sum::<usize>()).sum::<usize>()
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
            crate::membudget::report(&store.cache_key(scope), crate::membudget::Part::Index, idx.approx_bytes());
        }
    }
    crate::membudget::touch(&store.cache_key(scope));
    handle
}

/// The whole repo's file set and name index, for a graph assembled from a
/// working set: its files' imports still resolve against every file.
pub fn with_index<R>(store: &Store, scope: &str, f: impl FnOnce(&BTreeSet<String>, &BTreeMap<String, Vec<String>>) -> R) -> R {
    let handle = index_for(store, scope);
    let idx = handle.lock().unwrap_or_else(|e| e.into_inner());
    f(&idx.known, &idx.names)
}

/// Drop one repo's records and index (the memory budget's eviction).
pub fn evict_key(key: &str) {
    if let Ok(mut c) = records_cache().lock() {
        c.remove(key);
    }
    if let Some(all) = INDEXES.get() {
        if let Ok(mut map) = all.lock() {
            map.remove(key);
        }
    }
}

/// Drop every repo's records and index (memory pressure).
pub fn evict_all() -> usize {
    let mut dropped = 0;
    if let Ok(mut c) = records_cache().lock() {
        dropped += c.len();
        c.clear();
    }
    if let Some(all) = INDEXES.get() {
        if let Ok(mut map) = all.lock() {
            map.clear();
        }
    }
    dropped
}

/// Drop every repo whose records and index went unused for `idle_s`: an
/// idle repo costs no memory, and its next use rebuilds them. Returns how
/// many repos were dropped.
pub fn evict_idle(idle_s: f64) -> usize {
    let cutoff = crate::store::now() - idle_s;
    let mut dropped = 0;
    let idle: Vec<String> = crate::membudget::idle_keys(cutoff);
    for key in &idle {
        let held_records = records_cache().lock().map(|c| c.contains_key(key)).unwrap_or(false);
        let held_index = INDEXES.get().and_then(|all| all.lock().ok().map(|m| m.contains_key(key))).unwrap_or(false);
        if held_records || held_index {
            dropped += 1;
        }
        evict_key(key);
    }
    if let Some(all) = INDEXES.get() {
        if let Ok(mut map) = all.lock() {
            let before = map.len();
            map.retain(|_, handle| handle.lock().map(|idx| idx.last_used >= cutoff).unwrap_or(false));
            dropped += before - map.len();
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

    fn importing(path: &str, name: &str, from_module: &str, imported: &str) -> Value {
        json!({"path": path, "language": "python",
               "symbols": {name: {"kind": "function", "edges": [[imported, "", "calls"]]}},
               "imports": [{"module": from_module, "names": [[imported, imported]]}]})
    }

    #[test]
    fn a_huge_repo_keeps_its_working_set_in_memory_and_resolves_against_every_file() {
        set_test_resident_limit(Some(3));
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let scope = "w:huge";
        // six files; the oldest defines what the newest calls
        update_file(&db, scope, "lib/core.py", rec("lib/core.py", &["compute"]), 1_000.0);
        for i in 0..4 {
            let path = format!("lib/m{i}.py");
            update_file(&db, scope, &path, rec(&path, &[&format!("f{i}")]), 1_001.0 + i as f64);
        }
        update_file(&db, scope, "app/main.py", importing("app/main.py", "run", "lib.core", "compute"), 1_010.0);
        std::thread::sleep(std::time::Duration::from_millis(5));

        let records = records_shared(&db, scope);
        assert_eq!(records.len(), 3, "the working set only: {:?}", records.keys().collect::<Vec<_>>());
        assert!(records.contains_key("app/main.py") && !records.contains_key("lib/core.py"));
        assert_eq!(file_count(&db, scope), (6, true));

        let graph = crate::graphview::snapshot(&db, scope);
        assert_eq!(graph.partial, Some(6));
        // the call resolves to a file outside the working set
        let targets: Vec<&String> = graph.out.get("app/main.py::run").map(|m| m.keys().collect()).unwrap_or_default();
        assert!(targets.iter().any(|t| t.as_str() == "lib/core.py::compute"), "{targets:?}");

        // touching the old file brings it in; the least recently written leaves
        update_file(&db, scope, "lib/core.py", rec("lib/core.py", &["compute", "helper"]), 1_020.0);
        let records = records_shared(&db, scope);
        assert_eq!(records.len(), 3);
        assert!(records.contains_key("lib/core.py"));
        let graph = crate::graphview::snapshot(&db, scope);
        assert!(graph.nodes.contains_key("lib/core.py::helper"));
        set_test_resident_limit(None);
    }
}
