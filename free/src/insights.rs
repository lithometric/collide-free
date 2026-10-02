//! Read-side answers over the ledger: `recap`, the "what happened here"
//! question.
//!
//! `journal` folds ledger rows into agent SESSIONS — one entry per
//! (user, session-or-agent) run, split on a 30-minute silence — and `costs`
//! folds the same window into token/dollar accounting. `recap` is the two
//! together, which is what an agent asking "what did my agents do / what
//! changed today" actually wants: git records committed OUTCOMES, the
//! ledger records the PROCESS — uncommitted edits, which model spent which
//! tokens, sessions that never got committed at all.
//!
//! Everything here is pure read; nothing mutates state.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::compat::python_round;
use crate::store::{now, LedgerRow, Store};

/// Hard bound on ledger rows walked per question, matching the Python
/// driver's tail-paged walk: on a very busy scope this reports on the most
/// RECENT rows in the window rather than growing without bound.
const MAX_ROWS: usize = 4000;
const SESSION_GAP_S: f64 = 30.0 * 60.0;

// $/Mtok by model-id prefix as (INPUT, OUTPUT) — never one blended number.
// The four things an agent turn bills are priced off different rates, and a
// blend cannot stand in for them: a cache READ costs a tenth of the INPUT
// rate, so charging it a blended input+output rate overstates it 5x on Opus.
// Claude rows are Anthropic's first-party list prices (Opus 5 $5/$25,
// Sonnet 5 $2/$10, Haiku 4.5 $1/$5, Fable 5 $10/$50); the non-Claude rows
// are public list prices and, like everything here, an estimate of scale
// rather than an invoice. Order matters only in that it is the order
// Python's dict iterates in; no prefix here is a prefix of another.
const MODEL_USD_PER_MTOK: &[(&str, (f64, f64))] = &[
    ("claude-fable", (10.0, 50.0)),
    ("claude-opus", (5.0, 25.0)),
    ("claude-sonnet", (2.0, 10.0)),
    ("claude-haiku", (1.0, 5.0)),
    ("gpt-5", (1.25, 10.0)),
    ("gpt-4", (2.5, 10.0)),
    ("gemini", (1.25, 10.0)),
];
/// An unknown model string: a mid-tier frontier model's shape ($3 in / $15
/// out, i.e. the same 1:5 input:output ratio every row above has).
/// Deliberate and documented rather than silently free.
const DEFAULT_USD_PER_MTOK: (f64, f64) = (3.0, 15.0);
/// A cache read costs this share of an input token, a cache write this much
/// more than one — the standard 5-minute-TTL multipliers.
pub(crate) const CACHE_READ_SHARE: f64 = 0.1;
pub(crate) const CACHE_WRITE_SHARE: f64 = 1.25;

/// The four usage counters a hook-reported row can carry, plus `tokens`:
/// the pre-v20 single field, which was uncached input + output only.
const USAGE_FIELDS: [&str; 4] =
    ["input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"];

/// (input, output) $/Mtok for a model id.
pub(crate) fn usd_per_mtok(model: &str) -> (f64, f64) {
    let lowered = model.to_lowercase();
    for (prefix, rates) in MODEL_USD_PER_MTOK {
        if lowered.starts_with(prefix) {
            return *rates;
        }
    }
    DEFAULT_USD_PER_MTOK
}

/// What a cache-read token costs: a tenth of the model's INPUT rate.
pub(crate) fn cache_read_usd_per_mtok(model: &str) -> f64 {
    usd_per_mtok(model).0 * CACHE_READ_SHARE
}

/// (input, output, cache_read, cache_creation, legacy) for one ledger row.
///
/// A row written by a v20+ hook carries the four real counters; an older row
/// carries only `tokens`, which was input+output with the cache fields
/// deliberately dropped. The two are never mixed: when the four are present
/// `tokens` is the same turn counted worse, so it is ignored.
pub(crate) fn row_usage(payload: &Value) -> [i64; 5] {
    let mut counts = [0i64; 5];
    for (slot, field) in USAGE_FIELDS.iter().enumerate() {
        counts[slot] = int(payload, field);
    }
    if counts.iter().any(|c| *c != 0) {
        return counts;
    }
    [0, 0, 0, 0, int(payload, "tokens")]
}

/// Every token the turn was billed for, cached ones included.
pub(crate) fn usage_tokens(usage: &[i64; 5]) -> i64 {
    usage[0] + usage[1] + usage[2] + usage[3] + usage[4]
}

