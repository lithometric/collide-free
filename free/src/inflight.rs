//! Work in flight: code another agent is writing right now, told to the
//! agents whose own code needs it, before any of it is pushed.
//!
//! Study 3 found the gap: five agents built one feature in five clones, and
//! the routes needed a metrics module a teammate had not pushed. With nothing
//! to go on, agents wrote their own copy of its functions or stalled waiting.
//! The server had the answer the whole time: the author's hook reported the
//! file, signatures and all, the moment it was saved.
//!
//! Two things are in flight: a file new to the repo (its whole interface),
//! and symbols a write ADDED to an existing file, or renamed something to
//! (those names and signatures). An import names what it needs: the module,
//! resolved to a repo path by the same resolver the graph uses for every
//! language it parses (Python, TS/JS with `@/`-style aliases, Rust, Go,
//! Java, C#, PHP, C/C++, Ruby), and the names it takes from it. So:
//!
//! - the module resolves and another agent has in-flight work there that
//!   the import needs: the importer is told on its next step;
//! - the module resolves but a name it takes is not there yet: remembered
//!   (`waitname:`), and told the moment another agent adds it;
//! - the module resolves nowhere yet: remembered (`waitspec:`, at most
//!   MAX_WAITING per session), and told the moment a file appears that it
//!   resolves to. A package import never resolves and simply expires.
//!
//! Each notice is delivered once, riding the deltas channel every hook event
//! already reads, and names the author the way the reader knows them.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::codegraph::path_key;
use crate::store::Store;

const INFLIGHT_TTL_S: f64 = 7_200.0;
const MAX_SIGNATURES: usize = 8;
const MAX_NOTICE_CHARS: usize = 700;
/// Unresolved imports remembered per session; the oldest go first.
const MAX_WAITING: usize = 40;

fn entry_prefix(scope: &str, path: &str) -> String {
    format!("inflight:{scope}:{}:", path_key(path))
}

fn notice_prefix(scope: &str, session: &str) -> String {
    format!("inflightmsg:{scope}:{session}:")
}

fn short_hash(text: &str) -> String {
    crate::hashing::sha256_hex(text).chars().take(16).collect()
}

/// Signatures of these symbols, the classes and functions first, capped.
fn interface(symbols: &Map<String, Value>, only: Option<&BTreeSet<String>>) -> String {
    let mut sigs: Vec<String> = symbols
        .iter()
        .filter(|(name, _)| only.map_or(true, |o| o.contains(*name)))
        .filter(|(_, s)| matches!(s.get("kind").and_then(Value::as_str), Some("function" | "class" | "method")) || only.is_some())
        .filter_map(|(name, s)| {
            let sig = s.get("signature").and_then(Value::as_str).unwrap_or("");
            Some(if sig.is_empty() { name.clone() } else { sig.to_string() })
        })
        .collect();
    let more = sigs.len().saturating_sub(MAX_SIGNATURES);
    sigs.truncate(MAX_SIGNATURES);
    let mut text = sigs.join("; ");
    if more > 0 {
        text.push_str(&format!("; … {more} more"));
    }
    text
}

/// The author, named the way the reader knows them.
fn author_for(store: &Store, reader: &str, author: &str) -> String {
    if author.is_empty() {
        return "another agent".to_string();
    }
    if author == reader {
        return "another session of yours".to_string();
    }
    let authors: BTreeSet<String> = [author.to_string()].into_iter().collect();
    crate::activity::identity_labels(store, reader, &authors)
        .get(author)
        .and_then(|l| l.get("label"))
        .and_then(Value::as_str)
        .unwrap_or(author)
        .to_string()
}

