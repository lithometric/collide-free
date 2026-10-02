//! The live view: who is here, what they are touching, what they have claimed.
//!
//! Everything else in this server answers a question about the code. This
//! answers a question about the people and agents working on it right now,
//! which is what the dashboard's Now panel shows and what an arriving agent
//! reads to know whether it is alone.
//!
//! The room is the WORKSPACE and the row is the AGENT. Presence markers are
//! keyed `focus:{workspace}:{user}#{session}` with the repo in the record, so
//! one read of the workspace prefix lists every agent of every repo — then
//! every record is put through the caller's per-repo allowlist (the
//! `visible` set, a non-optional argument) before it can become a row. A
//! person with two live sessions is two rows; the dashboard groups them.
//!
//! Only ephemeral and shared reads. Nothing here writes, and nothing here is
//! allowed to be expensive: it is polled.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::compat::python_round;
use crate::presence::{agent_id, split_scope};
use crate::store::{now, Store};

/// A workspace is considered online if it has been seen within three minutes.
/// Long enough to survive a slow turn, short enough that a dead agent stops
/// claiming to be present.
const ONLINE_WINDOW_S: f64 = 180.0;
// The protocol block's version, which the setup state is compared against so
// a stale block reports itself rather than silently going out of date — the
// one constant envelope.rs owns, never a second copy that can lag it.
use crate::envelope::BLOCK_VERSION;
const RECENT_EVENTS: usize = 50;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn ts_of(value: &Value) -> f64 {
    value.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
}

/// The first twelve characters of a merkle root — enough to compare two
/// workspaces by eye, short enough to sit in a table cell.
fn short(root: &str) -> String {
    root.chars().take(12).collect()
}

/// `{kind, path, note, age_s}` for the Now panel's status line.
fn last_action_view(record: Option<&Value>, stamp: f64) -> Value {
    let Some(record) = record else { return Value::Null };
    json!({
        "kind": text(record, "kind"),
        "path": text(record, "path"),
        "note": text(record, "note"),
        "ts": ts_of(record),
        "age_s": python_round(stamp - ts_of(record), 1),
        "via": record.get("via").cloned().unwrap_or(Value::Null),
    })
}

/// The newest record under a prefix, by timestamp. Never used for `focus:` —
/// that prefix holds one record PER AGENT, and picking one would fold the
/// agents back into a single row.
fn newest(store: &Store, prefix: &str) -> Option<Value> {
    store
        .eph_scan(prefix)
        .into_iter()
        .map(|(_key, value)| value)
        .max_by(|a, b| ts_of(a).partial_cmp(&ts_of(b)).unwrap_or(std::cmp::Ordering::Equal))
}

/// The newest edit a row may claim. `lastedit:` is keyed by person, so a
/// session row takes only the records its own session wrote (the record says
/// which); a person-level row — no session — takes the person's newest,
/// whoever's session wrote it.
fn own_edit(store: &Store, scope: &str, user: &str, session: &str) -> Option<Value> {
    store
        .eph_scan(&format!("lastedit:{scope}:{user}:"))
        .into_iter()
        .map(|(_key, value)| value)
        .filter(|record| session.is_empty() || text(record, "session") == session)
        .max_by(|a, b| ts_of(a).partial_cmp(&ts_of(b)).unwrap_or(std::cmp::Ordering::Equal))
}

/// What a person is part-way through writing in one repo. A draft is
/// presence, not state: it says symbols are in motion without claiming they
/// landed. Drafts are keyed by person, so every agent row of that person in
/// that repo carries the same one.
fn editing_view(store: &Store, scope: &str, user: &str, stamp: f64) -> Value {
    let Some(latest) = newest(store, &format!("draft:{scope}:{user}:")) else {
        return Value::Null;
    };
    json!({
        "path": text(&latest, "path"),
        "symbols": latest.get("symbols_changed").cloned().unwrap_or(json!([])),
        "parse": text(&latest, "parse"),
        "ts": ts_of(&latest),
        "age_s": python_round(stamp - ts_of(&latest), 1),
        "hunks": latest.get("hunks").cloned().unwrap_or(json!([])),
        "lines_added": latest.get("lines_added").cloned().unwrap_or(json!(0)),
        "lines_removed": latest.get("lines_removed").cloned().unwrap_or(json!(0)),
    })
}