/// What that usage cost, each counter at its own rate.
///
/// The legacy `tokens` field is priced at the OUTPUT rate: it is uncached
/// input + output, and in a cached agent loop uncached input is a rounding
/// error next to output (~100 vs ~1500 tokens a turn), so output is the rate
/// that fits what the field actually held.
pub(crate) fn usage_usd(model: &str, usage: &[i64; 5]) -> f64 {
    let (inp, out) = usd_per_mtok(model);
    (usage[0] as f64 * inp
        + usage[1] as f64 * out
        + usage[2] as f64 * inp * CACHE_READ_SHARE
        + usage[3] as f64 * inp * CACHE_WRITE_SHARE
        + usage[4] as f64 * out)
        / 1_000_000.0
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn int(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `YYYY-MM-DD` in UTC for a ledger row's timestamp, matching Python's
/// `time.strftime("%Y-%m-%d", time.gmtime(ts))`.
pub(crate) fn day_key(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()).max(0.0) * 1e9).round() as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos).unwrap_or_default().format("%Y-%m-%d").to_string()
}

/// A counter that remembers what it saw first — see `briefing::Tally`, which
/// this mirrors. Python dicts preserve insertion order and Python's `sorted`
/// is stable, so `most_common` / a `sorted(..., key=lambda kv: -kv[1])` both
/// break ties by "first seen", not alphabetically.
#[derive(Default)]
struct Tally {
    counts: BTreeMap<String, i64>,
    order: Vec<String>,
}

impl Tally {
    fn add(&mut self, key: &str, n: i64) {
        match self.counts.get_mut(key) {
            Some(count) => *count += n,
            None => {
                self.counts.insert(key.to_string(), n);
                self.order.push(key.to_string());
            }
        }
    }

    fn get(&self, key: &str) -> i64 {
        self.counts.get(key).copied().unwrap_or(0)
    }

    /// Key/count pairs, most first, ties in first-seen order; capped at
    /// `limit` when given.
    fn entries_desc(&self, limit: Option<usize>) -> Vec<(String, i64)> {
        let mut ranked: Vec<(usize, &String)> = self.order.iter().enumerate().collect();
        ranked.sort_by_key(|(position, key)| (-self.counts[*key], *position));
        let entries = ranked.into_iter().map(|(_, key)| (key.clone(), self.get(key)));
        match limit {
            Some(n) => entries.take(n).collect(),
            None => entries.collect(),
        }
    }

    fn most_common(&self, limit: usize) -> Vec<String> {
        self.entries_desc(Some(limit)).into_iter().map(|(key, _)| key).collect()
    }
}

fn tally_object(tally: &Tally, limit: Option<usize>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, count) in tally.entries_desc(limit) {
        map.insert(key, json!(count));
    }
    Value::Object(map)
}

/// Ledger rows for this scope at or after `since_ts`, oldest first, capped
/// to the most recent `MAX_ROWS`. The Rust store answers the scope+ts range
/// in one query — already what the Python driver's tail-paged walk works to
/// produce — so the only thing left to replicate is the bound: on a scope
/// with more than `MAX_ROWS` rows inside the window, keep the newest ones
/// (the tail of this ascending vec), same as walking backward from "now"
/// and stopping at `MAX_ROWS`.
fn walk_ledger(store: &Store, scope: &str, since_ts: f64) -> Vec<LedgerRow> {
    let mut rows = store.ledger_since(scope, since_ts);
    if rows.len() > MAX_ROWS {
        rows = rows.split_off(rows.len() - MAX_ROWS);
    }
    rows
}

/// `user` or `owner`, whichever the row carries.
fn session_user(payload: &Value) -> String {
    let user = text(payload, "user");
    if !user.is_empty() {
        return user;
    }
    text(payload, "owner")
}

/// The key a row's session groups under: an explicit session id, else the
/// agent name, else a catch-all — matching Python's
/// `payload.get('session') or payload.get('agent') or 'work'`.
fn session_subkey(payload: &Value) -> String {
    let session = text(payload, "session");
    if !session.is_empty() {
        return session;
    }
    let agent = text(payload, "agent");
    if !agent.is_empty() {
        return agent;
    }
    "work".to_string()
}

/// First 240 Unicode characters — matching Python's `s[:240]`, which slices
/// by code point, not byte.
fn truncate_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

