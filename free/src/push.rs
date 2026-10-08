//! Messages pushed to an agent the moment they are sent, instead of waiting
//! for its next hook call. `POST /inbox/wait` holds the caller until a
//! message is there for it (or a short while passes), and hands each
//! message to exactly one taker:
//!
//! - a **session** listener (`collide listen`, one per open Claude Code
//!   session) takes a message at once when it was sent in the session's
//!   own repo, and from another repo of the workspace after a short grace,
//!   so an open session in the right repo gets first pick;
//! - the machine's **wake** watcher (`collide wake`) takes what no open
//!   session took after a longer grace, and only for a repo the machine has
//!   a checkout of and no agent of the person is working in right now: it
//!   starts an agent there to weigh the message.
//!
//! A message the hooks or the MCP tools already showed whole (the
//! `mcpmsg:` mark) is not pushed again, and a pushed one is marked shown for
//! the session it went to, so the hooks do not repeat it either.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::presence::split_scope;
use crate::store::{now, Store};

/// How long an open session in the message's own repo has to take it
/// before a session in another repo of the workspace may.
pub const OTHER_REPO_GRACE_S: f64 = 20.0;
/// How long open sessions have before the wake watcher may start an agent.
pub const WAKE_GRACE_S: f64 = 45.0;
/// Older messages are left to the inbox: a watcher switched on today does
/// not start agents for last month's.
pub const MAX_AGE_S: f64 = 24.0 * 3600.0;
/// One taker per message, for as long as a message can be taken.
const CLAIM_TTL_S: f64 = MAX_AGE_S + 3600.0;
/// The longest one wait holds the caller.
pub const MAX_WAIT_S: f64 = 25.0;

/// Wakes every waiting `/inbox/wait` when a message is filed.
pub fn arrivals() -> &'static tokio::sync::Notify {
    static ARRIVED: OnceLock<tokio::sync::Notify> = OnceLock::new();
    ARRIVED.get_or_init(tokio::sync::Notify::new)
}

