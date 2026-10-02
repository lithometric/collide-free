//! Durable team memory, anchored to code.
//!
//! A note here is not a string in a table. It is an annotation on a node of
//! the Merkle tree — `auth.py::validateToken`, `auth.py`, `src/auth/` — and
//! the anchor's hash and revision count are captured at write time. That is
//! what lets a note report honestly, later, whether the code it describes has
//! moved underneath it: the substrate for staleness, confidence and conflict
//! detection, none of which can be reconstructed after the fact.
//!
//! The honesty layer is the other half. A recalled note reports whether the
//! code under it moved (`stale`), how many times it was rewritten while the
//! note survived, whether something the anchor DEPENDS on changed after the
//! note was written (`possibly_stale`), and whether a sibling note was
//! written against a different version of the same anchor (`conflict`).
//! Confidence comes from that event history and never from a timestamp: age
//! alone says nothing about whether a fact is still true.

use serde_json::{json, Map, Value};

use crate::compat::python_round;
use crate::hashing;
use crate::repo::path_key;
use crate::semantics;
use crate::store::{now, Store};

const MAX_FACT: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// `path::symbol` — one symbol
    Symbol { path: String, symbol: String },
    /// `path` — a whole file
    File { path: String },
    /// `dir/` — a subtree, which carries no hash
    Dir { path: String },
}

impl Anchor {
    pub fn to_value(&self) -> Value {
        match self {
            Anchor::Symbol { path, symbol } => json!({"kind": "symbol", "path": path, "symbol": symbol}),
            Anchor::File { path } => json!({"kind": "file", "path": path}),
            Anchor::Dir { path } => json!({"kind": "dir", "path": path}),
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Anchor::Symbol { path, .. } | Anchor::File { path } | Anchor::Dir { path } => path,
        }
    }

    pub fn symbol(&self) -> &str {
        match self {
            Anchor::Symbol { symbol, .. } => symbol,
            _ => "",
        }
    }
}

/// The anchor grammar. A trailing slash means a subtree; `::` separates a
/// symbol from its file; anything else is a file.
pub fn parse_anchor(anchor: &str) -> Option<Anchor> {
    let anchor = anchor.trim();
    if anchor.is_empty() {
        return None;
    }
    if let Some((path, symbol)) = anchor.split_once("::") {
        let path = path.trim().trim_matches('/').to_string();
        let symbol = symbol.trim().to_string();
        if path.is_empty() || symbol.is_empty() {
            return None;
        }
        return Some(Anchor::Symbol { path, symbol });
    }
    if anchor.ends_with('/') {
        return Some(Anchor::Dir { path: anchor.trim_matches('/').to_string() });
    }
    Some(Anchor::File { path: anchor.trim_matches('/').to_string() })
}

/// The identity of an anchor as a string, for grouping siblings.
pub fn anchor_key(anchor: &Value) -> String {
    format!(
        "{}::{}#{}",
        anchor.get("path").and_then(Value::as_str).unwrap_or(""),
        anchor.get("symbol").and_then(Value::as_str).unwrap_or(""),
        anchor.get("kind").and_then(Value::as_str).unwrap_or(""),
    )
}

/// Symbols of the freshest clean parse of a path across every workspace —
/// the closest thing to "the current tree" when each workspace has its own.
pub fn freshest_symbols(store: &Store, scope: &str, path: &str) -> Option<Map<String, Value>> {
    let stamp = now();
    let mut best: Option<(f64, Map<String, Value>)> = None;
    for (_user, record) in store.get_file_all_users(scope, path) {
        let Some(served) = semantics::serve(Some(&record), stamp) else { continue };
        let ts = record
            .get("last_clean")
            .and_then(|clean| clean.get("ts"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if best.as_ref().map(|(seen, _)| ts > *seen).unwrap_or(true) {
            best = Some((ts, served.symbols));
        }
    }
    best.map(|(_, symbols)| symbols)
}

/// `(current_hash, rev_count, rewrite_history)` for an anchor, read from the
/// freshest clean parse across every workspace. A directory anchor has no
/// hash — its staleness is honestly unknown rather than guessed.
pub fn anchor_state(store: &Store, scope: &str, anchor: &Anchor) -> (Option<String>, i64, Vec<f64>) {
    if matches!(anchor, Anchor::Dir { .. }) {
        return (None, 0, Vec::new());
    }
    let path = anchor.path();
    let symbols = freshest_symbols(store, scope, path);

    let current = symbols.as_ref().and_then(|symbols| match anchor {
        Anchor::Symbol { symbol, .. } => symbols
            .get(symbol)
            .and_then(|entry| entry.get("hash"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => {
            let flat: std::collections::BTreeMap<String, String> = symbols
                .iter()
                .filter_map(|(name, entry)| {
                    entry.get("hash").and_then(Value::as_str).map(|h| (name.clone(), h.to_string()))
                })
                .collect();
            if flat.is_empty() { None } else { Some(hashing::file_hash(&flat)) }
        }
    });

    let record = store.kv_get("symrev", &format!("{scope}:{}:{}", path_key(path), anchor.symbol()));
    let rev = record
        .as_ref()
        .and_then(|record| record.get("count").and_then(Value::as_i64))
        .unwrap_or(0);
    let history: Vec<f64> = record
        .as_ref()
        .and_then(|record| record.get("history").and_then(Value::as_array))
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    Value::Object(map) => map.get("ts").and_then(Value::as_f64),
                    other => other.as_f64(),
                })
                .collect()
        })
        .unwrap_or_default();
    (current, rev, history)
}

