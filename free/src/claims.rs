//! Gap-free atomic claims: the next number in a sequence, handed out once.
//!
//! Two agents each creating "migration 048" at the same moment is a
//! collision the code graph cannot see — the files are different, so no
//! symbol overlaps — and it surfaces only when both land. `claim` hands out
//! the next number of a sequence in one atomic statement, so two agents
//! asking together get 048 and 049, never 048 twice and never a gap.
//!
//! A sequence is a directory (`db/migrations/`, `docs/adr/`) or a bare name
//! (`release`). A directory sequence starts above the highest number any
//! file in it already carries (the first digit run of the file name, e.g.
//! `0047_add_users.sql` → 47, width 4), so adopting claims mid-project never
//! reissues a number. Claims then run through everything else:
//!
//!   - the write: a new file in the directory that takes a claimed number
//!     fills the claim when its claimer writes it, and is a CLASH when
//!     anyone else does — a `claim_conflict` on the ledger and the feed, the
//!     writer told on the /report answer, and both people messaged,
//!     anchored to the file, so the notice carries staleness and outcomes;
//!   - the graph: a filled claim's symbols carry GRID_CLAIMED for a day, a
//!     clashing file's GRID_CLAIM_CLASH until it is renamed away;
//!   - the briefing: open claims by others in the "right now" lines;
//!   - the dashboard: the grid answer lists live claims.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

pub const CLAIM_BUCKET: &str = "claim";
pub const COUNTER_BUCKET: &str = "claimseq";
const DEFAULT_WIDTH: usize = 3;
/// An open claim nobody filled stops warning after this long.
pub const OPEN_TTL_S: f64 = 3.0 * 86_400.0;
/// A filled claim stays lit on the grid this long.
pub const FILLED_SHOW_S: f64 = 86_400.0;
const MAX_TITLE: usize = 120;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `db/migrations/`, `adr`: trimmed, no leading `./` or `/`. `None` when empty.
pub fn normalize(sequence: &str) -> Option<String> {
    let mut seq = sequence.trim().replace('\\', "/");
    while let Some(rest) = seq.strip_prefix("./") {
        seq = rest.to_string();
    }
    let seq = seq.trim_start_matches('/').to_string();
    if seq.is_empty() || seq == "/" { None } else { Some(seq) }
}

/// The number a file name carries: its first run of digits, and how many
/// digits wide it is. `0047_add_users.sql` → (47, 4); `README.md` → None.
pub fn number_of(file_name: &str) -> Option<(i64, usize)> {
    let digits: String = file_name
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() || digits.len() > 12 {
        return None;
    }
    digits.parse().ok().map(|n| (n, digits.len()))
}

/// The directory of a path, with its trailing slash; "" at the root.
fn dir_of(path: &str) -> String {
    path.rsplit_once('/').map(|(dir, _)| format!("{dir}/")).unwrap_or_default()
}

fn base_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, base)| base).unwrap_or(path)
}

/// How far back the ledger is read for numbers a directory already used:
/// files with no parser (a `.sql` migration) never reach the map, but
/// every write of one is on the ledger.
const LEDGER_LOOKBACK_S: f64 = 180.0 * 86_400.0;

/// The highest number the directory's files already carry, and their width:
/// the map's files, and every path written there on the ledger.
fn highest_seen(store: &Store, scope: &str, dir: &str) -> (i64, usize) {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for workspace in store.list_workspaces(scope) {
        paths.extend(store.list_files(scope, &text(&workspace, "user")).into_iter().map(|(path, _)| path));
    }
    for row in store.ledger_since(scope, now() - LEDGER_LOOKBACK_S) {
        if row.kind == "edit_reported" {
            paths.insert(text(&row.payload, "path"));
        }
    }
    let mut high = 0i64;
    let mut width = 0usize;
    for path in paths.iter().filter(|p| p.starts_with(dir) && !p[dir.len()..].contains('/')) {
        if let Some((n, w)) = number_of(base_of(path)) {
            high = high.max(n);
            width = width.max(w);
        }
    }
    (high, if width == 0 { DEFAULT_WIDTH } else { width })
}

fn claim_key(scope: &str, sequence: &str, number: i64) -> String {
    format!("{scope}:{sequence}#{number}")
}