/// Queue one notice for `session`, once per (path, author) pair.
#[allow(clippy::too_many_arguments)]
fn queue(
    store: &Store, scope: &str, reader: &str, session: &str, path: &str, author: &str, author_session: &str,
    new_file: bool, iface: &str,
) {
    if session.is_empty() || (author == reader && author_session == session) {
        return;
    }
    let who = author_for(store, reader, author);
    let mut text = if new_file {
        format!(
            "Collide in flight: {path} is new work by {who}, probably not on main yet, and your code imports it. \
Build against its interface instead of waiting or writing your own copy; pull when it lands (Collide says when origin moves)."
        )
    } else {
        format!(
            "Collide in flight: {who} is adding to {path} what your code takes from it, probably not on main yet. \
Build against it instead of waiting or writing your own; pull when it lands (Collide says when origin moves)."
        )
    };
    if !iface.is_empty() {
        text.push_str(&format!(" Interface: {iface}"));
    }
    if text.chars().count() > MAX_NOTICE_CHARS {
        text = text.chars().take(MAX_NOTICE_CHARS - 1).collect::<String>() + "…";
    }
    let _ = store.eph_set(
        &format!("{}{}", notice_prefix(scope, session), short_hash(&format!("{path}|{author}|{author_session}"))),
        &json!({"path": path, "text": text, "author": author, "author_session": author_session}),
        Some(INFLIGHT_TTL_S),
    );
}

/// What an import takes by name: original names, not aliases or `*`.
fn taken_names(import: &Value) -> BTreeSet<String> {
    import
        .get("names")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let pair = pair.as_array()?;
            let original = pair.get(1).or_else(|| pair.first())?.as_str()?;
            (!original.is_empty() && original != "*").then(|| original.to_string())
        })
        .collect()
}