pub struct SaveInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub fact: &'a str,
    pub tags: &'a [&'a str],
    pub agent: &'a str,
    pub anchor: &'a str,
    pub auto: &'a str,
    /// id of a note this one retires. Resolving a belief conflict is a ledger
    /// event, not a delete: the losing note stays readable, marked.
    pub supersedes: &'a str,
}


/// Write one anchored note. Returns the Python response shape.
pub fn save(store: &Store, input: &SaveInput) -> Value {
    let fact: String = input.fact.trim().chars().take(MAX_FACT).collect();
    if fact.is_empty() {
        return json!({"ok": false, "error": "empty fact"});
    }
    let stamp = now();
    let id = crate::compat::new_id();

    // lowercased, deduplicated, sorted — so tag lookups do not depend on how
    // whoever wrote the note happened to capitalise it
    let tags: Vec<String> = input
        .tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut memory = Map::new();
    memory.insert("id".into(), json!(id));
    memory.insert("fact".into(), json!(fact));
    memory.insert("tags".into(), json!(tags));
    memory.insert("owner".into(), json!(input.user_id));
    memory.insert("agent".into(), json!(input.agent));
    memory.insert("created".into(), json!(stamp));
    if !input.auto.is_empty() {
        memory.insert("auto".into(), json!(input.auto));
    }

    let parsed = parse_anchor(input.anchor);
    if !input.anchor.trim().is_empty() {
        let Some(anchor) = parsed.as_ref() else {
            return json!({"ok": false, "error": "bad anchor; use \"path::symbol\", \"path\", or \"dir/\""});
        };
        let (current, rev, _history) = anchor_state(store, input.scope, anchor);
        memory.insert("anchor".into(), anchor.to_value());
        // null when the anchor was absent at write time, which is itself a
        // fact worth keeping: the note describes code that is not there yet
        memory.insert("anchor_hash".into(), current.map(Value::from).unwrap_or(Value::Null));
        memory.insert("anchor_rev".into(), json!(rev));
    }

    let mut superseded = "";
    if !input.supersedes.is_empty() {
        let key = format!("{}:{}", input.scope, input.supersedes);
        let Some(mut target) = store.kv_get("memory", &key) else {
            return json!({
                "ok": false,
                "error": format!("supersedes target {} not found", input.supersedes),
            });
        };
        if let Some(map) = target.as_object_mut() {
            map.insert("superseded_by".into(), json!(id));
        }
        let _ = store.kv_put("memory", &key, &target, stamp);
        memory.insert("supersedes".into(), json!(input.supersedes));
        superseded = input.supersedes;
    }

    let memory = Value::Object(memory);
    let _ = store.kv_put("memory", &format!("{}:{id}", input.scope), &memory, stamp);

    let mut row = Map::new();
    row.insert("memory_id".into(), json!(id));
    row.insert("owner".into(), json!(input.user_id));
    row.insert("agent".into(), json!(input.agent));
    row.insert("tags".into(), json!(tags));
    if let Some(anchor) = parsed.as_ref() {
        row.insert("anchor".into(), anchor.to_value());
    }
    if !input.auto.is_empty() {
        row.insert("auto".into(), json!(input.auto));
    }
    let _ = store.ledger_append(input.scope, "memory_saved", &Value::Object(row), stamp);
    if !superseded.is_empty() {
        let _ = store.ledger_append(
            input.scope, "memory_superseded",
            &json!({"memory_id": superseded, "by": id, "owner": input.user_id}),
            stamp,
        );
    }
    json!({"ok": true, "memory_id": id})
}

// ----------------------------------------------------------- the honesty layer

/// Confidence from event history, never from timestamps.
///
/// Surviving an anchor rewrite raises it: the note stayed relevant while the
/// code moved underneath it, which is evidence. A contradiction caps it. An
/// anchor rewritten within an hour of the note being written marks the note
/// suspect — that is someone writing a fact and immediately changing the code
/// it describes. Age on its own means nothing and is deliberately not read.
pub fn confidence(memory: &Value, rewrites: i64, conflicted: bool, history: &[f64], outcomes: &Outcomes) -> f64 {
    if crate::compat::truthy(memory.get("superseded_by")) {
        return 0.1;
    }
    let mut score = 0.5 + (rewrites.min(4) as f64) * 0.1;
    let created = memory.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    if history.iter().any(|ts| created < *ts && *ts < created + 3600.0) {
        score -= 0.2;
    }
    if conflicted {
        score = score.min(0.3);
    }
    // reality checking the note, not agents voting on it: the dependents'
    // tests passing after a change to the anchored file raises it, failing
    // lowers it, and another person coming straight back to the file to fix
    // something lowers it. Each capped, so one loud day cannot swing it.
    score += 0.1 * outcomes.passes.min(2) as f64;
    score -= 0.2 * outcomes.failures.min(2) as f64;
    score -= 0.1 * outcomes.refixes.min(2) as f64;
    python_round(score.clamp(0.05, 0.95), 2)
}