/// `claim`: the next number of `sequence`, reserved for the caller.
/// `after` is the highest number the caller already sees in use (its own
/// checkout may hold files the server never saw); 0 when it does not say.
#[allow(clippy::too_many_arguments)]
pub fn claim(
    store: &Store, scope: &str, user: &str, session: &str, agent: &str, sequence: &str, title: &str, after: i64,
    via: &str,
) -> Value {
    let Some(sequence) = normalize(sequence) else {
        return json!({"ok": false, "error": "sequence required: a directory like \"db/migrations/\" or a name like \"release\""});
    };
    let is_dir = sequence.ends_with('/');
    let (floor, width) = if is_dir { highest_seen(store, scope, &sequence) } else { (0, DEFAULT_WIDTH) };
    let floor = floor.max(after.max(0));
    let stamp = now();
    let number = match store.kv_next(COUNTER_BUCKET, &format!("{scope}:{sequence}"), floor, stamp) {
        Ok(n) => n,
        Err(problem) => return json!({"ok": false, "error": format!("could not claim: {problem}")}),
    };
    let formatted = format!("{number:0width$}");
    let title: String = title.trim().chars().take(MAX_TITLE).collect();
    let record = json!({
        "sequence": sequence, "number": number, "formatted": formatted, "title": title,
        "user": user, "session": session, "agent": agent, "ts": stamp, "status": "open", "path": Value::Null,
    });
    let _ = store.kv_put(CLAIM_BUCKET, &claim_key(scope, &sequence, number), &record, stamp);
    let row = json!({"sequence": sequence, "number": number, "formatted": formatted, "title": title,
                     "user": user, "agent": agent});
    let _ = store.ledger_append(scope, "claim_made", &row, stamp);
    let mut event = row;
    event["kind"] = json!("claim");
    event["action"] = json!("made");
    crate::events::publish(store, scope, event, via);
    let hint = if is_dir {
        format!("{formatted} is yours: name the new file {sequence}{formatted}_<name>. Any other file that takes {formatted} there is flagged as a clash.")
    } else {
        format!("{sequence} {formatted} is yours; nobody else will be handed it.")
    };
    json!({"ok": true, "sequence": sequence, "number": number, "formatted": formatted, "hint": hint})
}

/// Every claim of a scope that still matters: open ones inside their TTL,
/// filled ones inside the day they stay lit.
pub fn live(store: &Store, scope: &str) -> Vec<Value> {
    let stamp = now();
    let mut out: Vec<Value> = store
        .kv_list(CLAIM_BUCKET, &format!("{scope}:"))
        .into_iter()
        .map(|(_, record)| record)
        .filter(|record| match text(record, "status").as_str() {
            "open" => stamp - num(record, "ts") < OPEN_TTL_S,
            "filled" => stamp - num(record, "filled_at") < FILLED_SHOW_S,
            _ => false,
        })
        .collect();
    out.sort_by(|a, b| {
        text(a, "sequence").cmp(&text(b, "sequence")).then(num(a, "number").partial_cmp(&num(b, "number")).unwrap_or(std::cmp::Ordering::Equal))
    });
    out
}

