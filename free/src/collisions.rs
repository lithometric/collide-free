//! `check_collisions`: what another workspace changed under the code you are
//! about to touch.
//!
//! Precision beats recall throughout. When the answer is uncertain the honest
//! response is silence, because an agent that learns to distrust these results
//! stops calling — and the call is the entire product. So: no clean baseline
//! of our own means no opinion, a removal seen once is "unknown" rather than
//! "removed", and a partial parse on the other side is served as its last
//! clean one, marked stale with its age.
//!
//! Fails open. Any internal error returns an empty result, never an exception
//! into an agent's turn.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::hashing::Tree;
use crate::repo::path_key;
use crate::semantics::{self, Served};
use crate::store::{now, Store};

const CERTAIN: &str = "certain";
const LIKELY: &str = "likely";
const UNCONFIRMED: &str = "unconfirmed";
const UNKNOWN: &str = "unknown";
const MAX_SALIENCE_PATHS: usize = 20;

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn field(symbol: &Value, key: &str) -> String {
    symbol.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn confidence_for(freshness: &str) -> &'static str {
    if freshness == semantics::LIVE { CERTAIN } else { LIKELY }
}

/// Diff the viewer's symbols for one file against another workspace's state
/// for the same file. The caller attaches path and author.
fn diff_against(
    viewer: &Map<String, Value>, other_record: &Value, stamp: f64,
) -> Vec<Value> {
    let Some(Served { symbols: other, freshness, age_s }) =
        semantics::serve(Some(other_record), stamp)
    else {
        return Vec::new(); // no clean parse over there, ever: stay silent
    };
    let base = confidence_for(freshness);

    let entry = |symbol: &str, kind: &str, detail: Value, confidence: &str| -> Value {
        json!({
            "symbol": symbol, "kind": kind, "detail": detail,
            "confidence": confidence, "freshness": freshness, "age_s": age_s,
        })
    };

    let mut removed: Vec<&String> = viewer.keys().filter(|name| !other.contains_key(*name)).collect();
    let mut added: Vec<&String> = other.keys().filter(|name| !viewer.contains_key(*name)).collect();
    let modified: Vec<&String> = viewer
        .keys()
        .filter(|name| {
            other.get(*name).map(|o| field(&viewer[*name], "hash") != field(o, "hash"))
                .unwrap_or(false)
        })
        .collect();

    let mut out: Vec<Value> = Vec::new();

    // exactly one out and one in is a rename, not a delete plus an add
    if removed.len() == 1 && added.len() == 1 {
        let (old_name, new_name) = (removed[0].clone(), added[0].clone());
        out.push(entry(
            &old_name,
            "renamed",
            json!({"new_name": new_name, "signature": field(&other[&new_name], "signature")}),
            base,
        ));
        removed.clear();
        added.clear();
    }

    let prev_symbols = other_record
        .get("prev_clean")
        .filter(|v| !v.is_null())
        .and_then(|clean| clean.get("symbols"))
        .and_then(Value::as_object);

    for name in removed {
        match prev_symbols {
            // absent from two consecutive clean parses over there
            Some(prev) if !prev.contains_key(name) => out.push(entry(
                name, "removed", json!("absent from two consecutive clean parses"), base)),
            _ => out.push(entry(
                name, UNKNOWN,
                json!("absent from the latest clean parse only; removal unconfirmed"),
                UNCONFIRMED)),
        }
    }
    for name in added {
        out.push(entry(name, "added", json!(field(&other[name], "signature")), base));
    }
    for name in modified {
        let signature_changed =
            field(&viewer[name], "signature") != field(&other[name], "signature");
        let detail = if signature_changed {
            format!("signature changed to: {}", field(&other[name], "signature"))
        } else {
            "implementation changed, signature unchanged".to_string()
        };
        let mut row = entry(name, "modified", json!(detail), base);
        if let Some(map) = row.as_object_mut() {
            map.insert("signature_change".into(), json!(signature_changed));
        }
        out.push(row);
    }
    out
}