struct Session {
    user: String,
    agent: String,
    model: String,
    start: f64,
    end: f64,
    edits: i64,
    tokens: i64,
    lines_added: i64,
    lines_removed: i64,
    parse_failures: i64,
    paths: Tally,
    rationales: Vec<String>,
    intents_completed: i64,
    watchdog: i64,
}

/// Ledger rows folded into sessions: one entry per (user, session-id or
/// agent) run, split on a 30-minute silence.
pub fn journal(store: &Store, scope: &str, days: f64) -> Value {
    let since = now() - days.max(0.02) * 86_400.0;
    let rows = walk_ledger(store, scope, since); // oldest first already

    let mut sessions: Vec<Session> = Vec::new();
    let mut open_by_key: HashMap<String, usize> = HashMap::new();
    // one turn's tokens, however many files it touched
    let mut seen_turns: BTreeSet<String> = BTreeSet::new();

    for row in &rows {
        let payload = &row.payload;
        let user = session_user(payload);
        if user.is_empty() {
            continue;
        }
        let agent = text(payload, "agent");
        let key = format!("{user}:{}", session_subkey(payload));
        let ts = row.ts;

        let needs_new = match open_by_key.get(&key) {
            Some(&idx) => ts - sessions[idx].end > SESSION_GAP_S,
            None => true,
        };
        if needs_new {
            sessions.push(Session {
                user: user.clone(),
                agent: agent.clone(),
                model: text(payload, "model"),
                start: ts,
                end: ts,
                edits: 0,
                tokens: 0,
                lines_added: 0,
                lines_removed: 0,
                parse_failures: 0,
                paths: Tally::default(),
                rationales: Vec::new(),
                intents_completed: 0,
                watchdog: 0,
            });
            open_by_key.insert(key.clone(), sessions.len() - 1);
        }
        let idx = open_by_key[&key];
        let current = &mut sessions[idx];
        current.end = current.end.max(ts);

        match row.kind.as_str() {
            "edit_reported" => {
                current.edits += 1;
                let turn = text(payload, "turn_id");
                if !seen_turns.contains(&turn) {
                    if !turn.is_empty() {
                        seen_turns.insert(turn);
                    }
                    current.tokens += usage_tokens(&row_usage(payload));
                }
                current.lines_added += int(payload, "lines_added");
                current.lines_removed += int(payload, "lines_removed");
                if crate::compat::truthy(payload.get("model")) {
                    current.model = text(payload, "model");
                }
                // "partial" = source that failed to parse cleanly (a real
                // smell); "unsupported" is just a non-code file and must
                // NOT count
                if text(payload, "parse") == "partial" {
                    current.parse_failures += 1;
                }
                if crate::compat::truthy(payload.get("path")) {
                    current.paths.add(&text(payload, "path"), 1);
                }
            }
            "intent_completed" => {
                current.intents_completed += 1;
                if crate::compat::truthy(payload.get("rationale")) {
                    current.rationales.push(truncate_chars(&text(payload, "rationale"), 240));
                }
            }
            "watchdog" => current.watchdog += 1,
            _ => {}
        }
    }

    // most recently started first, ties in the order sessions were opened
    sessions.sort_by(|a, b| b.start.partial_cmp(&a.start).unwrap_or(std::cmp::Ordering::Equal));

    let out: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "user": s.user, "agent": s.agent, "model": s.model,
                "start": s.start, "end": s.end,
                "edits": s.edits, "tokens": s.tokens,
                "lines_added": s.lines_added, "lines_removed": s.lines_removed,
                "parse_failures": s.parse_failures,
                "rationales": s.rationales,
                "intents_completed": s.intents_completed,
                "watchdog": s.watchdog,
                // the raw per-session Counter never reaches the response,
                // only its top 4 — matching Python's
                // `{**{k: v for k, v in entry.items() if k != "paths"}, "top_paths": top}`
                "top_paths": s.paths.most_common(4),
            })
        })
        .collect();

    json!({"sessions": out, "window_days": days})
}

/// Token accounting for the window, counting each TURN once.
///
/// One assistant turn writes one ledger row per file it touched, every row
/// carrying that whole turn's usage — so summing the rows multiplied the
/// bill by files-per-turn. `turn_id` rides the row precisely so this can fold
/// them back together; rows from before it existed carry none and are counted
/// individually, as they always were.
pub fn costs(store: &Store, scope: &str, days: f64) -> Value {
    costs_across(store, &[scope.to_string()], days)
}

