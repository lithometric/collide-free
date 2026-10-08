//! The read side of the live code graph: assemble it, cluster it, and reduce
//! it to the orientation an agent reads instead of grepping.
//!
//! `codegraph` owns the write side — one file at a time, folded in on every
//! clean report. This module assembles the whole thing from those per-file
//! records, which is what makes the graph reflect the current file set rather
//! than a stale accumulation: a file nobody has reported lately simply is not
//! there, and its edges are not either.
//!
//! The reduction matters more than the graph. A repo map is small enough to
//! sit in a briefing, and every teammate's reported edit sharpens it, which
//! is the whole economic argument: orientation gets cheaper as more agents
//! work, instead of each one paying to grep the tree again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use collide_core::graph as core_graph;
use serde_json::{json, Value};

use crate::codegraph::FileRecord;
use crate::store::Store;

const GRAPH_BUCKET: &str = "graph";
/// How much each relation counts when clustering. Inheritance binds hardest,
/// a bare name reference least — drawing them alike makes every subsystem
/// look like every other one.
const EDGE_WEIGHT: [(&str, f64); 5] = [
    ("inherits", 3.0), ("calls", 2.0), ("uses_type", 1.5),
    ("references", 1.0), ("co_change", 1.0),
];
/// Assembling the graph and clustering it are the two expensive things this
/// module does, and neither result changes while the file set does not. Both
/// are cached on the snapshot fingerprint, which moves the moment any file is
/// reported — so a stale answer is not reachable, only a redundant one is
/// avoided. The briefing calls both on every session start, which is what
/// makes this worth having rather than a premature optimisation.
const SNAPSHOT_TTL_S: f64 = 30.0;

type SnapshotCache = Mutex<HashMap<String, (String, Arc<Snapshot>, f64)>>;
type CommunityCache = Mutex<HashMap<String, (String, Arc<Vec<Community>>)>>;

fn snapshot_cache() -> &'static SnapshotCache {
    static CACHE: OnceLock<SnapshotCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn community_cache() -> &'static CommunityCache {
    static CACHE: OnceLock<CommunityCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Louvain is a heuristic, so the seed is part of the answer.
const LOUVAIN_SEED: u64 = 7;
const LOUVAIN_RESOLUTION: f64 = 1.0;

fn weight_of(kind: &str) -> f64 {
    EDGE_WEIGHT.iter().find(|(name, _)| *name == kind).map(|(_, w)| *w).unwrap_or(1.0)
}

pub fn node_id(path: &str, symbol: &str) -> String {
    format!("{path}::{symbol}")
}

pub fn split_node(nid: &str) -> (&str, &str) {
    nid.split_once("::").unwrap_or((nid, ""))
}

/// External nodes are named, not located — they stand for something outside
/// the reported file set, and counting them would inflate every total.
pub fn internal(nid: &str) -> bool {
    !nid.starts_with("ext:")
}

#[derive(Clone, Default)]
pub struct NodeData {
    pub path: String,
    pub symbol: String,
    pub kind: String,
    pub language: String,
    /// The file's record, shared with the record set: the exact facts a
    /// dependent carries (where it lives, what it takes, what it calls; for
    /// a file, the names in scope) are read from it, not copied per node.
    record: Option<Arc<FileRecord>>,
}

impl NodeData {
    fn entry(&self) -> Option<&crate::codegraph::SymbolRecord> {
        if self.symbol.is_empty() {
            return None;
        }
        self.record.as_ref()?.symbol(&self.symbol)
    }

    pub fn span(&self) -> Value {
        self.entry().map(|e| e.span.clone()).unwrap_or_else(|| json!([0, 0]))
    }

    pub fn params(&self) -> Value {
        self.entry().map(|e| e.params.clone()).unwrap_or_else(|| json!([]))
    }

    pub fn sites(&self) -> Value {
        self.entry().map(|e| e.sites.clone()).unwrap_or_else(|| json!([]))
    }

    /// A file node's names in scope: its top-level names and every name its
    /// imports bind. Empty for every other node.
    pub fn scope(&self) -> Value {
        let Some(record) = self.record.as_ref().filter(|_| self.symbol.is_empty() && self.kind == "file") else {
            return json!([]);
        };
        let mut scope: BTreeSet<&str> = record.symbols.iter().map(|s| s.name.as_str()).collect();
        for local in &record.import_locals {
            if !local.is_empty() && local != "*" {
                scope.insert(local);
            }
        }
        json!(scope.into_iter().collect::<Vec<_>>())
    }
}

#[derive(Clone)]
pub struct EdgeData {
    pub kind: String,
    pub confidence: String,
}

/// A directed graph with the few operations the reductions need. Parallel
/// edges collapse, which is what the Python side's DiGraph does too: a second
/// edge between the same pair replaces the first rather than adding to it.
#[derive(Default)]
pub struct Snapshot {
    pub nodes: BTreeMap<String, NodeData>,
    pub out: BTreeMap<String, BTreeMap<String, EdgeData>>,
    pub incoming: BTreeMap<String, BTreeSet<String>>,
    pub files: usize,
    /// Identifies this exact file set at these exact timestamps, so a cached
    /// clustering can be trusted only while the graph has not moved.
    pub fingerprint: String,
    /// Assembled from a huge repo's working set: `Some(files in the repo)`.
    /// Dependents outside the working set are then not in `incoming`.
    pub partial: Option<usize>,
}

impl Snapshot {
    pub fn has_node(&self, nid: &str) -> bool {
        self.nodes.contains_key(nid)
    }

    pub fn in_degree(&self, nid: &str) -> usize {
        self.incoming.get(nid).map(BTreeSet::len).unwrap_or(0)
    }

    pub fn out_degree(&self, nid: &str) -> usize {
        self.out.get(nid).map(BTreeMap::len).unwrap_or(0)
    }

    pub fn predecessors(&self, nid: &str) -> impl Iterator<Item = &String> {
        self.incoming.get(nid).into_iter().flatten()
    }

    fn add_node(&mut self, nid: String, data: NodeData) {
        self.nodes.entry(nid).or_insert(data);
    }

    fn add_edge(&mut self, from: String, to: String, data: EdgeData) {
        self.incoming.entry(to.clone()).or_default().insert(from.clone());
        self.out.entry(from).or_default().insert(to, data);
    }

    /// Edges that land somewhere in this repo. An edge into `ext:` names a
    /// dependency the file set does not contain, which is worth showing on
    /// the graph and wrong to count as repo structure.
    pub fn internal_edge_count(&self) -> usize {
        self.out.values().flat_map(BTreeMap::keys).filter(|to| internal(to)).count()
    }

    /// Roughly what this graph holds in memory, for the cache budget.
    fn approx_bytes(&self) -> usize {
        let nodes: usize = self.nodes.iter().map(|(id, n)| 200 + id.len() + n.path.len() + n.symbol.len()).sum();
        let out: usize = self.out.iter().map(|(from, m)| 80 + from.len() + m.keys().map(|to| 96 + to.len()).sum::<usize>()).sum();
        let incoming: usize = self.incoming.iter().map(|(to, set)| 80 + to.len() + set.iter().map(|f| 48 + f.len()).sum::<usize>()).sum();
        nodes + out + incoming
    }

    /// A copy, for a patch when another holder still has this graph.
    fn duplicate(&self) -> Snapshot {
        Snapshot {
            nodes: self.nodes.clone(),
            out: self.out.clone(),
            incoming: self.incoming.clone(),
            files: self.files,
            fingerprint: self.fingerprint.clone(),
            partial: self.partial,
        }
    }
}

fn records(store: &Store, scope: &str) -> BTreeMap<String, Arc<FileRecord>> {
    crate::codegraph::records_shared(store, scope)
        .iter()
        .filter(|(path, _)| !crate::codegraph::is_collide_artifact(path))
        .map(|(path, record)| (path.clone(), Arc::clone(record)))
        .collect()
}

/// Identifies a file set at given timestamps. Python builds it over sorted
/// paths, and a BTreeMap is already sorted.
fn fingerprint(records: &BTreeMap<String, Arc<FileRecord>>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (path, record) in records {
        // serde_json writes a float the way Python's str() does, which the
        // chain hash already depends on — the same agreement is what makes
        // this fingerprint comparable across the two servers
        let ts = serde_json::to_string(&json!(record.ts)).unwrap_or_else(|_| "0.0".into());
        hasher.update(format!("{path}:{ts}\n").as_bytes());
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn indexes(records: &BTreeMap<String, Arc<FileRecord>>) -> (BTreeSet<String>, BTreeMap<String, Vec<String>>) {
    let known: BTreeSet<String> = records.keys().cloned().collect();
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, record) in records {
        for name in crate::codegraph::contributions_of(record) {
            names.entry(name).or_default().push(path.clone());
        }
    }
    (known, names)
}

/// One resolved edge: (from, to, kind, confidence).
type Resolved = (String, String, String, String);

/// What a repo's graph was assembled from, kept so the next edit moves it
/// instead of rebuilding it. `deps` is, per file, every name its resolution
/// looked up in the name index; a file whose names are untouched by an edit
/// resolves exactly as before, which is what makes patching exact.
struct Built {
    records: BTreeMap<String, Arc<FileRecord>>,
    known: BTreeSet<String>,
    names: BTreeMap<String, Vec<String>>,
    deps: HashMap<String, BTreeSet<String>>,
    graph: Arc<Snapshot>,
}

type BuiltCache = Mutex<HashMap<String, Arc<Mutex<Option<Built>>>>>;

fn built_cache() -> &'static BuiltCache {
    static CACHE: OnceLock<BuiltCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The nodes one record contributes: its file, then each symbol.
fn file_nodes(path: &str, record: &Arc<FileRecord>) -> Vec<(String, NodeData)> {
    let mut out = vec![(node_id(path, ""), NodeData {
        path: path.to_string(), symbol: String::new(),
        kind: "file".into(), language: record.language.clone(),
        record: Some(Arc::clone(record)),
    })];
    for symbol in &record.symbols {
        out.push((node_id(path, &symbol.name), NodeData {
            path: path.to_string(),
            symbol: symbol.name.clone(),
            kind: symbol.kind.clone(),
            language: record.language.clone(),
            record: Some(Arc::clone(record)),
        }));
    }
    out
}

/// Every name `resolve_edges` can look up in the name index for this
/// record: its raw edges' targets and members, and what its imports bind.
fn name_deps(record: &FileRecord) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for symbol in &record.symbols {
        for (target, member, _) in &symbol.edges {
            deps.insert(target.clone());
            if !member.is_empty() {
                deps.insert(member.clone());
            }
        }
    }
    for spec in &record.imports {
        for (_, original) in &spec.names {
            deps.insert(original.clone());
        }
    }
    deps
}

/// One file's edges against a file set and name index. A record brought in
/// by graph_import has no raw edges to resolve and keeps the ones it came
/// with; every other record resolves fresh — edges stored at write time
/// were resolved against the names of that moment, and a teammate adding a
/// function since can move where a call lands.
fn resolve_file(
    path: &str, record: &FileRecord, known: &BTreeSet<String>, names: &BTreeMap<String, Vec<String>>,
) -> Vec<Resolved> {
    if record.imported {
        return record.imported_edges.clone();
    }
    let symbols: Vec<core_graph::SymbolEdges> = record
        .symbols
        .iter()
        .map(|symbol| core_graph::SymbolEdges { name: &symbol.name, edges: &symbol.edges })
        .collect();
    core_graph::resolve_edges(path, &record.language, &symbols, &record.imports, known, names)
        .into_iter()
        .map(|edge| (edge.from, edge.to, edge.kind, edge.confidence.to_string()))
        .collect()
}

/// The node an edge target stands for when no record defines it.
fn placeholder(to: &str) -> NodeData {
    // an edge can point outside the reported file set, and that is
    // information rather than an error: the node is created so the
    // dependency is visible, marked for what it is
    if let Some(name) = to.strip_prefix("ext:") {
        NodeData { path: String::new(), symbol: name.to_string(), kind: "external".into(), language: String::new(), record: None }
    } else {
        let (to_path, to_symbol) = split_node(to);
        NodeData {
            path: to_path.to_string(),
            symbol: to_symbol.to_string(),
            kind: if to_symbol.is_empty() { "file".into() } else { "unknown".into() },
            language: String::new(),
            record: None,
        }
    }
}

fn add_resolved(graph: &mut Snapshot, resolved: Vec<Resolved>) {
    for (from, to, kind, confidence) in resolved {
        if !graph.has_node(&to) {
            let data = placeholder(&to);
            graph.add_node(to.clone(), data);
        }
        graph.add_edge(from, to, EdgeData { kind, confidence });
    }
}

/// The file set and name index a graph resolves against: the records' own,
/// or — for a huge repo's working set — the whole repo's (`whole`), so an
/// import of a file outside the working set still resolves.
type Index = (BTreeSet<String>, BTreeMap<String, Vec<String>>);

/// Assemble the whole graph from scratch: every file's nodes, then every
/// file's edges in path order. Resolving is a pure function of one record
/// and the file set, so every file resolves on its own core.
fn full_build(records: BTreeMap<String, Arc<FileRecord>>, whole: Option<(Index, usize)>) -> Built {
    let (partial, (known, names)) = match whole {
        Some((index, total)) => (Some(total), index),
        None => (None, indexes(&records)),
    };
    let mut graph = Snapshot {
        files: records.len(),
        fingerprint: fingerprint(&records),
        partial,
        ..Default::default()
    };
    for (path, record) in &records {
        for (nid, data) in file_nodes(path, record) {
            graph.add_node(nid, data);
        }
    }
    use rayon::prelude::*;
    let ordered: Vec<(&String, &Arc<FileRecord>)> = records.iter().collect();
    let resolved_all: Vec<Vec<Resolved>> = ordered
        .par_iter()
        .map(|(path, record)| resolve_file(path, record, &known, &names))
        .collect();
    for resolved in resolved_all {
        add_resolved(&mut graph, resolved);
    }
    let deps = records.iter().map(|(path, record)| (path.clone(), name_deps(record))).collect();
    Built { records, known, names, deps, graph: Arc::new(graph) }
}

