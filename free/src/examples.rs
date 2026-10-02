//! Worked examples: a finished, tested task, kept so the next task like it
//! starts from it.
//!
//! Collide already sees a task end to end without git: the prompt (as a
//! vector, never its words), every write the session makes, and the test
//! runs the hook sees in its shell. When a session's tests pass after its
//! last write, what it added is kept as an example: the functions it added
//! or changed (names and signatures, never code: a native hook never sends
//! code), the house helpers they call, and the files. A later task that
//! means the same kind of work AND points at the same code gets one line:
//! who did it, that its tests passed, what it added and what it used.
//!
//! The guards, each against a way an example goes wrong:
//! * wrong: only a session whose tests passed after its LAST write is kept,
//!   and only when it changed code outside the tests; each later use is
//!   scored by its outcome, and a poorly scoring example stops being shown;
//!   anyone can retire one (`retire`).
//! * stale: the signatures of the helpers it used are kept, and any that
//!   changed, moved or went since means it is not shown; it ages out after
//!   MAX_AGE_S unless it keeps being used.
//! * cost: one example per task, once per session, a few hundred
//!   characters; every showing is a ledger row with its size.
//! * noise: new-code tasks only, a high meaning floor on the task without
//!   its process sentences, and the example's code must overlap what this
//!   task's briefing points at.
//! * clashing: a live teammate in the same code wins; no example then.
//! * leaking: the same visibility as recipes and notes, and the plan's
//!   history window.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::{now, Store};

pub const BUCKET: &str = "example";
const TASK_TTL_S: f64 = 6.0 * 3600.0;
/// How close a task must mean the example's task. Calibrated on the reuse
/// study's three rounds (bge-small, process sentences removed): tasks of the
/// same kind scored 0.80 to 0.95, different kinds up to 0.83, bug reports
/// against feature tasks at most 0.74. The code-overlap check does the rest.
const MATCH_FLOOR: f32 = 0.80;
const MAX_AGE_S: f64 = 30.0 * 86_400.0;
/// Scored uses before the outcome decides; below this share of good ones it
/// is no longer shown.
const MIN_SCORED: u64 = 3;
const MIN_GOOD_SHARE: f64 = 0.25;
const MAX_FILES: usize = 3;
const MAX_ADDED: usize = 4;
const MAX_USES: usize = 5;

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn task_key(scope: &str, session: &str) -> String {
    format!("extask:{scope}:{session}")
}

fn shown_key(scope: &str, session: &str) -> String {
    format!("exshown:{scope}:{session}")
}

