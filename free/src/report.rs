//! The write path: an agent edited a file, and here is what it now contains.
//!
//! Everything downstream depends on this being right. The parse decides what
//! symbols exist, the semantics decide what counts as a change, the merkle
//! root decides whether two workspaces have diverged, the hot markers decide
//! what the gate blocks on, and the ledger row is the permanent record.
//!
//! The content is parsed, hashed and dropped here. It is never written to
//! disk, never logged, and never leaves this function.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::codegraph;
use crate::diff;
use crate::hashing::{file_hash, Tree};
use crate::repo::path_key;
use crate::semantics;
use crate::store::{now, Store};

const MAX_LEDGER_EVENTS: usize = 12;
const MAX_LEDGER_HUNKS: usize = 8;
const MAX_FIRST_SIGHT_SYMBOLS: usize = 50;
const LINEHASH_TTL_S: f64 = 30.0 * 86400.0;
const LASTEDIT_TTL_S: f64 = 86400.0;
/// only certain, blocking-grade changes feed the gate's fast path
const BLOCKING_KINDS: [&str; 2] = ["renamed", "removed"];

/// Per-row cost, which model spent it, and WHICH TURN — attached to every
/// edit row. In a cached agent loop the cache counters dominate the bill, so
/// `tokens` (uncached input + output) alone understated it several-fold;
/// `tokens` still rides along for pre-v20 readers and pre-v20 hooks that send
/// only it. turn_id rides the row too: a turn writes ONE row per file it
/// touched, all carrying the same usage, and only turn_id lets the reader
/// fold them back into one turn's spend.
fn attach_cost(row: &mut Map<String, Value>, input: &ReportInput) {
    if input.tokens != 0 {
        row.insert("tokens".into(), json!(input.tokens));
    }
    for (field, count) in [
        ("input_tokens", input.input_tokens),
        ("output_tokens", input.output_tokens),
        ("cache_read_tokens", input.cache_read_tokens),
        ("cache_creation_tokens", input.cache_creation_tokens),
    ] {
        if count != 0 {
            row.insert(field.into(), json!(count));
        }
    }
    if !input.turn_id.is_empty() {
        row.insert("turn_id".into(), json!(input.turn_id));
    }
    if !input.model.is_empty() {
        row.insert("model".into(), json!(input.model));
    }
    if !input.branch.is_empty() {
        row.insert("branch".into(), json!(input.branch));
    }
}

pub struct ReportInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub path: &'a str,
    pub content: &'a str,
    /// A native hook that parsed on its machine sends the facts instead of
    /// the file: `{structure, line_hashes, names}` (collide_core::local).
    /// `content` is then empty and never existed on this server.
    pub local: Option<&'a Value>,
    pub agent: &'a str,
    pub model: &'a str,
    pub branch: &'a str,
    pub session: &'a str,
    pub tokens: i64,
    /// The four counters a turn is actually billed on. A v20+ hook sends
    /// them; older hooks send only `tokens` (uncached input + output).
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub turn_id: &'a str,
    /// Why this change, in the agent's words, at most 200 characters:
    /// asked for only on a hot symbol, kept on the row and the marker.
    pub why: &'a str,
    pub hot_ttl_s: f64,
    pub via: &'a str,
    pub workspace: &'a str,
    /// presence, not state: the symbols are broadcast but nothing durable moves
    pub draft: bool,
    /// True for a hook-path report (the machine reported mechanically),
    /// false for one the model made through the tool — the case the
    /// install nudge exists for. Python's `auto` parameter.
    pub auto: bool,
    /// The repo's own check, run on this content before it was reported:
    /// {command, ok}. A fact the next agent reads instead of re-running or
    /// re-reading to decide whether upstream work is good to go.
    pub verified: Option<Value>,
}

/// The hook ran the dependents' tests for an interface change it had
/// reported (the report's `verify` plan): grade the change and write the
/// grade onto its hot marker — kept, never deleted, because the gate reads
/// the marker to guard the FILE while the grade decides whether the graph
/// keeps the CALLERS red. Clear when every dependent was covered and every
/// test passed; partial when a dependent had no test; failing when one
/// failed; unknown when the run timed out. One ledger row and one event, so
/// the journal and the feed say it too. Python's `record_verification`.
pub fn record_verification(
    store: &Store, scope: &str, user_id: &str, session: &str, path: &str, run: &Value, hot_ttl_s: f64, via: &str,
) -> Value {
    let now = crate::store::now();
    let identity = if session.is_empty() { user_id.to_string() } else { format!("{user_id}#{session}") };
    let key = format!("hot:{scope}:{}:{identity}", path_key(path));
    let verified = crate::graphview::verification_from_run(run, now);
    let marker = store.eph_get(&key);
    if let Some(mut record) = marker.clone() {
        if let Some(map) = record.as_object_mut() {
            map.insert("verified".into(), verified.clone());
        }
        let started = record.get("ts").and_then(Value::as_f64).unwrap_or(now);
        let _ = store.eph_set(&key, &record, Some((started + hot_ttl_s - now).max(1.0)));
    }
    let symbols: Vec<String> = run
        .get("symbols")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut row = json!({"user": user_id, "session": session, "path": path, "symbols": symbols});
    for field in ["status", "tests", "passed", "failed", "uncovered", "skipped", "command", "elapsed_ms"] {
        row[field] = verified.get(field).cloned().unwrap_or(Value::Null);
    }
    let _ = store.ledger_append(scope, "interface_verified", &row, now);
    let failing = verified.get("failed").and_then(Value::as_array).map_or(0, |a| a.len());
    crate::events::publish(
        store,
        scope,
        json!({"kind": "interface_verified", "user": user_id, "path": path,
               "status": verified["status"], "symbols": symbols, "failing_tests": failing}),
        via,
    );
    json!({"ok": true, "status": verified["status"], "marker": marker.is_some()})
}

