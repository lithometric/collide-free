//! How often the briefing was enough, and what that was worth.
//!
//! The hook remembers which paths it briefed in a session and which it later
//! saw read or written, and settles each briefed path once: a HIT is a
//! briefed file written without being read first (the facts were enough); a
//! NEEDED is a briefed file read and then written (the Edit tool insists on
//! its own Read, so that read was never the briefing's to save); a MISS is a
//! briefed file read and never written, known when the session ends; a FOUND
//! is a file the briefing only named, in its "N more changed" tail, that the
//! session then wrote. Each settled path stands for the messages the agent
//! did not send: the search that finds a file (hit, needed, found) and the
//! read that learns it (hit only). A message not sent would have replayed the
//! whole context: tokens_saved = messages x the context size at the time,
//! priced at the model's cache-read rate (a tenth of input). The briefing's
//! own cost is counted alongside: its text entered the context once (a cache
//! write) and rode in every message since (cache reads). Per-scope tallies,
//! so the ratio and the running saving can sit on the dashboard, and one
//! ledger row per report so a page can show what each turn saved.

use std::collections::BTreeMap;

use serde_json::Map;

use serde_json::{json, Value};

use crate::store::Store;

pub const BUCKET: &str = "briefstat";
const MAX_ROWS: usize = 4000;

fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn float(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn round3(x: f64) -> f64 {
    crate::compat::python_round(x, 3)
}

/// What one report saved: the messages the briefing made unnecessary, each
/// the size of the context at the time, at the model's cache-read price.
///
/// A hit saved two messages, the search and the read; needed and found saved
/// the search; a written file whose callers the briefing listed saved the
/// callers search. Cache-read, not blended: the message the agent did not
/// send would have replayed a cached prefix, and a cache read costs a tenth
/// of the INPUT rate, not a tenth of some input+output blend, which is five
/// times more on Opus. Returns (messages, tokens, usd).
pub fn worth(hits: u64, needed: u64, found: u64, callers: u64, context_tokens: u64, model: &str) -> (u64, u64, f64) {
    let messages = 2 * hits + needed + found + callers;
    priced(messages, context_tokens, model)
}

/// `messages` not sent, each replaying `context_tokens` at the model's
/// cache-read rate: (messages, tokens, usd).
pub fn priced(messages: u64, context_tokens: u64, model: &str) -> (u64, u64, f64) {
    let tokens = messages * context_tokens;
    let usd = tokens as f64 / 1_000_000.0 * crate::insights::cache_read_usd_per_mtok(model);
    (messages, tokens, usd)
}

/// What the briefing itself cost: `injected` tokens entered the context once
/// (a cache write, 1.25x input) and `carried` is the sum, over every message
/// since, of the briefing tokens replayed in it (cache reads, a tenth of
/// input). Returns (tokens, usd).
pub fn cost(injected: u64, carried: u64, model: &str) -> (u64, f64) {
    let (input, _) = crate::insights::usd_per_mtok(model);
    let usd = (injected as f64 * crate::insights::CACHE_WRITE_SHARE + carried as f64 * crate::insights::CACHE_READ_SHARE)
        / 1_000_000.0
        * input;
    (injected + carried, usd)
}

/// One report from the hook, as posted.
pub struct Outcome<'a> {
    pub hits: u64,
    pub misses: u64,
    pub needed: u64,
    pub found: u64,
    pub callers: u64,
    pub context_tokens: u64,
    pub model: &'a str,
    pub brief_injected: u64,
    pub brief_carried: u64,
}

/// The context this session was last seen carrying, and on what model: the
/// per-agent token record first (every report and presence post refreshes
/// it), then the newest usage-bearing ledger row of the session in the last
/// day, then the user's newest across sessions (a `collide-hook apply` run
/// has no session of its own). Zeros when nothing is known. This is what a
/// saving is priced against when the event itself carries no usage.
pub fn last_usage(store: &Store, scope: &str, session: &str, user: &str) -> (u64, String) {
    if !session.is_empty() {
        if let Some(record) = store.eph_get(&crate::presence::tokens_key(scope, user, session)) {
            let context = record.get("context").and_then(Value::as_u64).unwrap_or(0);
            if context > 0 {
                return (context, record.get("model").and_then(Value::as_str).unwrap_or("").to_string());
            }
        }
    }
    let stamp = crate::store::now();
    let handle = usage_index(store, scope, stamp);
    let index = handle.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = |entry: Option<&(f64, u64, String)>| entry.filter(|(ts, _, _)| *ts >= stamp - USAGE_WINDOW_S).map(|(_, c, m)| (*c, m.clone()));
    if !session.is_empty() {
        if let Some(found) = fresh(index.by_session.get(session)) {
            return found;
        }
    }
    fresh(index.by_user.get(user)).unwrap_or((0, String::new()))
}

