//! Landing: the rebase-test-push loop, done by `collide-hook land` instead of
//! the model. Study 5's agents spent over half their tool calls getting work
//! into origin: push, rejected, pull --rebase, rerun the tests, push, rejected
//! again, each lap a message that replays the whole context. The hook runs
//! that loop as a subprocess; the server's part is small:
//!
//! * one push turn per repo at a time (`acquire` / `release`, a lease), so
//!   concurrent agents queue instead of racing and losing;
//! * the renames that landed, so the hook can adapt the agent's own commits
//!   to a teammate's rename after it rebases onto it (`renames`);
//! * the record: a `landed` ledger row and live event, and the messages the
//!   agent did not send, as a saving.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::store::Store;

pub const LEASE_BUCKET: &str = "landlease";
/// Long enough for a rebase and the repo's tests; a hook that dies holding
/// the turn blocks nobody for longer than this.
pub const LEASE_S: f64 = 180.0;
const RENAME_TTL_S: f64 = 7.0 * 86400.0;
/// A rebase the hook did is at least the rejected push, the pull and the test
/// run the agent would have sent.
const MESSAGES_PER_REBASE: u64 = 3;
/// A file adapted to a landed rename: the read and the edit.
const MESSAGES_PER_ADAPTED_FILE: u64 = 2;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Ask for the repo's push turn. Granted when free, expired, or already held
/// by this agent; otherwise says who has it and for how long at most.
pub fn acquire(store: &Store, scope: &str, caller: &str, holder: &str, now: f64) -> Value {
    match store.kv_lease(LEASE_BUCKET, scope, holder, LEASE_S, now) {
        Ok((true, _, until)) => json!({"ok": true, "held": true, "until": until}),
        Ok((false, current, until)) => {
            let user = current.split('#').next().unwrap_or("").to_string();
            let users: BTreeSet<String> = [user.clone()].into_iter().collect();
            let who = crate::activity::identity_labels(store, caller, &users)
                .get(&user)
                .and_then(|l| l.get("label"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(user);
            json!({"ok": true, "held": false, "holder": who, "wait_s": (until - now).max(0.0)})
        }
        // a store failure never stops a push: land without a turn
        Err(_) => json!({"ok": true, "held": true, "until": now}),
    }
}

/// Renames on the lint registry, old name to new: what the hook may rewrite
/// in the agent's own commits once the rename is on origin (it checks that).
pub fn renames(store: &Store, scope: &str, now: f64) -> Map<String, Value> {
    let registry = store.kv_get("semlint", scope).unwrap_or_else(|| json!({}));
    let mut out = Map::new();
    if let Some(entries) = registry.get("renames").and_then(Value::as_object) {
        for (old, info) in entries {
            let new = text(info, "new");
            let fresh = info.get("ts").and_then(Value::as_f64).unwrap_or(0.0) >= now - RENAME_TTL_S;
            if fresh && !new.is_empty() && new != *old {
                out.insert(old.clone(), json!(new));
            }
        }
    }
    out
}

/// Give the turn back, and when the push landed, record it: a ledger row, a
/// live event for the dashboard, and the messages it saved.
#[allow(clippy::too_many_arguments)]
pub fn release(
    store: &Store, scope: &str, user: &str, session: &str, agent: &str, holder: &str, body: &Value, now: f64, via: &str,
) -> Value {
    let _ = store.kv_release_lease(LEASE_BUCKET, scope, holder);
    let outcome = text(body, "outcome");
    if outcome != "landed" {
        return json!({"ok": true, "released": true});
    }
    let rebases = count(body, "rebases").min(20);
    let adapted: Vec<Value> = body.get("adapted").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().take(20).collect();
    let renamed: Vec<Value> = body.get("renames_applied").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().take(20).collect();
    let row = json!({
        "user": user, "session": session, "agent": agent,
        "commit": text(body, "commit").chars().take(40).collect::<String>(),
        "branch": text(body, "branch").chars().take(120).collect::<String>(),
        "commits": count(body, "commits").min(1000),
        "rebases": rebases,
        "over": count(body, "over").min(1000),
        "authors": body.get("authors").cloned().unwrap_or(json!([])),
        "renames_applied": renamed, "adapted": adapted,
        "tests": text(body, "tests").chars().take(200).collect::<String>(),
        "tests_ok": body.get("tests_ok").cloned().unwrap_or(Value::Null),
        "seconds": body.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
    });
    let _ = store.ledger_append(scope, "landed", &row, now);
    // it is on origin now: no longer in flight for anyone
    crate::inflight::landed(store, scope, user, session);
    // and whoever's copy is behind what just landed hears it on their next step
    crate::freshness::announce_landing(store, scope, user, session);
    crate::events::publish(store, scope, json!({"kind": "landed", "user": user, "agent": agent, "session": session,
        "commit": row["commit"], "over": row["over"], "rebases": rebases, "renames_applied": row["renames_applied"]}), via);
    let messages = rebases * MESSAGES_PER_REBASE + adapted.len() as u64 * MESSAGES_PER_ADAPTED_FILE;
    if messages > 0 {
        crate::briefstat::record_saving(
            store, scope, "land_saved", user, agent, session, messages,
            &json!({"rebases": rebases, "files": adapted.len(), "over": row["over"]}), now,
        );
    }
    json!({"ok": true, "released": true, "recorded": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_turn_at_a_time_and_a_dead_holder_expires() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let a = acquire(&db, "w:r", "a@x", "a@x#1", 1_000.0);
        assert_eq!(a["held"], true);
        let b = acquire(&db, "w:r", "b@x", "b@x#2", 1_001.0);
        assert_eq!(b["held"], false, "{b}");
        assert!(b["wait_s"].as_f64().unwrap() > 170.0);
        // the holder asking again renews, it does not lock itself out
        assert_eq!(acquire(&db, "w:r", "a@x", "a@x#1", 1_002.0)["held"], true);
        // another repo is another queue
        assert_eq!(acquire(&db, "w:other", "b@x", "b@x#2", 1_002.0)["held"], true);
        // released: the next agent gets it
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "conflict"}), 1_003.0, "test");
        assert_eq!(acquire(&db, "w:r", "b@x", "b@x#2", 1_004.0)["held"], true);
        // a holder that died: its turn lapses
        assert_eq!(acquire(&db, "w:r", "c@x", "c@x#3", 1_004.0 + LEASE_S + 1.0)["held"], true);
        // releasing a turn you do not hold takes nothing from its holder
        release(&db, "w:r", "b@x", "2", "hook", "b@x#2", &json!({}), 1_300.0, "test");
        assert_eq!(acquire(&db, "w:r", "b@x", "b@x#2", 1_301.0)["held"], false);
    }

    #[test]
    fn a_landing_is_recorded_with_what_it_saved() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        acquire(&db, "w:r", "a@x", "a@x#1", 1_000.0);
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1",
                &json!({"outcome": "landed", "commit": "abc", "rebases": 2, "over": 3, "adapted": ["t.py"]}), 1_001.0, "test");
        let rows = db.ledger_since("w:r", 0.0);
        let landed = rows.iter().find(|r| r.kind == "landed").expect("a landed row");
        assert_eq!(landed.payload["over"], 3);
        let saved = rows.iter().find(|r| r.kind == "land_saved").expect("a saving");
        // what it had in flight is not in flight any more
        let _ = db.eph_set("inflight:w:r:k:1", &json!({"user": "a@x", "session": "1", "path": "p.py"}), Some(60.0));
        let _ = db.eph_set("inflight:w:r:k:2", &json!({"user": "b@x", "session": "2", "path": "q.py"}), Some(60.0));
        let _ = db.eph_set("inflightmsg:w:r:s9:x", &json!({"text": "t", "author": "a@x", "author_session": "1"}), Some(60.0));
        acquire(&db, "w:r", "a@x", "a@x#1", 1_002.0);
        release(&db, "w:r", "a@x", "1", "hook", "a@x#1", &json!({"outcome": "landed", "commit": "def"}), 1_003.0, "test");
        assert!(db.eph_get("inflight:w:r:k:1").is_none() && db.eph_get("inflightmsg:w:r:s9:x").is_none());
        assert!(db.eph_get("inflight:w:r:k:2").is_some(), "another agent's work is still in flight");
        assert_eq!(saved.payload["messages"], 2 * MESSAGES_PER_REBASE + MESSAGES_PER_ADAPTED_FILE);
    }

    #[test]
    fn renames_come_from_the_lint_registry_while_fresh() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        db.kv_put("semlint", "w:r", &json!({"renames": {
            "amount_due": {"new": "checkout_total", "by": "a@x", "ts": 1_000_000.0},
            "old_one": {"new": "new_one", "by": "a@x", "ts": 1.0},
        }}), 1_000.0).unwrap();
        let got = renames(&db, "w:r", 1_000_100.0);
        assert_eq!(got.get("amount_due"), Some(&json!("checkout_total")));
        assert!(!got.contains_key("old_one"), "a week-old rename has had its chance");
    }
}