/// [`costs`] over several repos at once (a workspace's, as the dashboard
/// shows them), with `by_repo` and `repos` when there is more than one. One
/// scope reads exactly as `costs` always did.
pub fn costs_across(store: &Store, scopes: &[String], days: f64) -> Value {
    let since = now() - days.max(0.02) * 86_400.0;
    // Python's costs() walks the un-reversed _walk_ledger result, which is
    // newest-first; that order decides which row of a turn is the one kept,
    // which entry wins a tie in the sorted-by-count output below, and the
    // summation order for est_usd (float addition is not associative), so it
    // has to match here too.
    let mut rows: Vec<(String, LedgerRow)> = Vec::new();
    for scope in scopes {
        let repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(scope).to_string();
        let mut mine = walk_ledger(store, scope, since);
        mine.reverse();
        rows.extend(mine.into_iter().map(|row| (repo.clone(), row)));
    }
    if scopes.len() > 1 {
        rows.sort_by(|(_, a), (_, b)| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal).then(b.seq.cmp(&a.seq)));
    }
    let mut by_repo = Tally::default();

    let mut by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_model = Tally::default();
    let mut by_user = Tally::default();
    let mut by_path_root = Tally::default();
    let mut spend_by_model: HashMap<String, [i64; 5]> = HashMap::new();
    let mut by_kind = [0i64; 5];
    let mut seen_turns: BTreeSet<String> = BTreeSet::new();
    let mut total: i64 = 0;

    for (repo, row) in &rows {
        let payload = &row.payload;
        let usage = row_usage(payload);
        let tokens = usage_tokens(&usage);
        if tokens == 0 {
            continue;
        }
        let turn = text(payload, "turn_id");
        if !turn.is_empty() {
            if seen_turns.contains(&turn) {
                continue; // the same turn, reported again for another file
            }
            seen_turns.insert(turn);
        }
        total += tokens;
        by_repo.add(repo, tokens);
        for slot in 0..5 {
            by_kind[slot] += usage[slot];
        }

        *by_day.entry(day_key(row.ts)).or_insert(0) += tokens;

        let model = text(payload, "model");
        let model = if model.is_empty() { "unknown".to_string() } else { model };
        by_model.add(&model, tokens);
        let spend = spend_by_model.entry(model).or_insert([0i64; 5]);
        for slot in 0..5 {
            spend[slot] += usage[slot];
        }

        let user = text(payload, "user");
        if !user.is_empty() {
            by_user.add(&user, tokens);
        }

        let path = text(payload, "path");
        let root = path.split('/').next().unwrap_or("");
        by_path_root.add(if root.is_empty() { "(root)" } else { root }, tokens);
    }

    // same order Python's `spend_by_model` dict iterates: first-seen in the
    // newest-first walk above, which `by_model.order` records
    let est: f64 = by_model
        .order
        .iter()
        .map(|model| usage_usd(model, spend_by_model.get(model).unwrap_or(&[0i64; 5])))
        .sum();

    let mut out = json!({
        "window_days": days,
        "total_tokens": total,
        "by_day": by_day,
        "by_model": tally_object(&by_model, None),
        "by_user": tally_object(&by_user, None),
        "by_path_root": tally_object(&by_path_root, Some(8)),
        "by_kind": {"input": by_kind[0], "output": by_kind[1], "cache_read": by_kind[2],
                    "cache_creation": by_kind[3], "legacy": by_kind[4]},
        "turns": seen_turns.len(),
        "est_usd": python_round(est, 2),
        "estimate_note": "rough per-model input/output list rates, cache reads at a tenth of \
                          input and cache writes at 1.25x — scale, not an invoice",
    });
    if scopes.len() > 1 {
        out["by_repo"] = tally_object(&by_repo, None);
        out["repos"] = json!(scopes.len());
    }
    out
}

/// What happened in this repo, from the ledger: agent sessions plus the
/// window's token/cost accounting. PREFER THIS OVER `git log` for "what
/// changed / what did my agents do / summarize today" — git records
/// committed OUTCOMES; the ledger records the PROCESS: uncommitted edits,
/// which model spent which tokens, decisions with their rationales, and
/// attempts that were reverted.
pub fn recap(store: &Store, scope: &str, days: f64) -> Value {
    let window = days.max(0.02).min(90.0);
    let mut out = journal(store, scope, window);
    let costs_value = costs(store, scope, window);
    if let Some(map) = out.as_object_mut() {
        map.insert("costs".to_string(), costs_value);
    }
    out
}

