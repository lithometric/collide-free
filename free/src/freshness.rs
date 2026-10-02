//! Whose change is in your copy.
//!
//! Every agent works in its own checkout. When a teammate changes a file,
//! the change is in THEIR copy; it reaches yours only after they push and you
//! pull. Three agents on one repo showed what happens without this: the one
//! fixing revenue edited `dashboard_tiles.py` a minute after a teammate had
//! renamed the function it calls, pushed, and moved on; her copy still had
//! the old name, and she paid for it at the rebase (and, without Collide, the
//! merged code was wrong two runs in three).
//!
//! So each copy is fingerprinted: the content hash of every file an agent
//! reads or writes, and the latest teammate write of each file. A write to a
//! file a teammate changed after your copy was taken is a collision about to
//! happen, and the gate says so once, with what to do: pull first when their
//! change is pushed, or work elsewhere while it is still only in their copy.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::codegraph::path_key;
use crate::store::{now, Store};

/// How long a copy fingerprint and a latest write are kept.
const TTL_S: f64 = 7_200.0;
/// A teammate write older than this is history, not a live collision.
const LIVE_S: f64 = 1_800.0;

/// A copy's fingerprint: a hash over its line hashes, which a hook that
/// parses locally sends instead of the text, and the server computes from
/// the text otherwise — the same fingerprint either way.
pub fn fingerprint(line_hashes: &[String]) -> String {
    let digest = Sha256::digest(line_hashes.join(",").as_bytes());
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

pub fn fingerprint_content(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    fingerprint(&crate::diff::line_hashes(&lines))
}

fn copy_key(scope: &str, user: &str, session: &str, path: &str) -> String {
    format!("copyhash:{scope}:{user}:{session}:{}", path_key(path))
}

fn write_key(scope: &str, path: &str) -> String {
    format!("writehash:{scope}:{}", path_key(path))
}

fn checkout_key(scope: &str, user: &str, session: &str) -> String {
    format!("checkout:{scope}:{user}:{session}")
}

/// Which working copy a session is in (the hooks send it with each call).
pub fn note_checkout(store: &Store, scope: &str, user: &str, session: &str, checkout: &str) {
    if session.is_empty() || checkout.is_empty() {
        return;
    }
    let key = checkout_key(scope, user, session);
    if store.eph_get(&key).and_then(|v| v.as_str().map(str::to_string)).as_deref() != Some(checkout) {
        let _ = store.eph_set(&key, &json!(checkout), Some(TTL_S));
    }
}

fn same_checkout(store: &Store, scope: &str, a: (&str, &str), b: (&str, &str)) -> bool {
    let get = |(u, s): (&str, &str)| store.eph_get(&checkout_key(scope, u, s)).and_then(|v| v.as_str().map(str::to_string));
    matches!((get(a), get(b)), (Some(x), Some(y)) if x == y)
}

/// This session's copy of `path` holds `content` (it read it, or wrote it).
pub fn saw(store: &Store, scope: &str, user: &str, session: &str, path: &str, hash: &str, wrote: bool) {
    if session.is_empty() || hash.is_empty() {
        return;
    }
    let stamp = now();
    let _ = store.eph_set(&copy_key(scope, user, session, path),
        &json!({"h": hash, "ts": stamp, "wrote": wrote, "user": user, "session": session, "path": path}), Some(TTL_S));
    if wrote {
        let _ = store.eph_set(&write_key(scope, path), &json!({"h": hash, "ts": stamp, "user": user, "session": session, "path": path}), Some(TTL_S));
    }
}

/// A teammate's change to `path` that this session's copy does not have.
pub struct Behind {
    pub user: String,
    pub session: String,
    pub age_s: f64,
    /// their change is on origin (they landed after writing it): a pull gets it
    pub pushed: bool,
    pub written_at: f64,
    /// when this session last looked at the file
    pub copied_at: f64,
}

/// Whether this session's copy of `path` is behind a teammate's live write:
/// None when it has their change, when the latest write is its own, when it
/// never looked at the file, or when it wrote the file itself since.
pub fn behind(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Option<Behind> {
    if session.is_empty() {
        return None;
    }
    let write = store.eph_get(&write_key(scope, path))?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (w_user, w_session) = (text(&write, "user"), text(&write, "session"));
    if w_user == user && w_session == session {
        return None;
    }
    // one working copy: their change is already on this disk
    if same_checkout(store, scope, (user, session), (&w_user, &w_session)) {
        return None;
    }
    let written_at = write.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let stamp = now();
    if stamp - written_at > LIVE_S {
        return None;
    }
    let copy = store.eph_get(&copy_key(scope, user, session, path))?;
    if copy.get("h") == write.get("h") {
        return None;
    }
    // its own edit since: the copies have diverged, and landing merges them
    let copied_at = copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    if copy.get("wrote").and_then(Value::as_bool).unwrap_or(false) && copied_at > written_at {
        return None;
    }
    let pushed = store
        .ledger_since_kinds(scope, written_at, &["landed"])
        .iter()
        .any(|row| text(&row.payload, "user") == w_user && text(&row.payload, "session") == w_session);
    Some(Behind { user: w_user, session: w_session, age_s: stamp - written_at, pushed, written_at, copied_at })
}

/// The gate's say on a write to `path`: a block the first time a copy is
/// behind a given teammate write, then out of the way (a retry goes through,
/// so nobody is ever stuck on it).
pub fn gate_note(store: &Store, scope: &str, user: &str, session: &str, path: &str, who: &str) -> Option<String> {
    let behind = behind(store, scope, user, session, path)?;
    // it has looked at the file since their write: that read was told whose
    // change its copy lacks, and a copy read since may well hold theirs plus
    // its own (the block is for an edit made on a copy read before it)
    if behind.copied_at > behind.written_at {
        return None;
    }
    let told = format!("behindtold:{scope}:{user}:{session}:{}:{:.3}", path_key(path), behind.written_at);
    if store.eph_get(&told).is_some() {
        return None;
    }
    let _ = store.eph_set(&told, &json!({"ts": now()}), Some(TTL_S));
    let age = crate::deltas::age_text(behind.age_s);
    Some(if behind.pushed {
        format!(
            "{who} changed {path} {age} and pushed it; your copy does not have it yet. Pull first (git pull --rebase), \
             re-read {path}, then make your edit on top of theirs — editing the old copy is a merge conflict waiting to happen."
        )
    } else {
        format!(
            "{who} is changing {path} in their own checkout ({age}); it is not pushed, so your copy does not have it. \
             Do not redo their change yourself: make your edit fit theirs, then retry the write and it goes through. \
             When you push, Collide rebases onto their change once it lands."
        )
    })
}

/// A teammate's work just landed: every other session whose copy of a file
/// in it is behind hears so on its next step — pull now. Without this the
/// last thing it heard was "not pushed yet", and an agent redid the
/// teammate's rename by hand in its own copy before the gate sent it to pull.
pub fn announce_landing(store: &Store, scope: &str, user: &str, session: &str) -> usize {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let written: Vec<(String, String)> = store
        .eph_scan(&format!("writehash:{scope}:"))
        .into_iter()
        .filter(|(_, w)| text(w, "user") == user && text(w, "session") == session)
        .map(|(_, w)| (text(&w, "path"), text(&w, "h")))
        .filter(|(p, _)| !p.is_empty())
        .collect();
    if written.is_empty() {
        return 0;
    }
    // reader session -> (reader user, files it is behind on)
    let mut behind_by: std::collections::BTreeMap<String, (String, Vec<String>)> = std::collections::BTreeMap::new();
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        let (r_user, r_session, r_path) = (text(&copy, "user"), text(&copy, "session"), text(&copy, "path"));
        if (r_user == user && r_session == session) || r_session.is_empty() {
            continue;
        }
        let Some((_, h)) = written.iter().find(|(p, _)| *p == r_path) else { continue };
        if text(&copy, "h") == *h || same_checkout(store, scope, (&r_user, &r_session), (user, session)) {
            continue;
        }
        let entry = behind_by.entry(r_session).or_insert_with(|| (r_user.clone(), Vec::new()));
        if !entry.1.contains(&r_path) {
            entry.1.push(r_path);
        }
    }
    let told = behind_by.len();
    for (r_session, (r_user, paths)) in behind_by {
        let who = if r_user == user { "Another session of yours".to_string() } else { crate::inflight::author_label(store, &r_user, user) };
        let files = paths.join(", ");
        crate::inflight::queue_notice(
            store, scope, &r_session, &format!("landed|{user}|{session}|{files}"),
            &format!("Collide: {who} just pushed their change to {files}; your copy is behind it. Before your next edit to \
those files, pull (git pull --rebase) and re-read them; until then, carry on. Do not redo their change yourself: it is on origin."),
            user, session, &paths,
        );
    }
    told
}

/// A session ended: it is no longer working anywhere, however recently it
/// read or wrote (the overlap notice told people to split work with an
/// agent that had already gone).
pub fn note_ended(store: &Store, scope: &str, session: &str) {
    if !session.is_empty() {
        let _ = store.eph_set(&format!("sessended:{scope}:{session}"), &json!({"ts": now()}), Some(TTL_S));
    }
}

/// Other live sessions that have read or written any of `paths` in the last
/// `within_s`: (user, session, the files among `paths` they are in).
pub fn working_in(store: &Store, scope: &str, user: &str, session: &str, paths: &[String], within_s: f64) -> Vec<(String, String, Vec<String>)> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let cutoff = now() - within_s;
    let mut by: std::collections::BTreeMap<(String, String), Vec<String>> = std::collections::BTreeMap::new();
    for (_, copy) in store.eph_scan(&format!("copyhash:{scope}:")) {
        let (u, s, p) = (text(&copy, "user"), text(&copy, "session"), text(&copy, "path"));
        if s.is_empty() || (u == user && s == session) || !paths.contains(&p) {
            continue;
        }
        if copy.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < cutoff
            || store.eph_get(&format!("sessended:{scope}:{s}")).is_some()
        {
            continue;
        }
        let files = by.entry((u, s)).or_default();
        if !files.contains(&p) {
            files.push(p);
        }
    }
    by.into_iter().map(|((u, s), f)| (u, s, f)).collect()
}