/// What happened after a note was written, on the file it is anchored to:
/// how many verified test runs passed, how many failed, and how many times
/// a second person came back to the file within half an hour of someone
/// else's edit. Read from the ledger since the note, at most 30 days back.
/// A note anchored to nothing has no outcomes. Python's `_memory_outcomes`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcomes {
    pub passes: i64,
    pub failures: i64,
    pub refixes: i64,
}

pub const OUTCOME_WINDOW_S: f64 = 30.0 * 86400.0;
pub const REFIX_WINDOW_S: f64 = 1800.0;

pub fn outcomes(store: &Store, scope: &str, path: &str, created: f64, now: f64) -> Outcomes {
    let mut out = Outcomes::default();
    if path.is_empty() {
        return out;
    }
    let since = created.max(now - OUTCOME_WINDOW_S);
    let mut edits: Vec<(f64, String)> = Vec::new();
    for row in store.ledger_since(scope, since) {
        if row.payload.get("path").and_then(Value::as_str) != Some(path) {
            continue;
        }
        match row.kind.as_str() {
            "interface_verified" => match row.payload.get("status").and_then(Value::as_str) {
                Some("clear") => out.passes += 1,
                Some("failing") => out.failures += 1,
                _ => {}
            },
            "edit_reported" => edits.push((row.ts, row.payload.get("user").and_then(Value::as_str).unwrap_or("").to_string())),
            _ => {}
        }
    }
    for (i, (ts, user)) in edits.iter().enumerate() {
        if let Some((prev_ts, prev_user)) = edits[..i].last() {
            if prev_user != user && !user.is_empty() && ts - prev_ts <= REFIX_WINDOW_S {
                out.refixes += 1;
            }
        }
    }
    out
}