/// Move a built graph to a new record set, re-resolving only what the change
/// can reach: the changed files, and files that look up a name whose
/// definitions moved. The previous graph is taken, not copied: the patch
/// edits it in place, so a big repo never holds two graphs at once (a
/// holder still reading the old one is the exception, and gets its copy).
/// `Err` hands both back when the file set itself changed (a file came or
/// went — module paths can then resolve differently anywhere), which is
/// rare enough to rebuild for.
#[allow(clippy::result_large_err)]
fn patch(prev: Built, records: BTreeMap<String, Arc<FileRecord>>) -> Result<Built, (Built, BTreeMap<String, Arc<FileRecord>>)> {
    if records.len() != prev.records.len() || !records.keys().eq(prev.records.keys()) {
        return Err((prev, records));
    }
    let Built { records: old_records, known, mut names, mut deps, graph: old_graph } = prev;
    let changed: Vec<String> = records
        .iter()
        .filter(|(path, record)| !Arc::ptr_eq(record, &old_records[*path]))
        .map(|(path, _)| path.clone())
        .collect();
    // the name index, moved: names whose definition list changed are the
    // ones another file's resolution could see differently
    let mut moved: BTreeSet<String> = BTreeSet::new();
    for path in &changed {
        let before = crate::codegraph::contributions_of(&old_records[path]);
        let after = crate::codegraph::contributions_of(&records[path]);
        fn count(items: &[String]) -> BTreeMap<&str, usize> {
            let mut c: BTreeMap<&str, usize> = BTreeMap::new();
            for item in items {
                *c.entry(item.as_str()).or_default() += 1;
            }
            c
        }
        let (cb, ca) = (count(&before), count(&after));
        for name in cb.keys().chain(ca.keys()) {
            if cb.get(name) != ca.get(name) {
                moved.insert(name.to_string());
            }
        }
        for name in &before {
            if let Some(paths) = names.get_mut(name) {
                paths.retain(|p| p != path);
                if paths.is_empty() {
                    names.remove(name);
                }
            }
        }
    }
    drop(old_records);
    for path in &changed {
        for name in crate::codegraph::contributions_of(&records[path]) {
            let paths = names.entry(name).or_default();
            let at = paths.partition_point(|p| p.as_str() <= path.as_str());
            paths.insert(at, path.clone());
        }
    }
    for path in &changed {
        deps.insert(path.clone(), name_deps(&records[path]));
    }
    let mut redo: BTreeSet<String> = changed.iter().cloned().collect();
    if !moved.is_empty() {
        for (path, looked_up) in &deps {
            if !redo.contains(path) && looked_up.iter().any(|name| moved.contains(name)) {
                redo.insert(path.clone());
            }
        }
    }

    let mut graph = Arc::try_unwrap(old_graph).unwrap_or_else(|shared| shared.duplicate());
    graph.files = records.len();
    graph.fingerprint = fingerprint(&records);
    // take out what the redone files contributed: their outgoing edges, and
    // the changed files' own nodes
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for path in &redo {
        let prefix = format!("{path}::");
        let froms: Vec<String> = graph.out.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
        for from in froms {
            if let Some(edges) = graph.out.remove(&from) {
                for to in edges.into_keys() {
                    if let Some(set) = graph.incoming.get_mut(&to) {
                        set.remove(&from);
                        if set.is_empty() {
                            graph.incoming.remove(&to);
                        }
                    }
                    targets.insert(to);
                }
            }
        }
    }
    for path in &changed {
        let prefix = format!("{path}::");
        let ids: Vec<String> = graph.nodes.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
        for id in ids {
            graph.nodes.remove(&id);
            targets.insert(id);
        }
    }
    // real nodes first, as a full build adds them before any edge
    for path in &changed {
        for (nid, data) in file_nodes(path, &records[path]) {
            graph.nodes.insert(nid, data);
        }
    }
    // a node that only existed as some edge's target, and no longer is one,
    // is gone in a full build too
    for id in &targets {
        let (path, symbol) = split_node(id);
        let real = !id.starts_with("ext:") && records.get(path).is_some_and(|record| symbol.is_empty() || record.has_symbol(symbol));
        if real {
            continue;
        }
        if graph.incoming.get(id).map(BTreeSet::is_empty).unwrap_or(true) {
            graph.nodes.remove(id);
        } else if !graph.nodes.contains_key(id) {
            // a changed file's placeholder that an untouched edge still names
            graph.nodes.insert(id.clone(), placeholder(id));
        }
    }
    // then the redone files' edges, in path order
    let mut resolved: Vec<Vec<Resolved>> = Vec::new();
    for path in &redo {
        resolved.push(resolve_file(path, &records[path], &known, &names));
    }
    for batch in resolved {
        add_resolved(&mut graph, batch);
    }
    Ok(Built { records, known, names, deps, graph: Arc::new(graph) })
}

/// The whole graph, resolved against the current file set.
///
/// Built once per repo, then MOVED on each edit (see [`patch`]): an edit
/// re-resolves the file it touched and any file that looks up a name it
/// added or removed. Rebuilding the whole graph on every edit — every
/// teammate's "what changed" call paid it, and all of them at once after
/// each report — was what capped a busy repo. One caller builds while the
/// others wait for it, rather than each building the same graph.
///
/// A repo past [`crate::codegraph::resident_limit`] files is assembled from
/// its working set (the files most recently written or read) against the
/// whole repo's name index: memory follows the work, not the repo's size.
pub fn snapshot(store: &Store, scope: &str) -> Arc<Snapshot> {
    // the cheap test first: the graph rows' count and newest write. Only when
    // they moved is anything read (and then only what changed, see
    // codegraph::records_shared) and the graph moved
    let stat = store.kv_stat(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:"));
    let stat_key = format!("{}:{}", stat.0, stat.1);
    let stamp = crate::store::now();
    let cache_key = store.cache_key(scope);
    crate::membudget::touch(&cache_key);
    let fresh = |stat_key: &str| -> Option<Arc<Snapshot>> {
        let cache = snapshot_cache().lock().ok()?;
        let (seen, graph, at) = cache.get(&cache_key)?;
        (seen == stat_key && stamp - at < SNAPSHOT_TTL_S).then(|| Arc::clone(graph))
    };
    if let Some(graph) = fresh(&stat_key) {
        return graph;
    }
    let slot = built_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cache_key.clone())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone();
    let mut built = slot.lock().unwrap_or_else(|e| e.into_inner());
    // whoever held the slot may have just built this very state
    let stat = store.kv_stat(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:"));
    let stat_key = format!("{}:{}", stat.0, stat.1);
    if let Some(graph) = fresh(&stat_key) {
        return graph;
    }
    let records = records(store, scope);
    let (total, partial) = crate::codegraph::file_count(store, scope);
    // the cache's copy of the old graph goes first, so the patch below can
    // take the graph instead of copying it
    if let Ok(mut cache) = snapshot_cache().lock() {
        cache.remove(&cache_key);
    }
    let whole = || partial.then(|| (crate::codegraph::with_index(store, scope, |k, n| (k.clone(), n.clone())), total));
    let next = match built.take() {
        Some(prev) => match patch(prev, records) {
            Ok(next) => next,
            Err((prev, records)) => {
                // the old graph leaves memory before the new one is built
                drop(prev);
                full_build(records, whole())
            }
        },
        None => full_build(records, whole()),
    };
    let graph = Arc::clone(&next.graph);
    let bytes = next.graph.approx_bytes()
        + next.deps.iter().map(|(p, d)| 64 + p.len() + d.iter().map(|n| 32 + n.len()).sum::<usize>()).sum::<usize>()
        + next.names.iter().map(|(n, ps)| 72 + n.len() + ps.iter().map(|p| 24 + p.len()).sum::<usize>()).sum::<usize>();
    *built = Some(next);
    drop(built);
    if let Ok(mut cache) = snapshot_cache().lock() {
        cache.insert(cache_key.clone(), (stat_key, Arc::clone(&graph), stamp));
    }
    crate::membudget::report(&cache_key, crate::membudget::Part::Graph, bytes);
    graph
}

/// A full build of the same records, for tests that hold a patch to it.
#[cfg(test)]
fn rebuilt(store: &Store, scope: &str) -> Arc<Snapshot> {
    full_build(records(store, scope), None).graph
}

/// The partition, cached on the same fingerprint. Louvain is the single most
/// expensive thing computed here, and `repo_map` — which every briefing
/// carries — is what calls it.
pub fn communities_cached(
    graph: &Snapshot, scope: &str, co_change: &BTreeMap<(String, String), i64>,
) -> Arc<Vec<Community>> {
    let key = format!("{}:{}", graph.fingerprint, co_change.len());
    if let Ok(cache) = community_cache().lock() {
        if let Some((seen, comms)) = cache.get(scope) {
            if seen == &key {
                return Arc::clone(comms);
            }
        }
    }
    let comms = Arc::new(communities(graph, co_change));
    if let Ok(mut cache) = community_cache().lock() {
        cache.insert(scope.to_string(), (key, Arc::clone(&comms)));
    }
    comms
}

/// Drop the snapshots (and partitions) no repo has asked for in `idle_s`.
pub fn evict_idle(idle_s: f64) -> usize {
    let cutoff = crate::store::now() - idle_s;
    let mut kept: Vec<String> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    if let Ok(mut cache) = snapshot_cache().lock() {
        cache.retain(|key, entry| {
            let keep = entry.2 >= cutoff;
            if !keep {
                gone.push(key.clone());
            }
            keep
        });
        kept = cache.keys().cloned().collect();
    }
    // the patch state goes with its snapshot: it holds the same graph
    if let Ok(mut built) = built_cache().lock() {
        for key in &gone {
            built.remove(key);
        }
    }
    if let Ok(mut cache) = community_cache().lock() {
        cache.retain(|key, _| kept.iter().any(|k| k.ends_with(&format!("#{key}")) || k == key));
    }
    gone.len()
}

/// Drop one repo's graph, its patch state and its partition (the memory
/// budget's eviction).
pub fn evict_key(key: &str) {
    if let Ok(mut cache) = snapshot_cache().lock() {
        cache.remove(key);
    }
    if let Ok(mut built) = built_cache().lock() {
        built.remove(key);
    }
    let scope = key.split_once('#').map(|(_, s)| s).unwrap_or(key);
    if let Ok(mut cache) = community_cache().lock() {
        cache.remove(scope);
    }
}

/// Drop every repo's graph (memory pressure).
pub fn evict_all() -> usize {
    let mut dropped = 0;
    if let Ok(mut cache) = snapshot_cache().lock() {
        dropped = cache.len();
        cache.clear();
    }
    if let Ok(mut built) = built_cache().lock() {
        built.clear();
    }
    if let Ok(mut cache) = community_cache().lock() {
        cache.clear();
    }
    dropped
}

/// Any write to the graph invalidates both. `update_file` calls this.
pub fn invalidate(store: &Store, scope: &str) {
    if let Ok(mut cache) = snapshot_cache().lock() {
        cache.remove(&store.cache_key(scope));
    }
    if let Ok(mut cache) = community_cache().lock() {
        cache.remove(scope);
    }
}

/// Exact node, else a symbol match by bare name within the path, else by name
/// anywhere — and only when exactly one candidate matches. An ambiguous match
/// resolves to nothing rather than to a guess.
pub fn find_node(graph: &Snapshot, path: &str, symbol: &str) -> Option<String> {
    let exact = node_id(path, symbol);
    if graph.has_node(&exact) {
        return Some(exact);
    }
    fn bare(symbol: &str) -> &str {
        symbol.rsplit("::").next().unwrap_or(symbol).rsplit('.').next().unwrap_or(symbol)
    }
    if !symbol.is_empty() {
        let in_path: Vec<&String> = graph
            .nodes
            .iter()
            .filter(|(_, data)| data.path == path && bare(&data.symbol) == symbol)
            .map(|(nid, _)| nid)
            .collect();
        if in_path.len() == 1 {
            return Some(in_path[0].clone());
        }
        let anywhere: Vec<&String> = graph
            .nodes
            .iter()
            .filter(|(_, data)| data.symbol == symbol || bare(&data.symbol) == symbol)
            .map(|(nid, _)| nid)
            .collect();
        if anywhere.len() == 1 {
            return Some(anywhere[0].clone());
        }
        return None;
    }
    let by_suffix: Vec<&String> = graph
        .nodes
        .iter()
        .filter(|(_, data)| {
            data.kind == "file" && (data.path == path || data.path.ends_with(&format!("/{path}")))
        })
        .map(|(nid, _)| nid)
        .collect();
    if by_suffix.len() == 1 { Some(by_suffix[0].clone()) } else { None }
}

/// Directory names that mean "this tree is scaffolding", in every language
/// the parser reads. A segment match, not a prefix: server/tests/ and
/// src/test/java/ and web/src/__tests__/ all land here.
const TEST_DIRS: [&str; 6] = ["tests", "test", "__tests__", "spec", "specs", "testing"];

/// Does this path hold test scaffolding?
///
/// Convention only — the graph never reads a file's contents — but the
/// conventions of all twelve languages the parser covers: a tests/ (test/,
/// spec/, specs/, `__tests__`/, testing/) directory anywhere above the file;
/// pytest's `test_*.py` and `conftest.py`; Go's `*_test.go`, Rust's
/// `*_test.rs`, RSpec's `*_spec.rb`; Jest/Vitest's `*.test.ts` and
/// `*.spec.tsx`; JUnit/xUnit's `FooTest.java` and `FooTests.cs`.
///
/// The CamelCase suffixes are case-sensitive on purpose: `contest.py`,
/// `latest.ts` and `manifest.rs` are not tests.
pub fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or("");
    if parts.iter().any(|part| TEST_DIRS.contains(part)) {
        return true;
    }
    if name == "conftest.py" {
        return true;
    }
    let stem = name.split_once('.').map(|(head, _)| head).unwrap_or(name);
    if stem.starts_with("test_")
        || stem.starts_with("test-")
        || stem.ends_with("_test")
        || stem.ends_with("-test")
        || stem.ends_with("_spec")
        || stem.ends_with("-spec")
    {
        return true;
    }
    if name.contains(".test.") || name.contains(".spec.") {
        return true;
    }
    stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("Spec")
        || stem.ends_with("Specs")
}

/// One candidate hub, with both rankings it can be read under.
struct HubRow {
    /// -(code_dependents * 2 + depends_on): what the SHIPPED code leans on
    by_product: i64,
    /// -(dependents * 2 + depends_on): the unfiltered connectivity ranking
    by_total: i64,
    id: String,
    test: bool,
    row: Value,
}

/// Every symbol something leans on, with its dependents split into the code
/// that ships and the test suite. The split is the whole point: a fixture
/// with 242 dependents and a class with 229 are not the same fact, and only
/// one of them says anything about the product.
fn hub_rows(graph: &Snapshot) -> Vec<HubRow> {
    let mut rows: Vec<HubRow> = Vec::new();
    for (nid, data) in &graph.nodes {
        if !internal(nid) || data.kind == "file" {
            continue;
        }
        let (dependents, depends_on) = (graph.in_degree(nid), graph.out_degree(nid));
        if dependents + depends_on == 0 {
            continue;
        }
        let from_tests = graph
            .incoming
            .get(nid)
            .map(|preds| {
                preds
                    .iter()
                    .filter(|pred| {
                        is_test_path(graph.nodes.get(*pred).map(|d| d.path.as_str()).unwrap_or(""))
                    })
                    .count()
            })
            .unwrap_or(0);
        let code = dependents - from_tests;
        let test = is_test_path(&data.path);
        let mut row = serde_json::Map::new();
        row.insert("id".into(), json!(nid));
        row.insert("path".into(), json!(data.path));
        row.insert("symbol".into(), json!(data.symbol));
        row.insert("kind".into(), json!(data.kind));
        row.insert("dependents".into(), json!(dependents));
        row.insert("depends_on".into(), json!(depends_on));
        row.insert("code_dependents".into(), json!(code));
        row.insert("test_dependents".into(), json!(from_tests));
        if test {
            row.insert("test".into(), json!(true));
        }
        rows.push(HubRow {
            by_product: -((code * 2 + depends_on) as i64),
            by_total: -((dependents * 2 + depends_on) as i64),
            id: nid.clone(),
            test,
            row: Value::Object(row),
        });
    }
    rows
}