/// How far back a usage-bearing row counts.
const USAGE_WINDOW_S: f64 = 86_400.0;

/// Per repo: the newest usage-bearing row of each session and of each user
/// in the last day — what `last_usage` scanned the day's ledger for on every
/// saving. Kept current by reading only the rows since it was last read.
#[derive(Default)]
struct UsageIndex {
    upto: f64,
    seq: i64,
    by_session: std::collections::HashMap<String, (f64, u64, String)>,
    by_user: std::collections::HashMap<String, (f64, u64, String)>,
    used: f64,
}

type UsageHandle = std::sync::Arc<std::sync::Mutex<UsageIndex>>;

fn usage_index(store: &Store, scope: &str, stamp: f64) -> UsageHandle {
    static ALL: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, UsageHandle>>> = std::sync::OnceLock::new();
    let all = ALL.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let handle = {
        let mut map = all.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 4096 {
            map.retain(|_, h| h.lock().map(|i| stamp - i.used < 3_600.0).unwrap_or(false));
        }
        map.entry(store.cache_key(scope)).or_default().clone()
    };
    {
        let mut index = handle.lock().unwrap_or_else(|e| e.into_inner());
        index.used = stamp;
        // every saving asks; once a second is current enough to price one
        if index.upto > 0.0 && stamp - index.upto < 1.0 {
            drop(index);
            return handle;
        }
        // rows land with their own timestamps, a little out of order: re-read
        // a few seconds back and skip what was already folded in by seq
        // the first read covers the day, and only rows that carry usage are
        // parsed; after it, the few seconds since the last read
        let rows = if index.upto == 0.0 {
            store.ledger_since_containing(scope, stamp - USAGE_WINDOW_S, &["\"input_tokens\"", "\"cache_read_tokens\"", "\"cache_creation_tokens\""])
        } else {
            store.ledger_since(scope, index.upto - 5.0)
        };
        let mut last = index.seq;
        for row in rows {
            if row.seq <= index.seq {
                continue;
            }
            last = last.max(row.seq);
            let usage = crate::insights::row_usage(&row.payload);
            let context = usage[0] + usage[2] + usage[3];
            if context <= 0 {
                continue;
            }
            let model = row.payload.get("model").and_then(Value::as_str).unwrap_or("").to_string();
            let entry = (row.ts, context as u64, model);
            if let Some(session) = row.payload.get("session").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                index.by_session.insert(session.to_string(), entry.clone());
            }
            if let Some(user) = row.payload.get("user").and_then(Value::as_str) {
                index.by_user.insert(user.to_string(), entry);
            }
        }
        index.seq = last;
        index.upto = stamp;
    }
    handle
}

/// One saving from a source other than the briefing tally, priced here at
/// the session's last known context and written as one ledger row of `kind`
/// (`map_answered`, `deltas_delivered`, `batch_applied`) with `detail`
/// alongside. The savings page reads these rows back by kind.
#[allow(clippy::too_many_arguments)]
pub fn record_saving(
    store: &Store, scope: &str, kind: &str, user: &str, agent: &str, session: &str, messages: u64, detail: &Value, now: f64,
) -> Value {
    let (context, model) = last_usage(store, scope, session, user);
    let (messages, tokens_saved, est_usd) = priced(messages, context, &model);
    let mut payload = Map::new();
    payload.insert("user".into(), json!(user));
    payload.insert("agent".into(), json!(agent));
    payload.insert("session".into(), json!(session));
    payload.insert("model".into(), json!(model));
    payload.insert("context_tokens".into(), json!(context));
    payload.insert("messages".into(), json!(messages));
    payload.insert("tokens_saved".into(), json!(tokens_saved));
    payload.insert("est_usd".into(), json!(est_usd));
    if let Some(extra) = detail.as_object() {
        for (key, value) in extra {
            payload.insert(key.clone(), value.clone());
        }
    }
    store.ledger_append_later(scope, kind, &Value::Object(payload), now);
    json!({"ok": true, "messages": messages, "tokens_saved": tokens_saved, "est_usd": round3(est_usd)})
}