/// Live intents in this scope, with idleness and remaining lease.
pub fn active_intents(store: &Store, scope: &str, idle_after_s: f64) -> Vec<Value> {
    let stamp = now();
    let mut intents: Vec<Value> = Vec::new();
    for (_key, intent) in store.eph_scan(&format!("intent:{scope}:")) {
        let mut intent = intent;
        let owner = text(&intent, "owner");
        let mut last_edit = 0.0_f64;
        if let Some(paths) = intent.get("paths").and_then(Value::as_array) {
            for path in paths.iter().filter_map(Value::as_str) {
                if let Some(record) =
                    store.eph_get(&format!("lastedit:{scope}:{owner}:{}", path_key(path)))
                {
                    last_edit = last_edit
                        .max(record.get("ts").and_then(Value::as_f64).unwrap_or(0.0));
                }
            }
        }
        let created = intent.get("created").and_then(Value::as_f64).unwrap_or(stamp);
        let reference = if last_edit > 0.0 { last_edit } else { created };
        let expires = intent.get("expires_at").and_then(Value::as_f64).unwrap_or(stamp);
        if let Some(map) = intent.as_object_mut() {
            map.insert("idle".into(), json!(stamp - reference > idle_after_s));
            // the moment the claim was last backed by an edit (or made): what
            // the pressure gradient ages a claim from
            map.insert("reference_ts".into(), json!(reference));
            map.insert(
                "ttl_remaining_s".into(),
                json!(crate::compat::python_round((expires - stamp).max(0.0), 1)),
            );
        }
        intents.push(intent);
    }
    intents.sort_by(|a, b| {
        let created = |v: &Value| v.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        created(a).partial_cmp(&created(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    intents
}

/// Live intents across the workspace as this caller may see it.
///
/// An intent is a person's declaration, not a repo's: bob announcing
/// "reworking checkout" in the web repo is exactly what an agent in the api
/// repo should know about. The key stays `intent:{scope}:{id}` — the repo is
/// still the unit of code — and the READ fans across `visible`, the caller's
/// allowlist from `access::visible_scopes` (non-optional, so a caller that
/// forgot it does not compile). An intent names paths and symbols, so a
/// member invited to one repo must not see the other's through this door.
///
/// The queried scope's own intents come first, then the rest of the
/// workspace, each in `created` order — the same "queried repo ranks first"
/// convention as recall and recipes. Foreign rows carry `from_repo` so a
/// reader can tell whose repo a path belongs to; local rows are unchanged.
pub fn visible_intents(
    store: &Store, scope: &str, visible: &BTreeSet<String>, idle_after_s: f64,
) -> Vec<Value> {
    let mut intents = active_intents(store, scope, idle_after_s);
    for other in visible.iter().filter(|other| other.as_str() != scope) {
        let repo = crate::presence::split_scope(other).1;
        for mut intent in active_intents(store, other, idle_after_s) {
            if let Some(map) = intent.as_object_mut() {
                map.insert("from_repo".into(), json!(repo));
            }
            intents.push(intent);
        }
    }
    intents.sort_by(|a, b| {
        let rank = |v: &Value| {
            (v.get("from_repo").is_some(), v.get("created").and_then(Value::as_f64).unwrap_or(0.0))
        };
        let (fa, ca) = rank(a);
        let (fb, cb) = rank(b);
        fa.cmp(&fb).then(ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal))
    });
    intents
}

fn project(intent: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    for key in keys {
        out.insert(key.to_string(), intent.get(*key).cloned().unwrap_or(json!("")));
    }
    Value::Object(out)
}

fn intent_matches(intent: &Value, path: Option<&str>, symbols: &BTreeSet<String>) -> bool {
    if let Some(path) = path {
        let covers = intent
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| paths.iter().any(|p| p.as_str() == Some(path)))
            .unwrap_or(false);
        if covers {
            return true;
        }
    }
    if symbols.is_empty() {
        return false;
    }
    intent
        .get("symbols")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).any(|n| symbols.contains(n)))
        .unwrap_or(false)
}

pub struct CheckInput<'a> {
    pub scope: &'a str,
    pub user_id: &'a str,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub idle_after_s: f64,
    /// The workspace scopes this caller may read (`access::visible_scopes`):
    /// live intents fan across them, so a teammate's declaration in the other
    /// repo of the workspace is in this repo's `intents`. The merkle
    /// comparison stays per repo — trees are the unit of code.
    pub visible: BTreeSet<String>,
}

/// The deepest directory (with trailing slash) every path lies under; ""
/// when they share none. Python: merkle.common_dir.
pub fn common_dir(paths: &BTreeSet<String>) -> String {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    let Some(first) = sorted.first() else { return String::new() };
    let mut parts: Vec<&str> = first.split('/').collect();
    parts.pop();
    for path in sorted.iter().skip(1) {
        while !parts.is_empty() && !path.starts_with(&format!("{}/", parts.join("/"))) {
            parts.pop();
        }
    }
    if parts.is_empty() { String::new() } else { format!("{}/", parts.join("/")) }
}