fn focus_view(marker: Option<&Value>, stamp: f64) -> Value {
    let Some(marker) = marker else { return Value::Null };
    // an agent with a focus marker and no action is reading: that is the
    // default a marker means, not a missing value
    let action = match text(marker, "action") {
        empty if empty.is_empty() => "reading".to_string(),
        named => named,
    };
    json!({
        "path": text(marker, "path"),
        "action": action,
        "branch": text(marker, "branch"),
        "ts": ts_of(marker),
        "age_s": python_round(stamp - ts_of(marker), 1),
    })
}

/// The per-agent markers, gathered under one identity.
#[derive(Default)]
struct Agent {
    user: String,
    session: String,
    focus: Option<Value>,
    last_action: Option<Value>,
    tokens: Option<Value>,
    /// live subagents of this session: worker id -> type
    workers: BTreeMap<String, String>,
}

impl Agent {
    /// The repo this agent is in: the freshest marker's, since a session
    /// that `cd`s to another repo moves its one marker rather than leaving
    /// two behind.
    fn repo_id(&self) -> String {
        [&self.focus, &self.last_action, &self.tokens]
            .into_iter()
            .flatten()
            .map(|record| text(record, "repo_id"))
            .find(|repo| !repo.is_empty())
            .unwrap_or_default()
    }
}

/// Everything one row is built from.
struct RowInput<'a> {
    user: &'a str,
    session: &'a str,
    repo_id: &'a str,
    tree: Option<&'a Value>,
    /// This row's own newest reported edit in this repo (`own_edit`).
    current: Option<&'a Value>,
    focus: Option<&'a Value>,
    last_action: Option<&'a Value>,
    tokens: Option<&'a Value>,
    editing: Value,
    task: String,
    /// live subagents of this session, worker id -> type ("" when none named)
    workers: &'a BTreeMap<String, String>,
}

fn row(input: &RowInput, stamp: f64) -> Value {
    // one rule for both kinds of row: the newest record that knows a field
    // answers for it. Candidates are the row's own markers and its own newest
    // edit; only a person-level (sessionless) row may also claim the
    // checkout's timestamp — a tree is the person's, not one agent's.
    let sessionless = input.session.is_empty();
    let current = input.current;
    let mut candidates: Vec<&Value> = [input.focus, input.last_action, current].into_iter().flatten().collect();
    candidates.sort_by(|a, b| ts_of(b).partial_cmp(&ts_of(a)).unwrap_or(std::cmp::Ordering::Equal));
    let newest_with = |key: &str| -> String {
        candidates.iter().map(|record| text(record, key)).find(|value| !value.is_empty()).unwrap_or_default()
    };

    let tree_updated = if sessionless {
        input.tree.and_then(|tree| tree.get("updated").and_then(Value::as_f64)).unwrap_or(0.0)
    } else {
        0.0
    };
    // the freshest of: checkout, reported edit, presence. Reporting "16
    // minutes ago" while an agent is actively reading was a lie the tree
    // timestamp alone could not avoid.
    let last_seen = candidates.iter().map(|record| ts_of(record)).fold(tree_updated, f64::max);
    let idle_s = python_round(stamp - last_seen, 1);
    let paused = input.focus.map(|m| text(m, "action")) == Some("limit".into())
        || input.last_action.map(|a| text(a, "kind")) == Some("limit".into());
    // the hook's SessionEnd: the row keeps its spend but is not online and
    // not an agent that is here
    let ended = input.last_action.map(|a| text(a, "kind")) == Some("ended".into());
    let agent = newest_with("agent");

    // a watcher-reported edit was seen on disk; anything else is the agent's
    // own claim, and the two are different kinds of fact
    let source = match current {
        Some(edit) if text(edit, "agent").starts_with("collide-watch") => "disk",
        Some(_) => "claim",
        None if !candidates.is_empty() || !input.editing.is_null() || input.tokens.is_some() => "claim",
        None => "",
    };
    let current_path = current
        .map(|edit| edit.get("path").cloned().unwrap_or(Value::Null))
        .or_else(|| input.focus.map(|m| m.get("path").cloned().unwrap_or(Value::Null)))
        .or_else(|| input.last_action.map(|a| a.get("path").cloned().unwrap_or(Value::Null)))
        .or_else(|| input.editing.get("path").cloned())
        .unwrap_or(Value::Null);
    let root = input.tree.map(|tree| text(tree, "root")).unwrap_or_default();
    let mut worker_types: Vec<&String> = input.workers.values().filter(|t| !t.is_empty()).collect();
    worker_types.sort();
    worker_types.dedup();
    let updated = input
        .tree
        .and_then(|tree| tree.get("updated").cloned())
        .unwrap_or_else(|| json!(last_seen));

    json!({
        "user": input.user,
        "session": input.session,
        "agent_id": agent_id(input.user, input.session),
        "repo_id": input.repo_id,
        "root": root,
        "root_short": short(&root),
        "updated": updated,
        "last_activity_s": idle_s,
        "current_path": current_path,
        "agent": agent,
        "model": newest_with("model"),
        "branch": newest_with("branch"),
        "task": input.task,
        "tokens_total": input.tokens.and_then(|t| t.get("total").and_then(Value::as_i64)).unwrap_or(0),
        "last_turn": input.tokens.map(|t| text(t, "last_turn")).unwrap_or_default(),
        "online": idle_s < ONLINE_WINDOW_S && !paused && !ended,
        "paused": paused,
        "ended": ended,
        "source": source,
        "editing": input.editing,
        "focus": focus_view(input.focus, stamp),
        "last_action": last_action_view(input.last_action, stamp),
        // the hands this one conversation has on the repo right now: its
        // subagents, each a worker, counted while their markers live
        "workers": input.workers.len(),
        "worker_types": worker_types,
    })
}

