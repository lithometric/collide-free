//! Line-level change ranges, computed from per-line hashes.
//!
//! The previous source was discarded the moment it was parsed, so "what
//! changed" is answered by diffing the hashes of the old lines against the
//! new ones. Snippets come from the NEW content only and live in TTL'd
//! ephemeral events; the durable ledger keeps ranges alone.
//!
//! The matcher is a faithful port of CPython's `difflib.SequenceMatcher` with
//! `autojunk=False` and no junk predicate. That is not a stylistic choice:
//! hunks land in the ledger and on the dashboard's change pages, so a
//! different-but-reasonable diff algorithm would make the two servers
//! disagree about what an edit did.

use std::collections::HashMap;

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

pub const MAX_HUNKS: usize = 8;
pub const MAX_HUNK_SNIPPET: usize = 4;
pub const MAX_SNIPPET_LINE: usize = 160;

pub fn line_hashes(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let mut hasher = Sha1::new();
            hasher.update(line.as_bytes());
            format!("{:x}", hasher.finalize())[..8].to_string()
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Match {
    a: usize,
    b: usize,
    size: usize,
}

/// CPython's `find_longest_match`, minus the junk handling that a `None`
/// predicate makes unreachable.
fn find_longest_match(
    a: &[String], b: &[String], b2j: &HashMap<&str, Vec<usize>>,
    alo: usize, ahi: usize, blo: usize, bhi: usize,
) -> Match {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for i in alo..ahi {
        let mut newj2len: HashMap<usize, usize> = HashMap::new();
        if let Some(indices) = b2j.get(a[i].as_str()) {
            for &j in indices {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|prev| j2len.get(&prev)).copied().unwrap_or(0) + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = newj2len;
    }
    while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }
    Match { a: besti, b: bestj, size: bestsize }
}

fn matching_blocks(a: &[String], b: &[String]) -> Vec<Match> {
    let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, item) in b.iter().enumerate() {
        b2j.entry(item.as_str()).or_default().push(index);
    }
    let (la, lb) = (a.len(), b.len());
    let mut queue = vec![(0usize, la, 0usize, lb)];
    let mut blocks: Vec<Match> = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let found = find_longest_match(a, b, &b2j, alo, ahi, blo, bhi);
        if found.size > 0 {
            blocks.push(found);
            if alo < found.a && blo < found.b {
                queue.push((alo, found.a, blo, found.b));
            }
            if found.a + found.size < ahi && found.b + found.size < bhi {
                queue.push((found.a + found.size, ahi, found.b + found.size, bhi));
            }
        }
    }
    blocks.sort_by_key(|m| (m.a, m.b, m.size));

    // merge adjacent blocks, then the terminating sentinel
    let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
    let mut merged: Vec<Match> = Vec::new();
    for block in blocks {
        if i1 + k1 == block.a && j1 + k1 == block.b {
            k1 += block.size;
        } else {
            if k1 > 0 {
                merged.push(Match { a: i1, b: j1, size: k1 });
            }
            i1 = block.a;
            j1 = block.b;
            k1 = block.size;
        }
    }
    if k1 > 0 {
        merged.push(Match { a: i1, b: j1, size: k1 });
    }
    merged.push(Match { a: la, b: lb, size: 0 });
    merged
}

pub struct Opcode {
    pub tag: &'static str,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

pub fn opcodes(a: &[String], b: &[String]) -> Vec<Opcode> {
    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    for block in matching_blocks(a, b) {
        let tag = if i < block.a && j < block.b {
            Some("replace")
        } else if i < block.a {
            Some("delete")
        } else if j < block.b {
            Some("insert")
        } else {
            None
        };
        if let Some(tag) = tag {
            out.push(Opcode { tag, i1: i, i2: block.a, j1: j, j2: block.b });
        }
        i = block.a + block.size;
        j = block.b + block.size;
        if block.size > 0 {
            out.push(Opcode { tag: "equal", i1: block.a, i2: i, j1: block.b, j2: j });
        }
    }
    out
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Change ranges between the previous and current report of a file. First
/// sight yields nothing — there is nothing to compare against.
pub fn hunks(old_hashes: &[String], new_hashes: &[String], new_lines: &[&str]) -> Vec<Value> {
    if old_hashes.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Value> = Vec::new();
    for code in opcodes(old_hashes, new_hashes) {
        if code.tag == "equal" {
            continue;
        }
        // no source on this server (a local parse): the range, no quote
        let lines: Vec<String> = new_lines
            .get(code.j1..code.j2)
            .unwrap_or(&[])
            .iter()
            .take(MAX_HUNK_SNIPPET)
            .map(|line| clip(line.trim_end(), MAX_SNIPPET_LINE))
            .collect();
        out.push(json!({
            "op": code.tag,
            "old_start": code.i1 + 1, "old_end": code.i2,
            "new_start": code.j1 + 1, "new_end": code.j2,
            "lines": lines,
        }));
        // the marker goes on at exactly MAX_HUNKS, not only past it — the
        // Python original appends and then breaks, so a file with exactly
        // eight hunks still says it was cut
        if out.len() >= MAX_HUNKS {
            out.push(json!({"op": "truncated"}));
            break;
        }
    }
    out
}

pub fn hunk_totals(hunks: &[Value]) -> (i64, i64) {
    let read = |hunk: &Value, key: &str, default: i64| {
        hunk.get(key).and_then(Value::as_i64).unwrap_or(default)
    };
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        let op = hunk.get("op").and_then(Value::as_str).unwrap_or("");
        if op == "replace" || op == "insert" {
            added += read(hunk, "new_end", 0) - read(hunk, "new_start", 1) + 1;
        }
        if op == "replace" || op == "delete" {
            removed += read(hunk, "old_end", 0) - read(hunk, "old_start", 1) + 1;
        }
    }
    (added, removed)
}