/// A write under a claimed directory: fills the claim when its claimer
/// takes the number, is a clash when anyone else does. Returns what the
/// writer is told, `None` for every write claims have nothing to say about.
pub fn on_write(store: &Store, scope: &str, user: &str, session: &str, path: &str, via: &str) -> Option<Value> {
    let (number, _) = number_of(base_of(path))?;
    let dir = dir_of(path);
    if dir.is_empty() {
        return None;
    }
    let key = claim_key(scope, &dir, number);
    let mut record = store.kv_get(CLAIM_BUCKET, &key)?;
    let stamp = now();
    let owner = text(&record, "user");
    let filled_path = text(&record, "path");
    if filled_path == path {
        return None; // the claimed file, written again
    }
    let open = text(&record, "status") == "open" && stamp - num(&record, "ts") < OPEN_TTL_S;
    // the claim belongs to one AGENT: another session of the same person
    // taking the number is as much a clash as a teammate taking it
    let claimer = text(&record, "session");
    let same_agent = owner == user && (claimer.is_empty() || session.is_empty() || claimer == session);
    if open && same_agent {
        record["status"] = json!("filled");
        record["path"] = json!(path);
        record["filled_at"] = json!(stamp);
        let _ = store.kv_put(CLAIM_BUCKET, &key, &record, stamp);
        let row = json!({"sequence": dir, "number": number, "formatted": text(&record, "formatted"),
                         "user": user, "path": path});
        let _ = store.ledger_append(scope, "claim_filled", &row, stamp);
        let mut event = row;
        event["kind"] = json!("claim");
        event["action"] = json!("filled");
        crate::events::publish(store, scope, event, via);
        return Some(json!({"status": "filled", "formatted": text(&record, "formatted")}));
    }
    if !open && filled_path.is_empty() {
        return None; // an expired, never-filled claim no longer holds the number
    }
    // a clash: the number belongs to someone else's claim (or to the file
    // that already filled it). Told once per clashing path.
    let mut clashes: Vec<String> = record
        .get("clashes").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_string).collect();
    if clashes.iter().any(|p| p == path) {
        return None;
    }
    clashes.push(path.to_string());
    record["clashes"] = json!(clashes);
    let _ = store.kv_put(CLAIM_BUCKET, &key, &record, stamp);
    let formatted = text(&record, "formatted");
    let title = text(&record, "title");
    let holder = if filled_path.is_empty() { owner.clone() } else { format!("{owner} ({filled_path})") };
    let notice = format!(
        "{formatted} in {dir} is claimed by {holder}{}; {path} takes the same number. Call claim(repo_id, \"{dir}\") for the next free number and rename this file.",
        if title.is_empty() { String::new() } else { format!(" for \"{title}\"") });
    let row = json!({"sequence": dir, "number": number, "formatted": formatted, "owner": owner,
                     "user": user, "path": path});
    let _ = store.ledger_append(scope, "claim_conflict", &row, stamp);
    let mut event = row;
    event["kind"] = json!("claim");
    event["action"] = json!("conflict");
    crate::events::publish(store, scope, event, via);
    // both people hear it, pointed at the clashing file
    let anchor = crate::memory::parse_anchor(path).expect("a path parses as an anchor");
    let (hash, rev, _) = crate::memory::anchor_state(store, scope, &anchor);
    for (to, message) in [
        (user.to_string(), notice.clone()),
        (owner.clone(), format!("{user} wrote {path}, which takes {formatted} in {dir} — your claim. They were told to take the next number.")),
    ] {
        if to.is_empty() {
            continue;
        }
        let id = crate::compat::new_id();
        let _ = store.kv_put("inbox", &format!("{scope}:{to}:{id}"), &json!({
            "id": id, "to": to, "from": "collide", "message": message, "agent": "collide", "ts": stamp,
            "anchor": path, "anchor_hash": hash, "anchor_rev": rev, "routed": false,
        }), stamp);
    }
    Some(json!({"status": "conflict", "formatted": formatted, "owner": owner, "notice": notice}))
}

/// The gate's half: an agent is about to create `path`, a numbered file in
/// a numbered folder (`after` = the highest number its checkout already has
/// there). Claiming happens here, with no step from the agent: a free
/// number is claimed for this agent and the write goes ahead; a number
/// another agent holds (or a file already has) is refused, and the next free
/// number is claimed for this agent instead and named in the refusal.
/// `None` = allow.
#[allow(clippy::too_many_arguments)]
/// What a clash the gate stopped would have cost: the re-read that finds the
/// other file and the rename, two messages (the same floor as traffic control).
const CLASH_MESSAGES: u64 = 2;