// ------------------------------------------------------------------ search

/// Notes store anchors as `{"kind","path","symbol"}` objects, or (older
/// notes) as a bare `"path::symbol"` string — normalize both to (path,
/// symbol). Duplicated from `agenttools::note_anchor` (private there, and
/// small enough that sharing it was not worth a new `pub` seam) — see
/// `Tally` above for the same call on `briefing::Tally`.
fn note_anchor(note: &Value) -> (String, String) {
    match note.get("anchor") {
        Some(Value::Object(map)) => (
            map.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
            map.get("symbol").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        Some(Value::String(raw)) => match raw.split_once("::") {
            Some((path, symbol)) => (path.to_string(), symbol.to_string()),
            None => (raw.clone(), String::new()),
        },
        _ => (String::new(), String::new()),
    }
}

fn note_text(note: &Value) -> String {
    let t = text(note, "text");
    if !t.is_empty() { t } else { text(note, "fact") }
}

/// Substring search across ledger facts and memory notes, case-insensitive.
/// Mirrors `insights.search`.
///
/// Matches are collected newest-first from each source (ledger, then
/// memory) and capped at `limit` DURING the scan — so on a scope with more
/// hits than `limit`, which rows make it in depends on scan order, even
/// though the printed order (sorted by `ts` descending, below) does not.
/// `walk_ledger` returns oldest-first capped to the most recent `MAX_ROWS`;
/// reversing it here reproduces Python's `_walk_ledger`, which is
/// newest-first by construction (its tail-paged walk stitches pages that
/// way).
pub fn search(store: &Store, scope: &str, q: &str, limit: usize) -> Value {
    let needle = q.trim().to_lowercase();
    if needle.is_empty() {
        return json!({"q": q, "results": []});
    }
    // (ts, entry); a final stable sort by -ts reproduces Python's
    // `results.sort(key=lambda r: -(r.get("ts") or 0))` over the same set.
    let mut results: Vec<(f64, Value)> = Vec::new();

    let rows = walk_ledger(store, scope, 0.0);
    for row in rows.iter().rev() {
        if results.len() >= limit {
            break;
        }
        let payload = &row.payload;
        let symbols: Vec<String> = payload
            .get("symbols_changed")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let hay = format!(
            "{} {} {} {} {} {} {}",
            text(payload, "path"), symbols.join(" "), text(payload, "rationale"),
            text(payload, "intent_id"), text(payload, "user"), text(payload, "owner"),
            row.kind,
        )
        .to_lowercase();
        if !hay.contains(&needle) {
            continue;
        }
        let by = {
            let user = text(payload, "user");
            if !user.is_empty() { user } else { text(payload, "owner") }
        };
        let mut symbols_sorted = symbols;
        symbols_sorted.sort();
        symbols_sorted.truncate(6);
        let rationale = text(payload, "rationale");
        let mut entry = json!({
            "type": if row.kind == "intent_completed" && crate::compat::truthy(payload.get("rationale")) {
                "rationale"
            } else {
                "ledger"
            },
            "kind": row.kind, "ts": row.ts, "seq": row.seq,
            "by": by, "path": text(payload, "path"),
            "symbols": symbols_sorted,
        });
        if crate::compat::truthy(payload.get("rationale")) {
            entry["text"] = json!(truncate_chars(&rationale, 240));
        }
        results.push((row.ts, entry));
    }

    for (_key, note) in store.kv_list("memory", &format!("{scope}:")) {
        if results.len() >= limit {
            break;
        }
        let (a_path, a_symbol) = note_anchor(&note);
        let tags: Vec<String> = note
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let hay = format!("{} {} {} {}", note_text(&note), a_path, a_symbol, tags.join(" ")).to_lowercase();
        if !hay.contains(&needle) {
            continue;
        }
        let created = note.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let anchor = if !a_symbol.is_empty() { format!("{a_path}::{a_symbol}") } else { a_path };
        let entry = json!({
            "type": if note.get("auto").and_then(Value::as_str) == Some("scar") { "scar" } else { "note" },
            "ts": created, "by": text(&note, "by"),
            "text": truncate_chars(&note_text(&note), 240),
            "anchor": anchor,
        });
        results.push((created, entry));
    }

    // stable: ties (equal ts) keep ledger-before-memory / scan order, same
    // as Python's stable `list.sort`
    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    let out: Vec<Value> = results.into_iter().map(|(_, entry)| entry).collect();
    json!({"q": q, "results": out})
}