/// name -> path over the freshest clean symbols of every workspace.
///
/// Cross-file reference resolution is name-based and conservative here:
/// over-warning beats silent rot. When a name is defined in several files the
/// smallest path wins — neither the workspace nor the file listing is ordered,
/// so picking the first arrival made the answer depend on row order. Arbitrary
/// is fine; unstable is not, because a flag that comes and goes on its own
/// teaches an agent to ignore it.
pub fn symbol_index(store: &Store, scope: &str) -> std::collections::BTreeMap<String, String> {
    let mut index: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for workspace in store.list_workspaces(scope) {
        let user = workspace.get("user").and_then(Value::as_str).unwrap_or("");
        for (path, record) in store.list_files(scope, user) {
            let Some(symbols) = record
                .get("last_clean")
                .and_then(|clean| clean.get("symbols"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            for name in symbols.keys() {
                match index.get(name) {
                    Some(seen) if seen.as_str() <= path.as_str() => {}
                    _ => {
                        index.insert(name.clone(), path.clone());
                    }
                }
            }
        }
    }
    index
}

/// Budget for the forward slice. Cheap and silent beats thorough and slow:
/// this runs inside a recall, and a soft flag is not worth a long walk.
const SLICE_DEPTH: usize = 2;
const SLICE_REFS: usize = 20;
const SLICE_SEEN: usize = 50;
const SLICE_FRONTIER: usize = 25;

/// The forward-slice soft flag: did something the anchor DEPENDS on change
/// after this note was written?
///
/// This is the case a hash comparison cannot catch. The note is about A, A is
/// untouched, and the note is false anyway because callee B changed. Walked to
/// depth 2 over the references the parser extracted, and reported with the
/// originating change as the reason rather than as a bare flag.
pub fn possibly_stale(
    store: &Store, scope: &str, memory: &Value,
    index: &std::collections::BTreeMap<String, String>,
) -> Option<Value> {
    let anchor = memory.get("anchor")?;
    if anchor.get("kind").and_then(Value::as_str) != Some("symbol") {
        return None;
    }
    let anchor_path = anchor.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let anchor_symbol = anchor.get("symbol").and_then(Value::as_str).unwrap_or("").to_string();
    let created = memory.get("created").and_then(Value::as_f64).unwrap_or(0.0);

    let origin = (anchor_path.clone(), anchor_symbol.clone());
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut frontier: Vec<(String, String)> = vec![origin.clone()];

    for _depth in 0..SLICE_DEPTH {
        let mut next: Vec<(String, String)> = Vec::new();
        for (path, name) in &frontier {
            let symbols = freshest_symbols(store, scope, path).unwrap_or_default();
            let refs: Vec<String> = symbols
                .get(name)
                .and_then(|entry| entry.get("refs"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .take(SLICE_REFS)
                        .collect()
                })
                .unwrap_or_default();
            for reference in refs {
                let target_path = if symbols.contains_key(&reference) {
                    path.clone()
                } else {
                    match index.get(&reference) {
                        Some(found) => found.clone(),
                        None => continue,
                    }
                };
                let target = (target_path.clone(), reference.clone());
                if seen.contains(&target) || target == origin {
                    continue;
                }
                seen.insert(target.clone());
                let rev = store.kv_get(
                    "symrev", &format!("{scope}:{}:{reference}", path_key(&target_path)));
                let history = rev
                    .as_ref()
                    .and_then(|rev| rev.get("history").and_then(Value::as_array))
                    .cloned()
                    .unwrap_or_default();
                for entry in history {
                    let ts = match &entry {
                        Value::Object(map) => map.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
                        other => other.as_f64().unwrap_or(0.0),
                    };
                    if ts > created {
                        return Some(json!({
                            "symbol": reference,
                            "path": target_path,
                            "changed_at": ts,
                            "why": format!(
                                "{anchor_symbol} depends on {reference}, \
                                 which changed after this note was written"),
                        }));
                    }
                }
                next.push(target);
            }
            if seen.len() > SLICE_SEEN {
                return None; // budget: stay cheap, stay silent
            }
        }
        next.truncate(SLICE_FRONTIER);
        frontier = next;
    }
    // the other direction: a note about how a symbol is USED is false when
    // a caller changed, even though the symbol itself did not
    let rev_key = format!("{scope}:{}", crate::graphview::node_id(&anchor_path, &anchor_symbol));
    let mut dependents: Vec<String> = store
        .kv_get(crate::codegraph::REV_BUCKET, &rev_key)
        .and_then(|doc| doc.get("dependents").and_then(Value::as_object).cloned())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    dependents.sort();
    for nid in dependents.iter().take(25) {
        let (dep_path, dep_symbol) = crate::graphview::split_node(nid);
        if dep_path.is_empty() || dep_symbol.is_empty() {
            continue;
        }
        let rev = store.kv_get("symrev", &format!("{scope}:{}:{dep_symbol}", path_key(dep_path)));
        let history = rev
            .as_ref()
            .and_then(|rev| rev.get("history").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        for entry in history {
            let ts = match &entry {
                Value::Object(map) => map.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
                other => other.as_f64().unwrap_or(0.0),
            };
            if ts > created {
                return Some(json!({
                    "symbol": dep_symbol,
                    "path": dep_path,
                    "changed_at": ts,
                    "why": format!(
                        "{anchor_symbol} is called by {dep_symbol}, \
                         which changed after this note was written"),
                }));
            }
        }
    }
    None
}

/// Attach the honesty layer to anchored notes.
///
/// Stale notes are annotated, never dropped: one that survived a dozen
/// rewrites may be the most important context on the anchor, and silently
/// hiding it would be the one failure mode this whole layer exists to avoid.
pub fn annotate(store: &Store, scope: &str, memories: &[Value]) -> Vec<Value> {
    // conflicts are judged against ALL notes on an anchor, not just the
    // queried subset — a contradiction outside the query still counts
    let mut by_anchor: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    if memories.iter().any(|m| m.get("anchor").map(|a| !a.is_null()).unwrap_or(false)) {
        for (_key, sibling) in store.kv_list("memory", &format!("{scope}:")) {
            if let Some(anchor) = sibling.get("anchor").filter(|a| !a.is_null()) {
                by_anchor.entry(anchor_key(anchor)).or_default().push(sibling.clone());
            }
        }
    }

    let mut states: std::collections::BTreeMap<String, (Option<String>, i64, Vec<f64>)> =
        std::collections::BTreeMap::new();
    let mut slice_index: Option<std::collections::BTreeMap<String, String>> = None;
    let mut out: Vec<Value> = Vec::with_capacity(memories.len());

    for memory in memories {
        let mut view = memory.clone();
        let Some(anchor_value) = memory.get("anchor").filter(|a| !a.is_null()).cloned() else {
            out.push(view);
            continue;
        };
        let key = anchor_key(&anchor_value);
        let kind = anchor_value.get("kind").and_then(Value::as_str).unwrap_or("");
        let parsed = match kind {
            "symbol" => Anchor::Symbol {
                path: anchor_value.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                symbol: anchor_value.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
            },
            "dir" => Anchor::Dir {
                path: anchor_value.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            },
            _ => Anchor::File {
                path: anchor_value.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            },
        };
        let state = states
            .entry(key.clone())
            .or_insert_with(|| anchor_state(store, scope, &parsed))
            .clone();
        let (current, rev, history) = state;
        let anchored_path = anchor_value.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        let created = memory.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let after = outcomes(store, scope, &anchored_path, created, crate::store::now());

        let written_at_rev = memory.get("anchor_rev").and_then(Value::as_i64).unwrap_or(0);
        let rewrites = (rev - written_at_rev).max(0);
        let written_hash = memory.get("anchor_hash").and_then(Value::as_str);
        let stale = written_hash.is_some_and(|hash| !hash.is_empty())
            && current.as_deref() != written_hash;

        let siblings: Vec<&Value> = by_anchor
            .get(&key)
            .map(|all| {
                all.iter()
                    .filter(|sibling| {
                        sibling.get("id") != memory.get("id")
                            && !crate::compat::truthy(sibling.get("superseded_by"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // compared as the hash each was written against, not as raw JSON:
        // an absent field and an explicit null are the same fact — "the
        // anchor did not exist when this was written" — and Python's dict
        // lookup returns None for both
        fn hash_of(note: &Value) -> Option<&str> {
            note.get("anchor_hash").and_then(Value::as_str)
        }
        let conflicts: Vec<String> = siblings
            .iter()
            .filter(|sibling| hash_of(sibling) != hash_of(memory))
            .filter_map(|sibling| sibling.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();

        if let Some(map) = view.as_object_mut() {
            map.insert("rewrites".into(), json!(rewrites));
            map.insert("stale".into(), json!(stale));
            if !conflicts.is_empty()
                && !crate::compat::truthy(memory.get("superseded_by"))
            {
                map.insert("conflict".into(), json!(conflicts));
            }
            map.insert(
                "confidence".into(),
                json!(confidence(memory, rewrites, !conflicts.is_empty(), &history, &after)),
            );
        }
        if !stale && kind == "symbol" {
            let index = slice_index.get_or_insert_with(|| symbol_index(store, scope));
            if let Some(reason) = possibly_stale(store, scope, memory, index) {
                if let Some(map) = view.as_object_mut() {
                    map.insert("possibly_stale".into(), reason);
                }
            }
        }
        out.push(view);
    }
    out
}

// ------------------------------------------------------------------- recall

/// Substring + tag + anchor-path match, newest first, annotated.
///
/// Searches the WHOLE WORKSPACE rather than one repo scope. A note should be
/// findable no matter which of a workspace's repo ids — or a renamed alias, or
/// a bare name a client happened to send — it was filed under; the workspace
/// is the unit people think in, and fragmenting memory across repo-id
/// spellings made notes silently unfindable. The queried repo's own notes rank
/// first so a repo-scoped call still leads with that repo.
///
/// Workspace-wide is not workspace-BLIND: membership is workspace-wide but
/// access is per repo, so the contributing scopes are filtered through the
/// caller's own allowlist. A note carries paths, symbol names and settled
/// rationales; a member invited to one repo must not read another's here.
pub fn recall(
    store: &Store, workspace: &str, uid: &str, user_id: &str, scope: &str, query: &str, tags: &[String],
    limit: i64, oldest_ts: f64, share_knowledge: bool,
) -> Value {
    let wanted: std::collections::BTreeSet<String> = tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    let needle = query.trim().to_lowercase();

    let entries = store.kv_list("memory", &format!("{workspace}:"));
    let member = crate::auth::member(store, workspace, uid);
    // key is "<ws>:<repo>:<id>"; strip the trailing id to get the scope
    let scope_of = |key: &str| key.rsplit_once(':').map(|(head, _)| head.to_string())
        .unwrap_or_else(|| key.to_string());
    // the contributing scopes come from the scope table, never from this
    // bucket's own keys: remove_repo clears shared_trees and leaves memory
    // rows behind, so enumerating from the keys resurrected a removed repo's
    // notes. The queried scope is always in — the caller was authorised for
    // it, and a repo with notes but no tree yet is in no table to be listed
    // from.
    let mut visible = crate::access::visible_scopes(store, workspace, member.as_ref());
    visible.insert(scope.to_string());

    let mut matches: Vec<(String, Value)> = Vec::new();
    for (key, memory) in entries {
        let mem_scope = scope_of(&key);
        if !visible.contains(&mem_scope) {
            continue;
        }
        // the plan's two knowledge limits: how far back a note may be
        // (ledger_days), and whether teammates' notes reach this caller
        if oldest_ts > 0.0 && memory.get("created").and_then(Value::as_f64).unwrap_or(0.0) < oldest_ts {
            continue;
        }
        if !share_knowledge && !crate::access::own_knowledge(&memory, user_id) {
            continue;
        }
        let anchor_text = memory
            .get("anchor")
            .filter(|a| !a.is_null())
            .map(|anchor| anchor_key(anchor).to_lowercase())
            .unwrap_or_default();
        if !needle.is_empty() {
            let fact = memory.get("fact").and_then(Value::as_str).unwrap_or("").to_lowercase();
            if !fact.contains(&needle) && !anchor_text.contains(&needle) {
                continue;
            }
        }
        if !wanted.is_empty() {
            let has = memory
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter()
                        .filter_map(Value::as_str)
                        .any(|tag| wanted.contains(tag))
                })
                .unwrap_or(false);
            if !has {
                continue;
            }
        }
        matches.push((mem_scope, memory));
    }

    let total = matches.len();
    // queried repo's notes first so they survive truncation, then newest
    matches.sort_by(|a, b| {
        let key = |pair: &(String, Value)| {
            (
                pair.0 == scope,
                pair.1.get("created").and_then(Value::as_f64).unwrap_or(0.0),
            )
        };
        let (a_scope, a_created) = key(a);
        let (b_scope, b_created) = key(b);
        b_scope
            .cmp(&a_scope)
            .then(b_created.partial_cmp(&a_created).unwrap_or(std::cmp::Ordering::Equal))
    });
    matches.truncate(limit.clamp(1, 100) as usize);

    let mut by_scope: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (mem_scope, memory) in matches {
        by_scope.entry(mem_scope).or_default().push(memory);
    }
    let mut annotated: Vec<Value> = Vec::new();
    for (mem_scope, memories) in &by_scope {
        annotated.extend(annotate(store, mem_scope, memories));
    }
    annotated.sort_by(|a, b| {
        let created = |v: &Value| v.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        created(b).partial_cmp(&created(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    json!({"memories": annotated, "total_matched": total})
}

// -------------------------------------------------------------- scar mining

/// Failed attempts mined from the ledger with zero voluntary writes.
///
/// This is the memory agents never write themselves. Nobody logs "I tried
/// this and it did not work" — they move on — so the one record of a failed
/// attempt is the shape it left in the ledger: an intent declared, edits
/// landing on its target symbols, then a later edit restoring the prior hash.
/// That pattern is a revert, and a revert is the most expensive thing for the
/// next agent not to know.
///
/// Revert detection reads the SCOPE-level hash trajectory rather than one
/// workspace's, so a restore from anywhere counts. Deduped by intent and
/// symbol, and its value compounds with the age of the ledger.
pub fn mine_scars(store: &Store, scope: &str, since_s: f64) -> Vec<Value> {
    let stamp = now();
    let rows = store.ledger_since(scope, stamp - since_s);

    // insertion order, which is ledger order: an intent declared first is
    // mined first, so the notes come out in the order the work happened
    let mut order: Vec<String> = Vec::new();
    let mut intents: std::collections::BTreeMap<String, (Value, f64)> =
        std::collections::BTreeMap::new();
    let mut edits: Vec<(Value, f64)> = Vec::new();

    for row in &rows {
        match row.kind.as_str() {
            "intent_declared" => {
                let id = row.payload.get("intent_id").and_then(Value::as_str).unwrap_or("");
                if !intents.contains_key(id) {
                    order.push(id.to_string());
                }
                intents.insert(id.to_string(), (row.payload.clone(), row.ts));
            }
            "edit_reported"
                if row
                    .payload
                    .get("symbols_changed")
                    .and_then(Value::as_object)
                    .map(|changed| !changed.is_empty())
                    .unwrap_or(false) =>
            {
                edits.push((row.payload.clone(), row.ts));
            }
            _ => {}
        }
    }

    let mut minted: Vec<Value> = Vec::new();
    for intent_id in &order {
        let Some((intent, declared_at)) = intents.get(intent_id) else { continue };
        let targets: std::collections::BTreeSet<String> = intent
            .get("symbols")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if targets.is_empty() {
            continue;
        }
        let owner = intent.get("owner").and_then(Value::as_str).unwrap_or("");

        // an edit belongs to an intent when it says so, or when the same
        // person touched one of the intent's symbols after declaring it
        let linked: Vec<&(Value, f64)> = edits
            .iter()
            .filter(|(payload, ts)| {
                if payload.get("intent_id").and_then(Value::as_str) == Some(intent_id.as_str()) {
                    return true;
                }
                payload.get("user").and_then(Value::as_str) == Some(owner)
                    && *ts >= *declared_at
                    && payload
                        .get("symbols_changed")
                        .and_then(Value::as_object)
                        .map(|changed| changed.keys().any(|name| targets.contains(name)))
                        .unwrap_or(false)
            })
            .collect();

        for symbol in &targets {
            let Some((payload, edit_ts)) = linked.iter().find(|(payload, _)| {
                payload
                    .get("symbols_changed")
                    .and_then(Value::as_object)
                    .map(|changed| changed.contains_key(symbol))
                    .unwrap_or(false)
            }) else {
                continue;
            };
            let delta = payload.get("symbols_changed").and_then(|c| c.get(symbol));
            let original = delta.and_then(|d| d.get("before")).and_then(Value::as_str);
            let Some(original) = original else { continue }; // no baseline to revert TO
            if delta.and_then(|d| d.get("after")).and_then(Value::as_str) == Some(original) {
                continue; // never actually changed it
            }

            let path = payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let rev = store.kv_get("symrev", &format!("{scope}:{}:{symbol}", path_key(&path)));
            let reverter = rev
                .as_ref()
                .and_then(|rev| rev.get("history").and_then(Value::as_array))
                .and_then(|history| {
                    history.iter().find(|entry| {
                        entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > *edit_ts
                            && entry.get("hash").and_then(Value::as_str) == Some(original)
                    })
                });
            let Some(reverter) = reverter else { continue }; // the change stuck: not a scar

            let marker = format!("{scope}:scar:{intent_id}:{symbol}");
            if store.kv_get("scar", &marker).is_some() {
                continue; // already minted
            }

            let attempted = match intent.get("operations").and_then(Value::as_array) {
                Some(ops) if !ops.is_empty() => ops
                    .iter()
                    .map(|op| {
                        format!(
                            "{}({})",
                            op.get("op").and_then(Value::as_str).unwrap_or(""),
                            op.get("symbol").and_then(Value::as_str).unwrap_or(""),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => intent
                    .get("change_type")
                    .and_then(Value::as_str)
                    .unwrap_or("change")
                    .to_string(),
            };
            let summary = intent
                .get("summary")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| intent.get("after").and_then(Value::as_str).filter(|s| !s.is_empty()))
                .unwrap_or("n/a");
            let reverted_by = reverter
                .get("by")
                .and_then(Value::as_str)
                .filter(|by| !by.is_empty())
                .unwrap_or("?");
            let after_s = python_round(
                reverter.get("ts").and_then(Value::as_f64).unwrap_or(stamp) - *edit_ts, 0) as i64;

            let anchor = Anchor::Symbol { path: path.clone(), symbol: symbol.clone() };
            let (current, sym_rev, _history) = anchor_state(store, scope, &anchor);
            let id = crate::compat::new_id();
            let memory = json!({
                "id": id,
                "fact": format!(
                    "[scar {intent_id}] attempted {attempted} on {symbol} by {}; \
                     reverted by {reverted_by} {after_s}s later — summary was: {summary}",
                    if owner.is_empty() { "?" } else { owner }),
                "tags": ["scar"],
                "owner": "collide",
                "agent": "collide-miner",
                "auto": "scar",
                // whose knowledge this is: the intent's owner and whoever
                // reverted it — what a plan that does not share knowledge
                // reads to hand a player their own scars (own_knowledge)
                "intent_owner": owner,
                "reverted_by": reverted_by,
                "created": stamp,
                "anchor": anchor.to_value(),
                "anchor_hash": current.map(Value::from).unwrap_or(Value::Null),
                "anchor_rev": sym_rev,
            });
            let _ = store.kv_put("memory", &format!("{scope}:{id}"), &memory, stamp);
            let _ = store.kv_put(
                "scar", &marker, &json!({"memory_id": id, "ts": stamp}), stamp);
            let _ = store.ledger_append(
                scope, "memory_saved",
                &json!({
                    "memory_id": id, "owner": "collide", "agent": "collide-miner",
                    "tags": ["scar"], "anchor": anchor.to_value(), "auto": "scar",
                }),
                stamp,
            );
            minted.push(memory);
        }
    }
    minted
}

/// Mine fresh scars, then serve the newest scar notes annotated — the
/// briefing is where a session absorbs what was already tried.
pub fn scars_for_briefing(store: &Store, scope: &str, limit: usize) -> Vec<Value> {
    mine_scars(store, scope, 30.0 * 86_400.0);
    let mut scars: Vec<Value> = store
        .kv_list("memory", &format!("{scope}:"))
        .into_iter()
        .map(|(_key, memory)| memory)
        .filter(|memory| memory.get("auto").and_then(Value::as_str) == Some("scar"))
        .collect();
    scars.sort_by(|a, b| {
        let created = |v: &Value| v.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        created(b).partial_cmp(&created(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    scars.truncate(limit);
    annotate(store, scope, &scars)
}

// ------------------------------------------------------ delivery by locality

/// Notes anchored to the touched symbols, their files, and every ancestor
/// directory up to the root.
///
/// This is the delivery mechanism that matters. Agents do not know what they
/// do not know, so query-based retrieval fails silently — the agent never
/// thinks to search and the note never surfaces. Here the note arrives because
/// the agent touched the thing it is about. Superseded notes stay out; stale
/// ones stay in, flagged, because a note that survived the code moving under
/// it may be the most important context on that code.
pub fn locality_memories(
    store: &Store, scope: &str, paths: &[String], symbols: &[String], limit: usize,
) -> Vec<Value> {
    let touched_paths: std::collections::BTreeSet<String> = paths
        .iter()
        .map(|path| path.trim().trim_matches('/').to_string())
        .filter(|path| !path.is_empty())
        .collect();
    let touched_symbols: std::collections::BTreeSet<String> = symbols
        .iter()
        .map(|symbol| symbol.trim().to_string())
        .filter(|symbol| !symbol.is_empty())
        .collect();
    if touched_paths.is_empty() && touched_symbols.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<Value> = Vec::new();
    for (_key, memory) in store.kv_list("memory", &format!("{scope}:")) {
        let Some(anchor) = memory.get("anchor").filter(|a| !a.is_null()) else { continue };
        if crate::compat::truthy(memory.get("superseded_by")) {
            continue;
        }
        let kind = anchor.get("kind").and_then(Value::as_str).unwrap_or("");
        let anchor_path = anchor.get("path").and_then(Value::as_str).unwrap_or("");
        let anchor_symbol = anchor.get("symbol").and_then(Value::as_str).unwrap_or("");
        let matched = match kind {
            "symbol" => {
                touched_paths.contains(anchor_path) || touched_symbols.contains(anchor_symbol)
            }
            "file" => touched_paths.contains(anchor_path),
            // a directory anchor covers every path beneath it; the empty path
            // is the repo root and covers everything
            _ => {
                anchor_path.is_empty()
                    || touched_paths.iter().any(|path| {
                        path == anchor_path || path.starts_with(&format!("{anchor_path}/"))
                    })
            }
        };
        if matched {
            hits.push(memory);
        }
    }

    let mut annotated = annotate(store, scope, &hits);
    // most trustworthy first, then newest — a confident old note beats a
    // shaky new one, which is the ordering the confidence score exists for
    annotated.sort_by(|a, b| {
        let key = |note: &Value| {
            (
                note.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
                note.get("created").and_then(Value::as_f64).unwrap_or(0.0),
            )
        };
        let (a_confidence, a_created) = key(a);
        let (b_confidence, b_created) = key(b);
        b_confidence
            .partial_cmp(&a_confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_created.partial_cmp(&a_created).unwrap_or(std::cmp::Ordering::Equal))
    });
    annotated.truncate(limit);
    annotated
}

/// The freshest clean parse of one symbol across every workspace, with the
/// provenance that says how much to trust it.
///
/// `how` is the field worth reading: an agent-claimed parse and a
/// disk-confirmed one are different kinds of fact, and collapsing them would
/// let a claim that was never written to disk masquerade as the state of the
/// repo.
pub fn get_symbol(store: &Store, scope: &str, path: &str, symbol: &str) -> Value {
    let stamp = now();
    let mut best: Option<(f64, Value)> = None;

    for (user, record) in store.get_file_all_users(scope, path) {
        let Some(served) = semantics::serve(Some(&record), stamp) else { continue };
        if !symbol.is_empty() && !served.symbols.contains_key(symbol) {
            continue;
        }
        let entry = served.symbols.get(symbol).cloned().unwrap_or(Value::Null);
        let ts = record
            .get("last_clean")
            .and_then(|clean| clean.get("ts"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if best.as_ref().map(|(seen, _)| ts > *seen).unwrap_or(true) {
            let provenance = record.get("provenance").cloned().unwrap_or_else(|| json!({}));
            let source = provenance.get("source").and_then(Value::as_str).unwrap_or("claim");
            let via = provenance
                .get("agent")
                .and_then(Value::as_str)
                .filter(|agent| !agent.is_empty())
                .unwrap_or("unknown agent");
            let confirmation = if source == "disk" {
                "disk-confirmed by the watcher"
            } else {
                "agent-claimed, not disk-confirmed"
            };
            // one symbol, or — with no symbol named — the whole module:
            // every symbol's entry, the module's interface in one answer
            let mut found = json!({
                "found": true,
                "path": path,
                "author": user,
                "freshness": served.freshness,
                "age_s": served.age_s,
                "provenance": provenance,
                "how": format!(
                    "reported by {user} via {via} ({confirmation}); clean parse {}s ago ({}){}",
                    crate::compat::python_round(served.age_s, 0) as i64, served.freshness,
                    match provenance.get("verified").and_then(|v| v.get("command")).and_then(Value::as_str) {
                        Some(cmd) if provenance.pointer("/verified/ok").and_then(Value::as_bool).unwrap_or(true) => format!("; verified by `{cmd}` before it was reported"),
                        Some(cmd) => format!("; the repo check `{cmd}` FAILED after this write"),
                        None => String::new(),
                    }),
            });
            if let Some(map) = found.as_object_mut() {
                if symbol.is_empty() {
                    map.insert("symbols".into(), json!(served.symbols));
                } else {
                    map.insert("symbol".into(), entry);
                }
            }
            best = Some((ts, found));
        }
    }

    // a file the map learned by observation — indexed at session start, or
    // read — has no write record and no author, but its signatures and facts
    // are exact: serve them from the graph record, marked as observed
    if best.is_none() {
        let key = format!("{scope}:{}", crate::codegraph::path_key(path));
        if let Some(record) = store.kv_get(crate::codegraph::GRAPH_BUCKET, &key) {
            let entries = record.get("symbols").and_then(Value::as_object).cloned().unwrap_or_default();
            let has = symbol.is_empty() || entries.contains_key(symbol);
            if !entries.is_empty() && has {
                let mut symbols = serde_json::Map::new();
                for (name, entry) in &entries {
                    let mut item = json!({"name": name});
                    if let Some(map) = item.as_object_mut() {
                        for key in ["kind", "signature", "hash", "span", "params", "sites", "edges", "doc"] {
                            if let Some(v) = entry.get(key) {
                                map.insert(key.to_string(), v.clone());
                            }
                        }
                    }
                    symbols.insert(name.clone(), item);
                }
                let ts = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                let mut provenance = json!({"source": "observed", "ts": ts});
                let mut how = "indexed from a read or the tree, not from a write: signatures and facts exact, no author".to_string();
                if let Some(mark) = record.get("verified").filter(|m| m.is_object()) {
                    provenance["verified"] = mark.clone();
                    if let Some(cmd) = mark.get("command").and_then(Value::as_str) {
                        how.push_str(&if mark.get("ok").and_then(Value::as_bool).unwrap_or(true) {
                            format!("; verified by `{cmd}` on this tree")
                        } else {
                            format!("; check FAILED (`{cmd}`) on this tree")
                        });
                    }
                }
                let mut found = json!({
                    "found": true,
                    "path": path,
                    "author": "",
                    "freshness": "observed",
                    "age_s": crate::compat::python_round((stamp - ts).max(0.0), 3),
                    "provenance": provenance,
                    "how": how,
                });
                if let Some(map) = found.as_object_mut() {
                    if symbol.is_empty() {
                        map.insert("symbols".into(), Value::Object(symbols));
                    } else if let Some(one) = symbols.get(symbol) {
                        map.insert("symbol".into(), one.clone());
                    }
                }
                best = Some((ts, found));
            }
        }
    }
    let Some((_ts, mut found)) = best else {
        return json!({"found": false, "path": path, "name": symbol});
    };
    // the module's own docstring rides the graph record (both paths)
    if symbol.is_empty() {
        let key = format!("{scope}:{}", crate::codegraph::path_key(path));
        let doc = store
            .kv_get(crate::codegraph::GRAPH_BUCKET, &key)
            .and_then(|record| record.get("doc").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        if !doc.is_empty() {
            if let Some(map) = found.as_object_mut() {
                map.insert("doc".into(), json!(doc));
            }
        }
    }
    let names: Vec<String> = if symbol.is_empty() {
        found.get("symbols").and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect()).unwrap_or_default()
    } else {
        vec![symbol.to_string()]
    };
    let anchored = locality_memories(store, scope, &[path.to_string()], &names, 10);
    if !anchored.is_empty() {
        if let Some(map) = found.as_object_mut() {
            map.insert("anchored_memories".into(), json!(anchored));
        }
    }
    found
}