pub fn gate_check(
    store: &Store, scope: &str, user: &str, session: &str, agent: &str, path: &str, after: i64, via: &str,
) -> Option<String> {
    let base = base_of(path);
    if !base.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (number, width) = number_of(base)?;
    let dir = dir_of(path);
    if dir.is_empty() || width > 8 {
        return None;
    }
    let rest = &base[width..];
    let stamp = now();
    let key = claim_key(scope, &dir, number);
    let counter = format!("{scope}:{dir}");
    let fresh = |n: i64| {
        json!({
            "sequence": dir, "number": n, "formatted": format!("{n:0width$}"), "title": "",
            "user": user, "session": session, "agent": agent, "ts": stamp, "status": "open", "path": Value::Null,
            "auto": true,
        })
    };
    let taken_by = match store.kv_get(CLAIM_BUCKET, &key) {
        Some(record) => {
            let same_agent = text(&record, "user") == user
                && (text(&record, "session").is_empty() || session.is_empty() || text(&record, "session") == session);
            let filled = text(&record, "path");
            if same_agent || filled == path {
                return None;
            }
            Some(if filled.is_empty() { text(&record, "user") } else { format!("{} ({filled})", text(&record, "user")) })
        }
        None if number <= after => Some("a file already in this checkout".to_string()),
        None => {
            // free: claim it for this agent, atomically; losing the race means taken
            if store.kv_put_if_absent(CLAIM_BUCKET, &key, &fresh(number), stamp).unwrap_or(false) {
                let _ = store.kv_raise(COUNTER_BUCKET, &counter, number, stamp);
                let _ = store.ledger_append(scope, "claim_made", &json!({
                    "sequence": dir, "number": number, "formatted": format!("{number:0width$}"), "title": "",
                    "user": user, "agent": agent, "auto": true,
                }), stamp);
                return None;
            }
            Some("another agent, a moment ago".to_string())
        }
    };
    let holder = taken_by.unwrap_or_default();
    let next = store.kv_next(COUNTER_BUCKET, &counter, number.max(after), stamp).ok()?;
    let _ = store.kv_put(CLAIM_BUCKET, &claim_key(scope, &dir, next), &fresh(next), stamp);
    let formatted_next = format!("{next:0width$}");
    let row = json!({"sequence": dir, "number": next, "formatted": formatted_next, "title": "",
                     "user": user, "agent": agent, "auto": true, "instead_of": number});
    let _ = store.ledger_append(scope, "claim_made", &row, stamp);
    // the clash that did not happen, on the savings page beside traffic control
    crate::briefstat::record_saving(
        store, scope, "claim_prevented", user, agent, session, CLASH_MESSAGES,
        &json!({"paths": [path, format!("{dir}{formatted_next}{rest}")], "sequence": dir}), stamp,
    );
    let mut event = row;
    event["kind"] = json!("claim");
    event["action"] = json!("made");
    crate::events::publish(store, scope, event, via);
    Some(format!(
        "{} in {dir} is taken by {holder}. {formatted_next} is now reserved for you: write {dir}{formatted_next}{rest} instead.",
        format!("{number:0width$}")
    ))
}

/// Grid overlays: the files a claim filled in the last day, and the files
/// clashing with a claim right now.
pub fn grid_paths(store: &Store, scope: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut filled = BTreeSet::new();
    let mut clashing = BTreeSet::new();
    for record in live(store, scope) {
        if text(&record, "status") == "filled" {
            filled.insert(text(&record, "path"));
        }
    }
    for (_, record) in store.kv_list(CLAIM_BUCKET, &format!("{scope}:")) {
        for path in record.get("clashes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            clashing.insert(path.to_string());
        }
    }
    (filled, clashing)
}

/// The dashboard's list: live claims, compact.
pub fn summary(store: &Store, scope: &str) -> Vec<Value> {
    live(store, scope)
        .into_iter()
        .map(|r| json!({
            "sequence": r["sequence"], "formatted": r["formatted"], "title": r["title"], "user": r["user"],
            "ts": r["ts"], "status": r["status"], "path": r["path"],
            "clashes": r.get("clashes").cloned().unwrap_or(json!([])),
        }))
        .collect()
}