/// Finer motion: a draft shows teammates which symbols are moving RIGHT NOW,
/// before the write lands. Presence only — the durable tree, the ledger and
/// the compliance stats never see it.
fn report_draft(
    store: &Store, input: &ReportInput, parsed: Option<&collide_core::engine::ParseOutput>,
    hunks: &[Value], lines_added: i64, lines_removed: i64, stamp: f64,
) -> Value {
    let record = store.get_file(input.scope, input.user_id, input.path).unwrap_or(Value::Null);
    let baseline = record
        .get("last_clean")
        .and_then(|clean| clean.get("symbols"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let current = parsed.map(symbol_map).unwrap_or_default();
    let status = parsed.map(|p| p.status).unwrap_or("unsupported");

    let changed: Vec<String> = current
        .iter()
        .filter(|(name, symbol)| {
            baseline
                .get(*name)
                .and_then(|prior| prior.get("hash"))
                != symbol.get("hash")
        })
        .map(|(name, _)| name.clone())
        .collect();
    let removed: Vec<String> = if current.is_empty() {
        Vec::new()
    } else {
        baseline.keys().filter(|name| !current.contains_key(*name)).cloned().collect()
    };
    let changed_capped: Vec<&String> = changed.iter().take(20).collect();
    let removed_capped: Vec<&String> = removed.iter().take(10).collect();
    let hunks_capped: Vec<&Value> = hunks.iter().take(crate::diff::MAX_HUNKS).collect();

    let _ = store.eph_set(
        &format!("draft:{}:{}:{}", input.scope, input.user_id, path_key(input.path)),
        &json!({
            "path": input.path, "user": input.user_id, "agent": input.agent, "ts": stamp,
            "parse": status, "symbols_changed": changed_capped, "symbols_removed": removed_capped,
            "hunks": hunks_capped, "lines_added": lines_added, "lines_removed": lines_removed,
        }),
        Some(600.0),
    );
    let _ = store.eph_set(
        &format!("lastedit:{}:{}:{}", input.scope, input.user_id, path_key(input.path)),
        &json!({"path": input.path, "ts": stamp, "agent": input.agent, "session": input.session}),
        Some(LASTEDIT_TTL_S),
    );
    crate::events::publish(
        store,
        input.scope,
        json!({
            "kind": "edit", "stage": "draft", "user": input.user_id, "agent": input.agent,
            "path": input.path, "parse": status,
            "symbols_changed": changed_capped, "symbols_removed": removed_capped,
            "hunks": hunks, "lines_added": lines_added, "lines_removed": lines_removed,
        }),
        input.via,
    );
    crate::events::with_deltas(store, input.scope, input.user_id, json!({
        "ok": true, "stage": "draft", "parse": status, "path": input.path,
        "symbols_changed": changed, "symbols_removed": removed,
        "hunks": hunks, "lines_added": lines_added, "lines_removed": lines_removed,
        "note": "draft broadcast: teammates see the motion; state is unchanged. Send the final report_edit (without draft) when the write lands.",
    }))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn symbol_map(result: &collide_core::engine::ParseOutput) -> Map<String, Value> {
    let mut out = Map::new();
    for symbol in &result.symbols {
        let mut entry = Map::new();
        entry.insert("name".into(), json!(symbol.name));
        entry.insert("kind".into(), json!(symbol.kind));
        entry.insert("signature".into(), json!(symbol.signature));
        entry.insert("hash".into(), json!(symbol.hash));
        if !symbol.refs.is_empty() {
            entry.insert("refs".into(), json!(symbol.refs));
        }
        if !symbol.edges.is_empty() {
            let edges: Vec<Value> = symbol
                .edges
                .iter()
                .map(|(target, member, kind)| json!([target, member, kind]))
                .collect();
            entry.insert("edges".into(), json!(edges));
        }
        entry.insert("span".into(), json!([symbol.span.0, symbol.span.1]));
        if !symbol.params.is_empty() {
            entry.insert("params".into(), json!(symbol.params));
        }
        if !symbol.doc.is_empty() {
            entry.insert("doc".into(), json!(symbol.doc));
        }
        if !symbol.sites.is_empty() {
            let sites: Vec<Value> = symbol.sites.iter().map(|(t, l, a)| json!([t, l, a])).collect();
            entry.insert("sites".into(), json!(sites));
        }
        out.insert(symbol.name.clone(), Value::Object(entry));
    }
    out
}

/// The parsed imports in the shape the graph stores them.
fn import_specs(parsed: &collide_core::engine::ParseOutput) -> Vec<Value> {
    parsed
        .imports
        .iter()
        .map(|import| {
            let names: Vec<Value> = import
                .names
                .iter()
                .map(|(local, original)| json!([local, original]))
                .collect();
            json!({"module": import.module, "names": names})
        })
        .collect()
}

const COPY_TTL_S: f64 = 86400.0;

fn copy_key(scope: &str, user_id: &str, session: &str, path: &str) -> String {
    format!("copy:{scope}:{user_id}:{session}:{}", path_key(path))
}

/// A file as this agent's own copy holds it, name to signature, from its last
/// read or write: what `semantics::across_copies` tells its own changes by.
pub fn remember_copy(store: &Store, scope: &str, user_id: &str, session: &str, path: &str, signatures: &BTreeMap<String, String>) {
    let _ = store.eph_set(&copy_key(scope, user_id, session, path), &json!(signatures), Some(COPY_TTL_S));
}

fn own_copy(store: &Store, scope: &str, user_id: &str, session: &str, path: &str) -> Option<Map<String, Value>> {
    store.eph_get(&copy_key(scope, user_id, session, path)).and_then(|v| v.as_object().cloned())
}

pub struct ObserveInput<'a> {
    pub scope: &'a str,
    pub path: &'a str,
    pub content: &'a str,
    /// the hook's own parse, when it sent one instead of the file
    pub structure: Option<&'a Value>,
    pub hot_ttl_s: f64,
}

/// A file the agent looked at, or one the session-start index swept: parsed
/// in memory, folded into the code graph, discarded. None of the edit
/// semantics run — no tree advance, no ledger row, no hot markers, no
/// tripwires, no watchdog — because nobody changed anything; the map just
/// learned what is there. Only a clean parse teaches it: a file that does
/// not parse would teach it nothing true.
pub fn observe(store: &Store, input: &ObserveInput) -> Value {
    observe_batch(store, input.scope, &[(input.path, input.content, input.structure)], input.hot_ttl_s)
        .pop()
        .unwrap_or_else(|| json!({"path": input.path, "parse": "unsupported", "map": "skipped"}))
}

/// A file's symbols as its copy holds them, name to signature: what the gate
/// checks a read against (`gate::acknowledge_read`). None when it does not parse.
pub fn symbol_signatures(path: &str, content: &str, structure: Option<&Value>) -> Option<BTreeMap<String, String>> {
    let parsed = match structure {
        Some(structure) => collide_core::local::from_structure(path, structure),
        None => collide_core::engine::parse(path, content),
    }?;
    Some(parsed.symbols.iter().map(|s| (s.name.clone(), s.signature.clone())).collect())
}

/// Many files at once, the graph read once for all of them
/// (`codegraph::observe_files`). One answer per file, in order.
pub fn observe_batch(store: &Store, scope: &str, files: &[(&str, &str, Option<&Value>)], hot_ttl_s: f64) -> Vec<Value> {
    let now = crate::store::now();
    let mut answers: Vec<Option<Value>> = Vec::with_capacity(files.len());
    let mut batch: Vec<(String, Value)> = Vec::new();
    let mut pending: Vec<(usize, String, String, Map<String, Value>)> = Vec::new(); // (answer slot, path, doc, symbols)
    for (path, content, structure) in files {
        let parsed = match structure {
            Some(structure) => collide_core::local::from_structure(path, structure),
            None => collide_core::engine::parse(path, content),
        };
        let Some(parsed) = parsed else {
            answers.push(Some(json!({"path": path, "parse": "unsupported", "map": "skipped"})));
            continue;
        };
        if parsed.status != semantics::CLEAN {
            answers.push(Some(json!({"path": path, "parse": parsed.status, "map": "skipped"})));
            continue;
        }
        let symbols: BTreeMap<String, Value> = symbol_map(&parsed).into_iter().collect();
        let imports = import_specs(&parsed);
        let record = codegraph::file_record(path, parsed.language, &symbols, &imports, now, &parsed.doc);
        pending.push((answers.len(), path.to_string(), parsed.doc.clone(), symbols.into_iter().collect()));
        batch.push((path.to_string(), record));
        answers.push(None);
    }
    let outcomes = codegraph::observe_files(store, scope, batch, now, hot_ttl_s);
    for ((slot, path, doc, symbols), outcome) in pending.into_iter().zip(outcomes) {
        if outcome == "indexed" {
            // one file (an agent's read) is live; a batch is an index
            crate::embed::enqueue(scope, &path, &doc, &symbols, files.len() == 1);
        }
        answers[slot] = Some(json!({"path": path, "parse": "clean", "map": outcome}));
    }
    answers.into_iter().map(|a| a.unwrap_or(Value::Null)).collect()
}

/// Other agents with a stake in this file right now: live hot markers by
/// another identity, plus open intents naming the path by another agent.
/// Python's `_others_on_file`, same count.
pub fn others_on_file(store: &Store, scope: &str, path: &str, identity: &str, user_id: &str) -> usize {
    let mut agents: BTreeSet<String> = BTreeSet::new();
    for (key, _marker) in store.eph_scan(&format!("hot:{scope}:{}:", path_key(path))) {
        let who = key.rsplit(':').next().unwrap_or("");
        if !who.is_empty() && who != identity {
            agents.insert(who.to_string());
        }
    }
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        let names = intent.get("paths").and_then(Value::as_array).map(|a| a.iter().any(|p| p.as_str() == Some(path))).unwrap_or(false);
        if !names {
            continue;
        }
        let owner = intent.get("owner").and_then(Value::as_str).unwrap_or("");
        let session = intent.get("session").and_then(Value::as_str).unwrap_or("");
        let who = if session.is_empty() { owner.to_string() } else { format!("{owner}#{session}") };
        if who != identity && who != user_id {
            agents.insert(who);
        }
    }
    agents.len()
}

/// The rationale for an edit the hook already recorded from disk: one
/// `rationale` ledger row, and the reason set on the live hot marker and
/// the last-edit record so the briefing and deltas can say why. Nothing is
/// parsed; the row is a few dozen bytes. Python's `record_rationale`.
pub fn record_rationale(
    store: &Store, scope: &str, user_id: &str, session: &str, agent: &str, path: &str, why: &str,
) -> Value {
    let why: String = why.trim().chars().take(200).collect();
    if why.is_empty() || path.is_empty() {
        return json!({"ok": false, "reason": "why is required"});
    }
    let stamp = now();
    let identity = if session.is_empty() { user_id.to_string() } else { format!("{user_id}#{session}") };
    let hot_key = format!("hot:{scope}:{}:{identity}", path_key(path));
    if let Some(mut marker) = store.eph_get(&hot_key) {
        if let Some(map) = marker.as_object_mut() {
            map.insert("why".into(), json!(why));
        }
        let started = marker.get("ts").and_then(Value::as_f64).unwrap_or(stamp);
        let _ = store.eph_set(&hot_key, &marker, Some((started + 900.0 - stamp).max(1.0)));
    }
    let last_key = format!("lastedit:{scope}:{user_id}:{}", path_key(path));
    if let Some(mut record) = store.eph_get(&last_key) {
        if let Some(map) = record.as_object_mut() {
            map.insert("why".into(), json!(why));
        }
        let _ = store.eph_set(&last_key, &record, Some(LASTEDIT_TTL_S));
    }
    let _ = store.ledger_append(
        scope,
        "rationale",
        &json!({"user": user_id, "session": session, "agent": agent, "path": path, "why": why}),
        stamp,
    );
    json!({"ok": true, "recorded": "rationale", "path": path, "why": why})
}

/// Files edited in the last minutes by one agent, and the pattern that
/// falls out of them: after path A, this agent (and others) usually touch
/// path B next. Every write appends to the agent's ring and bumps the
/// co-edit counter of each recent partner; the counters are what prefetch
/// reads. Python's `_learn_coedits`, same records.
const RECENT_EDITS: usize = 20;
const RECENT_EDITS_TTL_S: f64 = 3600.0;
const COEDIT_WINDOW_S: f64 = 600.0;
/// Partners seen together fewer times than this are not a pattern yet.
pub const PREFETCH_MIN: i64 = 3;
const PREFETCH_SYMBOLS: usize = 6;

pub fn learn_and_prefetch(
    store: &Store, scope: &str, workspace: &str, user: &str, session: &str, path: &str, stamp: f64,
) -> Option<Value> {
    let ring_key = format!("recent_edits:{scope}:{}", if session.is_empty() { user.to_string() } else { format!("{user}#{session}") });
    let mut ring: Vec<Value> = store.eph_get(&ring_key).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let partners: Vec<String> = ring
        .iter()
        .filter(|e| e.get("ts").and_then(Value::as_f64).map(|t| stamp - t <= COEDIT_WINDOW_S).unwrap_or(false))
        .filter_map(|e| e.get("path").and_then(Value::as_str).map(str::to_string))
        .filter(|p| p != path)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for partner in &partners {
        for (a, b) in [(path, partner.as_str()), (partner.as_str(), path)] {
            let key = format!("{scope}:{}", path_key(a));
            let mut record = store.kv_get("coedit", &key).unwrap_or_else(|| json!({"path": a, "with": {}}));
            let count = record.pointer(&format!("/with/{}", b.replace('/', "~1"))).and_then(Value::as_i64).unwrap_or(0);
            if let Some(with) = record.get_mut("with").and_then(Value::as_object_mut) {
                with.insert(b.to_string(), json!(count + 1));
            }
            let _ = store.kv_put("coedit", &key, &record, stamp);
        }
    }
    ring.retain(|e| e.get("path").and_then(Value::as_str) != Some(path));
    ring.push(json!({"path": path, "ts": stamp}));
    if ring.len() > RECENT_EDITS {
        ring.drain(..ring.len() - RECENT_EDITS);
    }
    let _ = store.eph_set(&ring_key, &json!(ring), Some(RECENT_EDITS_TTL_S));
    // the prediction: accumulated history makes it, so it is a Business feature
    if !crate::access::plan_for(store, workspace).flag("prefetch", false) {
        return None;
    }
    let record = store.kv_get("coedit", &format!("{scope}:{}", path_key(path)))?;
    let with = record.get("with").and_then(Value::as_object)?;
    let (next, count) = with
        .iter()
        .filter_map(|(p, c)| c.as_i64().map(|n| (p.clone(), n)))
        .filter(|(p, _)| p != path)
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))?;
    if count < PREFETCH_MIN {
        return None;
    }
    let view = crate::memory::get_symbol(store, scope, &next, "");
    if !view.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let signatures: Vec<String> = view
        .get("symbols")
        .and_then(Value::as_object)
        .map(|m| m.iter().take(PREFETCH_SYMBOLS).map(|(name, e)| e.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(name).to_string()).collect())
        .unwrap_or_default();
    let text = format!(
        "Collide prefetch: after {path}, agents usually touch {next} next ({count}x). Its facts, so you need not open it: {}",
        if signatures.is_empty() { "no symbols".to_string() } else { signatures.join("; ") }
    );
    Some(json!({"path": next, "after": path, "count": count, "text": text}))
}