/// The connectivity ranking with nothing taken out — every dependent counts
/// the same, wherever it lives. `hub_lists` is what a reader wants; this is
/// what it measures itself against.
pub fn god_nodes(graph: &Snapshot, limit: usize) -> Vec<Value> {
    let mut rows = hub_rows(graph);
    rows.sort_by(|a, b| a.by_total.cmp(&b.by_total).then(a.id.cmp(&b.id)));
    rows.into_iter().take(limit).map(|row| row.row).collect()
}

/// The two hub lists and what it took to separate them.
pub struct HubLists {
    pub hubs: Vec<Value>,
    pub test_hubs: Vec<Value>,
    pub test_hubs_omitted: usize,
    pub scope: &'static str,
}

/// What this repo leans on — answered about the repo, not about its test
/// suite.
///
/// Test scaffolding depends on everything and ships nothing, so left in it
/// wins the ranking outright: on Collide's own map five of the six most
/// depended-on symbols were fixtures. So the default list is product symbols
/// ranked by the dependents that are themselves product code, and the test
/// suite gets its own list rather than being deleted from the answer.
///
/// A repo that is nothing but tests still gets a hub list — the unfiltered
/// one, flagged `scope: "all"`, because an empty panel is a worse answer than
/// a candid one.
pub fn hub_lists(graph: &Snapshot, limit: usize) -> HubLists {
    let mut rows = hub_rows(graph);
    rows.sort_by(|a, b| a.by_total.cmp(&b.by_total).then(a.id.cmp(&b.id)));
    let mut product: Vec<&HubRow> = rows.iter().filter(|row| !row.test).collect();
    // every symbol here is scaffolding. Say so and hand back the unfiltered
    // ranking — an empty panel would be the worse answer. A graph with
    // nothing in it at all is not this case and stays "product": no
    // filtering happened, so none needs admitting.
    if product.is_empty() && !rows.is_empty() {
        return HubLists {
            hubs: rows.iter().take(limit).map(|row| row.row.clone()).collect(),
            test_hubs: Vec::new(),
            test_hubs_omitted: 0,
            scope: "all",
        };
    }
    product.sort_by(|a, b| a.by_product.cmp(&b.by_product).then(a.id.cmp(&b.id)));
    HubLists {
        hubs: product.into_iter().take(limit).map(|row| row.row.clone()).collect(),
        test_hubs: rows.iter().filter(|row| row.test).take(limit).map(|row| row.row.clone()).collect(),
        test_hubs_omitted: rows.iter().take(limit).filter(|row| row.test).count(),
        scope: "product",
    }
}

// ------------------------------------------------------------ communities

pub struct Community {
    pub id: usize,
    pub label: String,
    /// Unique within one map: the dominant directory, qualified by the top
    /// hub when two subsystems share that directory.
    pub name: String,
    /// The dominant directory itself, unqualified.
    pub dir: String,
    pub members: Vec<String>,
    pub hubs: Vec<String>,
    /// Display names of the top three hubs, in rank order — what the name and
    /// the label are built from.
    pub hub_names: Vec<String>,
    pub files: Vec<String>,
}

fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(cut) => path[..cut].to_string(),
        None => ".".to_string(),
    }
}

/// The weighted undirected projection Louvain clusters over: symbols only,
/// because file nodes would bridge every subsystem to every other one and
/// wash the partition out.
fn weighted_undirected(
    graph: &Snapshot, co_change: &BTreeMap<(String, String), i64>,
) -> (Vec<(String, String, f64)>, Vec<String>) {
    let members: BTreeSet<&String> = graph
        .nodes
        .iter()
        .filter(|(nid, data)| internal(nid) && data.kind != "file")
        .map(|(nid, _)| nid)
        .collect();

    // the clustering's own weights: real numbers, because each relation
    // contributes a different amount
    let mut weights: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut bump = |a: &str, b: &str, w: f64| {
        // undirected: one entry per unordered pair, keyed in a fixed order
        let key = if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        };
        *weights.entry(key).or_insert(0.0) += w;
    };

    for (from, targets) in &graph.out {
        if !members.contains(from) {
            continue;
        }
        for (to, edge) in targets {
            if members.contains(to) {
                bump(from, to, weight_of(&edge.kind));
            }
        }
    }
    for ((a, b), count) in co_change {
        if members.contains(a) && members.contains(b) {
            bump(a, b, weight_of("co_change") * (*count as f64));
        }
    }

    let touched: BTreeSet<&String> =
        weights.keys().flat_map(|(a, b)| [a, b]).collect();
    let isolated: Vec<String> = members
        .iter()
        .filter(|nid| !touched.contains(**nid))
        .map(|nid| (*nid).clone())
        .collect();
    let edges: Vec<(String, String, f64)> =
        weights.into_iter().map(|((a, b), w)| (a, b, w)).collect();
    (edges, isolated)
}

/// Louvain over the weighted symbol graph — the subsystems. Each carries a
/// name unique within the map (its dominant directory, qualified by its top
/// hub when two clusters share that directory) and the symbols with the most
/// weight inside it, which together read as a name a person recognises.
pub fn communities(
    graph: &Snapshot, co_change: &BTreeMap<(String, String), i64>,
) -> Vec<Community> {
    let (edges, isolated) = weighted_undirected(graph, co_change);
    if edges.is_empty() && isolated.is_empty() {
        return Vec::new();
    }

    // node -> total incident weight, for ranking hubs inside a community
    let mut strength: BTreeMap<&String, f64> = BTreeMap::new();
    for (a, b, w) in &edges {
        *strength.entry(a).or_insert(0.0) += w;
        *strength.entry(b).or_insert(0.0) += w;
    }

    let parts = if edges.is_empty() {
        isolated.iter().map(|nid| vec![nid.clone()]).collect()
    } else {
        collide_core::community::louvain(&edges, LOUVAIN_RESOLUTION, LOUVAIN_SEED, &isolated)
    };

    let mut out: Vec<Community> = Vec::new();
    for part in parts {
        let mut members = part;
        members.sort();
        if members.is_empty() {
            continue;
        }
        let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
        let mut dir_order: Vec<String> = Vec::new();
        for member in &members {
            let dir = dirname(graph.nodes.get(member).map(|d| d.path.as_str()).unwrap_or(""));
            if !dirs.contains_key(&dir) {
                dir_order.push(dir.clone());
            }
            *dirs.entry(dir).or_insert(0) += 1;
        }
        // most members wins; ties go to the directory seen first, which is
        // what Python's Counter does
        let top_dir = dir_order
            .iter()
            .max_by_key(|dir| (dirs[*dir], std::cmp::Reverse(dir_order.iter().position(|d| d == *dir))))
            .cloned()
            .unwrap_or_else(|| ".".into());

        let mut ranked = members.clone();
        ranked.sort_by(|a, b| {
            let weight = |nid: &String| strength.get(nid).copied().unwrap_or(0.0);
            weight(b)
                .partial_cmp(&weight(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(b))
        });
        let hub_names: Vec<String> = ranked
            .iter()
            .take(3)
            .filter_map(|nid| {
                graph.nodes.get(nid).map(|data| {
                    if data.symbol.is_empty() { data.path.clone() } else { data.symbol.clone() }
                })
            })
            .filter(|name| !name.is_empty())
            .collect();
        let files: BTreeSet<String> = members
            .iter()
            .map(|nid| graph.nodes.get(nid).map(|d| d.path.clone()).unwrap_or_default())
            .collect();

        out.push(Community {
            id: 0,
            label: String::new(),
            name: String::new(),
            dir: top_dir,
            hubs: ranked.into_iter().take(5).collect(),
            hub_names,
            files: files.into_iter().collect(),
            members,
        });
    }
    // biggest subsystem first, because that is the one an agent orients by
    out.sort_by_key(|community| std::cmp::Reverse(community.members.len()));
    for (index, community) in out.iter_mut().enumerate() {
        community.id = index;
    }
    // after the ids exist, because the last-resort tail is one of them
    name_communities(&mut out);
    out
}

/// Give every subsystem a name that is unique inside one map, in place.
///
/// The dominant directory alone is not a name. Two Louvain communities can
/// both sit mostly in one folder, and the map then prints what looks like the
/// same row twice with different counts — the reader reads a counting bug
/// where there is none.
///
/// Extending the path cannot fix it: the dominant directory is already a full
/// dirname, so a collision means both clusters live in the SAME folder, not
/// in a shared parent — there is no deeper path to extend to. What actually
/// tells them apart is what each one is built around, so an ambiguous
/// directory is qualified with that cluster's top hub:
///
/// ```text
/// server/tests · call        (the fixtures every suite imports)
/// server/tests · paired      (the differential harness)
/// ```
///
/// Deterministic, because everything it reads is: the partition is seeded,
/// hubs are ranked with an explicit tie-break, and the last-resort numeric
/// tail is the community's own id. Two subsystems can only reach that tail by
/// sharing both a directory and a top hub name.
pub fn name_communities(comms: &mut [Community]) {
    let mut shared: BTreeMap<String, usize> = BTreeMap::new();
    for community in comms.iter() {
        *shared.entry(community.dir.clone()).or_insert(0) += 1;
    }
    let mut used: BTreeSet<String> = BTreeSet::new();
    for community in comms.iter_mut() {
        let ambiguous = shared.get(&community.dir).copied().unwrap_or(0) > 1;
        let qualifier = if ambiguous {
            community.hub_names.first().cloned().unwrap_or_default()
        } else {
            String::new()
        };
        let mut name = if qualifier.is_empty() {
            community.dir.clone()
        } else {
            format!("{} · {qualifier}", community.dir)
        };
        if used.contains(&name) {
            name = format!("{name} #{}", community.id);
        }
        used.insert(name.clone());
        // the qualifier already names the top hub; repeating it in the label
        // would read as a stutter
        let rest: &[String] = if qualifier.is_empty() {
            &community.hub_names
        } else {
            &community.hub_names[1..]
        };
        let label =
            if rest.is_empty() { name.clone() } else { format!("{name}: {}", rest.join(", ")) };
        community.label = label.chars().take(120).collect();
        community.name = name;
    }
}

// --------------------------------------------------------------- overlays

/// What makes this Collide's graph rather than a static map: who owns each
/// region, what is leased right now, and what was tried and settled there.
#[derive(Default)]
pub struct Overlays {
    /// node -> edits per person, in the order the ledger first saw each
    pub owners: BTreeMap<String, Vec<(String, i64)>>,
    pub in_flight: BTreeMap<String, Vec<Value>>,
    pub memories: BTreeMap<String, MemorySlot>,
    pub breaking: BTreeMap<String, Value>,
    /// The order each map was discovered in. Ties in every ranking below fall
    /// back to it, because the Python side's dicts preserve insertion order
    /// and a stable sort leaves equal elements in it — sorting ties by name
    /// instead would be just as deterministic and disagree on every one.
    pub in_flight_order: Vec<String>,
    pub breaking_order: Vec<String>,
    /// An episode COUNT, so it is serialised as the integer it is. Only the
    /// clustering promotes it to a weight.
    pub co_change: BTreeMap<(String, String), i64>,
    /// node -> how many reported edits landed on it inside the window.
    pub touches: BTreeMap<String, i64>,
}

#[derive(Default, Clone)]
pub struct MemorySlot {
    pub notes: i64,
    pub scars: i64,
    pub rationales: i64,
    pub latest: String,
    pub latest_ts: f64,
}

impl MemorySlot {
    fn to_value(&self) -> Value {
        json!({
            "notes": self.notes, "scars": self.scars, "rationales": self.rationales,
            "latest": self.latest, "latest_ts": self.latest_ts,
        })
    }
}

fn tally_add(tally: &mut Vec<(String, i64)>, user: &str) {
    match tally.iter_mut().find(|(name, _)| name == user) {
        Some((_, count)) => *count += 1,
        None => tally.push((user.to_string(), 1)),
    }
}

/// Half an hour: long enough to cover one piece of work, short enough that
/// two unrelated tasks in the same session are not fused into one episode.
const CO_CHANGE_WINDOW_S: f64 = 30.0 * 60.0;
/// A single episode touching more than this is a sweep, not a co-change, and
/// linking every pair in it would connect the whole repo to itself.
const CO_CHANGE_MAX_GROUP: usize = 60;

/// Symbols edited together: the same intent, else the same person's session
/// inside a window. The weight is how many such episodes there were.
///
/// This is the edge no static indexer can have. Two symbols with no syntactic
/// relationship that always move together are related, and the only place
/// that shows is the write path.
pub fn co_change_from_rows(
    rows: &[crate::store::LedgerRow],
) -> BTreeMap<(String, String), i64> {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut session_bucket: BTreeMap<(String, String), (f64, String)> = BTreeMap::new();

    for row in rows {
        if row.kind != "edit_reported" {
            continue;
        }
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
        let changed = row.payload.get("symbols_changed").and_then(Value::as_object);
        let Some(changed) = changed.filter(|c| !c.is_empty()) else { continue };
        if path.is_empty() {
            continue;
        }
        let nodes: BTreeSet<String> =
            changed.keys().map(|symbol| node_id(path, symbol)).collect();

        let intent = row.payload.get("intent_id").and_then(Value::as_str).unwrap_or("");
        let key = if !intent.is_empty() {
            format!("intent:{intent}")
        } else {
            let who = (
                row.payload.get("user").and_then(Value::as_str).unwrap_or("").to_string(),
                row.payload.get("session").and_then(Value::as_str).unwrap_or("").to_string(),
            );
            let (start, key) =
                session_bucket.get(&who).cloned().unwrap_or((0.0, String::new()));
            if key.is_empty() || row.ts - start > CO_CHANGE_WINDOW_S {
                let fresh = format!("session:{}:{}:{}", who.0, who.1, row.ts as i64);
                session_bucket.insert(who, (row.ts, fresh.clone()));
                fresh
            } else {
                key
            }
        };
        groups.entry(key).or_default().extend(nodes);
    }

    let mut weights: BTreeMap<(String, String), i64> = BTreeMap::new();
    for members in groups.values() {
        if members.len() < 2 || members.len() > CO_CHANGE_MAX_GROUP {
            continue;
        }
        let members: Vec<&String> = members.iter().collect();
        for (index, a) in members.iter().enumerate() {
            for b in &members[index + 1..] {
                *weights.entry(((*a).clone(), (*b).clone())).or_insert(0) += 1;
            }
        }
    }
    weights
}

/// node -> who has been editing it, from the ledger. Ownership is observed,
/// never declared: the person who keeps touching a symbol owns it whatever
/// any file header says.
pub fn ownership_from_rows(rows: &[crate::store::LedgerRow]) -> BTreeMap<String, Vec<(String, i64)>> {
    let mut owners: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
    for row in rows {
        if row.kind != "edit_reported" {
            continue;
        }
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
        let user = row.payload.get("user").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() || user.is_empty() {
            continue;
        }
        tally_add(owners.entry(node_id(path, "")).or_default(), user);
        if let Some(changed) = row.payload.get("symbols_changed").and_then(Value::as_object) {
            for symbol in changed.keys() {
                tally_add(owners.entry(node_id(path, symbol)).or_default(), user);
            }
        }
    }
    owners
}

/// node -> how many reported edits landed on it inside the window.
///
/// The shading on the dashboard's graph: the same ledger rows ownership is
/// read from, counted rather than attributed. Every edit counts, including
/// the ones that carry no user — a touch is a touch whoever made it.
///
/// Attribution is exact. A row's `symbols_changed` names the symbols that
/// actually moved, and only those nodes count, alongside the file node the
/// edit landed in. An edit that named no symbol is a touch on its file and on
/// nothing else: spreading it across every symbol in the file would shade a
/// whole subsystem for one line moved somewhere in it.
pub fn touches_from_rows(rows: &[crate::store::LedgerRow]) -> BTreeMap<String, i64> {
    let mut touches: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        if row.kind != "edit_reported" {
            continue;
        }
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        *touches.entry(node_id(path, "")).or_insert(0) += 1;
        if let Some(changed) = row.payload.get("symbols_changed").and_then(Value::as_object) {
            for symbol in changed.keys() {
                *touches.entry(node_id(path, symbol)).or_insert(0) += 1;
            }
        }
    }
    touches
}

