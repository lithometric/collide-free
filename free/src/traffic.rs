//! Traffic control: `plan_work` as a live gate.
//!
//! Knowing two agents will collide and letting them both run means paying
//! for the collision anyway: thousands of tokens to unwind and a merge.
//! Holding one of them for a minute is cheap. So a plan is checked against
//! everything live: a group whose files no other agent has a claim on runs
//! now; a group that overlaps another agent's open intent, a hot marker by
//! another identity, or whose typed ops the intent algebra says CONFLICT (or
//! cannot prove commute) is held. Runnable groups are claimed on the spot
//! when asked (`dispatch`), so other planners see them at once. Held groups
//! wait in the planner's queue; every time an intent completes the queue is
//! re-checked, and a group that has come free is released into the
//! planner's next deltas line. The clearest place this lands is subagents
//! spawned in parallel, blind to each other: the parent gets a provable
//! split instead of a guess. Python's `_traffic_gate` / `_release_held`,
//! same verdicts, same rows.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

const HELD_TTL_S: f64 = 3600.0;
const RELEASED_TTL_S: f64 = 3600.0;
/// A held group later released is a collision that did not happen: at least
/// the re-read and the re-edit it would have cost, two messages.
const PREVENTED_MESSAGES: u64 = 2;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn agent_of(owner: &str, session: &str) -> String {
    if session.is_empty() { owner.to_string() } else { format!("{owner}#{session}") }
}

/// Queues are per PERSON, not per session: plan_work arrives on the MCP
/// session while releases are read on the hook's session, and the two ids
/// differ. A release reaches whichever of the planner's sessions steps next.
pub fn held_key(scope: &str, user: &str, _session: &str) -> String {
    format!("held:{scope}:{user}")
}

pub fn released_key(scope: &str, user: &str, _session: &str) -> String {
    format!("released:{scope}:{user}")
}

/// Who else is on a group right now: other agents' open intents naming any
/// of its files, other identities' hot markers on them, and other agents'
/// typed operations the algebra cannot prove commute with the group's.
/// Sorted, deduplicated agent ids; empty means the group may run.
pub fn blockers(store: &Store, scope: &str, me: &str, files: &[String], ops: &[Value]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let wanted: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut others: Vec<Value> = Vec::new();
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        let who = agent_of(&text(&intent, "owner"), &text(&intent, "session"));
        if who == me {
            continue;
        }
        if strings(&intent, "paths").iter().any(|p| wanted.contains(p.as_str())) {
            out.insert(who.clone());
        }
        others.push(intent);
    }
    if !ops.is_empty() {
        for finding in crate::operations::intent_conflicts(ops, &others) {
            let id = text(&finding, "intent_id");
            if let Some(intent) = others.iter().find(|i| text(i, "intent_id") == id) {
                out.insert(agent_of(&text(intent, "owner"), &text(intent, "session")));
            }
        }
    }
    for file in files {
        for (key, _marker) in store.eph_scan(&format!("hot:{scope}:{}:", crate::repo::path_key(file))) {
            let who = key.rsplit(':').next().unwrap_or("").to_string();
            if !who.is_empty() && who != me {
                out.insert(who);
            }
        }
    }
    out.into_iter().collect()
}

/// The typed operations a group's units carry, in the shape the algebra
/// takes: a unit with `op`, `symbol` (and its other fields) is one operation.
fn group_ops(units: &[Value], members: &[usize]) -> Vec<Value> {
    members
        .iter()
        .filter_map(|m| units.get(*m))
        .filter(|u| !text(u, "op").is_empty() && !text(u, "symbol").is_empty())
        .cloned()
        .collect()
}

/// The plan, gated against everything live. Adds to each group its verdict
/// (`run_now` / `hold`) and who blocks it; lists `run_now` and `hold`; with
/// `dispatch`, claims every runnable group as an intent the subagent should
/// heartbeat and complete, and queues the held ones for release.
pub fn gate(
    store: &Store, scope: &str, repo_id: &str, user: &str, session: &str, plan: Value, units: &[Value], dispatch: bool,
    idle_after_s: f64,
) -> Value {
    let me = agent_of(user, session);
    let stamp = now();
    // the plan's id is what it is of: the person and the units, so both
    // halves name the same plan the same way and a re-plan finds its queue
    let plan_id = {
        use sha1::{Digest, Sha1};
        let key: Vec<String> = units.iter().map(|u| format!("{}::{}:{}", text(u, "path"), text(u, "symbol"), text(u, "op"))).collect();
        let mut hasher = Sha1::new();
        hasher.update(format!("{user}|{}", key.join("|")).as_bytes());
        format!("P-{}", &format!("{:x}", hasher.finalize())[..6])
    };
    let mut groups: Vec<Value> = plan.get("groups").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut run_now: Vec<usize> = Vec::new();
    let mut hold: Vec<usize> = Vec::new();
    let mut held: Vec<Value> = Vec::new();
    for (n, group) in groups.iter_mut().enumerate() {
        let members: Vec<usize> = group.get("units").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|v| v as usize).collect()).unwrap_or_default();
        let files = strings(group, "files");
        let ops = group_ops(units, &members);
        let who = blockers(store, scope, &me, &files, &ops);
        let map = group.as_object_mut().expect("group is an object");
        map.insert("n".into(), json!(n));
        map.insert("blocked_by".into(), json!(who));
        if who.is_empty() {
            map.insert("verdict".into(), json!("run_now"));
            run_now.push(n);
            if dispatch {
                // claim it now: the files the units name are what the agent
                // will write; the blast radius only decided the grouping
                let paths: Vec<String> = members.iter().filter_map(|m| units.get(*m)).map(|u| text(u, "path")).filter(|p| !p.is_empty()).collect::<BTreeSet<_>>().into_iter().collect();
                let symbols: Vec<String> = members.iter().filter_map(|m| units.get(*m)).map(|u| text(u, "symbol")).filter(|s| !s.is_empty()).collect::<BTreeSet<_>>().into_iter().collect();
                let summary = format!("plan {plan_id} group {n}: {}", symbols.join(", "));
                let declared = crate::intents::declare(store, &crate::intents::DeclareInput {
                    scope, user_id: user, repo_id, paths, symbols, change_type: "refactor", before: "", after: "",
                    summary: &summary, agent: "plan_work", session, reference: "",
                    operations: if ops.is_empty() { None } else { Some(json!(ops)) },
                    idempotency_key: "", ttl_s: 1800.0, idle_after_s, via: "plan_work",
                });
                if let Some(id) = declared.get("intent_id").and_then(Value::as_str) {
                    map.insert("intent_id".into(), json!(id));
                }
            }
        } else {
            map.insert("verdict".into(), json!("hold"));
            hold.push(n);
            held.push(json!({"n": n, "units": members, "files": files, "ops": ops, "blocked_by": who, "plan": plan_id}));
        }
    }
    if dispatch {
        let key = held_key(scope, user, session);
        if held.is_empty() {
            store.eph_delete(&key);
        } else {
            let _ = store.eph_set(&key, &json!({"plan": plan_id, "groups": held, "ts": stamp, "user": user, "session": session, "repo_id": repo_id}), Some(HELD_TTL_S));
        }
    }
    let released = take_released(store, scope, user, session);
    let mut out = plan;
    if let Some(map) = out.as_object_mut() {
        map.insert("groups".into(), json!(groups));
        map.insert("plan".into(), json!(plan_id));
        map.insert("run_now".into(), json!(run_now));
        map.insert("hold".into(), json!(hold));
        map.insert("released".into(), json!(released));
        map.insert("dispatched".into(), json!(dispatch));
        map.insert("traffic".into(), json!(format!(
            "{} group(s) can run now, {} held until the agents in their way finish; releases arrive in your context as Collide lines",
            run_now.len(), hold.len())));
    }
    out
}