pub fn check_collisions(store: &Store, input: &CheckInput) -> Value {
    let stamp = now();
    let wanted_paths: BTreeSet<String> = input.paths.iter().cloned().collect();
    let wanted_symbols: BTreeSet<String> = input.symbols.iter().cloned().collect();

    let viewer_tree = store
        .get_tree(input.scope, input.user_id)
        .map(|value| Tree::from_json(&value))
        .unwrap_or_else(Tree::empty);
    let intents = visible_intents(store, input.scope, &input.visible, input.idle_after_s);

    const INTENT_KEYS: [&str; 12] = [
        "intent_id", "owner", "paths", "symbols", "change_type", "before", "after",
        "summary", "idle", "ttl_remaining_s", "agent", "operations",
    ];
    let relevant: Vec<Value> = intents
        .iter()
        .filter(|intent| {
            if wanted_paths.is_empty() && wanted_symbols.is_empty() {
                return true;
            }
            // a path is relative to its own repo, so a path match means
            // something only for an intent in THIS repo — "src/app.py" over
            // there is not the caller's "src/app.py". A symbol name travels.
            let local = intent.get("from_repo").is_none();
            (local && wanted_paths.iter().any(|p| intent_matches(intent, Some(p), &BTreeSet::new())))
                || intent_matches(intent, None, &wanted_symbols)
        })
        .map(|intent| {
            let mut row = project(intent, &INTENT_KEYS);
            if let Some(map) = row.as_object_mut() {
                map.insert("ref".into(), intent.get("ref").cloned().unwrap_or(json!("")));
                if let Some(from_repo) = intent.get("from_repo") {
                    map.insert("from_repo".into(), from_repo.clone());
                }
            }
            row
        })
        .collect();

    let mut collisions: Vec<Value> = Vec::new();
    let mut compared = 0;
    let mut fast_path_hits = 0;
    // "has anything under my subtree changed" is one compare: the common
    // directory of the wanted paths bounds every collision they could have,
    // so trees that agree on that subtree are done before any per-symbol
    // work — the empty prefix is the whole-repo fast path
    let prefix = common_dir(&wanted_paths);
    let viewer_sub = viewer_tree.subtree_root(&prefix);

    for workspace in store.list_workspaces(input.scope) {
        let other_user = text(&workspace, "user");
        if other_user.is_empty() || other_user == input.user_id {
            continue;
        }
        compared += 1;
        let other_tree = store
            .get_tree(input.scope, &other_user)
            .map(|value| Tree::from_json(&value))
            .unwrap_or_else(Tree::empty);
        if other_tree.root == viewer_tree.root || other_tree.subtree_root(&prefix) == viewer_sub {
            fast_path_hits += 1; // one comparison and we are done
            continue;
        }
        for path in Tree::diverging_files(&viewer_tree, &other_tree) {
            if !wanted_paths.is_empty() && !wanted_paths.contains(&path) {
                continue; // exact path matching only
            }
            let Some(viewer_record) = store.get_file(input.scope, input.user_id, &path) else {
                continue;
            };
            let Some(viewer_served) = semantics::serve(Some(&viewer_record), stamp) else {
                continue; // no clean baseline of our own: stay silent
            };
            let Some(other_record) = store.get_file(input.scope, &other_user, &path) else {
                continue;
            };
            for mut entry in diff_against(&viewer_served.symbols, &other_record, stamp) {
                let mut names = BTreeSet::new();
                names.insert(text(&entry, "symbol"));
                if let Some(detail) = entry.get("detail").filter(|d| d.is_object()) {
                    let renamed = text(detail, "new_name");
                    if !renamed.is_empty() {
                        names.insert(renamed);
                    }
                }
                if !wanted_symbols.is_empty()
                    && names.intersection(&wanted_symbols).next().is_none()
                {
                    continue; // exact symbol matching only
                }
                // a covering intent is a declaration about THIS repo's
                // path, so only the local ones are candidates — the merkle
                // path is per repo and stays that way
                let covering: Vec<Value> = intents
                    .iter()
                    .filter(|intent| {
                        intent.get("from_repo").is_none()
                            && text(intent, "owner") == other_user
                            && intent_matches(intent, Some(&path), &names)
                    })
                    .map(|intent| {
                        project(intent, &["intent_id", "change_type", "before", "after",
                                          "summary", "idle", "agent"])
                    })
                    .collect();
                if let Some(map) = entry.as_object_mut() {
                    map.insert("path".into(), json!(path));
                    map.insert("author".into(), json!(other_user));
                    map.insert("intents".into(), json!(covering));
                }
                collisions.push(entry);
            }
        }
    }

    let mut salience = Map::new();
    for wanted in wanted_paths.iter().take(MAX_SALIENCE_PATHS) {
        if let Some(record) =
            store.kv_get("salience", &format!("{}:{}", input.scope, path_key(wanted)))
        {
            salience.insert(
                wanted.clone(),
                record.get("score").cloned().unwrap_or(json!(0)),
            );
        }
    }

    json!({
        "collisions": collisions,
        "intents": relevant,
        "root": viewer_tree.root,
        "subtree": {"prefix": prefix, "root": viewer_sub},
        "workspaces_compared": compared,
        "fast_path_hits": fast_path_hits,
        "salience": salience,
    })
}