/// node -> the live intents leased over it. A claim is a lease with a TTL,
/// which is why this can be shown as fact: it either holds now or it does not.
fn str_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A claim older than this without an edit landing counts as fully stale.
pub const PRESSURE_STALE_S: f64 = 900.0;
/// Collisions on a file before this many stop raising its pressure further.
pub const PRESSURE_HISTORY_DIV: f64 = 4.0;

/// Live risk per node, 0 to 1, from the intents open on it: how many agents
/// converge on it (one is cold, two on one symbol are warm at once), how
/// close the others are on the graph (a dependent one hop out weighs 0.6, two
/// hops 0.3), how long the claims have sat without an edit landing, and how
/// often the file has collided before. `1 - exp(-crowd * staleness * history
/// / 2)`, rounded to two decimals, computed identically by Python's
/// `pressure_from_intents` so both halves colour the same squares. Red on the
/// grid stays for a change that actually broke an interface; this is the
/// yellow that builds before one does.
pub fn pressure_from_intents(
    intents: &[Value], graph: &Snapshot, collisions_by_path: &BTreeMap<String, i64>, now: f64,
) -> BTreeMap<String, f64> {
    // (agent, age since its last landed edit or since the claim) per node
    let mut claims: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for intent in intents {
        let owner = str_of(intent, "owner");
        let session = str_of(intent, "session");
        let agent = if session.is_empty() { owner.clone() } else { format!("{owner}#{session}") };
        let created = intent.get("created").and_then(Value::as_f64).unwrap_or(now);
        let reference = intent.get("reference_ts").and_then(Value::as_f64).unwrap_or(created);
        let age = (now - reference).max(0.0);
        let symbols: Vec<String> = intent
            .get("symbols")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        for path in intent.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            for symbol in &symbols {
                let nid = find_node(graph, path, symbol).unwrap_or_else(|| node_id(path, symbol));
                claims.entry(nid).or_default().push((agent.clone(), age));
            }
            claims.entry(node_id(path, "")).or_default().push((agent.clone(), age));
        }
    }
    let agents_on = |nid: &str| -> BTreeSet<String> {
        claims.get(nid).map(|v| v.iter().map(|(a, _)| a.clone()).collect()).unwrap_or_default()
    };
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for (nid, mine) in &claims {
        let here = agents_on(nid);
        let mut counted = here.clone();
        let mut crowd = (here.len() as f64 - 1.0).max(0.0);
        for (depth, level) in blast_radius(graph, nid, 2).iter().enumerate() {
            let weight = if depth == 0 { 0.6 } else { 0.3 };
            for node in level {
                for agent in agents_on(&str_of(node, "id")) {
                    if counted.insert(agent) {
                        crowd += weight;
                    }
                }
            }
        }
        if crowd <= 0.0 {
            continue;
        }
        let age = mine.iter().map(|(_, a)| *a).fold(0.0_f64, f64::max);
        let staleness = 1.0 + (age / PRESSURE_STALE_S).min(1.0);
        let path = graph
            .nodes
            .get(nid)
            .map(|n| n.path.clone())
            .unwrap_or_else(|| nid.split("::").next().unwrap_or("").to_string());
        let history = 1.0 + (*collisions_by_path.get(&path).unwrap_or(&0) as f64 / PRESSURE_HISTORY_DIV).min(1.0);
        let pressure = 1.0 - (-(crowd * staleness * history) / 2.0).exp();
        out.insert(nid.clone(), crate::compat::python_round(pressure, 2));
    }
    out
}