/// Called wherever a message is filed into an inbox.
pub fn arrived() {
    arrivals().notify_waiters();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Taker {
    Session,
    Wake,
}

impl Taker {
    pub fn parse(role: &str) -> Taker {
        if role == "wake" { Taker::Wake } else { Taker::Session }
    }
}

pub struct Ask<'a> {
    pub taker: Taker,
    pub workspace: &'a str,
    pub user_id: &'a str,
    /// the caller's own repo scope; for the wake watcher, none in particular
    pub scope: &'a str,
    pub session: &'a str,
    /// the repo scopes the caller may read, `access::visible_scopes`
    pub visible: &'a BTreeSet<String>,
    /// the wake watcher's checkouts: scope -> the repo id as the machine spells it
    pub checkouts: &'a [(String, String)],
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Some agent of this person is working in the repo right now (a hook or a
/// tool call in the last three minutes): the hooks will hand it the message.
fn someone_live(store: &Store, workspace: &str, user_id: &str, scope: &str) -> bool {
    let repo = split_scope(scope).1;
    store
        .eph_scan(&format!("focus:{workspace}:{user_id}#"))
        .iter()
        .any(|(_, marker)| text(marker, "repo_id") == repo)
}

/// The messages this caller takes now, each claimed so no one else does.
/// `filter` applies the plan's messaging rule to the candidates.
pub fn take(store: &Store, ask: &Ask, filter: impl Fn(Vec<Value>) -> Vec<Value>) -> Vec<Value> {
    let stamp = now();
    let mut scopes: Vec<&str> = Vec::new();
    if !ask.scope.is_empty() {
        scopes.push(ask.scope);
    }
    scopes.extend(ask.visible.iter().map(String::as_str).filter(|s| *s != ask.scope));

    let mut candidates: Vec<Value> = Vec::new();
    for scope in &scopes {
        for (key, mut record) in store.kv_list("inbox", &format!("{scope}:{}:", ask.user_id)) {
            let id = key.rsplit_once(':').map(|(_, id)| id).unwrap_or("").to_string();
            if let Some(map) = record.as_object_mut() {
                map.insert("id".into(), json!(id));
                map.insert("_scope".into(), json!(scope));
            }
            candidates.push(record);
        }
    }
    let mut taken: Vec<Value> = Vec::new();
    for mut message in filter(candidates) {
        let id = text(&message, "id");
        let scope = text(&message, "_scope");
        let age = stamp - message.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        if id.is_empty() || age > MAX_AGE_S {
            continue;
        }
        let claim = format!("pushed:{}:{id}", ask.user_id);
        if store.eph_get(&claim).is_some() {
            continue;
        }
        // shown whole by a hook or a tool reply: that agent has it
        if store.eph_get(&format!("mcpmsg:{}:{id}", ask.user_id)).is_some() {
            let _ = store.eph_set(&claim, &json!({"ts": stamp, "by": "shown"}), Some(CLAIM_TTL_S));
            continue;
        }
        // only a message addressed to this person is worth a turn of its own:
        // a note to everyone at work, one routed by the code it is about and
        // Collide's own notices ride the next hook answer, as before
        let personal = message.get("broadcast").and_then(Value::as_bool) != Some(true)
            && message.get("routed").and_then(Value::as_bool) != Some(true)
            && text(&message, "from") != "collide";
        if !personal {
            continue;
        }
        let mut repo_id = split_scope(&scope).1.to_string();
        match ask.taker {
            Taker::Session => {
                if scope != ask.scope && age < OTHER_REPO_GRACE_S {
                    continue;
                }
            }
            Taker::Wake => {
                if age < WAKE_GRACE_S || someone_live(store, ask.workspace, ask.user_id, &scope) {
                    continue;
                }
                match ask.checkouts.iter().find(|(s, _)| *s == scope) {
                    Some((_, spelled)) => repo_id = spelled.clone(),
                    // no checkout of it here: another machine may have one
                    None => continue,
                }
            }
        }
        let by = if ask.taker == Taker::Wake { "wake" } else { "session" };
        let _ = store.eph_set(&claim, &json!({"ts": stamp, "by": by, "session": ask.session}), Some(CLAIM_TTL_S));
        if ask.taker == Taker::Session && !ask.session.is_empty() {
            // the hooks of that session, and the MCP replies, name it from now on
            let _ = store.eph_set(&format!("briefmsg:{}:{id}", ask.session), &json!({"ts": stamp}),
                Some(crate::access::TOLD_ONCE_TTL_S));
            let _ = store.eph_set(&format!("mcpmsg:{}:{id}", ask.user_id), &json!({"ts": stamp}), Some(3600.0));
        }
        if let Some(map) = message.as_object_mut() {
            map.remove("_scope");
            map.insert("repo_id".into(), json!(repo_id));
            map.insert("age_s".into(), json!(age.max(0.0).round()));
        }
        taken.push(message);
    }
    taken.sort_by(|a, b| {
        let ts = |m: &Value| m.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        ts(a).partial_cmp(&ts(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    taken
}

/// Messages one of this person's sessions sent to this session alone
/// (`collide message --to` naming it): taken off its queue as they are
/// pushed, as the hooks take them when they print them.
pub fn take_for_session(store: &Store, scope: &str, user_id: &str, session: &str) -> Vec<Value> {
    if session.is_empty() || scope.is_empty() {
        return Vec::new();
    }
    let inbox = crate::local::session_inbox(user_id, session);
    let mut taken = Vec::new();
    for (key, mut record) in store.kv_list("inbox", &format!("{scope}:{inbox}:")) {
        // a note to every session rides the hooks; one sent to this session alone comes now
        if record.get("broadcast").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let _ = store.kv_delete("inbox", &key);
        if let Some(map) = record.as_object_mut() {
            map.insert("repo_id".into(), json!(split_scope(scope).1));
            map.insert("own_session".into(), json!(true));
        }
        taken.push(record);
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    fn file(store: &Store, scope: &str, to: &str, id: &str, ago: f64) {
        let record = json!({"id": id, "to": to, "from": "beshoy@x.com", "message": "please look", "ts": now() - ago});
        store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &record, now()).unwrap();
    }

    fn ask<'a>(taker: Taker, scope: &'a str, session: &'a str, visible: &'a BTreeSet<String>, checkouts: &'a [(String, String)]) -> Ask<'a> {
        Ask { taker, workspace: "ws", user_id: "kevin@x.com", scope, session, visible, checkouts }
    }

    #[test]
    fn a_session_in_the_repo_takes_a_message_once_and_the_hooks_then_name_it() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 1.0);
        let visible = BTreeSet::from(["ws:app".to_string()]);
        let got = take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m);
        assert_eq!(got.len(), 1);
        assert_eq!(text(&got[0], "repo_id"), "app");
        assert!(store.eph_get("briefmsg:s1:m1").is_some());
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s2", &visible, &[]), |m| m).is_empty());
    }

    #[test]
    fn another_repo_waits_its_grace_and_the_wake_watcher_waits_longer() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 5.0);
        let visible = BTreeSet::from(["ws:app".to_string(), "ws:web".to_string()]);
        let checkouts = vec![("ws:app".to_string(), "github.com/acme/app".to_string())];
        assert!(take(&store, &ask(Taker::Session, "ws:web", "s1", &visible, &[]), |m| m).is_empty());
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());

        let store = self::store();
        file(&store, "ws:app", "kevin@x.com", "m2", WAKE_GRACE_S + 1.0);
        let got = take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m);
        assert_eq!(text(&got[0], "repo_id"), "github.com/acme/app");
    }

    #[test]
    fn the_wake_watcher_leaves_a_message_to_a_live_agent_a_missing_checkout_or_an_old_one() {
        let store = store();
        let visible = BTreeSet::from(["ws:app".to_string(), "ws:web".to_string()]);
        let checkouts = vec![("ws:app".to_string(), "app".to_string())];
        file(&store, "ws:web", "kevin@x.com", "elsewhere", 100.0);
        file(&store, "ws:app", "kevin@x.com", "stale", MAX_AGE_S + 10.0);
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());

        file(&store, "ws:app", "kevin@x.com", "live", 100.0);
        store.eph_set("focus:ws:kevin@x.com#s9", &json!({"repo_id": "app"}), Some(60.0)).unwrap();
        assert!(take(&store, &ask(Taker::Wake, "", "", &visible, &checkouts), |m| m).is_empty());
    }

    #[test]
    fn a_note_to_everyone_a_routed_one_and_collides_own_ride_the_hooks_instead() {
        let store = store();
        let visible = BTreeSet::from(["ws:app".to_string()]);
        for (id, extra) in [("b", json!({"broadcast": true})), ("r", json!({"routed": true})), ("c", json!({"from": "collide"}))] {
            let mut record = json!({"id": id, "to": "kevin@x.com", "from": "beshoy@x.com", "message": "fyi", "ts": now()});
            for (k, v) in extra.as_object().unwrap() {
                record[k] = v.clone();
            }
            store.kv_put("inbox", &format!("ws:app:kevin@x.com:{id}"), &record, now()).unwrap();
        }
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m).is_empty());
    }

    #[test]
    fn a_message_already_shown_whole_is_not_pushed() {
        let store = store();
        file(&store, "ws:app", "kevin@x.com", "m1", 1.0);
        store.eph_set("mcpmsg:kevin@x.com:m1", &json!({}), Some(60.0)).unwrap();
        let visible = BTreeSet::from(["ws:app".to_string()]);
        assert!(take(&store, &ask(Taker::Session, "ws:app", "s1", &visible, &[]), |m| m).is_empty());
    }
}