/// The task a prompt asks for, without its process sentences ("commit and
/// push", "if the push is rejected, pull with rebase"): they say how to
/// hand the work in, and every prompt from the same person carries them.
pub fn task_text(prompt: &str) -> String {
    const PROCESS: [&str; 8] = ["commit", "push", "rebase", "pull request", "merge", "git ", "rerun the tests", "when it is done"];
    prompt
        .split_inclusive(['.', '!', '?', '\n'])
        .filter(|s| {
            let lower = s.to_lowercase();
            !PROCESS.iter().any(|p| lower.contains(p))
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn is_test(path: &str) -> bool {
    crate::brief::is_test_path(path)
}

/// A new task for `session`: whatever it was doing before is over.
pub fn start_task(store: &Store, scope: &str, user: &str, session: &str, q: &[f32], adds_code: bool) {
    if session.is_empty() {
        return;
    }
    let _ = store.eph_set(
        &task_key(scope, session),
        &json!({"user": user, "q": crate::embed::encode_vec(q), "adds_code": adds_code, "started": now(),
                "before": {}, "wrote": [], "last_write": 0.0}),
        Some(TASK_TTL_S),
    );
}

/// A write is about to reach the map: remember the file's symbols as they
/// were before this task first touched it.
pub fn note_write(store: &Store, scope: &str, session: &str, path: &str) {
    let key = task_key(scope, session);
    let Some(mut task) = store.eph_get(&key) else { return };
    let Some(map) = task.as_object_mut() else { return };
    let before = map.entry("before").or_insert_with(|| json!({}));
    if before.get(path).is_none() {
        let hashes: BTreeMap<String, Value> = crate::codegraph::symbols_of(store, scope, path)
            .into_iter()
            .map(|(name, s)| (name, s.get("hash").cloned().unwrap_or(json!(""))))
            .collect();
        if let Some(b) = before.as_object_mut() {
            b.insert(path.to_string(), json!(hashes));
        }
    }
    let wrote = map.entry("wrote").or_insert_with(|| json!([]));
    if let Some(list) = wrote.as_array_mut() {
        if !list.iter().any(|p| p.as_str() == Some(path)) {
            list.push(json!(path));
        }
    }
    map.insert("last_write".into(), json!(now()));
    let _ = store.eph_set(&key, &task, Some(TASK_TTL_S));
}

/// The hook saw this session run the repo's tests. A pass after its last
/// write makes its task an example, and settles an example it was shown.
pub fn note_tests(store: &Store, scope: &str, user: &str, session: &str, ok: bool, command: &str) -> Option<String> {
    let stamp = now();
    let _ = store.eph_set(&format!("extests:{scope}:{session}"), &json!({"ok": ok, "ts": stamp, "command": command}), Some(TASK_TTL_S));
    let task = store.eph_get(&task_key(scope, session))?;
    let last_write = task.get("last_write").and_then(Value::as_f64).unwrap_or(0.0);
    if last_write <= 0.0 {
        return None;
    }
    if ok {
        settle(store, scope, session, true);
    }
    if !ok || !task.get("adds_code").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    save(store, scope, user, session, &task, command, stamp)
}

/// The session ended: an example it was shown and never got to passing
/// tests with counts against that example.
pub fn session_ended(store: &Store, scope: &str, session: &str) {
    let last = store.eph_get(&format!("extests:{scope}:{session}"));
    if last.as_ref().and_then(|l| l.get("ok")).and_then(Value::as_bool) == Some(false) {
        settle(store, scope, session, false);
    }
}

fn settle(store: &Store, scope: &str, session: &str, good: bool) {
    let key = shown_key(scope, session);
    let Some(mut shown) = store.eph_get(&key) else { return };
    if shown.get("settled").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }
    let (owner, id) = (text(&shown, "owner"), text(&shown, "id"));
    let ex_key = format!("{owner}:{id}");
    if let Some(mut example) = store.kv_get(BUCKET, &ex_key) {
        let field = if good { "good" } else { "bad" };
        let n = example.get(field).and_then(Value::as_u64).unwrap_or(0) + 1;
        example[field] = json!(n);
        let stamp = now();
        example["last_used"] = json!(stamp);
        let _ = store.kv_put(BUCKET, &ex_key, &example, stamp);
        let _ = store.ledger_append_later(scope, "example_outcome", &json!({"example": id, "session": session, "good": good}), stamp);
    }
    shown["settled"] = json!(true);
    let _ = store.eph_set(&key, &shown, Some(TASK_TTL_S));
}

fn save(store: &Store, scope: &str, user: &str, session: &str, task: &Value, command: &str, stamp: f64) -> Option<String> {
    let before = task.get("before").cloned().unwrap_or(json!({}));
    let wrote: Vec<String> = task.get("wrote").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let graph = crate::graphview::snapshot(store, scope);
    let mut files: Vec<Value> = Vec::new();
    let mut uses: BTreeMap<String, String> = BTreeMap::new();
    for path in wrote.iter().filter(|p| !is_test(p)) {
        let prior = before.get(path).and_then(Value::as_object).cloned().unwrap_or_default();
        let symbols = crate::codegraph::symbols_of(store, scope, path);
        let mut added: Vec<Value> = Vec::new();
        for (name, symbol) in &symbols {
            let hash = symbol.get("hash").cloned().unwrap_or(json!(""));
            if prior.get(name) == Some(&hash) {
                continue;
            }
            let kind = text(symbol, "kind");
            if !matches!(kind.as_str(), "function" | "method" | "class") {
                continue;
            }
            added.push(json!({"name": name, "signature": text(symbol, "signature"),
                              "doc": text(symbol, "doc").split(". ").next().unwrap_or("").trim_end_matches('.'),
                              "new": !prior.contains_key(name)}));
            // what it calls in other files: the helpers it was built on
            if let Some(targets) = graph.out.get(&format!("{path}::{name}")) {
                for target in targets.keys() {
                    let (t_path, t_name) = crate::graphview::split_node(target);
                    if t_path == path || t_name.is_empty() || !crate::graphview::internal(target) || is_test(t_path) {
                        continue;
                    }
                    let sig = crate::codegraph::symbols_of(store, scope, t_path)
                        .get(t_name).map(|s| text(s, "signature")).unwrap_or_default();
                    uses.insert(target.clone(), sig);
                }
            }
        }
        if !added.is_empty() {
            added.truncate(MAX_ADDED);
            files.push(json!({"path": path, "new": prior.is_empty(), "added": added}));
        }
    }
    if files.is_empty() {
        return None; // tests alone, or config: nothing a later task can build on
    }
    files.truncate(MAX_FILES);
    let started = task.get("started").and_then(Value::as_f64).unwrap_or(0.0);
    let mut hasher = Sha256::new();
    hasher.update(format!("{scope}\n{session}\n{started}").as_bytes());
    let id = format!("E-{}", &format!("{:x}", hasher.finalize())[..8]);
    let key = format!("{scope}:{id}");
    let prior = store.kv_get(BUCKET, &key);
    let uses: Vec<Value> = uses.into_iter().take(MAX_USES).map(|(node, sig)| json!({"node": node, "signature": sig})).collect();
    let record = json!({
        "id": id, "user": user, "by": user, "session": session, "q": text(task, "q"),
        "created": prior.as_ref().and_then(|p| p.get("created")).cloned().unwrap_or(json!(stamp)),
        "verified_at": stamp, "tests": command, "files": files, "uses": uses,
        "shown": prior.as_ref().and_then(|p| p.get("shown")).cloned().unwrap_or(json!(0)),
        "good": prior.as_ref().and_then(|p| p.get("good")).cloned().unwrap_or(json!(0)),
        "bad": prior.as_ref().and_then(|p| p.get("bad")).cloned().unwrap_or(json!(0)),
        "retired": false,
    });
    let _ = store.kv_put(BUCKET, &key, &record, stamp);
    if prior.is_none() {
        let _ = store.ledger_append_later(scope, "example_saved", &json!({"example": id, "user": user, "session": session,
            "files": record["files"].as_array().map(Vec::len).unwrap_or(0)}), stamp);
    }
    Some(id)
}

/// Retire an example: it is never shown again.
pub fn retire(store: &Store, scope: &str, id: &str) -> bool {
    let key = format!("{scope}:{id}");
    let Some(mut example) = store.kv_get(BUCKET, &key) else { return false };
    example["retired"] = json!(true);
    store.kv_put(BUCKET, &key, &example, now()).is_ok()
}

pub struct Offer<'a> {
    pub scope: &'a str,
    pub readable: &'a BTreeSet<String>,
    pub user: &'a str,
    pub session: &'a str,
    pub q: &'a [f32],
    /// what this task's briefing points at: files, and path::symbol nodes
    pub points_at: &'a BTreeSet<String>,
    pub history_s: Option<f64>,
    pub share_knowledge: bool,
}

/// The one example for this task, and the line that shows it; None when no
/// example passes every guard.
pub fn offer(store: &Store, o: &Offer) -> Option<(String, Value)> {
    if o.q.is_empty() || o.session.is_empty() || store.eph_get(&shown_key(o.scope, o.session)).is_some() {
        return None;
    }
    let stamp = now();
    let oldest = stamp - o.history_s.unwrap_or(MAX_AGE_S).min(MAX_AGE_S);
    let workspace = o.scope.split_once(':').map(|(ws, _)| ws).unwrap_or(o.scope);
    let points_at_file = |p: &str| o.points_at.contains(p);
    let mut best: Option<(f32, String, Value)> = None;
    for (key, example) in store.kv_list(BUCKET, &format!("{workspace}:")) {
        let owner = key.rsplit_once(':').map(|(owner, _)| owner).unwrap_or("").to_string();
        if !o.readable.contains(&owner) || example.get("retired").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        if text(&example, "session") == o.session {
            continue;
        }
        if !o.share_knowledge && !crate::access::own_knowledge(&example, o.user) {
            continue;
        }
        let fresh = example.get("last_used").or_else(|| example.get("verified_at")).and_then(Value::as_f64).unwrap_or(0.0);
        if fresh < oldest {
            continue;
        }
        let good = example.get("good").and_then(Value::as_u64).unwrap_or(0);
        let bad = example.get("bad").and_then(Value::as_u64).unwrap_or(0);
        if good + bad >= MIN_SCORED && (good as f64) / ((good + bad) as f64) < MIN_GOOD_SHARE {
            continue;
        }
        let score = crate::embed::cosine(o.q, &crate::embed::decode_vec(&text(&example, "q")));
        if score < MATCH_FLOOR || best.as_ref().is_some_and(|(b, _, _)| *b >= score) {
            continue;
        }
        // the same code: a helper it used, or a file it wrote, is what this
        // task's briefing points at
        let uses: Vec<String> = example.get("uses").and_then(Value::as_array)
            .map(|a| a.iter().map(|u| text(u, "node")).collect()).unwrap_or_default();
        let files: Vec<String> = example.get("files").and_then(Value::as_array)
            .map(|a| a.iter().map(|f| text(f, "path")).collect()).unwrap_or_default();
        let overlaps = uses.iter().any(|n| o.points_at.contains(n) || points_at_file(crate::graphview::split_node(n).0))
            || files.iter().any(|f| points_at_file(f));
        if !overlaps {
            continue;
        }
        // stale: a helper it was built on changed its signature or went
        let stale = example.get("uses").and_then(Value::as_array).into_iter().flatten().any(|u| {
            let node = text(u, "node");
            let (path, name) = crate::graphview::split_node(&node);
            crate::codegraph::symbols_of(store, &owner, path).get(name).map(|s| text(s, "signature")) != Some(text(u, "signature"))
        });
        if stale {
            continue;
        }
        best = Some((score, key.clone(), example));
    }
    let (score, key, mut example) = best?;
    let who = crate::inflight::author_label(store, &text(&example, "user"), o.user);
    let line = render(&example, &who, stamp);
    let shown = example.get("shown").and_then(Value::as_u64).unwrap_or(0);
    example["shown"] = json!(shown + 1);
    let owner = key.rsplit_once(':').map(|(owner, _)| owner).unwrap_or("").to_string();
    let _ = store.kv_put(BUCKET, &key, &example, stamp);
    let _ = store.eph_set(&shown_key(o.scope, o.session), &json!({"owner": owner, "id": text(&example, "id"), "ts": stamp}), Some(TASK_TTL_S));
    let _ = store.ledger_append_later(o.scope, "example_shown", &json!({"example": text(&example, "id"), "session": o.session,
        "user": o.user, "score": (score * 1000.0).round() / 1000.0, "chars": line.chars().count()}), stamp);
    Some((line, json!({"id": text(&example, "id"), "score": (score * 1000.0).round() / 1000.0})))
}

fn render(example: &Value, who: &str, stamp: f64) -> String {
    let age = crate::deltas::age_text(stamp - example.get("verified_at").and_then(Value::as_f64).unwrap_or(stamp));
    let mut added: Vec<String> = Vec::new();
    for file in example.get("files").and_then(Value::as_array).into_iter().flatten() {
        let path = text(file, "path");
        let names: Vec<String> = file.get("added").and_then(Value::as_array).into_iter().flatten()
            .map(|a| {
                let sig = text(a, "signature");
                let head = if sig.is_empty() { text(a, "name") } else { sig };
                format!("`{head}`")
            })
            .collect();
        added.push(format!("{} in {path}", names.join(", ")));
    }
    let uses: Vec<String> = example.get("uses").and_then(Value::as_array).into_iter().flatten()
        .map(|u| {
            let node = text(u, "node");
            let (path, name) = crate::graphview::split_node(&node);
            format!("`{name}` ({path})")
        })
        .collect();
    let good = example.get("good").and_then(Value::as_u64).unwrap_or(0);
    let track = if good > 0 { format!(", and {good} later task(s) built on it with passing tests") } else { String::new() };
    let mut line = format!("A task like yours was done {age} by {who}; its tests passed{track}. It added {}", added.join("; "));
    if !uses.is_empty() {
        line.push_str(&format!(", built on {}", uses.join(", ")));
    }
    line.push_str(". Build yours the same way, adapted to your task (read their file if you want the code); it is a reference, not a command.");
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_is_the_prompt_without_its_process_sentences() {
        let p = "accounting wants a monthly revenue csv. add `f()` in a/b.py. When it is done and the tests pass, commit and push to origin main; if the push is rejected, pull with rebase, keep everyone's work, rerun the tests and push again.";
        assert_eq!(task_text(p), "accounting wants a monthly revenue csv. add `f()` in a/b.py.");
    }
}
