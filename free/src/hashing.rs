//! Hashes that must match the Python server byte for byte.
//!
//! These are not internal details: the ledger chain hash is what makes the
//! record tamper-evident, and the merkle root is what two workspaces compare
//! to decide whether they have diverged. If the two implementations disagree
//! on either, the chain reads as broken and every collision check goes wrong.
//! `tests/test_rust_server.py` computes both in Python and in Rust over the
//! same inputs and asserts they are identical.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn empty_root() -> String {
    sha256_hex("collide:empty")
}

/// One JSON string, escaped the way Python's `json.dumps` escapes it.
///
/// Python defaults to `ensure_ascii=True`, which emits every character
/// outside printable ASCII as `\uXXXX` (a surrogate pair above U+FFFF).
/// serde emits raw UTF-8. For anything that is merely displayed that
/// difference is invisible; for the ledger's chain hash it is the difference
/// between a verified chain and a broken one, because the two servers share
/// one database and each re-verifies rows the other wrote. A single em dash
/// in an agent's summary, or an accent in a path, was enough — and every
/// fixture in the parity suite was pure ASCII, so nothing caught it.
///
/// `graphview::cypher_str` is the same transform for the Cypher exporter; it
/// is kept separate only because that module is the one that discovered the
/// problem, and unifying them is a refactor for its own commit.
pub fn ascii_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}", 0xd800 + (v >> 10), 0xdc00 + (v & 0x3ff)));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`, `ensure_ascii`
/// and all.
///
/// The sort is explicit and recursive. It used to lean on serde_json's Map
/// being a BTreeMap, which quietly stopped being true the moment insertion
/// order was preserved for the artifacts written to users' repos — and the
/// chain hash is the one place where "sorted" is a correctness property
/// rather than a formatting choice.
pub fn canonical_json(value: &serde_json::Value) -> String {
    compact(&sorted(value))
}

/// The compact separators, with Python's string escaping. Numbers, booleans
/// and null are left to serde, which agrees with Python across the integer
/// range and the epoch-scale floats these payloads actually carry.
fn compact(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let inner = map
                .iter()
                .map(|(key, item)| format!("{}:{}", ascii_string(key), compact(item)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        serde_json::Value::Array(items) => {
            format!("[{}]", items.iter().map(compact).collect::<Vec<_>>().join(","))
        }
        serde_json::Value::String(text) => ascii_string(text),
        other => other.to_string(),
    }
}

/// Keys sorted recursively — Python's `sort_keys=True`.
pub(crate) fn sorted(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sorted).collect())
        }
        other => other.clone(),
    }
}

/// One link of the ledger's hash chain.
pub fn chain_hash(
    prev_hash: &str,
    scope: &str,
    ts: f64,
    kind: &str,
    payload: &serde_json::Value,
) -> String {
    let body = canonical_json(&serde_json::json!({
        "scope": scope,
        "ts": ts,
        "kind": kind,
        "payload": payload,
    }));
    sha256_hex(&format!("{prev_hash}{body}"))
}

/// The same link as this half computed it before 2026-09-22 (b69c97a):
/// sorted keys, compact separators, but serde's raw UTF-8 instead of
/// Python's `\uXXXX` escapes. Rows written in that window carry hashes over
/// that encoding, so re-verifying them with [`chain_hash`] alone reports the
/// chain broken at the first one whose payload had a non-ASCII character —
/// an em dash in a summary was enough. Verification accepts either encoding
/// for a row's OWN hash (the link to the previous row stays exact), because
/// re-writing history to fix an encoder is the one thing a hash chain must
/// never do. Nothing writes this encoding any more; it exists only to read.
/// Python's `legacy_chain_hash`.
pub fn legacy_chain_hash(
    prev_hash: &str,
    scope: &str,
    ts: f64,
    kind: &str,
    payload: &serde_json::Value,
) -> String {
    let body = serde_json::to_string(&sorted(&serde_json::json!({
        "scope": scope,
        "ts": ts,
        "kind": kind,
        "payload": payload,
    })))
    .unwrap_or_default();
    sha256_hex(&format!("{prev_hash}{body}"))
}

#[cfg(test)]
mod legacy_link_tests {
    use super::{chain_hash, legacy_chain_hash};
    use serde_json::json;

    /// The two encodings agree on ASCII and part the moment a character
    /// needs escaping — the em dash that first broke the badge.
    #[test]
    fn the_legacy_link_differs_only_where_escaping_does() {
        let prev = "0".repeat(64);
        let ascii = json!({"user": "bob", "path": "a.py"});
        assert_eq!(chain_hash(&prev, "ws:r", 1.5, "k", &ascii), legacy_chain_hash(&prev, "ws:r", 1.5, "k", &ascii));
        let dash = json!({"summary": "renamed get_symbol — see notes"});
        assert_ne!(chain_hash(&prev, "ws:r", 1.5, "k", &dash), legacy_chain_hash(&prev, "ws:r", 1.5, "k", &dash));
    }
}

