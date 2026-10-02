//! Consolidation: the append-only episodic ledger distilled into the semantic
//! gist an agent needs at session start.
//!
//! This is the sleep step. The ledger records what happened, row by row,
//! forever; a session starting now cannot read all of it and should not have
//! to. What it needs is the gist — who changed what, which renames it has to
//! adapt to, where collisions cluster, what the repo says it does versus what
//! the ledger shows it doing.
//!
//! Every number here is counted from rows, never asserted. That is the whole
//! claim of the briefing: it reports the protocol's own behaviour back to the
//! agents following it, including when that behaviour is bad.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::compat::python_round;
use crate::store::{now, Store};

/// A counter that remembers what it saw first.
///
/// Python's `Counter.most_common` breaks ties by insertion order, and the
/// rows feeding these counters arrive in ledger sequence — so "first seen"
/// means "happened earliest", which is a real ordering and the one the
/// Python half reports. Sorting ties by name instead would be just as
/// deterministic and quietly disagree on every tie.
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

    /// Most first, ties in the order they were first seen.
    fn most_common(&self, limit: usize) -> Vec<String> {
        let mut ranked: Vec<(usize, &String)> = self.order.iter().enumerate().collect();
        ranked.sort_by_key(|(position, key)| (-self.counts[*key], *position));
        ranked.into_iter().take(limit).map(|(_, key)| key.clone()).collect()
    }
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `user` or `owner`, whichever the row carries — different kinds name the
/// same person differently, and a briefing that split them would report one
/// agent as two.
fn actor(payload: &Value) -> String {
    let user = text(payload, "user");
    if !user.is_empty() {
        return user;
    }
    let owner = text(payload, "owner");
    if !owner.is_empty() {
        return owner;
    }
    "?".to_string()
}

fn is_auto(payload: &Value) -> bool {
    crate::compat::truthy(payload.get("auto"))
}

