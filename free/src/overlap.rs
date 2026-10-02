//! Two tasks on the same code, said before either writes.
//!
//! Three agents on one repo: one fixing the revenue logic in
//! `settled_total`, one asked to rename it. The renamer was told only that
//! another session was "reading daybook.py" and went ahead; the fixer found
//! out from the rename itself and redid part of it. Neither needs to wait:
//! the rename is small and lands fast, the fix builds on it. What they need
//! is to know, up front, that the work is split. So when a task's code (what
//! its prompt names, or its closest match by meaning) is code another live
//! agent has read or written in the last minutes, both are told: do only
//! your part, make it fit theirs, leave theirs to them.

use serde_json::Value;

use crate::store::Store;

/// A match by meaning this close counts as the task's code.
const STRONG_MEANING: f64 = 0.80;
/// How recently the other agent must have read or written the file.
const LIVE_S: f64 = 1_200.0;

/// Files the task is about: the prompt's named symbols and files, and its
/// closest match by meaning when that is strong.
fn targets(store: &Store, scope: &str, named: &[String], meant: &[Value]) -> Vec<(String, String)> {
    let graph = crate::graphview::snapshot(store, scope);
    let mut out: Vec<(String, String)> = Vec::new();
    for name in named {
        let found = if name.contains('/') || name.ends_with(".py") || name.ends_with(".rs") || name.ends_with(".ts") {
            crate::graphview::find_node(&graph, name, "").map(|nid| (crate::graphview::split_node(&nid).0.to_string(), String::new()))
        } else {
            crate::graphview::find_node(&graph, "", name).map(|nid| {
                let (p, s) = crate::graphview::split_node(&nid);
                (p.to_string(), s.to_string())
            })
        };
        if let Some(pair) = found.filter(|(p, _)| !p.is_empty() && !crate::graphview::is_test_path(p)) {
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    if let Some(top) = meant.first() {
        let score = top.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        let path = top.get("path").and_then(Value::as_str).unwrap_or("");
        let symbol = top.get("symbol").and_then(Value::as_str).unwrap_or("");
        if score >= STRONG_MEANING && !path.is_empty() && !crate::graphview::is_test_path(path) {
            let pair = (path.to_string(), if symbol == crate::embed::MODULE { String::new() } else { symbol.to_string() });
            if !out.iter().any(|(p, _)| p == path) {
                out.push(pair);
            }
        }
    }
    out
}

fn shown(path: &str, symbol: &str) -> String {
    if symbol.is_empty() { path.to_string() } else { format!("{path}::{symbol}") }
}

/// The line for this session's briefing, when its task overlaps another live
/// agent's work; that agent gets a notice on its next step.
pub fn announce(store: &Store, scope: &str, user: &str, session: &str, named: &[String], meant: &[Value]) -> Option<String> {
    if session.is_empty() {
        return None;
    }
    let targets = targets(store, scope, named, meant);
    if targets.is_empty() {
        return None;
    }
    let paths: Vec<String> = targets.iter().map(|(p, _)| p.clone()).collect();
    let others = crate::freshness::working_in(store, scope, user, session, &paths, LIVE_S);
    if others.is_empty() {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    for (other_user, other_session, files) in &others {
        let who = crate::inflight::author_label(store, user, other_user);
        let about: Vec<String> = targets.iter().filter(|(p, _)| files.contains(p)).map(|(p, s)| shown(p, s)).collect();
        let me = crate::inflight::author_label(store, other_user, user);
        crate::inflight::queue_notice(
            store, scope, other_session, &format!("overlap|{user}|{session}|{}", about.join(",")),
            &format!(
                "Collide: {me} just started a task on code you are working in ({}). Align with them: finish only your \
part, make it fit theirs, and do not redo their part (Collide tells you when theirs lands).",
                about.join(", ")
            ),
            user, session, files,
        );
        names.push(format!("{who} ({})", files.join(", ")));
    }
    Some(format!(
        "Overlap: {} {} working in the code your task is about, right now. Align with them: do only your task's part, \
make it fit theirs, and leave theirs to them; they have been told the same.",
        names.join("; "),
        if others.len() == 1 { "is" } else { "are" }
    ))
}