/// The dashboard's live feed, and the resource an MCP client subscribes to.
///
/// `visible` is the caller's allowlist from `access::visible_scopes` — the
/// scopes of this workspace whose agents the caller may see. The queried
/// scope itself is always included: whoever calls this was authorised for it
/// before the call, and a repo with no tree yet (an agent that has only read
/// so far) is in no scope table to be listed from.
pub fn list_activity(
    store: &Store, scope: &str, repo_id: &str, idle_after_s: f64, visible: &BTreeSet<String>,
) -> Value {
    let stamp = now();
    let (workspace, this_repo) = split_scope(scope);
    let mut scopes: BTreeSet<String> = visible.clone();
    scopes.insert(scope.to_string());

    // intents stay per repo (the unit of code), so a task only ever
    // decorates rows in the queried repo. An intent that names its session
    // decorates that agent alone; one that does not decorates every agent
    // of its owner, as before.
    let intents = crate::collisions::active_intents(store, scope, idle_after_s);
    let task_for = |user: &str, session: &str| -> String {
        intents
            .iter()
            .find(|intent| {
                let owner = text(intent, "owner");
                let theirs = text(intent, "session");
                !owner.is_empty() && owner == user && (theirs.is_empty() || session.is_empty() || theirs == session)
            })
            .map(|intent| text(intent, "summary"))
            .unwrap_or_default()
    };

    // 1. the agents: one workspace-wide read per bucket, the repo taken from
    // the RECORD and checked against the allowlist. A record without
    // `repo_id` predates per-agent presence and cannot be placed, so it is
    // not shown; it ages out on its own TTL.
    let mut agents: BTreeMap<String, Agent> = BTreeMap::new();
    for bucket in ["focus", "lastact", "agenttokens", "worker"] {
        for (_key, record) in store.eph_scan(&format!("{bucket}:{workspace}:")) {
            let Some(repo) = record.get("repo_id").and_then(Value::as_str) else { continue };
            if !scopes.contains(&format!("{workspace}:{repo}")) {
                continue;
            }
            let user = text(&record, "user");
            if user.is_empty() {
                continue;
            }
            let session = text(&record, "session");
            let agent = agents.entry(agent_id(&user, &session)).or_insert_with(|| Agent {
                user: user.clone(), session: session.clone(), ..Agent::default()
            });
            match bucket {
                "focus" => agent.focus = Some(record),
                "lastact" => agent.last_action = Some(record),
                "worker" => {
                    let worker = text(&record, "worker");
                    if !worker.is_empty() {
                        agent.workers.insert(worker, text(&record, "type"));
                    }
                }
                _ => agent.tokens = Some(record),
            }
        }
    }

    // 2. the checkouts: who has a tree in which visible repo
    let mut trees: BTreeMap<(String, String), Value> = BTreeMap::new();
    for visible_scope in &scopes {
        for tree in store.list_workspaces(visible_scope) {
            trees.insert((visible_scope.clone(), text(&tree, "user")), tree);
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    // (scope, user) pairs already represented by an agent row
    let mut seated: BTreeSet<(String, String)> = BTreeSet::new();

    // 3. one row per agent. A worker marker alone (its session's own markers
    // not written yet, or aged out) places nobody: the repo comes from the
    // session's markers, as before
    for agent in agents.values() {
        let repo = agent.repo_id();
        if repo.is_empty() {
            continue;
        }
        let agent_scope = format!("{workspace}:{repo}");
        let seat = (agent_scope.clone(), agent.user.clone());
        let current = own_edit(store, &agent_scope, &agent.user, &agent.session);
        let task = if repo == this_repo { task_for(&agent.user, &agent.session) } else { String::new() };
        rows.push(row(&RowInput {
            user: &agent.user,
            session: &agent.session,
            repo_id: &repo,
            tree: trees.get(&seat),
            current: current.as_ref(),
            focus: agent.focus.as_ref(),
            last_action: agent.last_action.as_ref(),
            tokens: agent.tokens.as_ref(),
            editing: editing_view(store, &agent_scope, &agent.user, stamp),
            task,
            workers: &agent.workers,
        }, stamp));
        seated.insert(seat);
    }
    let no_workers: BTreeMap<String, String> = BTreeMap::new();

    // 4. a checkout with no live agent in that repo is still a person in the
    // room: the person-level row this endpoint always showed
    for ((tree_scope, user), tree) in &trees {
        if seated.contains(&(tree_scope.clone(), user.clone())) {
            continue;
        }
        let repo = split_scope(tree_scope).1;
        let current = own_edit(store, tree_scope, user, "");
        let task = if repo == this_repo { task_for(user, "") } else { String::new() };
        rows.push(row(&RowInput {
            user, session: "", repo_id: repo, tree: Some(tree), current: current.as_ref(),
            focus: None, last_action: None, tokens: None,
            editing: editing_view(store, tree_scope, user, stamp), task, workers: &no_workers,
        }, stamp));
        seated.insert((tree_scope.clone(), user.clone()));
    }

    // 5. someone drafting before their first clean final edit has neither a
    // tree nor, without hooks, a presence marker — the draft alone seats them
    for draft_scope in &scopes {
        for (_key, marker) in store.eph_scan(&format!("draft:{draft_scope}:")) {
            let user = text(&marker, "user");
            if user.is_empty() || seated.contains(&(draft_scope.clone(), user.clone())) {
                continue;
            }
            let repo = split_scope(draft_scope).1;
            let current = own_edit(store, draft_scope, &user, "");
            let task = if repo == this_repo { task_for(&user, "") } else { String::new() };
            rows.push(row(&RowInput {
                user: &user, session: "", repo_id: repo, tree: None, current: current.as_ref(),
                focus: None, last_action: None, tokens: None,
                editing: editing_view(store, draft_scope, &user, stamp), task, workers: &no_workers,
            }, stamp));
            seated.insert((draft_scope.clone(), user.clone()));
        }
    }

    // a stable order both halves agree on, and one the dashboard can group
    // by person without re-sorting
    rows.sort_by(|a, b| {
        (text(a, "user"), text(a, "session"), text(a, "repo_id"))
            .cmp(&(text(b, "user"), text(b, "session"), text(b, "repo_id")))
    });

    let setup = match store.kv_get("setup_state", scope) {
        Some(mut state) => {
            if let Some(map) = state.as_object_mut() {
                map.insert("latest_version".into(), json!(BLOCK_VERSION));
            }
            state
        }
        None => Value::Null,
    };

    let recent = store
        .eph_get(&format!("recent:{scope}"))
        .and_then(|record| record.get("events").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let tail = recent.len().saturating_sub(RECENT_EVENTS);

    let login_users: BTreeSet<String> =
        rows.iter().filter_map(|r| r.get("user").and_then(Value::as_str).map(str::to_string)).collect();
    let logins = crate::sharing::logins(store, crate::presence::split_scope(scope).0, &login_users, stamp);
    const INTENT_FIELDS: [&str; 13] = [
        "intent_id", "owner", "paths", "symbols", "change_type", "before", "after",
        "summary", "created", "idle", "operations", "ref", "ttl_remaining_s",
    ];
    json!({
        "repo_id": repo_id,
        // the PHYSICAL repo of the queried scope — the id rows in this repo
        // carry. After a rename (or under an aliased id) it differs from
        // `repo_id`, and the dashboard needs it to know which rows are "here"
        "scope_repo": this_repo,
        "setup": setup,
        "workspaces": rows,
        "intents": intents.iter().map(|intent| {
            let mut row = serde_json::Map::new();
            for field in INTENT_FIELDS.iter().chain(["agent"].iter()) {
                row.insert(
                    (*field).to_string(),
                    intent.get(*field).cloned().unwrap_or(json!("")),
                );
            }
            Value::Object(row)
        }).collect::<Vec<_>>(),
        "recent_events": recent[tail..].to_vec(),
        // each login's machines in the last two hours; on Free, more than one is flagged
        "logins": logins,
    })
}

// ------------------------------------------------------------- identity

/// Full display identity for agent-facing output.
///
/// Who-did-it should be legible without the reader mapping email addresses in
/// their head, so every actor carries a label rather than a raw address. The
/// label always ends with the username and email even when a nickname exists,
/// because a nickname alone is ambiguous across workspaces — "VINCY" is only
/// meaningful to whoever chose it.
///
/// Nicknames are private to the caller: they are that person's names for
/// other people, not a shared directory, so they are keyed by caller and
/// never leak between them.
pub fn identity_labels(
    store: &Store, caller: &str, users: &BTreeSet<String>,
) -> BTreeMap<String, Value> {
    let nicknames = store.kv_get("nicknames", caller).unwrap_or_else(|| json!({}));
    let mut out = BTreeMap::new();
    for user in users.iter().filter(|user| !user.is_empty()) {
        let username = store
            .kv_get("username_by_email", user)
            .map(|claim| text(&claim, "username").trim().to_string())
            .unwrap_or_default();
        let nick = nicknames
            .get(user)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let who = if !nick.is_empty() {
            nick.clone()
        } else if !username.is_empty() {
            username.clone()
        } else {
            user.split('@').next().unwrap_or(user).to_string()
        };
        let handle = if username.is_empty() { String::new() } else { format!("@{username}") };
        let label = if !nick.is_empty() {
            let inside: Vec<&str> =
                [handle.as_str(), user.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
            if inside.is_empty() { nick.clone() } else { format!("{nick} ({})", inside.join(", ")) }
        } else if !handle.is_empty() {
            format!("{handle} ({user})")
        } else {
            user.clone()
        };
        out.insert(user.clone(), json!({"who": who, "username": username, "label": label}));
    }
    out
}

/// Attach a resolved identity to every actor in an activity view.
///
/// Done here rather than inside `list_activity` because the labels depend on
/// WHO IS ASKING — nicknames are the caller's own — while the activity itself
/// does not.
pub fn label_actors(store: &Store, caller: &str, view: &mut Value) {
    let mut users: BTreeSet<String> = BTreeSet::new();
    let collect = |rows: Option<&Vec<Value>>, keys: &[&str], into: &mut BTreeSet<String>| {
        for row in rows.into_iter().flatten() {
            for key in keys {
                let value = text(row, key);
                if !value.is_empty() {
                    into.insert(value);
                    break;
                }
            }
        }
    };
    collect(view.get("workspaces").and_then(Value::as_array), &["user"], &mut users);
    collect(view.get("intents").and_then(Value::as_array), &["owner"], &mut users);
    collect(
        view.get("recent_events").and_then(Value::as_array), &["user", "owner"], &mut users);

    let labels = identity_labels(store, caller, &users);
    let apply = |rows: Option<&mut Vec<Value>>, keys: &[&str]| {
        for row in rows.into_iter().flatten() {
            let who = keys
                .iter()
                .map(|key| text(row, key))
                .find(|value| !value.is_empty())
                .unwrap_or_default();
            if let (Some(info), Some(map)) = (labels.get(&who), row.as_object_mut()) {
                map.insert("by".into(), info.get("label").cloned().unwrap_or(Value::Null));
            }
        }
    };
    let Some(view) = view.as_object_mut() else { return };
    apply(view.get_mut("workspaces").and_then(Value::as_array_mut), &["user"]);
    apply(view.get_mut("intents").and_then(Value::as_array_mut), &["owner"]);
    apply(view.get_mut("recent_events").and_then(Value::as_array_mut), &["user", "owner"]);
}