/// The ledger kinds the savings page reads, and the source each is shown under.
pub const SOURCE_KINDS: [(&str, &str); 8] = [
    ("brief_outcome", "briefings"), ("map_answered", "map"), ("deltas_delivered", "deltas"), ("batch_applied", "recipes"),
    ("collision_prevented", "traffic"), ("claim_prevented", "claims"), ("setup_one_command", "setup"), ("land_saved", "landing"),
];

pub fn source_of(kind: &str) -> Option<&'static str> {
    SOURCE_KINDS.iter().find(|(k, _)| *k == kind).map(|(_, s)| *s)
}

/// Messages a row stands for: a briefing row from before `messages` existed
/// priced hits alone, one message each.
pub fn row_messages(payload: &Value) -> u64 {
    if payload.get("messages").is_some() { count(payload, "messages") } else { count(payload, "hits") }
}

pub fn record(store: &Store, scope: &str, user: &str, agent: &str, session: &str, outcome: &Outcome, now: f64) -> Value {
    let (messages, tokens_saved, est_usd) =
        worth(outcome.hits, outcome.needed, outcome.found, outcome.callers, outcome.context_tokens, outcome.model);
    let (brief_cost_tokens, brief_cost_usd) = cost(outcome.brief_injected, outcome.brief_carried, outcome.model);
    let tally = store.kv_get(BUCKET, scope).unwrap_or_else(|| json!({}));
    let updated = json!({
        "hits": count(&tally, "hits") + outcome.hits,
        "misses": count(&tally, "misses") + outcome.misses,
        "needed": count(&tally, "needed") + outcome.needed,
        "found": count(&tally, "found") + outcome.found,
        "callers": count(&tally, "callers") + outcome.callers,
        "reports": count(&tally, "reports") + 1,
        "messages": count(&tally, "messages") + messages,
        "tokens_saved": count(&tally, "tokens_saved") + tokens_saved,
        "est_usd": float(&tally, "est_usd") + est_usd,
        "brief_cost_tokens": count(&tally, "brief_cost_tokens") + brief_cost_tokens,
        "brief_cost_usd": float(&tally, "brief_cost_usd") + brief_cost_usd,
        "first_ts": tally.get("first_ts").and_then(Value::as_f64).unwrap_or(now),
        "last_ts": now,
    });
    let _ = store.kv_put(BUCKET, scope, &updated, now);
    let _ = store.ledger_append(
        scope,
        "brief_outcome",
        &json!({"user": user, "agent": agent, "session": session,
                "hits": outcome.hits, "misses": outcome.misses, "needed": outcome.needed, "found": outcome.found,
                "callers": outcome.callers, "messages": messages, "context_tokens": outcome.context_tokens, "model": outcome.model,
                "tokens_saved": tokens_saved, "est_usd": est_usd,
                "brief_injected": outcome.brief_injected, "brief_carried": outcome.brief_carried,
                "brief_cost_tokens": brief_cost_tokens, "brief_cost_usd": brief_cost_usd}),
        now,
    );
    stats(store, scope)
}

pub fn stats(store: &Store, scope: &str) -> Value {
    let tally = store.kv_get(BUCKET, scope).unwrap_or_else(|| json!({}));
    let (hits, misses, needed) = (count(&tally, "hits"), count(&tally, "misses"), count(&tally, "needed"));
    // answered = hit or needed (the read before an edit is the Edit tool's, not the briefing's failure)
    let touched = hits + needed + misses;
    let rate = if touched > 0 { Some(round3((hits + needed) as f64 / touched as f64)) } else { None };
    json!({
        "ok": true, "hits": hits, "misses": misses, "needed": needed, "found": count(&tally, "found"),
        "callers": count(&tally, "callers"),
        "reports": count(&tally, "reports"), "messages": count(&tally, "messages"),
        "tokens_saved": count(&tally, "tokens_saved"), "est_usd": round3(float(&tally, "est_usd")),
        "brief_cost_tokens": count(&tally, "brief_cost_tokens"), "brief_cost_usd": round3(float(&tally, "brief_cost_usd")),
        "rate": rate, "first_ts": tally.get("first_ts").cloned().unwrap_or(Value::Null), "last_ts": tally.get("last_ts").cloned().unwrap_or(Value::Null),
        "how": HOW,
    })
}