pub fn in_flight_from_intents(
    intents: &[Value], graph: &Snapshot,
) -> (BTreeMap<String, Vec<Value>>, Vec<String>) {
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    macro_rules! claim {
        ($nid:expr, $brief:expr) => {{
            let nid: String = $nid;
            if !out.contains_key(&nid) {
                order.push(nid.clone());
            }
            out.entry(nid).or_default().push($brief);
        }};
    }
    for intent in intents {
        let operations: Vec<String> = intent
            .get("operations")
            .and_then(Value::as_array)
            .map(|ops| {
                ops.iter()
                    .take(6)
                    .map(|op| {
                        format!(
                            "{} {}",
                            op.get("op").and_then(Value::as_str).unwrap_or(""),
                            op.get("symbol").and_then(Value::as_str).unwrap_or(""),
                        )
                        .trim()
                        .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let brief = json!({
            "intent_id": intent.get("intent_id").cloned().unwrap_or(Value::Null),
            "owner": intent.get("owner").cloned().unwrap_or(Value::Null),
            "agent": intent.get("agent").and_then(Value::as_str).unwrap_or(""),
            "summary": intent.get("summary").and_then(Value::as_str).unwrap_or("")
                .chars().take(120).collect::<String>(),
            "operations": operations,
        });

        let symbols: BTreeSet<String> = intent
            .get("symbols")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let paths: Vec<String> = intent
            .get("paths")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        for path in &paths {
            for symbol in &symbols {
                let nid = find_node(graph, path, symbol).unwrap_or_else(|| node_id(path, symbol));
                claim!(nid, brief.clone());
            }
            // the file is claimed too, so an intent that names no symbols
            // still shows up over the region it covers
            claim!(node_id(path, ""), brief.clone());
        }
    }
    (out, order)
}

/// node -> what is remembered about it. A scar is the expensive one: it says
/// this was tried and reverted, which is the thing a fresh agent will
/// otherwise cheerfully try again.
pub fn memories_by_node(memories: &[Value]) -> BTreeMap<String, MemorySlot> {
    let mut out: BTreeMap<String, MemorySlot> = BTreeMap::new();
    for memory in memories {
        let Some(anchor) = memory.get("anchor").filter(|a| !a.is_null()) else { continue };
        let kind = anchor.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != "symbol" && kind != "file" {
            continue; // a directory anchor covers no single node
        }
        let nid = node_id(
            anchor.get("path").and_then(Value::as_str).unwrap_or(""),
            anchor.get("symbol").and_then(Value::as_str).unwrap_or(""),
        );
        let slot = out.entry(nid).or_default();
        slot.notes += 1;
        let auto = memory.get("auto").and_then(Value::as_str).unwrap_or("");
        if auto == "scar" {
            slot.scars += 1;
        }
        let tagged = memory
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| tags.iter().any(|tag| tag.as_str() == Some("rationale")))
            .unwrap_or(false);
        if auto == "rationale" || tagged {
            slot.rationales += 1;
        }
        let created = memory.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        if created > slot.latest_ts {
            slot.latest_ts = created;
            slot.latest = memory
                .get("fact")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(160)
                .collect();
        }
    }
    out
}

/// Attach the overlay facts to node rows in place.
fn decorate(nodes: Vec<Value>, overlays: &Overlays) -> Vec<Value> {
    nodes
        .into_iter()
        .map(|mut node| {
            let Some(nid) = node.get("id").and_then(Value::as_str).map(str::to_string) else {
                return node;
            };
            let Some(map) = node.as_object_mut() else { return node };
            if let Some(tally) = overlays.owners.get(&nid).filter(|t| !t.is_empty()) {
                // most edits wins, ties to whoever the ledger saw first
                let top = tally
                    .iter()
                    .enumerate()
                    .max_by_key(|(position, (_, count))| (*count, std::cmp::Reverse(*position)))
                    .map(|(_, (user, _))| user.clone())
                    .unwrap_or_default();
                map.insert("owner".into(), json!(top));
                map.insert("edits".into(), json!(tally.iter().map(|(_, n)| n).sum::<i64>()));
            }
            if let Some(intents) = overlays.in_flight.get(&nid) {
                map.insert("in_flight".into(), json!(intents));
            }
            if let Some(slot) = overlays.memories.get(&nid) {
                map.insert("memory".into(), slot.to_value());
            }
            node
        })
        .collect()
}

// --------------------------------------------------------------- repo map

/// The compact orientation an agent reads instead of grepping.
///
/// This is the piece the whole economic argument rests on. It is small enough
/// to ride in every briefing, and every teammate's reported edit sharpens it —
/// so the more agents working a repo, the cheaper it is for the next one to
/// know where it is, instead of each paying to walk the tree again.
/// The first path segment as a directory label; root files are `./`.
pub fn top_dir(path: &str) -> String {
    match path.split_once('/') {
        Some((head, _)) => format!("{head}/"),
        None => "./".to_string(),
    }
}

/// Top-level directories of the repo that contain none of `files`.
pub fn absent_dirs(graph: &Snapshot, files: &std::collections::BTreeSet<String>) -> Vec<String> {
    let present: std::collections::BTreeSet<String> = files.iter().map(|f| top_dir(f)).collect();
    let every: std::collections::BTreeSet<String> = graph
        .nodes
        .values()
        .filter(|data| data.kind == "file" && !data.path.is_empty())
        .map(|data| top_dir(&data.path))
        .collect();
    every.difference(&present).cloned().collect()
}

/// Shapes and exceptions: per top-level directory, the first-parameter
/// convention its functions follow and the ones that do not.
pub fn conventions(graph: &Snapshot, budget: usize) -> Vec<Value> {
    let mut by_dir: std::collections::BTreeMap<String, Vec<(String, String)>> = std::collections::BTreeMap::new();
    for (nid, data) in &graph.nodes {
        if !internal(nid) || matches!(data.kind.as_str(), "file" | "external" | "unknown" | "class") {
            continue;
        }
        let params = data.params();
        let Some(first) = params.as_array().and_then(|p| p.first()) else { continue };
        if data.path.is_empty() {
            continue;
        }
        let first = match first {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        by_dir.entry(top_dir(&data.path)).or_default().push((nid.clone(), first));
    }
    let mut rules: Vec<Value> = Vec::new();
    for (dir, members) in &by_dir {
        if members.len() < 3 {
            continue;
        }
        // most common first parameter; ties break toward the earliest seen,
        // as Counter.most_common does
        let mut tally: Vec<(String, usize)> = Vec::new();
        for (_nid, first) in members {
            match tally.iter_mut().find(|(name, _)| name == first) {
                Some((_, count)) => *count += 1,
                None => tally.push((first.clone(), 1)),
            }
        }
        let Some((first, holds)) = tally.iter().max_by(|a, b| a.1.cmp(&b.1)).cloned() else { continue };
        let (first, holds) = {
            // max_by returns the LAST max on ties; Counter.most_common the first
            let top = tally.iter().filter(|(_, c)| *c == holds).next().cloned().unwrap_or((first, holds));
            top
        };
        if (holds as f64) / (members.len() as f64) < 0.6 {
            continue;
        }
        let mut exceptions: Vec<String> = members.iter().filter(|(_, f)| *f != first).map(|(nid, _)| nid.clone()).collect();
        exceptions.sort();
        exceptions.truncate(8);
        rules.push(json!({"dir": dir, "rule": format!("first parameter `{first}`"), "holds": holds, "of": members.len(),
                          "exceptions": exceptions}));
        if rules.len() == budget {
            break;
        }
    }
    rules
}

pub fn repo_map(
    graph: &Snapshot, comms: &[Community], overlays: &Overlays, budget: usize,
) -> Value {
    let mut languages: Vec<(String, i64)> = Vec::new();
    for data in graph.nodes.values() {
        if data.kind == "file" && !data.language.is_empty() {
            tally_add(&mut languages, &data.language);
        }
    }
    languages.sort_by_key(|(name, count)| (-count, name.clone()));

    let symbol_count = graph
        .nodes
        .iter()
        .filter(|(nid, data)| {
            internal(nid) && !matches!(data.kind.as_str(), "file" | "external" | "unknown")
        })
        .count();
    let edge_count = graph.internal_edge_count();

    let owner_of = |members: &[String]| -> String {
        let mut tally: Vec<(String, i64)> = Vec::new();
        for member in members {
            for (user, count) in overlays.owners.get(member).into_iter().flatten() {
                match tally.iter_mut().find(|(name, _)| name == user) {
                    Some((_, running)) => *running += count,
                    None => tally.push((user.clone(), *count)),
                }
            }
        }
        tally
            .iter()
            .enumerate()
            .max_by_key(|(position, (_, count))| (*count, std::cmp::Reverse(*position)))
            .map(|(_, (user, _))| user.clone())
            .unwrap_or_default()
    };

    let mut subsystems: Vec<Value> = Vec::new();
    for community in comms.iter().take(budget) {
        let hubs: Vec<String> = community
            .hubs
            .iter()
            .take(3)
            .filter_map(|nid| {
                graph.nodes.get(nid).map(|data| {
                    if data.symbol.is_empty() { data.path.clone() } else { data.symbol.clone() }
                })
            })
            .filter(|name| !name.is_empty())
            .collect();
        let mut row = serde_json::Map::new();
        // `dir` is the subsystem's NAME — its dominant directory, qualified
        // when two subsystems share one. `dir_path` is that directory
        // unqualified, for anything that wants a real path back.
        row.insert("label".into(), json!(community.label));
        row.insert("dir".into(), json!(community.name));
        row.insert("dir_path".into(), json!(community.dir));
        row.insert("symbols".into(), json!(community.members.len()));
        row.insert("files".into(), json!(community.files.len()));
        row.insert("hubs".into(), json!(hubs));
        // each overlay contributes a field only when it exists at all, so a
        // cold graph does not report zero owners as though it knew
        if !overlays.owners.is_empty() {
            row.insert("owner".into(), json!(owner_of(&community.members)));
        }
        if !overlays.in_flight.is_empty() {
            let live = community
                .members
                .iter()
                .filter(|member| overlays.in_flight.contains_key(*member))
                .count();
            row.insert("in_flight".into(), json!(live));
        }
        if !overlays.memories.is_empty() {
            let scars: i64 = community
                .members
                .iter()
                .filter_map(|member| overlays.memories.get(member))
                .map(|slot| slot.scars)
                .sum();
            row.insert("scars".into(), json!(scars));
        }
        subsystems.push(Value::Object(row));
    }

    let split = hub_lists(graph, budget);
    let hubs = decorate(split.hubs, overlays);
    // the briefing's map lists only the top few test hubs: they are what the
    // suite leans on, which rarely changes what an agent does next
    let test_hubs = decorate(split.test_hubs.into_iter().take(MAP_TEST_HUBS).collect(), overlays);

    let mut live: Vec<(i64, usize, Value)> = Vec::new();
    for (position, nid) in overlays.in_flight_order.iter().enumerate() {
        let Some(intents) = overlays.in_flight.get(nid) else { continue };
        let Some(data) = graph.nodes.get(nid) else { continue };
        let by: BTreeSet<String> = intents
            .iter()
            .map(|intent| intent.get("owner").and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        let dependents = graph.in_degree(nid);
        live.push((-(dependents as i64), position, json!({
            "id": nid, "path": data.path, "symbol": data.symbol,
            "dependents": dependents, "by": by.into_iter().collect::<Vec<_>>(),
        })));
    }
    live.sort_by(|a, b| a.0.cmp(&b.0));

    let mut scarred: Vec<(i64, String, Value)> = overlays
        .memories
        .iter()
        .filter(|(_, slot)| slot.scars > 0)
        .map(|(nid, slot)| {
            let mut row = slot.to_value();
            if let Some(map) = row.as_object_mut() {
                map.insert("id".into(), json!(nid));
            }
            (-slot.scars, nid.clone(), row)
        })
        .collect();
    scarred.sort_by(|a, b| a.0.cmp(&b.0));

    json!({
        "files": graph.files,
        "symbols": symbol_count,
        "edges": edge_count,
        "languages": languages.into_iter()
            .map(|(name, count)| (name, json!(count)))
            .collect::<serde_json::Map<String, Value>>(),
        "subsystems": subsystems,
        // `hubs` is the product's load-bearing symbols. What the test suite
        // leans on is a different question and gets its own list rather than
        // disappearing: nothing here is dropped silently.
        "hubs": hubs,
        "test_hubs": test_hubs,
        "test_hubs_omitted": split.test_hubs_omitted,
        "hubs_scope": split.scope,
        "in_flight": live.into_iter().take(budget).map(|(_, _, row)| row).collect::<Vec<_>>(),
        "scarred": scarred.into_iter().take(budget).map(|(_, _, row)| row).collect::<Vec<_>>(),
        // the live breaking changes: what would collide if touched right now.
        // One entry per change, and without the ripple out to dependents —
        // both are on the graph for the dashboard, and both would bloat every
        // briefing that carries this.
        "breaking": distinct_breaking(&overlays.breaking, &overlays.breaking_order),
        "conventions": conventions(graph, budget),
    })
}

/// The briefing's slice of the graph, assembled from storage.
///
/// Best-effort and silent by design: a briefing must never fail because the
/// graph is cold. An empty graph returns nothing at all rather than a map of
/// zeroes, because "I do not know this repo yet" and "this repo is empty" are
/// different claims and only one of them is true.
pub fn briefing_map(store: &Store, scope: &str, idle_after_s: f64, budget: usize) -> Value {
    let graph = snapshot(store, scope);
    if graph.nodes.is_empty() {
        return Value::Null;
    }
    // no breaking overlay here: the briefing asks for owners, claims and
    // memories only. A live breaking change is on the dashboard's graph, and
    // the briefing reports the renames themselves from the ledger instead.
    let overlays = overlays_for(store, scope, &graph, idle_after_s, 30.0, false, false);
    let comms = communities_cached(&graph, scope, &overlays.co_change);
    repo_map(&graph, &comms, &overlays, budget)
}

/// Owners from the ledger, live claims from the intent leases, and what is
/// remembered about each node. Each piece is independent: one that comes back
/// empty leaves the structural graph intact rather than failing the whole map.
pub fn overlays_for(
    store: &Store, scope: &str, graph: &Snapshot, idle_after_s: f64, days: f64,
    breaking: bool, co_change: bool,
) -> Overlays {
    let rows = store.ledger_since(scope, crate::store::now() - days * 86_400.0);
    let memories: Vec<Value> =
        store.kv_list("memory", &format!("{scope}:")).into_iter().map(|(_k, m)| m).collect();
    let (in_flight, in_flight_order) = in_flight_from_intents(
        &crate::collisions::active_intents(store, scope, idle_after_s), graph);
    let (breaking, breaking_order) = if breaking {
        let markers: Vec<Value> =
            store.eph_scan(&format!("hot:{scope}:")).into_iter().map(|(_k, m)| m).collect();
        breaking_from_hot(&markers, graph, 2)
    } else {
        (BTreeMap::new(), Vec::new())
    };
    Overlays {
        owners: ownership_from_rows(&rows),
        in_flight,
        in_flight_order,
        memories: memories_by_node(&memories),
        breaking,
        breaking_order,
        co_change: if co_change { co_change_from_rows(&rows) } else { BTreeMap::new() },
        // unconditional, like `owners`: one more pass over rows already in
        // hand. Only `to_graphify` reads it, so the maps that do not ask for
        // it on the Python side are unchanged by having it here.
        touches: touches_from_rows(&rows),
    }
}

// ------------------------------------------------------- the blast radius

const BLAST_LIMIT: usize = 200;

/// Test hubs the briefing's map lists, at most.
const MAP_TEST_HUBS: usize = 3;

/// Calls a blast-radius dependent lists, at most.
const BLAST_CALLS: usize = 6;

/// Whether a call site `[name, line, args]` uses `target` (a node id): it
/// calls the symbol, or its arguments name it (`money(order.total)` uses
/// `Order`). Twin of graph.py's `_call_uses`.
fn call_uses(site: &Value, target: &str) -> bool {
    let symbol = target.rsplit("::").next().unwrap_or("");
    let short = symbol.rsplit('.').next().unwrap_or(symbol);
    if short.is_empty() {
        return false;
    }
    let name = site.get(0).and_then(Value::as_str).unwrap_or("");
    if name.rsplit('.').next().unwrap_or(name) == short {
        return true;
    }
    let args = site.get(2).and_then(Value::as_str).unwrap_or("");
    short.chars().count() >= 3 && args.to_lowercase().contains(&short.to_lowercase())
}

/// Transitive dependents of a node, by distance: what breaks if this symbol's
/// interface changes. Files count as dependents when a file-level edge — an
/// import of the whole module — reaches them.
pub fn blast_radius(graph: &Snapshot, nid: &str, depth: usize) -> Vec<Vec<Value>> {
    let mut levels: Vec<Vec<Value>> = Vec::new();
    let mut seen: BTreeSet<String> = [nid.to_string()].into_iter().collect();
    let mut frontier: Vec<String> = vec![nid.to_string()];
    let mut total = 0usize;
    let mut truncated = false;

    for _ in 1..=depth.max(1) {
        let mut next: Vec<Value> = Vec::new();
        for node in &frontier {
            for pred in graph.predecessors(node) {
                if seen.contains(pred) {
                    continue;
                }
                seen.insert(pred.clone());
                let data = graph.nodes.get(pred).cloned().unwrap_or_default();
                let edge = graph.out.get(pred).and_then(|targets| targets.get(node));
                // the exact facts an agent used to open the file for: where
                // the dependent lives, what it takes, what it passes, what it
                // can see — the parser's answers, not a model's
                let scope = if data.path.is_empty() { json!([]) } else {
                    graph.nodes.get(&node_id(&data.path, "")).map(NodeData::scope).unwrap_or(json!([]))
                };
                // only how it uses what it depends on: a call of that symbol,
                // or a call passing something named after it. Every other call
                // the dependent makes answered nothing and cost every message
                // after it (load_demo listed date(), timedelta() under Order)
                // none matching means the call goes through an alias (`import
                // compute_tax as tax`): then the dependent's calls, capped
                let all_sites = data.sites();
                let sites: Vec<&Value> = all_sites.as_array().into_iter().flatten().collect();
                let using: Vec<&Value> = sites.iter().copied().filter(|s| call_uses(s, node)).collect();
                let calls: Vec<Value> = (if using.is_empty() { sites } else { using }).into_iter()
                    .take(BLAST_CALLS)
                    .filter_map(|s| Some(json!({"name": s.get(0)?, "line": s.get(1)?, "args": s.get(2)?})))
                    .collect();
                let span = data.span();
                let span = if span.is_null() { json!([0, 0]) } else { span };
                let params = data.params();
                let params = if params.is_null() { json!([]) } else { params };
                next.push(json!({
                    "id": pred, "path": data.path, "symbol": data.symbol, "kind": data.kind,
                    "via": edge.map(|e| e.kind.as_str()).unwrap_or(""),
                    "confidence": edge.map(|e| e.confidence.as_str()).unwrap_or(""),
                    "through": node,
                    "span": span,
                    "params": params,
                    "calls": calls,
                    "scope": scope,
                }));
                total += 1;
                if total >= BLAST_LIMIT {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        if next.is_empty() {
            break;
        }
        let field = |row: &Value, key: &str| {
            row.get(key).and_then(Value::as_str).unwrap_or("").to_string()
        };
        next.sort_by(|a, b| {
            field(a, "path").cmp(&field(b, "path")).then(field(a, "symbol").cmp(&field(b, "symbol")))
        });
        frontier = next.iter().map(|row| field(row, "id")).collect();
        levels.push(next);
        if truncated {
            break;
        }
    }
    levels
}

/// node -> the live breaking change reaching it.
///
/// Hot markers are the gate's evidence: confidence-certain renames, removals
/// and signature changes still inside their TTL. Two things stop a marker
/// finding its node, and both are handled here. A rename's marker names the
/// OLD symbol, which the graph no longer has — the new name is what is there
/// now — so both names are anchored, and the file too, which is the one node
/// a removal cannot take with it. And a breaking change is never local: the
/// symbols that CALL the renamed one are what actually fails, so the radius
/// is walked out and marked with its distance.
///
/// The ripple follows the stale reference and nothing else. Rippling from the
/// new name as well would leave a caller that correctly adopted it inside the
/// blast radius forever, and the graph could never return to green.
pub fn breaking_from_hot(
    markers: &[Value], graph: &Snapshot, ripple: usize,
) -> (BTreeMap<String, Value>, Vec<String>) {
    let mut out: BTreeMap<String, (usize, Value)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();

    let claim = |nid: String, distance: usize, record: Value,
                     out: &mut BTreeMap<String, (usize, Value)>,
                     order: &mut Vec<String>| {
        // nearer beats further: a symbol that is itself renamed outranks one
        // that merely calls something renamed
        match out.get(&nid) {
            Some((seen, _)) if *seen <= distance => {}
            Some(_) => {
                out.insert(nid, (distance, record));
            }
            None => {
                order.push(nid.clone());
                out.insert(nid, (distance, record));
            }
        }
    };

    for marker in markers {
        let path = marker.get("path").and_then(Value::as_str).unwrap_or("");
        let author = marker.get("user").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        // the dependents' tests passed and every dependent had one: the
        // change is proven safe and stops being breaking. (The marker itself
        // stays for the gate, which guards the file, not the callers.)
        let verified = marker.get("verified").filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
        let status = verified.get("status").and_then(Value::as_str).unwrap_or("pending");
        if status == "clear" {
            continue;
        }
        let count = |key: &str| verified.get(key).and_then(Value::as_array).map_or(0, |a| a.len());
        for event in marker.get("events").and_then(Value::as_array).into_iter().flatten() {
            let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("");
            if symbol.is_empty() {
                continue;
            }
            let new_name = event
                .get("detail")
                .and_then(|detail| detail.get("new_name"))
                .and_then(Value::as_str);
            let mut record = serde_json::Map::new();
            record.insert("symbol".into(), json!(symbol));
            record.insert("kind".into(), json!(event.get("kind").and_then(Value::as_str).unwrap_or("")));
            record.insert("by".into(), json!(author));
            if let Some(new_name) = new_name.filter(|name| !name.is_empty()) {
                record.insert("new_name".into(), json!(new_name));
            }
            record.insert("verified".into(), json!(status));
            record.insert("failing_tests".into(), json!(count("failed")));
            record.insert("uncovered".into(), json!(count("uncovered")));
            let record = Value::Object(record);

            let stale = find_node(graph, path, symbol);
            let mut anchors: Vec<String> = Vec::new();
            for candidate in [
                stale.clone(),
                new_name.and_then(|name| find_node(graph, path, name)),
                find_node(graph, path, ""),
            ] {
                if let Some(nid) = candidate {
                    if !anchors.contains(&nid) {
                        anchors.push(nid);
                    }
                }
            }
            if anchors.is_empty() {
                // nothing of this file survives in the graph; keep the shape
                // so callers still see the change reported
                claim(node_id(path, symbol), 0, record.clone(), &mut out, &mut order);
                continue;
            }
            for nid in &anchors {
                claim(nid.clone(), 0, record.clone(), &mut out, &mut order);
            }
            let Some(stale) = stale.filter(|_| ripple > 0) else { continue };
            for (distance, level) in blast_radius(graph, &stale, ripple).into_iter().enumerate() {
                for row in level {
                    let Some(dep) = row.get("id").and_then(Value::as_str) else { continue };
                    claim(dep.to_string(), distance + 1, record.clone(), &mut out, &mut order);
                }
            }
        }
    }

    let mut result: BTreeMap<String, Value> = BTreeMap::new();
    for nid in &order {
        if let Some((distance, record)) = out.get(nid) {
            let mut row = record.clone();
            if let Some(map) = row.as_object_mut() {
                map.insert("distance".into(), json!(distance));
            }
            result.insert(nid.clone(), row);
        }
    }
    (result, order)
}

/// The grid's extra bit for a change whose dependents' tests FAILED.
pub const GRID_VERIFIED_BREAKING: i64 = 64;
/// How long the hook may spend running the dependents' tests, in seconds.
pub const VERIFY_BUDGET_S: f64 = 240.0;

/// What would prove a certain interface change safe, from the graph:
///
/// `dependents` are the files (outside the changed one, outside tests) that
/// depend on the changed symbols — the old name, the new name of a rename,
/// and the file itself, the same anchors the breaking overlay uses. `tests`
/// are the test files that reach those symbols or any symbol in a dependent,
/// and `uncovered` are the dependents no test file reaches. The hook runs
/// `tests`; the server grades the change from what comes back: clear when
/// every dependent is covered and every test passes, partial when some
/// dependent has no test, failing when a test fails.
///
/// Zero model tokens by design: the agent reads nothing and is told nothing
/// extra; the tests run detached on its machine, like the repo's check.
/// Python's `verify_plan`.
pub fn verify_plan(graph: &Snapshot, path: &str, hot_events: &[Value]) -> Value {
    let file_node = find_node(graph, path, "");
    let path_of = |nid: &str| graph.nodes.get(nid).map(|d| d.path.clone()).unwrap_or_default();
    let mut symbols: BTreeSet<String> = BTreeSet::new();
    let mut dependents: BTreeSet<String> = BTreeSet::new();
    let mut tests: BTreeSet<String> = BTreeSet::new();
    for event in hot_events {
        let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("");
        if symbol.is_empty() {
            continue;
        }
        symbols.insert(node_id(path, symbol));
        let new_name = event
            .get("detail")
            .and_then(|d| d.get("new_name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut anchors: Vec<String> = Vec::new();
        for candidate in [
            find_node(graph, path, symbol),
            if new_name.is_empty() { None } else { find_node(graph, path, new_name) },
            file_node.clone(),
        ] {
            if let Some(nid) = candidate {
                if !anchors.contains(&nid) {
                    anchors.push(nid);
                }
            }
        }
        for nid in &anchors {
            for pred in graph.predecessors(nid) {
                let ppath = path_of(pred);
                if ppath.is_empty() || ppath == path {
                    continue;
                }
                if is_test_path(&ppath) {
                    tests.insert(ppath);
                } else {
                    dependents.insert(ppath);
                }
            }
        }
    }
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for dep in &dependents {
        for (nid, data) in &graph.nodes {
            if &data.path != dep {
                continue;
            }
            for pred in graph.predecessors(nid) {
                let ppath = path_of(pred);
                if !ppath.is_empty() && &ppath != dep && is_test_path(&ppath) {
                    tests.insert(ppath);
                    covered.insert(dep.clone());
                }
            }
        }
    }
    let uncovered: Vec<&String> = dependents.iter().filter(|dep| !covered.contains(*dep)).collect();
    json!({
        "path": path, "symbols": symbols, "dependents": dependents, "tests": tests,
        "uncovered": uncovered, "budget_s": VERIFY_BUDGET_S,
    })
}

/// The marker's `verified` the moment the change lands: clear when nothing
/// depends on it, partial when something does but no test reaches it,
/// pending when there are tests to run. Python's `verification_at_report`.
pub fn verification_at_report(plan: &Value, now: f64) -> Value {
    let empty = |key: &str| plan.get(key).and_then(Value::as_array).map_or(true, |a| a.is_empty());
    if empty("dependents") {
        return json!({"status": "clear", "reason": "no dependents", "ts": now});
    }
    if empty("tests") {
        return json!({"status": "partial", "reason": "no tests", "uncovered": plan["uncovered"], "ts": now});
    }
    json!({"status": "pending", "tests": plan["tests"], "uncovered": plan["uncovered"], "ts": now})
}

/// The marker's `verified` from what the hook ran: failing beats partial
/// beats clear; a run that ran out of time proves nothing. Python's
/// `verification_from_run`.
pub fn verification_from_run(run: &Value, now: f64) -> Value {
    let strings = |key: &str| -> Vec<String> {
        run.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let (failed, passed, uncovered, tests) = (strings("failed"), strings("passed"), strings("uncovered"), strings("tests"));
    // test files the hook had no runner for: they prove nothing, so a
    // dependent they were meant to cover counts as uncovered
    let skipped = strings("skipped");
    let status = if crate::compat::truthy(run.get("timed_out")) {
        "unknown"
    } else if !failed.is_empty() {
        "failing"
    } else if !uncovered.is_empty() || !skipped.is_empty() {
        "partial"
    } else {
        "clear"
    };
    let command: String = run.get("command").and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
    json!({
        "status": status, "tests": tests, "passed": passed, "failed": failed, "uncovered": uncovered,
        "skipped": skipped, "command": command,
        "elapsed_ms": run.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0), "ts": now,
    })
}

/// One row per change, not one per node it is anchored to, and without the
/// ripple. A rename anchors on the old name, the new name and the file, which
/// is right for colouring a graph and wrong for a briefing.
pub fn distinct_breaking(breaking: &BTreeMap<String, Value>, order: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    for nid in order {
        let Some(info) = breaking.get(nid) else { continue };
        if info.get("distance").and_then(Value::as_u64).unwrap_or(0) != 0 {
            continue;
        }
        let key = (
            info.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
            info.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
            info.get("by").and_then(Value::as_str).unwrap_or("").to_string(),
            info.get("new_name").and_then(Value::as_str).unwrap_or("").to_string(),
        );
        if !seen.insert(key) {
            continue;
        }
        let mut row = serde_json::Map::new();
        row.insert("id".into(), json!(nid));
        for (field, value) in info.as_object().into_iter().flatten() {
            if field != "distance" {
                row.insert(field.clone(), value.clone());
            }
        }
        out.push(Value::Object(row));
    }
    out
}

// ----------------------------------------------------------------- export

/// Graphify's `graph.json` shape, so its viewer, Obsidian export and query
/// tooling can read a Collide graph — and so the dashboard has one payload
/// with the structure and every overlay on it.
///
/// Confidence is spelled the way Graphify spells it: EXTRACTED when the
/// target is named by an import or lives in the same file, INFERRED when a
/// unique name match resolved it, AMBIGUOUS when several files define that
/// name and the closest was chosen.
pub fn to_graphify(
    graph: &Snapshot, comms: &[Community], overlays: &Overlays, repo_id: &str,
) -> Value {
    let mut community_of: BTreeMap<&String, usize> = BTreeMap::new();
    for community in comms {
        for member in &community.members {
            community_of.insert(member, community.id);
        }
    }

    let mut nodes: Vec<Value> = Vec::new();
    for (nid, data) in &graph.nodes {
        if !internal(nid) {
            continue;
        }
        let mut node = serde_json::Map::new();
        node.insert("id".into(), json!(nid));
        node.insert(
            "label".into(),
            json!(if data.symbol.is_empty() { &data.path } else { &data.symbol }),
        );
        node.insert("type".into(), json!(data.kind));
        node.insert("source_file".into(), json!(data.path));
        node.insert("language".into(), json!(data.language));
        if let Some(id) = community_of.get(nid) {
            node.insert("community".into(), json!(id));
        }
        if let Some(tally) = overlays.owners.get(nid).filter(|t| !t.is_empty()) {
            let top = tally
                .iter()
                .enumerate()
                .max_by_key(|(position, (_, count))| (*count, std::cmp::Reverse(*position)))
                .map(|(_, (user, _))| user.clone())
                .unwrap_or_default();
            node.insert("owner".into(), json!(top));
        }
        if overlays.in_flight.contains_key(nid) {
            node.insert("in_flight".into(), json!(true));
        }
        if let Some(slot) = overlays.memories.get(nid) {
            node.insert("notes".into(), json!(slot.notes));
            node.insert("scars".into(), json!(slot.scars));
        }
        if let Some(hit) = overlays.breaking.get(nid) {
            // `breaking` stays a boolean for interop; how far the change is
            // from this node rides alongside it for the dashboard
            match hit.get("distance").and_then(Value::as_u64).unwrap_or(0) {
                0 => {
                    node.insert("breaking".into(), json!(true));
                }
                distance => {
                    node.insert("at_risk".into(), json!(distance));
                }
            }
        }
        // how often this symbol has been edited in the overlay window, for
        // shading the dot. Omitted at zero, like `at_risk`: a settled repo's
        // payload must not grow by a field that says nothing happened.
        if let Some(touched) = overlays.touches.get(nid).copied().filter(|count| *count != 0) {
            node.insert("touches".into(), json!(touched));
        }
        nodes.push(Value::Object(node));
    }

    let mut edges: Vec<Value> = Vec::new();
    for (from, targets) in &graph.out {
        for (to, edge) in targets {
            if !internal(to) {
                continue;
            }
            let relation = if edge.kind.is_empty() { "references" } else { edge.kind.as_str() };
            let confidence = if edge.confidence.is_empty() { "extracted" } else { &edge.confidence };
            edges.push(json!({
                "source": from, "target": to, "relation": relation,
                "confidence": confidence.to_uppercase(),
            }));
        }
    }

    for ((a, b), weight) in &overlays.co_change {
        edges.push(json!({
            "source": a, "target": b, "relation": "co_change",
            "confidence": "INFERRED", "weight": weight,
        }));
    }

    json!({
        "metadata": {
            "generator": "collide", "repo": repo_id,
            "generated_at": crate::store::now(),
            "fingerprint": graph.fingerprint,
            "files": graph.files,
            "edge_kinds": ["inherits", "calls", "uses_type", "references", "co_change"],
        },
        "nodes": nodes,
        "edges": edges,
        "communities": comms.iter().map(|community| json!({
            "id": community.id, "label": community.label,
            "size": community.members.len(), "members": community.members,
        })).collect::<Vec<_>>(),
    })
}

// --------------------------------------------------------- other exports

/// Python's `xml.etree.ElementTree` text-content escape: `&`, `<`, `>` only.
fn xml_escape_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The same, plus the extra characters ElementTree escapes in an ATTRIBUTE
/// value specifically (`"`, and `\r`/`\n`/`\t` as numeric character refs, so
/// they survive a naive line-based reader rather than being normalized away).
fn xml_escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#09;")
}

/// GraphML for Gephi/yEd, matching networkx's `generate_graphml(out)` output
/// byte for byte — `to_graphml` in `graph.py` builds a throwaway `DiGraph`
/// with exactly five node attributes (`label`, `kind`, `path`, `language`,
/// `community`) added in that kwarg order and two edge attributes (`kind`,
/// `confidence`), always in that order, and nothing else. networkx's writer
/// assigns key ids `dN` in first-seen (name, type, scope) order — nodes
/// before edges, since it serializes all nodes before any edge — and then
/// prints the `<key>` block in the REVERSE of that order (each new key is
/// spliced in at the front of the document as it is discovered). Because
/// this schema is fixed, that whole dance collapses to: `d0..d4` the moment
/// the first node is written, `d5..d6` the moment the first edge is (if
/// any) — so the key block can be built directly instead of re-deriving
/// networkx's bookkeeping.
///
/// Node/edge ORDER: `to_graphify` already iterates `graph.nodes`/`graph.out`
/// in the `Snapshot`'s sorted (BTreeMap) order rather than Python's true
/// insertion order; this reuses the same order for consistency. The two can
/// disagree on a repo with nested paths (Python's node order follows
/// `kv_list`'s `ORDER BY key` over `path.replace("/", "|")`, not a sort of
/// `path` itself) — see the module note on `records()`. Every `<key>` and
/// every `<data>` value still matches exactly; only the sequence of
/// `<node>`/`<edge>` elements in the file can differ.
pub fn to_graphml(graph: &Snapshot, comms: &[Community]) -> String {
    let mut community_of: BTreeMap<&String, i64> = BTreeMap::new();
    for community in comms {
        for member in &community.members {
            community_of.insert(member, community.id as i64);
        }
    }

    let node_ids: Vec<&String> = graph.nodes.keys().filter(|nid| internal(nid)).collect();
    let mut edges: Vec<(&String, &String, &EdgeData)> = Vec::new();
    for (from, targets) in &graph.out {
        for (to, edge) in targets {
            if internal(to) {
                edges.push((from, to, edge));
            }
        }
    }

    // (id, for, attr.name, attr.type), in DISCOVERY order; the printed
    // <key> block is this reversed.
    let mut keys: Vec<(String, &'static str, &'static str, &'static str)> = Vec::new();
    fn intern(
        keys: &mut Vec<(String, &'static str, &'static str, &'static str)>,
        for_: &'static str, name: &'static str, ty: &'static str,
    ) -> String {
        if let Some((id, ..)) = keys.iter().find(|(_, f, n, t)| *f == for_ && *n == name && *t == ty) {
            return id.clone();
        }
        let id = format!("d{}", keys.len());
        keys.push((id.clone(), for_, name, ty));
        id
    }
    let (d_label, d_kind_n, d_path, d_lang, d_comm) = if node_ids.is_empty() {
        Default::default()
    } else {
        (
            intern(&mut keys, "node", "label", "string"),
            intern(&mut keys, "node", "kind", "string"),
            intern(&mut keys, "node", "path", "string"),
            intern(&mut keys, "node", "language", "string"),
            intern(&mut keys, "node", "community", "long"),
        )
    };
    let (d_kind_e, d_conf) = if edges.is_empty() {
        Default::default()
    } else {
        (intern(&mut keys, "edge", "kind", "string"), intern(&mut keys, "edge", "confidence", "string"))
    };

    let mut lines: Vec<String> = vec![
        "<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://graphml.graphdrawing.org/xmlns http://graphml.graphdrawing.org/xmlns/1.0/graphml.xsd\">".to_string(),
    ];
    for (id, for_, name, ty) in keys.iter().rev() {
        lines.push(format!("  <key id=\"{id}\" for=\"{for_}\" attr.name=\"{name}\" attr.type=\"{ty}\" />"));
    }
    if node_ids.is_empty() && edges.is_empty() {
        lines.push("  <graph edgedefault=\"directed\" />".to_string());
    } else {
        lines.push("  <graph edgedefault=\"directed\">".to_string());
        for nid in &node_ids {
            let data = &graph.nodes[*nid];
            let label = if data.symbol.is_empty() { &data.path } else { &data.symbol };
            let community = community_of.get(nid).copied().unwrap_or(-1);
            lines.push(format!("    <node id=\"{}\">", xml_escape_attr(nid)));
            lines.push(format!("      <data key=\"{d_label}\">{}</data>", xml_escape_text(label)));
            lines.push(format!("      <data key=\"{d_kind_n}\">{}</data>", xml_escape_text(&data.kind)));
            lines.push(format!("      <data key=\"{d_path}\">{}</data>", xml_escape_text(&data.path)));
            lines.push(format!("      <data key=\"{d_lang}\">{}</data>", xml_escape_text(&data.language)));
            lines.push(format!("      <data key=\"{d_comm}\">{community}</data>"));
            lines.push("    </node>".to_string());
        }
        for (from, to, edge) in &edges {
            lines.push(format!(
                "    <edge source=\"{}\" target=\"{}\">", xml_escape_attr(from), xml_escape_attr(to)));
            lines.push(format!("      <data key=\"{d_kind_e}\">{}</data>", xml_escape_text(&edge.kind)));
            lines.push(format!("      <data key=\"{d_conf}\">{}</data>", xml_escape_text(&edge.confidence)));
            lines.push("    </edge>".to_string());
        }
        lines.push("  </graph>".to_string());
    }
    lines.push("</graphml>".to_string());
    lines.join("\n")
}

/// Python's `json.dumps(str(value))` — the exact escaping `to_cypher` wraps
/// every property value in. `serde_json::to_string` will not do here: Python
/// defaults to `ensure_ascii=True`, which escapes every character outside
/// printable ASCII as `\uXXXX` (a surrogate pair above U+FFFF); serde's
/// output is raw UTF-8. A symbol or path with an accented letter or emoji
/// would otherwise serialize to different bytes on the two servers.
fn cypher_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", 0xd800 + (v >> 10), 0xdc00 + (v & 0x3ff)));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// MERGE statements for Neo4j / FalkorDB: one `(:CollideNode:File|:Symbol)`
/// per internal node keyed by id, one relationship type per edge kind.
/// Idempotent (MERGE, not CREATE) so re-running an export after more of the
/// repo has been parsed only adds what's new. Same node/edge order caveat as
/// [`to_graphml`].
pub fn to_cypher(graph: &Snapshot, comms: &[Community]) -> String {
    let mut community_of: BTreeMap<&String, i64> = BTreeMap::new();
    for community in comms {
        for member in &community.members {
            community_of.insert(member, community.id as i64);
        }
    }

    let mut lines = vec![
        "// Collide code graph — MERGE is idempotent; re-run after every export".to_string(),
        "CREATE CONSTRAINT collide_node_id IF NOT EXISTS FOR (n:CollideNode) REQUIRE n.id IS UNIQUE;"
            .to_string(),
    ];
    for (nid, data) in &graph.nodes {
        if !internal(nid) {
            continue;
        }
        let label = if data.kind == "file" { "File" } else { "Symbol" };
        let name = if data.symbol.is_empty() { &data.path } else { &data.symbol };
        let community = community_of.get(nid).copied().unwrap_or(-1);
        lines.push(format!(
            "MERGE (n:CollideNode:{label} {{id: {}}}) SET n.name = {}, n.path = {}, n.kind = {}, \
             n.language = {}, n.community = {community};",
            cypher_str(nid), cypher_str(name), cypher_str(&data.path), cypher_str(&data.kind),
            cypher_str(&data.language),
        ));
    }
    for (from, targets) in &graph.out {
        for (to, edge) in targets {
            if !internal(to) {
                continue;
            }
            let rel = if edge.kind.is_empty() { "REFERENCES".to_string() } else { edge.kind.to_uppercase() };
            lines.push(format!(
                "MATCH (a:CollideNode {{id: {}}}), (b:CollideNode {{id: {}}}) MERGE (a)-[r:{rel}]->(b) \
                 SET r.confidence = {};",
                cypher_str(from), cypher_str(to), cypher_str(&edge.confidence),
            ));
        }
    }
    lines.join("\n") + "\n"
}

/// The dashboard's view: nodes, edges, communities and the repo map, with
/// every overlay already attached and the whole thing capped.
///
/// Anything live — a breaking change or something in its blast radius —
/// survives the cap regardless of degree. Trimming the one thing the view
/// exists to show would be the wrong economy.
/// The grid's per-square flags, one bit each — Python's `GRID_*` in graph.py.
pub const GRID_BREAKING: i64 = 1;
pub const GRID_AT_RISK_NEAR: i64 = 2;
pub const GRID_AT_RISK_FAR: i64 = 4;
pub const GRID_IN_FLIGHT: i64 = 8;
pub const GRID_SCARRED: i64 = 16;
pub const GRID_TEST: i64 = 32;
/// Defined in a file a claim filled in the last day: new, numbered, owned.
pub const GRID_CLAIMED: i64 = 128;
/// Defined in a file that took someone else's claimed number.
pub const GRID_CLAIM_CLASH: i64 = 256;

/// The dashboard's grid: EVERY symbol as one square, grouped by subsystem,
/// with the live overlays folded into a small bitmask — tens of kilobytes
/// where the force graph's payload was megabytes, because the grid draws all
/// the symbols and refetches on every edit.
///
/// `nodes` are `[id, community (-1 = none), touches, flags, language]`,
/// `links` are `[from, to]` indexes into `nodes` meaning "from depends on to"
/// (code edges only; co-change is a different kind of fact and stays on the
/// force graph's payload). Both lists are sorted, so the two halves answer
/// byte for byte. Python's `codegraph.grid_view`, from the same snapshot,
/// overlays and communities the force graph is built from.
/// The grid's heat window, in days. The force graph shaded by a month of
/// touches; at a busy repo's pace that painted four squares in five green,
/// which says nothing. A week is the owner's call (2026-09-23): a day made
/// the grid forget yesterday's work. Python's `GRID_HEAT_DAYS`.
pub const GRID_HEAT_DAYS: f64 = 7.0;

pub fn grid_view(store: &Store, scope: &str, repo_id: &str, idle_after_s: f64) -> Value {
    let graph = snapshot(store, scope);
    let overlays = overlays_for(store, scope, &graph, idle_after_s, GRID_HEAT_DAYS, true, true);
    let comms = communities_cached(&graph, scope, &overlays.co_change);
    let payload = to_graphify(&graph, &comms, &overlays, repo_id);
    let map = repo_map(&graph, &comms, &overlays, 12);
    let drafts: Vec<Value> = store.eph_scan(&format!("draft:{scope}:")).into_iter().map(|(_k, m)| m).collect();
    // the pressure gradient: the intents open right now against the file's
    // collision history (the salience counters the gate and checks keep)
    let intents = crate::collisions::active_intents(store, scope, idle_after_s);
    let collisions_by_path: BTreeMap<String, i64> = store
        .kv_list("salience", &format!("{scope}:"))
        .into_iter()
        .map(|(_k, v)| (str_of(&v, "path"), v.get("collisions").and_then(Value::as_i64).unwrap_or(0)))
        .collect();
    let pressure = pressure_from_intents(&intents, &graph, &collisions_by_path, crate::store::now());
    let mut grid = grid_from(&payload, &map, &comms, &drafts, &pressure);
    with_claims(store, scope, &mut grid);
    grid
}

/// Claims on the grid: a filled claim's symbols light up for a day, a
/// clashing file's until it is renamed away, and the live claims ride along
/// for the dashboard's list. Python's `grid_with_claims`.
pub fn with_claims(store: &Store, scope: &str, grid: &mut Value) {
    let (filled, clashing) = crate::claims::grid_paths(store, scope);
    if let Some(nodes) = grid.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes.iter_mut() {
            let path = node.get(0).and_then(Value::as_str).and_then(|id| id.split_once("::")).map(|(p, _)| p.to_string());
            let Some(path) = path else { continue };
            let mut extra = 0i64;
            if filled.contains(&path) {
                extra |= GRID_CLAIMED;
            }
            if clashing.contains(&path) {
                extra |= GRID_CLAIM_CLASH;
            }
            if extra != 0 {
                if let Some(flags) = node.get(3).and_then(Value::as_i64) {
                    node[3] = json!(flags | extra);
                }
            }
        }
    }
    grid["claims"] = json!(crate::claims::summary(store, scope));
}

/// "In flight" is what an agent is writing RIGHT NOW: a declared intent, or
/// a live draft marker (the gate's pre-write parse names the symbols in
/// motion, and it lives ten minutes) — most agents never declare an intent,
/// so without the drafts the violet squares never appeared.
fn grid_from(
    payload: &Value, map: &Value, comms: &[Community], drafts: &[Value], pressure: &BTreeMap<String, f64>,
) -> Value {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(0);
    let failing_ids: BTreeSet<String> = map
        .get("breaking")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| text(row, "verified") == "failing")
        .map(|row| text(row, "id"))
        .collect();
    let drafting: BTreeSet<String> = drafts
        .iter()
        .flat_map(|draft| {
            let path = text(draft, "path");
            draft
                .get("symbols_changed")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|name| !path.is_empty() && !name.is_empty())
                .map(|name| format!("{path}::{name}"))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut symbols: Vec<&Value> = payload
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|node| !matches!(text(node, "type").as_str(), "file" | "external" | "unknown"))
        .collect();
    symbols.sort_by_key(|node| text(node, "id"));
    let index: BTreeMap<String, usize> =
        symbols.iter().enumerate().map(|(i, node)| (text(node, "id"), i)).collect();
    let nodes: Vec<Value> = symbols
        .iter()
        .map(|node| {
            let mut flags = 0i64;
            if crate::compat::truthy(node.get("breaking")) {
                flags |= GRID_BREAKING;
                if failing_ids.contains(&text(node, "id")) {
                    flags |= GRID_VERIFIED_BREAKING;
                }
            }
            match number(node, "at_risk") {
                0 => {}
                1 => flags |= GRID_AT_RISK_NEAR,
                _ => flags |= GRID_AT_RISK_FAR,
            }
            if crate::compat::truthy(node.get("in_flight")) || drafting.contains(&text(node, "id")) {
                flags |= GRID_IN_FLIGHT;
            }
            if number(node, "scars") > 0 {
                flags |= GRID_SCARRED;
            }
            if is_test_path(&text(node, "source_file")) {
                flags |= GRID_TEST;
            }
            let community = node.get("community").and_then(Value::as_i64).unwrap_or(-1);
            let risk = pressure.get(&text(node, "id")).copied().unwrap_or(0.0);
            json!([text(node, "id"), community, number(node, "touches"), flags, text(node, "language"), risk])
        })
        .collect();
    let links: BTreeSet<(usize, usize)> = payload
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|edge| text(edge, "relation") != "co_change")
        .filter_map(|edge| Some((*index.get(&text(edge, "source"))?, *index.get(&text(edge, "target"))?)))
        .collect();
    let subsystems: Vec<Value> = comms
        .iter()
        .map(|community| json!({
            "id": community.id, "label": community.label, "name": community.name, "dir": community.dir,
            "hub": community.hub_names.iter().find(|name| !name.is_empty()).cloned().unwrap_or_default(),
            "size": community.members.len(),
        }))
        .collect();
    json!({
        "files": map.get("files").cloned().unwrap_or(json!(0)),
        "symbols": map.get("symbols").cloned().unwrap_or(json!(0)),
        "edges": map.get("edges").cloned().unwrap_or(json!(0)),
        "languages": map.get("languages").cloned().unwrap_or(json!({})),
        "breaking": map.get("breaking").cloned().unwrap_or(json!([])),
        "subsystems": subsystems,
        "nodes": nodes,
        "links": links.into_iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
    })
}

pub fn graph_view(
    store: &Store, scope: &str, repo_id: &str, idle_after_s: f64, limit: usize,
) -> Value {
    let graph = snapshot(store, scope);
    let overlays = overlays_for(store, scope, &graph, idle_after_s, 30.0, true, true);
    let comms = communities_cached(&graph, scope, &overlays.co_change);
    let mut payload = to_graphify(&graph, &comms, &overlays, repo_id);

    let nodes = payload.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default();
    if nodes.len() > limit {
        let mut keep: BTreeSet<String> = nodes
            .iter()
            .filter(|node| {
                crate::compat::truthy(node.get("breaking"))
                    || crate::compat::truthy(node.get("at_risk"))
            })
            .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let mut ranked: Vec<(i64, String)> = nodes
            .iter()
            .filter_map(|node| node.get("id").and_then(Value::as_str))
            .map(|nid| {
                (-((graph.in_degree(nid) * 2 + graph.out_degree(nid)) as i64), nid.to_string())
            })
            .collect();
        ranked.sort();
        keep.extend(ranked.into_iter().take(limit).map(|(_, nid)| nid));

        let kept: Vec<Value> = nodes
            .into_iter()
            .filter(|node| {
                node.get("id").and_then(Value::as_str).is_some_and(|nid| keep.contains(nid))
            })
            .collect();
        let edges: Vec<Value> = payload
            .get("edges")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|edge| {
                let inside = |key: &str| {
                    edge.get(key).and_then(Value::as_str).is_some_and(|nid| keep.contains(nid))
                };
                inside("source") && inside("target")
            })
            .collect();
        if let Some(map) = payload.as_object_mut() {
            map.insert("nodes".into(), json!(kept));
            map.insert("edges".into(), json!(edges));
            map.insert("truncated".into(), json!(true));
        }
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert("map".into(), repo_map(&graph, &comms, &overlays, 12));
    }
    payload
}

// ------------------------------------------------------------- the queries

/// What an agent gets when it asks about something the graph has not seen.
///
/// The honest answer names the reason rather than returning an empty result:
/// the graph is built from reported edits, so a file nobody has reported has
/// no node, and that is a gap in coverage rather than a fact about the code.
/// An empty answer would read as "nothing depends on this", which is the most
/// dangerous thing this could say wrongly.
pub fn graph_miss(graph: &Snapshot, path: &str, symbol: &str) -> Value {
    let near: Vec<String> = graph
        .nodes
        .values()
        .filter(|data| data.path == path && !data.symbol.is_empty())
        .map(|data| data.symbol.clone())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .take(20)
        .collect();
    let subject = if symbol.is_empty() { path } else { symbol };
    let mut out = serde_json::Map::new();
    out.insert("ok".into(), json!(false));
    out.insert(
        "error".into(),
        json!(format!("{} is not in the graph yet", crate::compat::python_repr(subject))),
    );
    out.insert("why".into(), json!(
        "the graph is built from reported edits — a file nobody has reported \
         since this workspace started has no node yet"));
    if !near.is_empty() {
        out.insert("symbols_in_path".into(), json!(near));
    }
    out.insert("fix".into(), json!("report_edit that file once, or check the path spelling"));
    Value::Object(out)
}

/// Everything that would have to change if this symbol's interface changes.
///
/// The verdict is the part worth reading. "Nothing depends on this" is a
/// genuinely useful thing to be told before a rename, and it can only be said
/// honestly because the miss case above refuses to say it by accident.
pub fn blast_radius_view(
    graph: &Snapshot, overlays: &Overlays, path: &str, symbol: &str, depth: usize,
) -> Value {
    let Some(node) = find_node(graph, path, symbol) else {
        return graph_miss(graph, path, symbol);
    };
    let depth = depth.clamp(1, 6);
    let levels = blast_radius(graph, &node, depth);

    let mut owners: BTreeSet<String> = BTreeSet::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut total = 0usize;
    let mut out_levels: Vec<Value> = Vec::new();
    for (index, level) in levels.into_iter().enumerate() {
        total += level.len();
        let mut decorated = decorate(level, overlays);
        // each node's `scope` (every name its module defines, ~100 for a big
        // file) serves recipe replay, not the agent asking what depends on
        // this: repeated per dependent it made a short answer 30K characters
        for entry in decorated.iter_mut() {
            if let Some(map) = entry.as_object_mut() {
                map.remove("scope");
            }
        }
        for entry in &decorated {
            if let Some(owner) = entry.get("owner").and_then(Value::as_str) {
                if !owner.is_empty() {
                    owners.insert(owner.to_string());
                }
            }
            if let Some(file) = entry.get("path").and_then(Value::as_str) {
                if !file.is_empty() {
                    files.insert(file.to_string());
                }
            }
        }
        out_levels.push(json!({
            "depth": index + 1, "count": decorated.len(), "nodes": decorated,
        }));
    }

    let verdict = if total == 0 {
        "nothing else in the parsed graph depends on this — safe to change the interface"
            .to_string()
    } else {
        format!(
            "{total} dependent symbol(s) across {} file(s); check them before changing \
             the signature",
            files.len())
    };
    json!({
        "ok": true,
        "node": node,
        "depth": depth,
        "total": total,
        "files": files.iter().cloned().collect::<Vec<_>>(),
        "levels": out_levels,
        "truncated": total >= BLAST_LIMIT,
        // the negative fact grep spends the most on: where NOTHING depends
        // on this, so no search there is needed
        "absent_in": absent_dirs(graph, &files),
        "owners": owners.into_iter().collect::<Vec<_>>(),
        "verdict": verdict,
    })
}

/// The neighbourhood of one symbol or file: what it depends on and what
/// depends on it. This is the orientation read that replaces opening a file
/// and following its imports by hand.
pub fn neighbors_view(
    graph: &Snapshot, overlays: &Overlays, path: &str, symbol: &str, depth: usize,
) -> Value {
    let Some(node) = find_node(graph, path, symbol) else {
        return graph_miss(graph, path, symbol);
    };
    let depth = depth.clamp(1, 3);

    let describe = |other: &String, edge: &EdgeData, via: &str| -> Value {
        let data = graph.nodes.get(other).cloned().unwrap_or_default();
        let mut row = serde_json::Map::new();
        row.insert("id".into(), json!(other));
        row.insert("path".into(), json!(data.path));
        row.insert("symbol".into(), json!(data.symbol));
        row.insert("kind".into(), json!(data.kind));
        row.insert("edge".into(), json!(edge.kind));
        row.insert("confidence".into(), json!(edge.confidence));
        if !via.is_empty() {
            row.insert("via".into(), json!(via));
        }
        Value::Object(row)
    };

    let mut depends_on: Vec<Value> = Vec::new();
    let mut dependents: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = [node.clone()].into_iter().collect();
    let mut frontier: Vec<String> = vec![node.clone()];

    for _ in 0..depth {
        let mut next: Vec<String> = Vec::new();
        for current in &frontier {
            let via = if *current == node { "" } else { current.as_str() };
            if let Some(targets) = graph.out.get(current) {
                for (successor, edge) in targets {
                    if seen.insert(successor.clone()) {
                        depends_on.push(describe(successor, edge, via));
                        next.push(successor.clone());
                    }
                }
            }
            for predecessor in graph.predecessors(current) {
                if seen.contains(predecessor) {
                    continue;
                }
                let edge = graph
                    .out
                    .get(predecessor)
                    .and_then(|targets| targets.get(current))
                    .cloned()
                    .unwrap_or(EdgeData { kind: String::new(), confidence: String::new() });
                seen.insert(predecessor.clone());
                dependents.push(describe(predecessor, &edge, via));
                next.push(predecessor.clone());
            }
        }
        frontier = next;
    }

    let data = graph.nodes.get(&node).cloned().unwrap_or_default();
    json!({
        "ok": true,
        "node": node,
        "path": data.path,
        "symbol": data.symbol,
        "kind": data.kind,
        "depends_on": decorate(depends_on, overlays),
        "dependents": decorate(dependents, overlays),
    })
}

/// How two symbols connect, with the relation on every hop.
///
/// Direction-agnostic on purpose: "these two are three hops apart through the
/// billing module" is the useful answer, and insisting on following edge
/// direction would report no connection between two things that plainly are
/// connected.
pub fn path_view(
    graph: &Snapshot, from_path: &str, from_symbol: &str, to_path: &str, to_symbol: &str,
) -> Value {
    let Some(a) = find_node(graph, from_path, from_symbol) else {
        return graph_miss(graph, from_path, from_symbol);
    };
    let Some(b) = find_node(graph, to_path, to_symbol) else {
        return graph_miss(graph, to_path, to_symbol);
    };

    // breadth-first over the undirected projection; the first path found is a
    // shortest one, and neighbours are visited in sorted order so two runs
    // over the same graph agree on which shortest path that is
    let mut came_from: BTreeMap<String, String> = BTreeMap::new();
    let mut seen: BTreeSet<String> = [a.clone()].into_iter().collect();
    let mut queue: std::collections::VecDeque<String> = [a.clone()].into_iter().collect();
    let mut found = a == b;

    while let Some(current) = queue.pop_front() {
        if current == b {
            found = true;
            break;
        }
        let mut adjacent: BTreeSet<&String> = BTreeSet::new();
        if let Some(targets) = graph.out.get(&current) {
            adjacent.extend(targets.keys());
        }
        adjacent.extend(graph.predecessors(&current));
        for other in adjacent {
            if seen.insert(other.clone()) {
                came_from.insert(other.clone(), current.clone());
                queue.push_back(other.clone());
            }
        }
    }

    if !found {
        return json!({"ok": true, "from": a, "to": b, "connected": false, "hops": []});
    }

    let mut nodes = vec![b.clone()];
    let mut cursor = b.clone();
    while cursor != a {
        let Some(previous) = came_from.get(&cursor).cloned() else { break };
        nodes.push(previous.clone());
        cursor = previous;
    }
    nodes.reverse();

    let hops: Vec<Value> = nodes
        .windows(2)
        .map(|pair| {
            let (x, y) = (&pair[0], &pair[1]);
            let (edge, direction) = match graph.out.get(x).and_then(|t| t.get(y)) {
                Some(edge) => (edge.clone(), "->"),
                None => (
                    graph
                        .out
                        .get(y)
                        .and_then(|t| t.get(x))
                        .cloned()
                        .unwrap_or(EdgeData { kind: String::new(), confidence: String::new() }),
                    "<-",
                ),
            };
            json!({"from": x, "to": y, "direction": direction,
                   "kind": edge.kind, "confidence": edge.confidence})
        })
        .collect();

    json!({
        "ok": true, "from": a, "to": b, "connected": true,
        "length": hops.len(), "nodes": nodes, "hops": hops,
    })
}

/// The fewest independent groups a list of work units collapses into: units
/// whose blast radii touch the same files are one group (two agents there
/// would collide, and one already has the context), and units that are the
/// same transform on the same symbol are one group (six call sites needing
/// the same change is one agent doing six ops). Python: service.plan_work —
/// same output, byte for byte.
pub fn plan_work_view(graph: &Snapshot, overlays: &Overlays, units: &[Value], depth: usize) -> Value {
    let depth = depth.clamp(1, 6);
    let units: Vec<&Value> = units.iter().filter(|u| u.is_object()).collect();
    let text = |u: &Value, key: &str| u.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let mut impacts: Vec<(std::collections::BTreeSet<String>, bool)> = Vec::new();
    for unit in &units {
        let path = text(unit, "path");
        let symbol = text(unit, "symbol");
        let mut files = std::collections::BTreeSet::new();
        let mut known = false;
        if !path.is_empty() {
            files.insert(path.clone());
            let view = blast_radius_view(graph, overlays, &path, &symbol, depth);
            if view.get("ok").and_then(Value::as_bool) == Some(true) {
                known = true;
                for file in view.get("files").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(f) = file.as_str() {
                        files.insert(f.to_string());
                    }
                }
            }
        }
        impacts.push((files, known));
    }
    let n = units.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut Vec<usize>, mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let same_transform = |a: &Value, b: &Value| {
        !text(a, "op").is_empty() && text(a, "op") == text(b, "op")
            && !text(a, "symbol").is_empty() && text(a, "symbol") == text(b, "symbol")
    };
    for i in 0..n {
        for j in (i + 1)..n {
            let shared = impacts[i].0.intersection(&impacts[j].0).next().is_some();
            if shared || same_transform(units[i], units[j]) {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut members: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        members.entry(r).or_default().push(i);
    }
    let mut groups_raw: Vec<Vec<usize>> = members.into_values().collect();
    groups_raw.sort_by_key(|g| g[0]);
    let mut groups = Vec::new();
    for group in groups_raw {
        let mut files = std::collections::BTreeSet::new();
        for m in &group {
            files.extend(impacts[*m].0.iter().cloned());
        }
        let shared = group.iter().enumerate().any(|(x, a)| {
            group.iter().skip(x + 1).any(|b| impacts[*a].0.intersection(&impacts[*b].0).next().is_some())
        });
        let why = if shared { "shared impact" } else if group.len() > 1 { "same transform" } else { "independent" };
        let unknown: Vec<usize> = group.iter().copied().filter(|m| !impacts[*m].1).collect();
        groups.push(json!({
            "units": group, "files": files.into_iter().collect::<Vec<_>>(), "why": why, "unknown": unknown,
        }));
    }
    let agents = groups.len();
    json!({
        "ok": true, "units": n, "groups": groups, "agents": agents, "collapsed": n - agents,
        "verdict": format!("{n} unit(s) -> {agents} agent(s); {} collapsed", n - agents),
    })
}

#[cfg(test)]
mod tests {
    use super::{is_test_path, name_communities, Community};

    fn community(id: usize, dir: &str, hubs: &[&str]) -> Community {
        Community {
            id,
            label: String::new(),
            name: String::new(),
            dir: dir.to_string(),
            members: Vec::new(),
            hubs: Vec::new(),
            hub_names: hubs.iter().map(|h| h.to_string()).collect(),
            files: Vec::new(),
        }
    }

    #[test]
    fn test_scaffolding_is_recognised_across_the_languages_the_parser_reads() {
        for path in [
            "server/tests/conftest.py",
            "server/tests/test_graph.py",
            "src/app/test_routes.py",
            "pkg/thing_test.go",
            "server/collide-rs/tests/smoke.rs",
            "crates/x/src/store_test.rs",
            "spec/models/user_spec.rb",
            "app/models/user_spec.rb",
            "web/src/Button.test.tsx",
            "web/src/Button.spec.ts",
            "web/src/__tests__/Button.tsx",
            "java/src/test/java/com/x/Thing.java",
            "java/src/main/java/com/x/ThingTest.java",
            "dotnet/Billing/LedgerTests.cs",
        ] {
            assert!(is_test_path(path), "{path} should read as a test file");
        }
        // the lookalikes: a name that merely CONTAINS "test" is not a test
        for path in [
            "server/collide-rs/src/store.rs",
            "src/collide/contest.py",
            "web/src/latest.ts",
            "server/collide-rs/src/manifest.rs",
            "nextjs-app/src/app/collide/MapPanel.tsx",
            "pkg/attestation.go",
        ] {
            assert!(!is_test_path(path), "{path} should NOT read as a test file");
        }
    }

    #[test]
    fn a_directory_two_subsystems_share_is_qualified_by_its_top_hub() {
        let mut comms = vec![
            community(0, "server/tests", &["call", "mcp_session", "ALICE"]),
            community(1, "server/tests", &["paired", "_reset_repo"]),
            community(2, "server/src/collide", &["CollideService", "create_app"]),
        ];
        name_communities(&mut comms);
        assert_eq!(comms[0].name, "server/tests · call");
        assert_eq!(comms[1].name, "server/tests · paired");
        // a directory only one subsystem sits in keeps its plain name
        assert_eq!(comms[2].name, "server/src/collide");
        // the qualifier is not repeated in the label
        assert_eq!(comms[0].label, "server/tests · call: mcp_session, ALICE");
        assert_eq!(comms[2].label, "server/src/collide: CollideService, create_app");
        let names: Vec<&str> = comms.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), names.iter().collect::<std::collections::BTreeSet<_>>().len());
    }

    #[test]
    fn the_last_resort_tail_is_the_community_id() {
        // same directory AND same top hub name: only the id can separate them
        let mut comms = vec![
            community(0, "pkg", &["run"]),
            community(1, "pkg", &["run"]),
        ];
        name_communities(&mut comms);
        assert_eq!(comms[0].name, "pkg · run");
        assert_eq!(comms[1].name, "pkg · run #1");
        assert_ne!(comms[0].name, comms[1].name);
    }

    #[test]
    fn a_subsystem_with_no_named_hub_still_gets_a_name() {
        let mut comms = vec![community(0, "pkg", &[]), community(1, "pkg", &[])];
        name_communities(&mut comms);
        assert_eq!(comms[0].name, "pkg");
        assert_eq!(comms[1].name, "pkg #1");
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    type Flat = (Vec<(String, String, String, String, String)>, Vec<(String, String, String, String)>, Vec<(String, Vec<String>)>);

    fn flat(g: &Snapshot) -> Flat {
        let nodes = g.nodes.iter().map(|(id, n)| (id.clone(), n.kind.clone(), n.path.clone(), n.symbol.clone(), format!("{}{}{}{}", n.language, n.span(), n.params(), n.scope()))).collect();
        let out = g.out.iter().flat_map(|(from, m)| m.iter().map(move |(to, e)| (from.clone(), to.clone(), e.kind.clone(), e.confidence.clone()))).collect();
        let incoming = g.incoming.iter().filter(|(_, s)| !s.is_empty()).map(|(to, s)| (to.clone(), s.iter().cloned().collect())).collect();
        (nodes, out, incoming)
    }

    /// A tiny deterministic generator, so a failure replays.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    const NAMES: [&str; 10] = ["run", "save", "load", "Model", "Model.save", "helper", "parse", "emit", "Config", "main"];
    const FILES: [&str; 8] = ["pkg/a.py", "pkg/b.py", "pkg/c.py", "lib/d.py", "lib/e.py", "f.py", "pkg/sub/g.py", "lib/h.py"];
    const MODULES: [&str; 5] = ["pkg.a", "pkg.b", "lib.d", "requests", "lib"];

    fn record(rng: &mut Rng, path: &str) -> Value {
        let mut symbols = serde_json::Map::new();
        for _ in 0..(1 + rng.next() % 4) {
            let name = rng.pick(&NAMES).to_string();
            let edges: Vec<Value> = (0..rng.next() % 4)
                .map(|_| {
                    let kind = rng.pick(&["calls", "references", "inherits", "uses_type"]);
                    let member = if rng.next() % 3 == 0 { rng.pick(&NAMES).to_string() } else { String::new() };
                    json!([rng.pick(&NAMES), member, kind])
                })
                .collect();
            symbols.insert(name, json!({"kind": "function", "edges": edges, "span": [1, 2], "params": ["x"], "sites": []}));
        }
        let imports: Vec<Value> = (0..rng.next() % 3)
            .map(|_| {
                let local = rng.pick(&NAMES);
                let original = if rng.next() % 4 == 0 { "*" } else { rng.pick(&NAMES) };
                json!({"module": rng.pick(&MODULES), "names": [[local, original]]})
            })
            .collect();
        json!({"path": path, "language": "python", "symbols": symbols, "imports": imports, "ts": rng.next() as f64})
    }

    #[test]
    fn a_patched_graph_equals_a_full_build_after_every_edit() {
        for seed in 1..=40u64 {
            let db = Store::open(std::path::Path::new(":memory:")).unwrap();
            let scope = format!("w:r{seed}");
            let mut rng = Rng(seed * 0x9E3779B97F4A7C15);
            let mut stamp = 1_000.0;
            // a starting tree of five files, then edits; now and then a new file
            for path in &FILES[..5] {
                stamp += 1.0;
                crate::codegraph::update_file(&db, &scope, path, record(&mut rng, path), stamp);
            }
            let _ = snapshot(&db, &scope);
            for step in 0..80 {
                let path = if rng.next() % 10 == 0 { rng.pick(&FILES) } else { rng.pick(&FILES[..5]) };
                stamp += 1.0;
                crate::codegraph::update_file(&db, &scope, path, record(&mut rng, path), stamp);
                let patched = snapshot(&db, &scope);
                let full = rebuilt(&db, &scope);
                assert_eq!(flat(&patched), flat(&full), "seed {seed} step {step} ({path})");
                assert_eq!(patched.files, full.files);
                assert_eq!(patched.fingerprint, full.fingerprint);
            }
        }
    }
}