/// Roll symbol hashes up into one file hash, order-independent.
pub fn file_hash(symbols: &BTreeMap<String, String>) -> String {
    let joined = symbols
        .iter()
        .map(|(name, hash)| format!("{name}:{hash}"))
        .collect::<Vec<_>>()
        .join("\n");
    sha256_hex(&joined)
}

pub fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => path[..index].to_string(),
        None => ".".to_string(),
    }
}

// Phase 1 uses part of this surface; the rest is what the endpoints
// still to move across will call. Kept whole so the tier is reviewable
// against the Python driver side by side rather than in fragments.
#[allow(dead_code)]
pub struct Tree {
    pub root: String,
    pub dirs: BTreeMap<String, String>,
    pub files: BTreeMap<String, String>,
}

#[allow(dead_code)]
impl Tree {
    pub fn empty() -> Self {
        Tree { root: empty_root(), dirs: BTreeMap::new(), files: BTreeMap::new() }
    }

    /// `files` maps path -> file hash.
    pub fn build(files: BTreeMap<String, String>) -> Self {
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, hash) in &files {
            grouped.entry(dir_of(path)).or_default().push(format!("{path}:{hash}"));
        }
        let dirs: BTreeMap<String, String> = grouped
            .into_iter()
            .map(|(dir, entries)| (dir, sha256_hex(&entries.join("\n"))))
            .collect();
        let root = if dirs.is_empty() {
            empty_root()
        } else {
            sha256_hex(
                &dirs
                    .iter()
                    .map(|(dir, hash)| format!("{dir}:{hash}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        Tree { root, dirs, files }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"root": self.root, "dirs": self.dirs, "files": self.files})
    }

    /// The root of the files under one path prefix: "has anything under
    /// `services/` changed" as a single compare, whatever the repo's size.
    /// Same recipe as the dir hashes (sorted `path:hash` lines), so the
    /// empty prefix is a root over every file and two trees agreeing on a
    /// subtree agree on every symbol in it. Python: merkle.subtree_root.
    pub fn subtree_root(&self, prefix: &str) -> String {
        let entries: Vec<String> = self
            .files
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(path, hash)| format!("{path}:{hash}"))
            .collect();
        if entries.is_empty() {
            empty_root()
        } else {
            sha256_hex(&entries.join("\n"))
        }
    }

    /// Walk only diverging branches. Roots equal means an empty answer after
    /// one comparison, which is what makes a collision check cheap in the
    /// common case where two workspaces agree.
    pub fn diverging_files(a: &Tree, b: &Tree) -> Vec<String> {
        if a.root == b.root {
            return Vec::new();
        }
        let mut diverging_dirs: BTreeMap<&str, ()> = BTreeMap::new();
        for dir in a.dirs.keys().chain(b.dirs.keys()) {
            if a.dirs.get(dir) != b.dirs.get(dir) {
                diverging_dirs.insert(dir.as_str(), ());
            }
        }
        let mut out: Vec<String> = Vec::new();
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        for path in a.files.keys().chain(b.files.keys()) {
            if seen.insert(path.as_str(), ()).is_some() {
                continue;
            }
            if diverging_dirs.contains_key(dir_of(path).as_str())
                && a.files.get(path) != b.files.get(path)
            {
                out.push(path.clone());
            }
        }
        out.sort();
        out
    }

    pub fn from_json(value: &serde_json::Value) -> Self {
        let read = |key: &str| -> BTreeMap<String, String> {
            value
                .get(key)
                .and_then(|v| v.as_object())
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        Tree {
            root: value.get("root").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            dirs: read("dirs"),
            files: read("files"),
        }
    }
}

#[cfg(test)]
mod subtree_tests {
    use super::*;

    fn tree(entries: &[(&str, &str)]) -> Tree {
        Tree::build(entries.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect())
    }

    #[test]
    fn a_change_outside_the_prefix_leaves_the_subtree_root_alone() {
        let a = tree(&[("services/svc0.py", "h1"), ("services/svc1.py", "h2"), ("jobs/job0.py", "h3")]);
        let b = tree(&[("services/svc0.py", "h1"), ("services/svc1.py", "h2"), ("jobs/job0.py", "CHANGED")]);
        assert_ne!(a.root, b.root);
        assert_eq!(a.subtree_root("services/"), b.subtree_root("services/"));
        assert_ne!(a.subtree_root("jobs/"), b.subtree_root("jobs/"));
        assert_ne!(a.subtree_root(""), b.subtree_root(""));
    }

    #[test]
    fn a_change_inside_the_prefix_moves_it() {
        let a = tree(&[("services/svc0.py", "h1"), ("services/svc1.py", "h2")]);
        let b = tree(&[("services/svc0.py", "h1"), ("services/svc1.py", "h9")]);
        assert_ne!(a.subtree_root("services/"), b.subtree_root("services/"));
        assert_eq!(a.subtree_root("nothing/"), empty_root());
    }
}