/// On a clean write. `was_known`: the graph had this file before the write.
/// `added`: symbols this write added to it or renamed something to. The
/// graph index already includes the write.
#[allow(clippy::too_many_arguments)]
pub fn on_write(
    store: &Store, scope: &str, user: &str, session: &str, path: &str, language: &str, was_known: bool,
    imports: &[Value], symbols: &Map<String, Value>, added: &[String], stamp: f64,
) {
    let agent = crate::presence::agent_id(user, session);
    // 1. what this write puts in flight
    let new_names: BTreeSet<String> = if was_known {
        added.iter().cloned().collect()
    } else {
        symbols.keys().cloned().collect()
    };
    if !new_names.is_empty() {
        let key = format!("{}{}", entry_prefix(scope, path), short_hash(&agent));
        let mut names: BTreeSet<String> = store
            .eph_get(&key)
            .and_then(|e| e.get("names").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .filter(|n| symbols.contains_key(n))
            .collect();
        names.extend(new_names.iter().cloned());
        let new_file = !was_known
            || store.eph_get(&key).and_then(|e| e.get("new_file").and_then(Value::as_bool)).unwrap_or(false);
        let _ = store.eph_set(
            &key,
            &json!({"path": path, "user": user, "session": session, "ts": stamp, "new_file": new_file,
                    "names": names.iter().collect::<Vec<_>>()}),
            Some(INFLIGHT_TTL_S),
        );
        // whoever was waiting for these names in this file
        for (wkey, waiting) in store.eph_scan(&format!("waitname:{scope}:{}:", path_key(path))) {
            let name = waiting.get("name").and_then(Value::as_str).unwrap_or("");
            if !new_names.contains(name) {
                continue;
            }
            let reader = waiting.get("user").and_then(Value::as_str).unwrap_or("");
            let reader_session = waiting.get("session").and_then(Value::as_str).unwrap_or("");
            let only: BTreeSet<String> = [name.to_string()].into_iter().collect();
            queue(store, scope, reader, reader_session, path, user, session, !was_known, &interface(symbols, Some(&only)));
            crate::deltas::touch(store, scope, reader_session, &[path.to_string()], stamp);
            store.eph_delete(&wkey);
        }
        // a new file: whoever imported a module that nothing resolved to, and
        // now resolves here
        if !was_known {
            for (wkey, waiting) in store.eph_scan(&format!("waitspec:{scope}:")) {
                let module = waiting.get("module").and_then(Value::as_str).unwrap_or("");
                let importer = waiting.get("importer").and_then(Value::as_str).unwrap_or("");
                let lang = waiting.get("language").and_then(Value::as_str).unwrap_or("");
                let hits = crate::codegraph::with_known(store, scope, |known| {
                    collide_core::graph::resolve_module(module, importer, lang, known).as_deref() == Some(path)
                });
                if !hits {
                    continue;
                }
                let reader = waiting.get("user").and_then(Value::as_str).unwrap_or("");
                let reader_session = waiting.get("session").and_then(Value::as_str).unwrap_or("");
                queue(store, scope, reader, reader_session, path, user, session, true, &interface(symbols, None));
                crate::deltas::touch(store, scope, reader_session, &[path.to_string()], stamp);
                store.eph_delete(&wkey);
            }
        }
    }

    // 2. what this write imports
    for import in imports {
        let module = import.get("module").and_then(Value::as_str).unwrap_or("");
        if module.is_empty() {
            continue;
        }
        let taken = taken_names(import);
        let target = crate::codegraph::with_known(store, scope, |known| {
            collide_core::graph::resolve_module(module, path, language, known)
        });
        match target {
            Some(target) if target != path => {
                let target_symbols: Map<String, Value> = crate::codegraph::symbols_of(store, scope, &target);
                // in-flight work there that this import needs
                for (_, entry) in store.eph_scan(&entry_prefix(scope, &target)) {
                    let author = entry.get("user").and_then(Value::as_str).unwrap_or("");
                    let author_session = entry.get("session").and_then(Value::as_str).unwrap_or("");
                    if author == user && author_session == session {
                        continue;
                    }
                    let new_file = entry.get("new_file").and_then(Value::as_bool).unwrap_or(false);
                    let names: BTreeSet<String> = entry
                        .get("names").and_then(Value::as_array).into_iter().flatten()
                        .filter_map(|v| v.as_str().map(str::to_string)).collect();
                    let wanted: BTreeSet<String> = if new_file && taken.is_empty() {
                        names.clone()
                    } else {
                        taken.intersection(&names).cloned().collect()
                    };
                    if wanted.is_empty() {
                        continue;
                    }
                    let iface = interface(&target_symbols, if new_file && taken.is_empty() { None } else { Some(&wanted) });
                    queue(store, scope, user, session, &target, author, author_session, new_file, &iface);
                    crate::deltas::touch(store, scope, session, &[target.clone()], stamp);
                }
                // names it takes that nobody has written yet
                for name in taken.iter().filter(|n| !target_symbols.contains_key(*n)) {
                    let _ = store.eph_set(
                        &format!("waitname:{scope}:{}:{}", path_key(&target), short_hash(&format!("{name}|{user}|{session}"))),
                        &json!({"name": name, "user": user, "session": session, "importer": path, "ts": stamp}),
                        Some(INFLIGHT_TTL_S),
                    );
                }
            }
            Some(_) => {}
            None => {
                // nothing resolves yet: remember it, a few per session
                let mine = format!("waitspec:{scope}:{}:", short_hash(&agent));
                let mut held = store.eph_scan(&mine);
                if held.len() >= MAX_WAITING {
                    held.sort_by(|a, b| {
                        let ta = a.1.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                        let tb = b.1.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                        ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    for (old, _) in held.iter().take(held.len() + 1 - MAX_WAITING) {
                        store.eph_delete(old);
                    }
                }
                let _ = store.eph_set(
                    &format!("{mine}{}", short_hash(&format!("{module}|{path}"))),
                    &json!({"module": module, "importer": path, "language": language, "user": user, "session": session, "ts": stamp}),
                    Some(INFLIGHT_TTL_S),
                );
            }
        }
    }
}

/// A teammate's new, unpushed file matches what `reader`'s session was asked
/// to do (`embed::match_tasks`): say so once, with its interface.
pub fn queue_task_match(
    store: &Store, scope: &str, reader: &str, session: &str, path: &str, author: &str, author_session: &str,
) {
    if session.is_empty() {
        return;
    }
    let who = author_for(store, reader, author);
    let iface = interface(&crate::codegraph::symbols_of(store, scope, path), None);
    let mut text = format!(
        "Collide: {who} just started {path}, and it matches what you are working on. It is not pushed yet: build \
against its interface, or keep to your own part until it lands (Collide says when origin moves)."
    );
    if !iface.is_empty() {
        text.push_str(&format!(" Interface: {iface}"));
    }
    if text.chars().count() > MAX_NOTICE_CHARS {
        text = text.chars().take(MAX_NOTICE_CHARS - 1).collect::<String>() + "…";
    }
    let _ = store.eph_set(
        &format!("{}{}", notice_prefix(scope, session), short_hash(&format!("{path}|{author}|{author_session}"))),
        &json!({"path": path, "text": text, "author": author, "author_session": author_session}),
        Some(INFLIGHT_TTL_S),
    );
    crate::deltas::touch(store, scope, session, &[path.to_string()], crate::store::now());
}

/// How `reader` knows `author`: nickname, handle, email, or "another session of yours".
pub fn author_label(store: &Store, reader: &str, author: &str) -> String {
    author_for(store, reader, author)
}

/// A one-off notice for a session's next step (delivered and removed by
/// `take_notices`), about `paths`.
#[allow(clippy::too_many_arguments)]
pub fn queue_notice(
    store: &Store, scope: &str, session: &str, id: &str, text: &str, author: &str, author_session: &str, paths: &[String],
) {
    if session.is_empty() {
        return;
    }
    let text: String = if text.chars().count() > MAX_NOTICE_CHARS { text.chars().take(MAX_NOTICE_CHARS - 1).collect::<String>() + "…" } else { text.to_string() };
    let _ = store.eph_set(
        &format!("{}{}", notice_prefix(scope, session), short_hash(id)),
        &json!({"path": paths.first().cloned().unwrap_or_default(), "text": text, "author": author, "author_session": author_session}),
        Some(INFLIGHT_TTL_S),
    );
    crate::deltas::touch(store, scope, session, paths, crate::store::now());
}

/// The author's work landed (`collide-hook land` pushed it): nothing of it is
/// in flight any more. Its entries go, and so do notices about it not yet
/// read; Study 5 told an agent a module was "probably not on main yet" 45s
/// after it was pushed and 26s after that agent had pulled it.
pub fn landed(store: &Store, scope: &str, user: &str, session: &str) -> usize {
    if session.is_empty() {
        return 0;
    }
    let mut cleared = 0;
    for (key, entry) in store.eph_scan(&format!("inflight:{scope}:")) {
        if entry.get("user").and_then(Value::as_str) == Some(user) && entry.get("session").and_then(Value::as_str) == Some(session) {
            store.eph_delete(&key);
            cleared += 1;
        }
    }
    for (key, notice) in store.eph_scan(&format!("inflightmsg:{scope}:")) {
        if notice.get("author").and_then(Value::as_str) == Some(user)
            && notice.get("author_session").and_then(Value::as_str) == Some(session)
        {
            store.eph_delete(&key);
            cleared += 1;
        }
    }
    cleared
}

/// The in-flight notices waiting for this session, removed as they are read.
pub fn take_notices(store: &Store, scope: &str, session: &str) -> Vec<String> {
    if session.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (key, notice) in store.eph_scan(&notice_prefix(scope, session)) {
        if let Some(text) = notice.get("text").and_then(Value::as_str) {
            out.push(text.to_string());
        }
        store.eph_delete(&key);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(signature: &str) -> Value {
        json!({"kind": "function", "signature": signature})
    }

    fn record(path: &str, language: &str, names: &[&str]) -> Value {
        let symbols: Map<String, Value> = names.iter().map(|n| (n.to_string(), sym(&format!("def {n}()")))).collect();
        json!({"path": path, "language": language, "symbols": symbols, "imports": []})
    }

    fn write(db: &Store, session: &str, path: &str, language: &str, names: &[&str], imports: Value, added: &[&str]) {
        let was_known = crate::codegraph::has_file(db, "w:r", path);
        crate::codegraph::update_file(db, "w:r", path, record(path, language, names), 1.0);
        let symbols: Map<String, Value> = names.iter().map(|n| (n.to_string(), sym(&format!("def {n}()")))).collect();
        let added: Vec<String> = added.iter().map(|s| s.to_string()).collect();
        on_write(db, "w:r", "u", session, path, language, was_known, imports.as_array().unwrap(), &symbols, &added, 1.0);
    }

    #[test]
    fn a_waiting_importer_hears_a_new_module_in_any_language() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        // Rust: a `use crate::metrics::revenue` of a module nobody wrote yet
        write(&db, "api", "src/main.rs", "rust", &["main"], json!([{"module": "crate::metrics", "names": [["revenue", "revenue"]]}]), &[]);
        write(&db, "api", "src/lib.rs", "rust", &["lib"], json!([]), &[]);
        assert!(take_notices(&db, "w:r", "api").is_empty());
        write(&db, "metrics", "src/metrics.rs", "rust", &["revenue"], json!([]), &[]);
        let told = take_notices(&db, "w:r", "api");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("src/metrics.rs is new work") && told[0].contains("def revenue()"), "{}", told[0]);
        assert!(take_notices(&db, "w:r", "metrics").is_empty(), "the author hears nothing");
    }

    #[test]
    fn a_function_added_to_an_existing_file_reaches_whoever_imports_it() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        // existing code: indexed, not written by anyone in flight
        crate::codegraph::update_file(&db, "w:r", "app/metrics.py", record("app/metrics.py", "python", &["revenue_by_day"]), 1.0);
        // the routes import a function that is not there yet
        write(&db, "api", "app/api.py", "python", &["summary"],
              json!([{"module": "app.metrics", "names": [["top_products", "top_products"]]}]), &[]);
        assert!(take_notices(&db, "w:r", "api").is_empty());
        // a teammate adds it: the importer is told its signature
        write(&db, "metrics", "app/metrics.py", "python", &["revenue_by_day", "top_products"], json!([]), &["top_products"]);
        let told = take_notices(&db, "w:r", "api");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("is adding to app/metrics.py") && told[0].contains("def top_products()"), "{}", told[0]);
        // and a later importer of that name is told straight away
        write(&db, "page", "app/page.py", "python", &["render"],
              json!([{"module": "app.metrics", "names": [["top_products", "top_products"]]}]), &[]);
        assert_eq!(take_notices(&db, "w:r", "page").len(), 1);
        // an import of something not in flight says nothing
        write(&db, "page2", "app/page2.py", "python", &["render2"],
              json!([{"module": "app.metrics", "names": [["revenue_by_day", "revenue_by_day"]]}]), &[]);
        assert!(take_notices(&db, "w:r", "page2").is_empty());
    }

    #[test]
    fn typescript_aliases_and_relative_imports_resolve() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        write(&db, "ui", "src/app/page.tsx", "tsx", &["Page"],
              json!([{"module": "@/lib/metrics", "names": [["revenue", "revenue"]]}, {"module": "./chart", "names": [["Chart", "Chart"]]}]), &[]);
        write(&db, "lib", "src/lib/metrics.ts", "typescript", &["revenue"], json!([]), &[]);
        write(&db, "charts", "src/app/chart.tsx", "tsx", &["Chart"], json!([]), &[]);
        let told = take_notices(&db, "w:r", "ui");
        assert_eq!(told.len(), 2, "{told:?}");
    }

    #[test]
    fn waiting_imports_are_capped_per_session() {
        let db = Store::open(std::path::Path::new(":memory:")).unwrap();
        for i in 0..(MAX_WAITING + 10) {
            let imports = json!([{"module": format!("pkg{i}.mod"), "names": []}]);
            write(&db, "busy", &format!("a{i}.py"), "python", &["f"], imports, &[]);
        }
        let agent = crate::presence::agent_id("u", "busy");
        assert!(db.eph_scan(&format!("waitspec:w:r:{}:", short_hash(&agent))).len() <= MAX_WAITING);
    }
}