/// Every saving in the window, newest first, from every source: the
/// briefing tally, grep answers from the map, deltas delivered, batches
/// applied. Totals, a per-source breakdown and a per-day series — the page
/// behind the dashboard's counter. Word for word the Python shape.
pub fn savings(store: &Store, scope: &str, days: f64) -> Value {
    savings_across(store, &[scope.to_string()], days)
}

/// [`savings`] over several repos at once (a workspace's, as the dashboard
/// shows them): every row carries its `repo`, and `by_repo` splits the
/// totals, largest first. One scope reads exactly as `savings` always did.
pub fn savings_across(store: &Store, scopes: &[String], days: f64) -> Value {
    let since = crate::store::now() - days.max(0.02) * 86_400.0;
    let mut rows: Vec<(String, crate::store::LedgerRow)> = Vec::new();
    for scope in scopes {
        let repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(scope).to_string();
        rows.extend(store.ledger_since(scope, since).into_iter().map(|row| (repo.clone(), row)));
    }
    rows.retain(|(_, row)| source_of(&row.kind).is_some());
    rows.sort_by(|(_, a), (_, b)| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal).then(a.seq.cmp(&b.seq)));
    if rows.len() > MAX_ROWS {
        rows = rows.split_off(rows.len() - MAX_ROWS);
    }
    rows.reverse();
    let mut by_day: BTreeMap<String, u64> = BTreeMap::new();
    let (mut hits, mut needed, mut misses, mut found, mut callers, mut messages, mut tokens_saved, mut brief_cost_tokens) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut est_usd, mut brief_cost_usd) = (0.0f64, 0.0f64);
    // per source: events, messages, tokens, usd — in the order the page lists them
    let mut per_source: BTreeMap<&str, (u64, u64, u64, f64)> = BTreeMap::new();
    let mut out: Vec<Value> = Vec::new();
    // per repo: messages, tokens, usd
    let mut per_repo: BTreeMap<String, (u64, u64, f64)> = BTreeMap::new();
    for (repo, row) in &rows {
        let p = &row.payload;
        let source = source_of(&row.kind).unwrap_or("briefings");
        let (h, n, m, f, c, t) = (
            count(p, "hits"), count(p, "needed"), count(p, "misses"), count(p, "found"), count(p, "callers"), count(p, "tokens_saved"),
        );
        let msgs = row_messages(p);
        let usd = float(p, "est_usd");
        hits += h; needed += n; misses += m; found += f; callers += c; messages += msgs; tokens_saved += t;
        est_usd += usd;
        brief_cost_tokens += count(p, "brief_cost_tokens");
        brief_cost_usd += float(p, "brief_cost_usd");
        let slot = per_source.entry(source).or_insert((0, 0, 0, 0.0));
        slot.0 += 1; slot.1 += msgs; slot.2 += t; slot.3 += usd;
        *by_day.entry(crate::insights::day_key(row.ts)).or_insert(0) += t;
        let mine = per_repo.entry(repo.clone()).or_insert((0, 0, 0.0));
        mine.0 += msgs; mine.1 += t; mine.2 += usd;
        out.push(json!({
            "ts": row.ts, "seq": row.seq, "source": source, "repo": repo,
            "user": p.get("user").and_then(Value::as_str).unwrap_or(""),
            "agent": p.get("agent").and_then(Value::as_str).unwrap_or(""),
            "session": p.get("session").and_then(Value::as_str).unwrap_or(""),
            "model": p.get("model").and_then(Value::as_str).unwrap_or(""),
            "hits": h, "needed": n, "misses": m, "found": f, "callers": c, "messages": msgs,
            "context_tokens": count(p, "context_tokens"), "tokens_saved": t, "est_usd": round3(usd),
            "brief_cost_usd": round3(float(p, "brief_cost_usd")),
            "symbols": count(p, "symbols"), "count": count(p, "count"), "files": count(p, "files"), "ops": count(p, "ops"),
            "rebases": count(p, "rebases"),
            "recipe": p.get("recipe").and_then(Value::as_str).unwrap_or(""),
            "group": p.get("group").and_then(Value::as_i64),
            "freed_by": p.get("freed_by").and_then(Value::as_str).unwrap_or(""),
            "paths": p.get("paths").and_then(Value::as_array).cloned().unwrap_or_default(),
        }));
    }
    let mut by_source = Map::new();
    for (_, source) in SOURCE_KINDS {
        let (events, msgs, tokens, usd) = per_source.get(source).cloned().unwrap_or((0, 0, 0, 0.0));
        by_source.insert(source.to_string(), json!({
            "events": events, "messages": msgs, "tokens_saved": tokens, "est_usd": round3(usd), "how": source_how(source),
        }));
    }
    let touched = hits + needed + misses;
    json!({
        "days": days, "reports": out.len(),
        "totals": {"hits": hits, "needed": needed, "misses": misses, "found": found, "callers": callers, "messages": messages,
                   "tokens_saved": tokens_saved, "est_usd": round3(est_usd),
                   "brief_cost_tokens": brief_cost_tokens, "brief_cost_usd": round3(brief_cost_usd),
                   "rate": if touched > 0 { Some(round3((hits + needed) as f64 / touched as f64)) } else { None }},
        "by_source": by_source,
        "by_repo": by_repo(per_repo),
        "repos": scopes.len(),
        "by_day": by_day,
        "rows": out,
        "how": HOW,
    })
}