/// Highest-salience paths: where the repo has actually been hurt before.
pub fn scar_tissue(store: &Store, scope: &str, limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> =
        store.kv_list("salience", &format!("{scope}:")).into_iter().map(|(_k, v)| v).collect();
    rows.sort_by(|a, b| {
        let score = |v: &Value| v.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        score(b).partial_cmp(&score(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

// ------------------------------------------------------------------- drift

/// A hit on a convention: who, through what agent, on which path.
struct Hit {
    user: String,
    agent: String,
    example: String,
}

fn drift_entry(convention: &str, source: &str, hits: &[Hit]) -> Value {
    let users: BTreeSet<&str> = hits.iter().map(|hit| hit.user.as_str()).collect();
    let agents: BTreeSet<&str> = hits
        .iter()
        .map(|hit| if hit.agent.is_empty() { "(unattributed)" } else { hit.agent.as_str() })
        .collect();
    json!({
        "convention": convention,
        "source": source,
        "violations": hits.len(),
        "users": users.into_iter().collect::<Vec<_>>(),
        "agents": agents.into_iter().collect::<Vec<_>>(),
        "examples": hits.iter().take(3).map(|hit| hit.example.clone()).collect::<Vec<_>>(),
    })
}

/// Documentation that audits itself.
///
/// The conventions the repo states — the protocol block's own rules — plus
/// language naming defaults, diffed against what the ledger shows agents
/// actually doing. It surfaces drift as a report and never edits the file,
/// which is the only honest thing to do with a rule nobody is following:
/// say so, and let a human decide whether the rule or the behaviour is wrong.
/// Only possible from the write-path position.
pub fn convention_drift(store: &Store, scope: &str, since_s: f64) -> Vec<Value> {
    let rows = store.ledger_since(scope, now() - since_s);
    let mut sessions_with_check: BTreeSet<(String, String)> = BTreeSet::new();
    let mut paths_checked: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unchecked: Vec<Hit> = Vec::new();
    let mut undeclared: Vec<Hit> = Vec::new();
    let mut misnamed: Vec<Hit> = Vec::new();

    for row in &rows {
        let (kind, payload) = (&row.kind, &row.payload);
        let user = actor(payload);
        let agent = text(payload, "agent");
        let session = text(payload, "session");
        match kind.as_str() {
            "check_performed" => {
                sessions_with_check.insert((user.clone(), session));
                // the enforce-mode gate checks before every write but has no
                // session of its own; it vouches for the path it cleared
                if let Some(paths) = payload.get("paths").and_then(Value::as_array) {
                    for checked in paths.iter().filter_map(Value::as_str) {
                        paths_checked.insert((user.clone(), checked.to_string()));
                    }
                }
            }
            "edit_reported" if !is_auto(payload) => {
                let path = text(payload, "path");
                if !sessions_with_check.contains(&(user.clone(), session))
                    && !paths_checked.contains(&(user.clone(), path.clone()))
                {
                    unchecked.push(Hit {
                        user: user.clone(), agent: agent.clone(), example: path.clone(),
                    });
                }
                if text(payload, "intent_id").is_empty() {
                    undeclared.push(Hit {
                        user: user.clone(), agent: agent.clone(), example: path.clone(),
                    });
                }
                for name in added_symbols(payload) {
                    if breaks_naming(&path, &name) {
                        misnamed.push(Hit {
                            user: user.clone(),
                            agent: agent.clone(),
                            example: format!("{name} in {path}"),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let mut drift = Vec::new();
    if !unchecked.is_empty() {
        drift.push(drift_entry(
            "call check_collisions before writing", "protocol block step 2", &unchecked));
    }
    if !undeclared.is_empty() {
        drift.push(drift_entry(
            "declare an intent before editing", "protocol block step 1", &undeclared));
    }
    if !misnamed.is_empty() {
        drift.push(drift_entry(
            "python symbols snake_case / TS symbols camelCase", "language default", &misnamed));
    }
    drift
}

/// Symbols this edit introduced, from either shape the row may carry.
fn added_symbols(payload: &Value) -> BTreeSet<String> {
    let mut added: BTreeSet<String> = BTreeSet::new();
    if let Some(events) = payload.get("events").and_then(Value::as_array) {
        for event in events {
            if event.get("kind").and_then(Value::as_str) == Some("added") {
                added.insert(text(event, "symbol"));
            }
        }
    }
    if let Some(changed) = payload.get("symbols_changed").and_then(Value::as_object) {
        for (name, delta) in changed {
            if delta.get("before").map(Value::is_null).unwrap_or(true) {
                added.insert(name.clone());
            }
        }
    }
    added.remove("");
    added
}

/// camelCase in Python, or snake_case in TypeScript. Deliberately narrow:
/// a naming rule that fires on anything ambiguous is a rule agents learn to
/// ignore, which costs more than the drift it reports.
fn breaks_naming(path: &str, name: &str) -> bool {
    let camel_in_py = path.ends_with(".py")
        && name.chars().next().is_some_and(char::is_lowercase)
        && name.chars().any(char::is_uppercase);
    let snake_in_ts = (path.ends_with(".ts") || path.ends_with(".tsx"))
        && name.trim_matches('_').contains('_')
        && name == name.to_lowercase();
    camel_in_py || snake_in_ts
}

// ---------------------------------------------------------------- activity

#[derive(Default)]
struct Person {
    edits: i64,
    checks: i64,
    paths: Tally,
    agents: BTreeSet<String>,
}

/// The whole briefing: the ledger distilled into what the people and agents
/// on this repo have been doing, plus the repo map that says where they did
/// it. Every number is counted from rows, never asserted.
pub fn activity(
    store: &Store, scope: &str, repo_id: &str, since_s: f64, idle_after_s: f64, viewer: &str, share_knowledge: bool,
) -> Value {
    let stamp = now();
    let cutoff = stamp - since_s;
    let rows = store.ledger_since(scope, cutoff);

    let mut users: BTreeMap<String, Person> = BTreeMap::new();
    let mut user_order: Vec<String> = Vec::new();
    let mut hotspots = Tally::default();
    let mut renames: Vec<Value> = Vec::new();
    let mut fired: Vec<Value> = Vec::new();
    let mut degraded = 0i64;
    let mut counts: BTreeMap<&str, i64> = [
        ("collisions", 0), ("blocks", 0), ("overrides", 0),
        ("intents_declared", 0), ("intents_completed", 0), ("intents_abandoned", 0),
    ]
    .into_iter()
    .collect();
    let mut sessions_with_check: BTreeSet<(String, String)> = BTreeSet::new();
    let mut paths_checked: BTreeSet<(String, String)> = BTreeSet::new();
    let mut edits_total = 0i64;
    let mut edits_unchecked = 0i64;

    for row in &rows {
        let (kind, payload) = (&row.kind, &row.payload);
        let user = actor(payload);
        let agent = text(payload, "agent");
        match kind.as_str() {
            "edit_reported" => {
                if !users.contains_key(&user) {
                    user_order.push(user.clone());
                }
                let person = users.entry(user.clone()).or_default();
                person.edits += 1;
                let path = text(payload, "path");
                if !path.is_empty() {
                    person.paths.add(&path, 1);
                }
                if !agent.is_empty() {
                    person.agents.insert(agent.clone());
                }
                // The check-before-write RATE grades protocol behaviour only.
                // Hook and watcher reports are mechanical and session-less by
                // construction, so counting them as unchecked writes tanked
                // the rate unfairly — and in enforce mode the gate DID check
                // those writes, which the path credit can prove.
                if !is_auto(payload) {
                    edits_total += 1;
                    let session = text(payload, "session");
                    let checked = sessions_with_check.contains(&(user.clone(), session))
                        || paths_checked.contains(&(user.clone(), path));
                    if !checked {
                        edits_unchecked += 1;
                    }
                }
            }
            "check_performed" => {
                if !users.contains_key(&user) {
                    user_order.push(user.clone());
                }
                let person = users.entry(user.clone()).or_default();
                person.checks += 1;
                if !agent.is_empty() {
                    person.agents.insert(agent.clone());
                }
                sessions_with_check.insert((user.clone(), text(payload, "session")));
                if let Some(paths) = payload.get("paths").and_then(Value::as_array) {
                    for path in paths.iter().filter_map(Value::as_str) {
                        paths_checked.insert((user.clone(), path.to_string()));
                    }
                }
            }
            "collision_returned" => {
                *counts.get_mut("collisions").unwrap() +=
                    payload.get("count").and_then(Value::as_i64).unwrap_or(1);
                if let Some(items) = payload.get("items").and_then(Value::as_array) {
                    for item in items {
                        let path = text(item, "path");
                        if !path.is_empty() {
                            hotspots.add(&path, 1);
                        }
                    }
                }
            }
            "intent_declared" => {
                *counts.get_mut("intents_declared").unwrap() += 1;
                let (before, after) = (text(payload, "before"), text(payload, "after"));
                if text(payload, "change_type") == "rename"
                    && !before.is_empty()
                    && !after.is_empty()
                {
                    renames.push(json!({
                        "before": before, "after": after, "owner": user, "agent": agent,
                    }));
                }
            }
            "intent_completed" => *counts.get_mut("intents_completed").unwrap() += 1,
            "intent_abandoned" => *counts.get_mut("intents_abandoned").unwrap() += 1,
            "gate_block" => *counts.get_mut("blocks").unwrap() += 1,
            "override" => *counts.get_mut("overrides").unwrap() += 1,
            "tripwire_fired" => fired.push(json!({
                "note": text(payload, "note"), "owner": text(payload, "owner"),
                "by": text(payload, "by"), "path": text(payload, "path"),
            })),
            "degraded" => degraded += 1,
            _ => {}
        }
    }

    let check_rate = if edits_total > 0 {
        let rate = 1.0 - (edits_unchecked as f64) / (edits_total as f64);
        Value::from(python_round(rate, 3))
    } else {
        Value::Null
    };

    // most edits first, ties in the order the ledger first saw them
    let mut ranked: Vec<(usize, &String)> = user_order.iter().enumerate().collect();
    ranked.sort_by_key(|(position, user)| (-users[*user].edits, *position));
    let ranked: Vec<(&String, &Person)> =
        ranked.into_iter().map(|(_, user)| (user, &users[user])).collect();

    let mut user_out = serde_json::Map::new();
    for (user, person) in &ranked {
        user_out.insert((*user).clone(), json!({
            "edits": person.edits,
            "checks": person.checks,
            "top_paths": person.paths.most_common(5),
            "agents": person.agents.iter().cloned().collect::<Vec<_>>(),
        }));
    }

    let mut summary: Vec<String> = Vec::new();
    for (user, person) in &ranked {
        if person.edits == 0 && person.checks == 0 {
            continue;
        }
        let via = if person.agents.is_empty() {
            String::new()
        } else {
            format!(" via {}", person.agents.iter().cloned().collect::<Vec<_>>().join(", "))
        };
        let paths = person.paths.most_common(5);
        let where_ = paths.first().map(|p| format!(", mostly {p}")).unwrap_or_default();
        summary.push(format!(
            "{user}: {} edits, {} checks{via}{where_}", person.edits, person.checks));
    }
    for rename in renames.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
        summary.push(format!(
            "rename: {} -> {} ({})",
            text(rename, "before"), text(rename, "after"), text(rename, "owner")));
    }
    if counts["collisions"] > 0 {
        let hot = hotspots.most_common(3).join(", ");
        summary.push(if hot.is_empty() {
            format!("{} collision(s)", counts["collisions"])
        } else {
            format!("{} collision(s); hotspots: {hot}", counts["collisions"])
        });
    }
    if counts["blocks"] > 0 || counts["overrides"] > 0 {
        summary.push(format!(
            "{} blocked write(s), {} override(s)", counts["blocks"], counts["overrides"]));
    }
    if counts["intents_abandoned"] > 0 {
        summary.push(format!(
            "{} intent(s) expired abandoned — work may be half-done",
            counts["intents_abandoned"]));
    }
    for tripwire in fired.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
        summary.push(format!(
            "tripwire fired: \"{}\" ({} touched {})",
            text(tripwire, "note"), text(tripwire, "by"), text(tripwire, "path")));
    }
    if degraded > 0 {
        summary.push(format!("{degraded} degraded window(s) — some activity went unobserved"));
    }
    if let Some(rate) = check_rate.as_f64() {
        summary.push(format!("check-before-write rate {}%", python_round(rate * 100.0, 0)));
    }

    let mut active: Vec<Value> = Vec::new();
    for (_key, tripwire) in store.kv_list("tripwire", &format!("{scope}:")) {
        let unfired = !crate::compat::truthy(tripwire.get("fired"));
        if unfired && tripwire.get("expires").and_then(Value::as_f64).unwrap_or(0.0) > stamp {
            active.push(json!({
                "id": tripwire.get("id").cloned().unwrap_or(Value::Null),
                "owner": tripwire.get("owner").cloned().unwrap_or(Value::Null),
                "note": tripwire.get("note").cloned().unwrap_or(Value::Null),
                "paths": tripwire.get("paths").cloned().unwrap_or(json!([])),
                "symbols": tripwire.get("symbols").cloned().unwrap_or(json!([])),
            }));
        }
    }

    // Key order is byte order on the wire, and the wire is what a prompt
    // cache hashes. Everything that depends only on repo STATE comes first,
    // in a fixed order; the two fields that change with the clock — the
    // window's cutoff and the ledger row count — come last, so two agents
    // briefed on the same state share every byte up to them.
    let mut out = serde_json::Map::new();
    out.insert("repo_id".into(), json!(repo_id));
    out.insert("window_days".into(), json!(python_round(since_s / 86400.0, 2)));
    out.insert("users".into(), Value::Object(user_out));
    out.insert("renames".into(), json!(renames));
    out.insert("hotspots".into(), json!(hotspots
        .most_common(10)
        .into_iter()
        .map(|path| json!({"path": path.clone(), "collisions": hotspots.get(&path)}))
        .collect::<Vec<_>>()));
    out.insert("scar_tissue".into(), json!(scar_tissue(store, scope, 10)));
    // where the plan does not share knowledge, the scars served are the
    // reader's own — the awareness half (who edited what, hotspots,
    // renames) is the radar and free
    let scars: Vec<Value> = crate::memory::scars_for_briefing(store, scope, 10)
        .into_iter()
        .filter(|scar| share_knowledge || viewer.is_empty() || crate::access::own_knowledge(scar, viewer))
        .collect();
    out.insert("scars".into(), json!(scars));
    out.insert("convention_drift".into(), json!(convention_drift(store, scope, since_s)));
    out.insert("tripwires_fired".into(), json!(fired));
    out.insert("tripwires_active".into(), json!(active));
    out.insert("degraded_windows".into(), json!(degraded));
    for (name, value) in &counts {
        out.insert((*name).to_string(), json!(value));
    }
    out.insert("check_before_write_rate".into(), check_rate);
    out.insert("summary".into(), json!(summary));
    // the compact map an agent reads INSTEAD of grepping. Every teammate's
    // reported edit sharpens it, so the more agents on a repo the cheaper
    // orientation gets for the next one.
    out.insert("repo_map".into(), crate::graphview::briefing_map(store, scope, idle_after_s, 8));
    out.insert("since".into(), json!(cutoff));
    out.insert("rows_considered".into(), json!(rows.len()));
    Value::Object(out)
}



// ------------------------------------------------------------- compliance

/// Per-person and per-agent protocol behaviour, counted from the ledger.
///
/// This is an observability report, not a request-path check — nothing here
/// is on a hot path and nothing here blocks anyone. The per-AGENT half is the
/// one that earns its keep: comparing check-before-write rates across agent
/// and model versions is a behavioural canary, and it is the only place a
/// regression in how a model follows the protocol would show up at all.
pub fn compliance_report(store: &Store, scope: &str, repo_id: &str, since: f64) -> Value {
    let rows = store.ledger_since(scope, since);

    #[derive(Default, Clone)]
    struct Stats {
        checks: i64,
        edits: i64,
        writes: i64,
        unchecked: i64,
    }

    let mut per_user: BTreeMap<String, Stats> = BTreeMap::new();
    let mut user_order: Vec<String> = Vec::new();
    let mut per_agent: BTreeMap<String, Stats> = BTreeMap::new();
    let mut agent_order: Vec<String> = Vec::new();
    let mut sessions_with_check: BTreeSet<(String, String)> = BTreeSet::new();
    let mut paths_checked: BTreeSet<(String, String)> = BTreeSet::new();
    let mut counts: BTreeMap<&str, i64> = [
        ("intents_declared", 0), ("intents_completed", 0), ("intents_abandoned", 0),
        ("collisions_returned", 0), ("blocks", 0), ("overrides", 0),
    ]
    .into_iter()
    .collect();
    // what the briefings were worth IN THIS WINDOW, from the same rows
    let (mut b_hits, mut b_needed, mut b_misses, mut b_reports, mut b_saved) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let (mut b_found, mut b_callers, mut b_messages, mut b_cost) = (0i64, 0i64, 0i64, 0.0f64);
    // every saving in the window by source: (events, messages, tokens, usd)
    let mut sources: BTreeMap<&str, (i64, i64, i64, f64)> = BTreeMap::new();
    let mut b_usd = 0.0f64;

    // rows arrive in sequence order, so "already seen" means "happened first"
    for row in &rows {
        let payload = &row.payload;
        let user = actor(payload);
        let session = text(payload, "session");
        let agent = {
            let named = text(payload, "agent");
            if named.is_empty() { "(unattributed)".to_string() } else { named }
        };
        let bucket = |key: &str, map: &mut BTreeMap<String, Stats>, order: &mut Vec<String>| {
            if !map.contains_key(key) {
                order.push(key.to_string());
                map.insert(key.to_string(), Stats::default());
            }
        };

        match row.kind.as_str() {
            "check_performed" => {
                bucket(&user, &mut per_user, &mut user_order);
                bucket(&agent, &mut per_agent, &mut agent_order);
                per_user.get_mut(&user).unwrap().checks += 1;
                per_agent.get_mut(&agent).unwrap().checks += 1;
                sessions_with_check.insert((user.clone(), session));
                // the gate checks mechanically and has no session of its own:
                // it vouches for the exact path it cleared
                if let Some(paths) = payload.get("paths").and_then(Value::as_array) {
                    for checked in paths.iter().filter_map(Value::as_str) {
                        paths_checked.insert((user.clone(), checked.to_string()));
                    }
                }
            }
            "edit_reported" => {
                bucket(&user, &mut per_user, &mut user_order);
                bucket(&agent, &mut per_agent, &mut agent_order);
                if is_auto(payload) {
                    // A mechanical report is not protocol BEHAVIOUR and never
                    // grades the rate. It is still a write, though, and
                    // counting it nowhere made a prolific hook-reporting
                    // member read as "no writes yet", which is simply false.
                    per_user.get_mut(&user).unwrap().writes += 1;
                    per_agent.get_mut(&agent).unwrap().writes += 1;
                } else {
                    let path = text(payload, "path");
                    let checked = sessions_with_check.contains(&(user.clone(), session))
                        || paths_checked.contains(&(user.clone(), path));
                    for stats in [
                        per_user.get_mut(&user).unwrap(),
                        per_agent.get_mut(&agent).unwrap(),
                    ] {
                        stats.edits += 1;
                        stats.writes += 1;
                        if !checked {
                            stats.unchecked += 1;
                        }
                    }
                }
            }
            "intent_declared" => *counts.get_mut("intents_declared").unwrap() += 1,
            "intent_completed" => *counts.get_mut("intents_completed").unwrap() += 1,
            "intent_abandoned" => *counts.get_mut("intents_abandoned").unwrap() += 1,
            "collision_returned" => *counts.get_mut("collisions_returned").unwrap() += 1,
            "gate_block" => *counts.get_mut("blocks").unwrap() += 1,
            "override" => *counts.get_mut("overrides").unwrap() += 1,
            "brief_outcome" | "map_answered" | "deltas_delivered" | "batch_applied" | "collision_prevented" | "claim_prevented"
            | "setup_one_command" | "land_saved" => {
                let messages = crate::briefstat::row_messages(payload) as i64;
                let usd = payload.get("est_usd").and_then(Value::as_f64).unwrap_or(0.0);
                let slot = sources.entry(crate::briefstat::source_of(&row.kind).unwrap_or("briefings")).or_insert((0, 0, 0, 0.0));
                slot.0 += 1;
                slot.1 += messages;
                slot.2 += as_int(payload, "tokens_saved");
                slot.3 += usd;
                if row.kind == "brief_outcome" {
                    b_hits += as_int(payload, "hits");
                    b_needed += as_int(payload, "needed");
                    b_misses += as_int(payload, "misses");
                    b_found += as_int(payload, "found");
                    b_callers += as_int(payload, "callers");
                    b_messages += messages;
                    b_saved += as_int(payload, "tokens_saved");
                    b_reports += 1;
                    b_usd += usd;
                    b_cost += payload.get("brief_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
                }
            }
            _ => {}
        }
    }

    let render = |order: &[String], stats: &BTreeMap<String, Stats>| -> Value {
        let mut out = serde_json::Map::new();
        for key in order {
            let Some(stat) = stats.get(key) else { continue };
            // no edits means no evidence, which is reported as unknown rather
            // than as a perfect score nobody earned
            let rate = if stat.edits > 0 {
                Value::from(python_round(
                    1.0 - (stat.unchecked as f64) / (stat.edits as f64), 3))
            } else {
                Value::Null
            };
            out.insert(key.clone(), json!({
                "checks": stat.checks,
                "edits": stat.edits,
                "writes": stat.writes,
                "edits_without_prior_check": stat.unchecked,
                "check_before_write_rate": rate,
            }));
        }
        Value::Object(out)
    };

    let mut result = serde_json::Map::new();
    result.insert("repo_id".into(), json!(repo_id));
    result.insert("since".into(), json!(since));
    result.insert("users".into(), render(&user_order, &per_user));
    result.insert("agents".into(), render(&agent_order, &per_agent));
    for (name, value) in &counts {
        result.insert((*name).to_string(), json!(value));
    }
    result.insert("rows_considered".into(), json!(rows.len()));
    // How often the briefing was enough — over the SAME window as the rest of
    // this report, and as the cost figure the dashboard prints beside it. This
    // used to read the `briefstat` kv bucket, which only ever grows: a lifetime
    // total sat under a "This week" heading next to a genuine 7-day cost, and
    // disagreed with the savings page (which reads these very rows) for no
    // reason a reader could see. The lifetime tally is still served, under
    // `lifetime`.
    let lifetime = crate::briefstat::stats(store, scope);
    let mut windowed =
        briefing_block(b_hits, b_needed, b_misses, b_reports, b_saved, b_usd, b_found, b_callers, b_messages, b_cost);
    if let Some(map) = windowed.as_object_mut() {
        map.insert(
            "lifetime".into(),
            briefing_block(
                as_int(&lifetime, "hits"), as_int(&lifetime, "needed"), as_int(&lifetime, "misses"),
                as_int(&lifetime, "reports"), as_int(&lifetime, "tokens_saved"),
                lifetime.get("est_usd").and_then(Value::as_f64).unwrap_or(0.0),
                as_int(&lifetime, "found"), as_int(&lifetime, "callers"), as_int(&lifetime, "messages"),
                lifetime.get("brief_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
            ),
        );
    }
    result.insert("briefing".into(), windowed);
    // every source together: what the dashboard's "This week" line prints
    let (mut all_messages, mut all_tokens, mut all_usd) = (0i64, 0i64, 0.0f64);
    let mut by_source = serde_json::Map::new();
    for (_, source) in crate::briefstat::SOURCE_KINDS {
        let (events, messages, tokens, usd) = sources.get(source).cloned().unwrap_or((0, 0, 0, 0.0));
        all_messages += messages;
        all_tokens += tokens;
        all_usd += usd;
        by_source.insert(source.to_string(),
            json!({"events": events, "messages": messages, "tokens_saved": tokens, "est_usd": python_round(usd, 3)}));
    }
    result.insert("savings".into(), json!({
        "messages": all_messages, "tokens_saved": all_tokens, "est_usd": python_round(all_usd, 3),
        "brief_cost_usd": python_round(b_cost, 3), "by_source": by_source,
    }));
    Value::Object(result)
}

fn as_int(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The briefing's hit/miss tally in the shape the dashboard shows.
///
/// `reports` is the honest name: the hooks post one at every Stop that
/// classified a new path, so one agent SESSION posts many. `sessions` is kept
/// as a deprecated alias only because the dashboard still reads it (and labels
/// it "N sessions", which was never true).
#[allow(clippy::too_many_arguments)]
fn briefing_block(
    hits: i64, needed: i64, misses: i64, reports: i64, tokens_saved: i64, est_usd: f64, found: i64, callers: i64,
    messages: i64, brief_cost_usd: f64,
) -> Value {
    let touched = hits + needed + misses;
    let rate = if touched > 0 {
        Value::from(python_round((hits + needed) as f64 / touched as f64, 3))
    } else {
        Value::Null
    };
    json!({"hits": hits, "needed": needed, "misses": misses, "found": found, "callers": callers, "messages": messages,
           "reports": reports, "sessions": reports, "rate": rate,
           "tokens_saved": tokens_saved, "est_usd": python_round(est_usd, 3),
           "brief_cost_usd": python_round(brief_cost_usd, 3)})
}