/// An intent completed: every held group in the scope is checked again, and
/// each one that has come free moves to its planner's released list, with a
/// collision-prevented row priced for the planner. Python's `_release_held`.
pub fn release_after(store: &Store, scope: &str, finished_user: &str, finished_session: &str) {
    let stamp = now();
    let freed_by = agent_of(finished_user, finished_session);
    for (key, record) in store.eph_scan(&format!("held:{scope}:")) {
        let user = text(&record, "user");
        let session = text(&record, "session");
        let me = agent_of(&user, &session);
        let mut still: Vec<Value> = Vec::new();
        let mut freed: Vec<Value> = Vec::new();
        for group in record.get("groups").and_then(Value::as_array).into_iter().flatten() {
            let files = strings(group, "files");
            let ops: Vec<Value> = group.get("ops").and_then(Value::as_array).cloned().unwrap_or_default();
            let who = blockers(store, scope, &me, &files, &ops);
            if who.is_empty() {
                let mut released = group.clone();
                if let Some(map) = released.as_object_mut() {
                    map.insert("freed_by".into(), json!(freed_by));
                    map.insert("ts".into(), json!(stamp));
                }
                freed.push(released);
            } else {
                let mut kept = group.clone();
                if let Some(map) = kept.as_object_mut() {
                    map.insert("blocked_by".into(), json!(who));
                }
                still.push(kept);
            }
        }
        if freed.is_empty() {
            continue;
        }
        let rkey = released_key(scope, &user, &session);
        let mut list: Vec<Value> = store.eph_get(&rkey).and_then(|v| v.as_array().cloned()).unwrap_or_default();
        for group in &freed {
            crate::briefstat::record_saving(
                store, scope, "collision_prevented", &user, "plan_work", &session, PREVENTED_MESSAGES,
                &json!({"group": group.get("n").cloned().unwrap_or(Value::Null), "paths": group.get("files").cloned().unwrap_or(json!([])),
                        "freed_by": freed_by, "plan": text(group, "plan")}),
                stamp,
            );
        }
        list.extend(freed);
        let _ = store.eph_set(&rkey, &json!(list), Some(RELEASED_TTL_S));
        if still.is_empty() {
            store.eph_delete(&key);
        } else {
            let mut updated = record.clone();
            if let Some(map) = updated.as_object_mut() {
                map.insert("groups".into(), json!(still));
            }
            let _ = store.eph_set(&key, &updated, Some(HELD_TTL_S));
        }
    }
}

/// The planner's released groups, handed over once and cleared.
pub fn take_released(store: &Store, scope: &str, user: &str, session: &str) -> Vec<Value> {
    let key = released_key(scope, user, session);
    let list: Vec<Value> = store.eph_get(&key).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    if !list.is_empty() {
        store.eph_delete(&key);
    }
    list
}

/// The lines a planner's next step carries for its released groups.
pub fn release_lines(store: &Store, scope: &str, user: &str, session: &str) -> Vec<String> {
    let users: BTreeSet<String> = take_released(store, scope, user, session)
        .into_iter()
        .map(|g| {
            let files = strings(&g, "files");
            let freed = text(&g, "freed_by");
            let who = freed.split('#').next().unwrap_or("").to_string();
            (g, files, who)
        })
        .map(|(g, files, who)| format!(
            "Collide traffic: group {} ({}) can run now, {} finished. Start it, or plan_work(dispatch=true) again to claim it.",
            g.get("n").and_then(Value::as_u64).unwrap_or(0), files.join(", "), if who.is_empty() { "the agent in its way".to_string() } else { who }))
        .collect();
    let _ = BTreeMap::<String, String>::new();
    users.into_iter().collect()
}