/// Briefing lines: open claims by anyone but the viewer's own session.
pub fn brief_lines(store: &Store, scope: &str, viewer: &str, session: &str, stamp: f64) -> Vec<String> {
    let mut by_seq: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in live(store, scope) {
        if text(&record, "status") != "open" || (text(&record, "user") == viewer && text(&record, "session") == session) {
            continue;
        }
        let mins = ((stamp - num(&record, "ts")) / 60.0).round().max(0.0) as i64;
        let title = text(&record, "title");
        by_seq.entry(text(&record, "sequence")).or_default().push(format!(
            "{} by {}{} {}m ago", text(&record, "formatted"), text(&record, "user"),
            if title.is_empty() { String::new() } else { format!(" (\"{title}\")") }, mins));
    }
    by_seq
        .into_iter()
        .map(|(seq, items)| format!("Claimed in {seq}: {} — call claim(repo_id, \"{seq}\") for your own number, never reuse these.", items.join("; ")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).expect("store")
    }

    #[test]
    fn the_gate_claims_for_the_agent_and_steers_a_second_one_to_the_next_number() {
        let db = store();
        // five migrations in the checkout; agent A writes 006: claimed, allowed
        assert!(gate_check(&db, "w:r", "u", "a", "", "db/m/006_orders.sql", 5, "").is_none());
        // agent B (same person, another session) also picks 006: refused, 007 reserved
        let told = gate_check(&db, "w:r", "u", "b", "", "db/m/006_invoices.sql", 5, "").unwrap();
        assert!(told.contains("007 is now reserved for you") && told.contains("db/m/007_invoices.sql"), "{told}");
        // B's retry with 007 is its own claim; A writing 006 again is fine
        assert!(gate_check(&db, "w:r", "u", "b", "", "db/m/007_invoices.sql", 5, "").is_none());
        assert!(gate_check(&db, "w:r", "u", "a", "", "db/m/006_orders.sql", 5, "").is_none());
        // a number the checkout already has is refused too
        let dup = gate_check(&db, "w:r", "u", "c", "", "db/m/003_x.sql", 5, "").unwrap();
        assert!(dup.contains("008 is now reserved"), "{dup}");
        // files that are not numbered pass untouched
        assert!(gate_check(&db, "w:r", "u", "a", "", "app/main.py", 0, "").is_none());
        // each refusal is a clash that did not happen, on the savings page
        let saved: Vec<_> = db.ledger_since("w:r", 0.0).into_iter().filter(|r| r.kind == "claim_prevented").collect();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].payload["messages"], json!(2));
        assert_eq!(saved[0].payload["paths"], json!(["db/m/006_invoices.sql", "db/m/007_invoices.sql"]));
        assert_eq!(crate::briefstat::savings(&db, "w:r", 1.0)["by_source"]["claims"]["events"], json!(2));
    }

    #[test]
    fn numbers_come_from_the_first_digit_run() {
        assert_eq!(number_of("0047_add_users.sql"), Some((47, 4)));
        assert_eq!(number_of("adr-012-auth.md"), Some((12, 3)));
        assert_eq!(number_of("README.md"), None);
        assert_eq!(normalize("./db/migrations/"), Some("db/migrations/".into()));
        assert_eq!(normalize("  "), None);
    }

    #[test]
    fn claims_are_gap_free_and_never_reissued() {
        let db = store();
        let first = claim(&db, "w:r", "alice", "s1", "", "db/migrations/", "users", 0, "");
        let second = claim(&db, "w:r", "bob", "s2", "", "db/migrations/", "orders", 0, "");
        assert_eq!(first["formatted"], json!("001"));
        assert_eq!(second["formatted"], json!("002"));
        let named = claim(&db, "w:r", "bob", "s2", "", "release", "", 0, "");
        assert_eq!(named["number"], json!(1));
    }

    #[test]
    fn many_threads_claiming_at_once_get_distinct_numbers() {
        let db = std::sync::Arc::new(store());
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let db = std::sync::Arc::clone(&db);
                std::thread::spawn(move || {
                    claim(&db, "w:r", &format!("u{i}"), "s", "", "adr/", "", 0, "")["number"].as_i64().unwrap()
                })
            })
            .collect();
        let mut got: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort();
        assert_eq!(got, (1..=16).collect::<Vec<_>>());
    }

    #[test]
    fn the_claimer_fills_and_anyone_else_clashes() {
        let db = store();
        claim(&db, "w:r", "alice", "s1", "", "db/migrations/", "users", 0, "");
        let filled = on_write(&db, "w:r", "alice", "s1", "db/migrations/001_users.sql", "").unwrap();
        assert_eq!(filled["status"], json!("filled"));
        assert!(on_write(&db, "w:r", "alice", "s1", "db/migrations/001_users.sql", "").is_none());
        let clash = on_write(&db, "w:r", "bob", "s2", "db/migrations/001_orders.sql", "").unwrap();
        assert_eq!(clash["status"], json!("conflict"));
        assert!(clash["notice"].as_str().unwrap().contains("claimed by alice"));
        // told once per clashing path, and both people have a message
        assert!(on_write(&db, "w:r", "bob", "s2", "db/migrations/001_orders.sql", "").is_none());
        assert_eq!(db.kv_list("inbox", "w:r:bob:").len(), 1);
        assert_eq!(db.kv_list("inbox", "w:r:alice:").len(), 1);
        // another session of the SAME person taking a claimed number is a clash too
        claim(&db, "w:r", "alice", "s1", "", "adr/", "", 0, "");
        let own_other_session = on_write(&db, "w:r", "alice", "s9", "adr/001_other.md", "").unwrap();
        assert_eq!(own_other_session["status"], json!("conflict"));
        let (lit, clashing) = grid_paths(&db, "w:r");
        assert!(lit.contains("db/migrations/001_users.sql"));
        assert!(clashing.contains("db/migrations/001_orders.sql"));
    }
}