/// The latest recorded write of `path`: (when, fingerprint).
pub fn latest_write(store: &Store, scope: &str, path: &str) -> Option<(f64, String)> {
    let w = store.eph_get(&write_key(scope, path))?;
    Some((w.get("ts").and_then(Value::as_f64)?, w.get("h").and_then(Value::as_str).unwrap_or("").to_string()))
}

/// For the read note: is the teammate's recent write in what this session
/// just read, and is it pushed?
pub fn read_view(store: &Store, scope: &str, user: &str, session: &str, path: &str) -> Value {
    match behind(store, scope, user, session, path) {
        None => json!({"in_copy": true}),
        Some(b) => json!({"in_copy": false, "pushed": b.pushed}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_behind_a_teammates_write_is_told_once_and_pushed_or_not() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        let (scope, p) = ("w:r", "shop/tiles.py");
        let (v1, v2) = (fingerprint_content("def tiles():\n    return settled_total()\n"), fingerprint_content("def tiles():\n    return paid_total()\n"));
        note_checkout(&db, scope, "alice", "a1", "clone-a");
        note_checkout(&db, scope, "bob", "b1", "clone-b");
        saw(&db, scope, "alice", "a1", p, &v1, false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        saw(&db, scope, "bob", "b1", p, &v2, true);
        // bob's change is only in his checkout
        let b = behind(&db, scope, "alice", "a1", p).expect("behind");
        assert!(!b.pushed);
        let note = gate_note(&db, scope, "alice", "a1", p, "Bob").expect("told");
        assert!(note.contains("not pushed"), "{note}");
        assert!(gate_note(&db, scope, "alice", "a1", p, "Bob").is_none(), "a retry goes through");
        // he lands: a pull gets it
        db.ledger_append(scope, "landed", &json!({"user": "bob", "session": "b1", "commit": "c1"}), now()).unwrap();
        assert!(behind(&db, scope, "alice", "a1", p).unwrap().pushed);
        assert_eq!(read_view(&db, scope, "alice", "a1", p)["pushed"], true);
        // the landing itself tells her, once, on her next step
        assert_eq!(announce_landing(&db, scope, "bob", "b1"), 1);
        let notices = crate::inflight::take_notices(&db, scope, "a1");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("just pushed their change to shop/tiles.py") && notices[0].contains("Before your next edit"), "{}", notices[0]);
        assert!(crate::inflight::take_notices(&db, scope, "a1").is_empty());
        // she pulls and re-reads: current
        saw(&db, scope, "alice", "a1", p, &v2, false);
        assert!(behind(&db, scope, "alice", "a1", p).is_none());
        assert_eq!(read_view(&db, scope, "alice", "a1", p)["in_copy"], true);
        // the writer is never behind its own write; another session in the
        // SAME checkout has the change on disk
        assert!(behind(&db, scope, "bob", "b1", p).is_none());
        note_checkout(&db, scope, "bob", "b2", "clone-b");
        saw(&db, scope, "bob", "b2", p, &v1, false);
        assert!(behind(&db, scope, "bob", "b2", p).is_none(), "same checkout");
        // a session that never looked at the file is not held
        assert!(behind(&db, scope, "carol", "c1", p).is_none());
    }
}