pub fn report_edit(store: &Store, input: &ReportInput) -> Value {
    let mut phases = crate::routestats::Phases::new("report");
    let stamp = now();
    crate::presence::bump_tokens(
        store, input.scope, input.user_id, input.session, input.tokens, input.turn_id,
        input.input_tokens + input.cache_read_tokens + input.cache_creation_tokens, input.model,
    );
    // writing supersedes reading: clear any lingering read-presence so the
    // Now line stops saying "reading X" once the agent has moved on
    crate::presence::clear_focus(store, input.scope, input.user_id, input.session);
    // a draft is presence, not state, and the Now line says so: Python
    // records the kind as "draft" for a draft report and "edit" only for a
    // landed one
    crate::presence::note_last_action(
        store, input.scope, input.user_id, input.session, if input.draft { "draft" } else { "edit" }, input.path,
        input.agent, input.model, input.branch, "", "",
    );

    // parsed on the hook's machine: the line hashes arrive ready, and there
    // are no lines to quote — hunks carry their ranges without snippets
    let local = input.local.filter(|l| l.get("structure").is_some());
    let lines: Vec<&str> = if local.is_some() { Vec::new() } else { input.content.split('\n').collect() };
    let new_hashes = match local {
        Some(l) => strings(l.get("line_hashes")),
        None => diff::line_hashes(&lines),
    };

    // this copy of the file now: what teammates' gates compare against
    crate::freshness::saw(store, input.scope, input.user_id, input.session, input.path, &crate::freshness::fingerprint(&new_hashes), true);

    // the durable record is the system of record and outlives the ephemeral
    // cache, so a "modified" event never pairs with a phantom "0 lines"
    phases.mark("presence");
    let durable = store.get_file(input.scope, input.user_id, input.path);
    phases.mark("get_file");
    let linehash_key =
        format!("linehash:{}:{}:{}", input.scope, input.user_id, path_key(input.path));
    let base_hashes = durable
        .as_ref()
        .map(|record| strings(record.get("line_hashes")))
        .filter(|hashes| !hashes.is_empty())
        .unwrap_or_else(|| {
            store
                .eph_get(&linehash_key)
                .map(|cached| strings(cached.get("hashes")))
                .unwrap_or_default()
        });
    let hunks = diff::hunks(&base_hashes, &new_hashes, &lines);
    let (lines_added, lines_removed) = diff::hunk_totals(&hunks);

    let parsed = match local {
        Some(l) => collide_core::local::from_structure(input.path, &l["structure"]),
        None => collide_core::engine::parse(input.path, input.content),
    };
    // the semantic lint runs at the last moment source exists; what survives
    // is symbol names and line numbers, never code — and a local parse sends
    // exactly that
    let autofix = crate::lint::autofix_mode(store, input.workspace);
    let findings = if autofix == "off" {
        Vec::new()
    } else {
        let raw = match local {
            Some(l) => crate::lint::lint_names(store, input.scope, input.user_id, input.session, l.get("names").unwrap_or(&Value::Null)),
            None => crate::lint::lint_report(store, input.scope, input.user_id, input.session, input.content),
        };
        crate::lint::track_reconciliations(store, input.scope, input.user_id, input.path, raw)
    };

    let _ = store.eph_set(
        &linehash_key,
        &json!({"hashes": new_hashes, "ts": stamp}),
        Some(LINEHASH_TTL_S),
    );

    if input.draft {
        return report_draft(store, input, parsed.as_ref(), &hunks, lines_added, lines_removed, stamp);
    }

    let Some(parsed) = parsed else {
        // no symbol parser for this language, but line-level motion needs
        // none: the edit still lands in presence, the feed and the ledger
        let _ = store.eph_set(
            &format!("lastedit:{}:{}:{}", input.scope, input.user_id, path_key(input.path)),
            &json!({"path": input.path, "ts": stamp, "agent": input.agent,
                    "model": input.model, "branch": input.branch, "session": input.session}),
            Some(LASTEDIT_TTL_S),
        );
        let mut row = json!({
            "user": input.user_id, "session": input.session, "agent": input.agent,
            "path": input.path, "parse": "unsupported", "auto": input.auto,
            "lines_added": lines_added, "lines_removed": lines_removed,
            "hunks": ledger_hunks(&hunks), "events": [],
        });
        if let Some(row) = row.as_object_mut() {
            attach_cost(row, input);
        }
        let _ = store.ledger_append(input.scope, "edit_reported", &row, stamp);
        crate::events::publish(
            store,
            input.scope,
            json!({"kind": "edit", "user": input.user_id, "agent": input.agent,
                   "path": input.path, "parse": "unsupported", "auto": input.auto,
                   "hunks": hunks, "lines_added": lines_added,
                   "lines_removed": lines_removed}),
            input.via,
        );
        let mut unsupported = json!({
            "ok": true, "parse": "unsupported", "path": input.path,
            "lines_added": lines_added, "lines_removed": lines_removed, "hunks": hunks,
        });
        // a migration is usually .sql: no parser, but it fills or clashes
        // with a claim all the same
        if let Some(claim) = crate::claims::on_write(store, input.scope, input.user_id, input.session, input.path, input.via) {
            unsupported["claim"] = claim;
        }
        return crate::events::with_deltas(store, input.scope, input.user_id, unsupported);
    };

    let status = parsed.status;
    let new_symbols = symbol_map(&parsed);
    // a first write after the tree was indexed diffs against the observed
    // record: a signature change is then an event the briefing can show,
    // not a first sight that says nothing
    let seed: Option<Value> = durable.clone().or_else(|| seed_from_observed(store, input.scope, input.path));
    let (mut record, mut events) =
        semantics::apply_parse(seed.as_ref(), status, new_symbols.clone(), stamp);
    // one person's agents share the record but not always a checkout: against
    // another session's baseline, a name this agent's own copy never had is
    // not one it renamed or removed (Study 5: a false rename blocked an agent
    // three times; a false removal linted another's new test)
    let baseline_session = durable
        .as_ref()
        .and_then(|d| d.get("provenance"))
        .and_then(|p| p.get("session"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // a first write is diffed against the map's copy, which is whoever wrote
    // or indexed it last: another copy too (Study 5 medium: two teammates'
    // new tests read as a rename, and the gate held a third agent five times)
    let other_copy = (!baseline_session.is_empty() && baseline_session != input.session)
        || (durable.is_none() && seed.is_some());
    if !input.session.is_empty() && other_copy {
        // no copy remembered (it read the file with `cat`, say): nothing it
        // lacks can be told apart from what it never pulled, so no absence
        // is certain; what it changed or added still is
        let own = own_copy(store, input.scope, input.user_id, input.session, input.path).unwrap_or_default();
        events = semantics::across_copies(events, &own);
        if let Some(pending) = record.get_mut("pending_removed").and_then(Value::as_object_mut) {
            pending.retain(|name, _| own.contains_key(name));
        }
    }
    if status == semantics::CLEAN && !input.session.is_empty() {
        let signatures: BTreeMap<String, String> = new_symbols
            .iter()
            .map(|(name, symbol)| (name.clone(), symbol.get("signature").and_then(Value::as_str).unwrap_or("").to_string()))
            .collect();
        remember_copy(store, input.scope, input.user_id, input.session, input.path, &signatures);
    }
    if let Some(map) = record.as_object_mut() {
        // source monitoring: disk-confirmed (the watcher) vs agent-claimed
        map.insert(
            "provenance".into(),
            {
                let mut prov = json!({
                    "user": input.user_id, "agent": input.agent, "session": input.session,
                    "source": if input.agent.starts_with("collide-watch") { "disk" } else { "claim" },
                    "ts": stamp,
                });
                if let Some(v) = &input.verified {
                    prov["verified"] = v.clone();
                }
                prov
            },
        );
        // the line-motion baseline for the NEXT report lives with the record,
        // so it lasts exactly as long as the symbols it accompanies
        map.insert("line_hashes".into(), json!(new_hashes));
    }
    phases.mark("diff_lint");
    let _ = store.put_file(input.scope, input.user_id, input.path, &record, stamp);
    phases.mark("put_file");

    let before_symbols = durable
        .as_ref()
        .and_then(|d| d.get("last_clean"))
        .and_then(|clean| clean.get("symbols"))
        .and_then(Value::as_object)
        .cloned();
    let after_symbols = record
        .get("last_clean")
        .and_then(|clean| clean.get("symbols"))
        .and_then(Value::as_object)
        .cloned();
    crate::supervise::bump_anchor_revs(
        store, input.scope, input.path,
        before_symbols.as_ref(), after_symbols.as_ref(), stamp, input.user_id,
    );
    let parsed_names: Vec<String> = if status == semantics::CLEAN {
        new_symbols.keys().cloned().collect()
    } else {
        Vec::new()
    };
    crate::supervise::fire_tripwires(
        store, input.scope, input.path, &parsed_names, input.user_id, input.agent, input.via);
    // a numbered file under a claimed directory fills or clashes with a claim
    let claimed = crate::claims::on_write(store, input.scope, input.user_id, input.session, input.path, input.via);

    // the tree only ever advances on clean parses: a broken file leaves the
    // workspace root, and everything derived from it, unchanged
    let mut tree = store
        .get_tree(input.scope, input.user_id)
        .map(|value| Tree::from_json(&value))
        .unwrap_or_else(Tree::empty);
    let old_root = tree.root.clone();
    if status == semantics::CLEAN {
        let mut files = tree.files.clone();
        let hashes: BTreeMap<String, String> = new_symbols
            .iter()
            .filter_map(|(name, symbol)| {
                Some((name.clone(), symbol.get("hash")?.as_str()?.to_string()))
            })
            .collect();
        files.insert(input.path.to_string(), file_hash(&hashes));
        tree = Tree::build(files);
        phases.mark("tree_build");
        let _ = store.put_tree(input.scope, input.user_id, &tree.to_json(), stamp);
        phases.mark("put_tree");

        // the live code graph advances on the WRITE, not on a commit
        let imports = import_specs(&parsed);
        let graph_symbols: BTreeMap<String, Value> = new_symbols
            .iter()
            .map(|(name, symbol)| (name.clone(), symbol.clone()))
            .collect();
        let graph_record = codegraph::file_record(
            input.path, parsed.language, &graph_symbols, &imports, stamp, &parsed.doc);
        let was_known = codegraph::has_file(store, input.scope, input.path);
        codegraph::update_file(store, input.scope, input.path, graph_record, stamp);
        phases.mark("graph_update");
        // a new module is in flight; an import of one in flight (or not yet
        // written) is told its interface on the importer's next step
        if !input.draft {
            // what this write added, or renamed something to: in flight for
            // whoever imports it
            let added: Vec<String> = events
                .iter()
                .filter_map(|e| match e.get("kind").and_then(Value::as_str) {
                    Some("added") => e.get("symbol").and_then(Value::as_str).map(str::to_string),
                    Some("renamed") => e.get("new_name").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                })
                .collect();
            crate::inflight::on_write(
                store, input.scope, input.user_id, input.session, input.path, parsed.language, was_known,
                &imports, &new_symbols, &added, stamp,
            );
        }
        // after the in-flight entry: embedding it checks it against teammates'
        // tasks, and that needs to know whose new work it is
        crate::embed::enqueue(input.scope, input.path, &parsed.doc, &new_symbols, true);
        phases.mark("inflight_embed");
    }

    // `session` names the agent that wrote: the key is per person, and the
    // roster hands each agent row its own edits by this field
    let _ = store.eph_set(
        &format!("lastedit:{}:{}:{}", input.scope, input.user_id, path_key(input.path)),
        &json!({"path": input.path, "ts": stamp, "agent": input.agent,
                "model": input.model, "branch": input.branch, "session": input.session}),
        Some(LASTEDIT_TTL_S),
    );
    // what this agent touched just before: the co-edit counters the prefetch
    // reads are learned here, one small record per write
    let prefetch = learn_and_prefetch(store, input.scope, input.workspace, input.user_id, input.session, input.path, stamp);
    phases.mark("prefetch");

    // the ledger row is a structured fact: per-symbol hashes before and after,
    // so scars, rationale, drift and confidence can all read from it later
    let before = durable
        .as_ref()
        .and_then(|d| d.get("last_clean"))
        .and_then(|c| c.get("symbols"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let after = record
        .get("last_clean")
        .and_then(|c| c.get("symbols"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let hash_of = |map: &Map<String, Value>, name: &str| -> Value {
        map.get(name)
            .and_then(|symbol| symbol.get("hash"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut deltas = Map::new();
    for event in &events {
        let Some(symbol) = event.get("symbol").and_then(Value::as_str) else { continue };
        deltas.insert(
            symbol.to_string(),
            json!({"before": hash_of(&before, symbol), "after": hash_of(&after, symbol)}),
        );
    }
    if deltas.is_empty() && durable.is_none() {
        // a genuine first sight emits no events, but the row should still say
        // what appeared, capped so rows stay bounded
        for (name, symbol) in after.iter().take(MAX_FIRST_SIGHT_SYMBOLS) {
            deltas.insert(
                name.clone(),
                json!({"before": Value::Null,
                       "after": symbol.get("hash").cloned().unwrap_or(Value::Null)}),
            );
        }
    }

    let touched_for_row: std::collections::BTreeSet<String> = deltas.keys().cloned().collect();
    let (_live, covering_for_row) = crate::supervise::covering_intent(
        store, input.scope, input.user_id, input.path, &touched_for_row);

    let mut payload = Map::new();
    payload.insert("user".into(), json!(input.user_id));
    payload.insert("session".into(), json!(input.session));
    payload.insert("agent".into(), json!(input.agent));
    payload.insert("path".into(), json!(input.path));
    payload.insert("parse".into(), json!(status));
    payload.insert("auto".into(), json!(input.auto));
    attach_cost(&mut payload, input);
    payload.insert("lines_added".into(), json!(lines_added));
    payload.insert("lines_removed".into(), json!(lines_removed));
    let why: String = input.why.trim().chars().take(200).collect();
    if !why.is_empty() {
        payload.insert("why".into(), json!(why));
    }
    if !deltas.is_empty() {
        payload.insert("symbols_changed".into(), Value::Object(deltas));
    }
    if !covering_for_row.is_empty() {
        payload.insert("intent_id".into(), json!(covering_for_row));
    }
    payload.insert("hunks".into(), json!(ledger_hunks(&hunks)));
    payload.insert("events".into(), json!(ledger_events(&events)));
    let _ = store.ledger_append(input.scope, "edit_reported", &Value::Object(payload), stamp);

    crate::supervise::watchdog(store, &crate::supervise::WatchdogInput {
        scope: input.scope, user_id: input.user_id, session: input.session,
        agent: input.agent, path: input.path, parse_status: status,
        lines_added, lines_removed, covering_intent: &covering_for_row,
        live_intents: _live, via: input.via,
    });

    // hot markers feed the gate's fast path: only certain, blocking-grade
    // changes (rename, removal, signature change)
    let hot: Vec<Value> = events
        .iter()
        .filter(|event| {
            let certain =
                event.get("confidence").and_then(Value::as_str) == Some(semantics::CERTAIN);
            let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
            let signature_change =
                event.get("signature_change").and_then(Value::as_bool).unwrap_or(false);
            certain && (BLOCKING_KINDS.contains(&kind) || signature_change)
        })
        .map(|event| {
            let mut copy = event.clone();
            if event.get("signature_change").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(map) = copy.as_object_mut() {
                    map.insert("kind".into(), json!("signature"));
                }
            }
            copy
        })
        .collect();
    let mut verify_request: Option<Value> = None;
    let mut ask_why: Option<Value> = None;
    if !hot.is_empty() {
        // one marker per SESSION: four agents on one credential are four
        // agents, and each one's certain changes must be visible to the others
        let identity = if input.session.is_empty() {
            input.user_id.to_string()
        } else {
            format!("{}#{}", input.user_id, input.session)
        };
        // what would prove the change safe: its dependents and their tests,
        // from the graph. The marker carries the grade (clear / partial /
        // pending) and the response carries the plan the hook runs.
        phases.mark("ledger_watchdog");
        let graph = crate::graphview::snapshot(store, input.scope);
        phases.mark("snapshot");
        let plan = crate::graphview::verify_plan(&graph, input.path, &hot);
        let verified = crate::graphview::verification_at_report(&plan, stamp);
        if verified.get("status").and_then(Value::as_str) == Some("pending") {
            verify_request = Some(plan);
        }
        let _ = store.eph_set(
            &format!("hot:{}:{}:{}", input.scope, path_key(input.path), identity),
            &json!({"path": input.path, "user": input.user_id, "session": input.session, "events": hot.clone(), "ts": stamp,
                    "verified": verified, "why": why}),
            Some(input.hot_ttl_s),
        );
        // a hot symbol with other agents on the file and no reason given:
        // ask for one line, once, with the write. Rationale is captured
        // where it is cheapest, at the decision the agent already made.
        if why.is_empty() {
            let others = others_on_file(store, input.scope, input.path, &identity, input.user_id);
            if others > 0 {
                let symbol = hot.first().and_then(|e| e.get("symbol")).and_then(Value::as_str).unwrap_or("").to_string();
                ask_why = Some(json!({"path": input.path, "symbol": symbol, "others": others}));
            }
        }
    }
    // those same certain changes become lint rules for everyone ELSE's next
    // report — the linter whose rules are your teammates' changes
    crate::lint::update_registry(store, input.scope, input.user_id, input.session, &hot, stamp);

    // an edit landing after a gate block on this path is an override: the
    // agent saw the collision and wrote anyway. Recorded, never prevented.
    crate::lint::note_override(
        store, input.scope, input.user_id, input.session, input.path, &path_key(input.path));

    crate::events::publish(
        store,
        input.scope,
        json!({"kind": "edit", "user": input.user_id, "agent": input.agent,
               "path": input.path, "parse": status, "root": tree.root, "auto": input.auto,
               "hunks": hunks, "lines_added": lines_added, "lines_removed": lines_removed}),
        input.via,
    );
    for event in &events {
        crate::events::publish(
            store,
            input.scope,
            json!({"kind": "symbol_event", "user": input.user_id,
                   "path": input.path, "event": event}),
            input.via,
        );
    }
    if tree.root != old_root {
        crate::events::publish(
            store,
            input.scope,
            json!({"kind": "root_changed", "user": input.user_id, "root": tree.root}),
            input.via,
        );
    }

    // exactly the Python response's fields, in the same shape: the hook and
    // the MCP tool both read this, and an extra key is as much of a
    // divergence as a missing one
    let served = semantics::serve(Some(&record), stamp);
    let (freshness, age_s) = served
        .map(|s| (s.freshness, s.age_s))
        .unwrap_or((semantics::LIVE, 0.0));
    let mut response = json!({
        "ok": true,
        "parse": status,
        "path": input.path,
        "root": tree.root,
        "freshness": freshness,
        "age_s": age_s,
        "symbol_count": if status == semantics::CLEAN {
            json!(new_symbols.len())
        } else {
            Value::Null
        },
        "lines_added": lines_added,
        "lines_removed": lines_removed,
        "hunks": hunks,
    });
    if !findings.is_empty() {
        if let Some(map) = response.as_object_mut() {
            map.insert("findings".into(), json!(findings));
            map.insert("autofix_mode".into(), json!(autofix));
        }
    }
    if let Some(plan) = verify_request {
        // the hook runs these tests detached and posts /verify; the agent
        // never sees this key and spends nothing on it
        if let Some(map) = response.as_object_mut() {
            map.insert("verify".into(), plan);
        }
    }
    if let Some(ask) = ask_why {
        if let Some(map) = response.as_object_mut() {
            map.insert("ask_why".into(), ask);
        }
    }
    if let Some(next) = prefetch {
        if let Some(map) = response.as_object_mut() {
            map.insert("prefetch".into(), next);
        }
    }
    if let Some(claim) = claimed {
        if let Some(map) = response.as_object_mut() {
            map.insert("claim".into(), claim);
        }
    }
    // active collisions between agents' work, redelivered until reconciled
    let mine: Vec<Value> = crate::lint::open_reconciliations(store, input.scope)
        .into_iter()
        .filter(|record| {
            let party = |key: &str| {
                record.get(key).and_then(Value::as_str).unwrap_or("") == input.user_id
            };
            party("a_user") || party("b_user")
        })
        .collect();
    if !mine.is_empty() {
        if let Some(map) = response.as_object_mut() {
            map.insert("reconciliations_pending".into(), json!({
                "count": mine.len(),
                "how": "active collision(s) between agents' work — call pending_reconciliations(repo_id) to claim and synthesize the union; never resolve by deleting either side's work",
            }));
        }
    }
    phases.mark("respond");
    let answer = crate::events::with_deltas(store, input.scope, input.user_id, response);
    phases.mark("deltas");
    answer
}

/// Line RANGES only — never source. Snippets live solely in the TTL'd event
/// ring, which is what keeps the "code is discarded" promise honest.
/// The observed graph record as the prior for this user's first write: its
/// last clean parse is the map's, so the diff has something to say.
fn seed_from_observed(store: &Store, scope: &str, path: &str) -> Option<Value> {
    let graph = store.kv_get(codegraph::GRAPH_BUCKET, &format!("{scope}:{}", codegraph::path_key(path)))?;
    let symbols = graph.get("symbols").filter(|s| s.is_object())?.clone();
    let ts = graph.get("ts").cloned().unwrap_or(json!(0.0));
    Some(json!({"last_clean": {"symbols": symbols, "ts": ts}, "seeded_from": "observed"}))
}

fn ledger_hunks(hunks: &[Value]) -> Vec<Value> {
    hunks
        .iter()
        .filter(|hunk| hunk.get("op").and_then(Value::as_str) != Some("truncated"))
        .take(MAX_LEDGER_HUNKS)
        .map(|hunk| {
            let mut row = Map::new();
            for key in ["op", "old_start", "old_end", "new_start", "new_end"] {
                row.insert(key.into(), hunk.get(key).cloned().unwrap_or(Value::Null));
            }
            Value::Object(row)
        })
        .collect()
}

fn ledger_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .take(MAX_LEDGER_EVENTS)
        .map(|event| {
            let mut row = Map::new();
            for key in ["symbol", "kind", "confidence"] {
                if let Some(value) = event.get(key) {
                    row.insert(key.into(), value.clone());
                }
            }
            for key in ["signature_before", "signature_after", "new_name"] {
                if let Some(Value::String(text)) = event.get(key) {
                    if !text.is_empty() {
                        row.insert(key.into(), json!(text.chars().take(240).collect::<String>()));
                    }
                }
            }
            Value::Object(row)
        })
        .collect()
}