/// Each repo's share of the saving, largest first.
fn by_repo(per_repo: BTreeMap<String, (u64, u64, f64)>) -> Vec<Value> {
    let mut repos: Vec<(String, (u64, u64, f64))> = per_repo.into_iter().collect();
    repos.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(a.0.cmp(&b.0)));
    repos
        .into_iter()
        .map(|(repo, (messages, tokens, usd))| {
            json!({"repo": repo, "messages": messages, "tokens_saved": tokens, "est_usd": round3(usd)})
        })
        .collect()
}

/// What the numbers mean, word for word on both halves and printed under the
/// savings page.
pub const HOW: &str = "Every unit is a message the agent did not send. A message not sent would have replayed the whole context the agent was carrying, priced at the model's cache-read rate (a tenth of input). The briefings' own cost, entering the context once and riding along in every message since, is counted beside the saving. Still a floor: what an agent would have done without Collide is estimated at the least it could have been.";

/// What one source counts, word for word on both halves.
pub fn source_how(source: &str) -> &'static str {
    match source {
        "briefings" => "A briefed file written without a read (hit) saved the search that finds a file and the read that learns it; read first, then written (needed) saved the search, since the Edit tool insists on its own read; a file the briefing only named, then written (found) saved the search; a briefed file whose callers the briefing listed, then written (callers) saved the callers search. A briefed file read and never written (miss) saved nothing.",
        "map" => "A grep the map answered in the same step, with the symbol's signature, docstring and callers: the read of that file, not sent.",
        "deltas" => "A teammate's change in what this session was working on, delivered into the session as it happened: the re-read that would have caught it, not sent, once per change.",
        "traffic" => "A group of work plan_work held because another agent was in its way, released when they finished: a collision that did not happen, worth at least the re-read and the re-edit it would have cost, two messages.",
        "claims" => "A new numbered file the gate stopped because another agent already held that number, and moved to the next free one: a clash that did not happen, worth at least the re-read and the rename it would have cost, two messages.",
        "setup" => "A setup done by one command: every file it wrote would have been a message of its own, and every file it merged into (settings, .mcp.json, AGENTS.md, .gitignore) a read before that write, less the one command that did it.",
        "landing" => "A push collide-hook land made for the agent: each time it rebased onto teammates' new commits was at least the rejected push, the pull and the test run the agent would have sent, three messages; each of the agent's files it adapted to a teammate's landed rename, the read and the edit.",
        "recipes" => "A typed-op batch collide-hook apply wrote and verified: every file landed without the agent reading or editing it, two messages a file less the one command that did it. A replayed recipe reused another agent's whole transformation.",
        _ => "",
    }
}
