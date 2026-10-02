//! The briefing the hooks print into the agent's context.
//!
//! Two scopes, one renderer. At session start the task is unknown, so the
//! briefing is by recency: the modules changed in the last week, newest
//! first. At prompt submit the task is known, so the hook sends the
//! identifiers it found in the prompt (never the prose) and the briefing
//! narrows to the modules those name — by path, by symbol, by docstring —
//! plus the modules that depend on the matched symbols, ranked, under a
//! byte budget, with a coverage line that names what it could not fit and
//! which identifiers matched nothing. The agent starts knowing the code its
//! ticket touches, and knows the edge of what it was told.
//!
//! Every module renders the same way: exact signatures and docstrings from
//! the parser, notes earlier agents anchored, and whether the repo check
//! passed after it was written. Served to the hook only, like `/observe`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{json, Value};

use crate::store::{now, Store};

const DEFAULT_WINDOW_S: f64 = 7.0 * 86_400.0;
const DEFAULT_BUDGET: usize = 6_000;
const MAX_BUDGET: usize = 24_000;
const MAX_NOTES: usize = 5;
/// A note below this confidence (reality-scored, see memory::outcomes) is
/// not served in briefings; get_symbol still returns it with its score.
const SERVE_CONFIDENCE: f64 = 0.25;
/// A note is a pointer, not the whole memory: the rest is one get_symbol away.
const MAX_NOTE_CHARS: usize = 240;
/// A docstring in a block is the convention, not the manual: clipped on a
/// word boundary, the whole text one get_symbol away.
const MAX_MODULE_DOC_CHARS: usize = 300;
const MAX_SYMBOL_DOC_CHARS: usize = 200;
/// Tenths of a block's cap the symbol lines may use when the module has notes.
const SYMBOL_SHARE: usize = 6;
const MAX_REST: usize = 20;
const MAX_IDENTIFIERS: usize = 40;
/// A module is briefed only on a strong match (path, module name, or an
/// exact symbol); docstring and partial-name hits only rank it.
const STRONG_MATCH: i64 = 3;
/// One module's block never eats the whole budget.
const MAX_BLOCK_SHARE: usize = 3;
const MIN_BLOCK: usize = 1_500;
const MAX_SYMBOL_LINES: usize = 40;
/// Partial-name and docstring hits count at most this much per module.
const WEAK_CAP: i64 = 2;
/// A symbol name defined in more modules than this is a common name, not a match.
const COMMON_NAME: usize = 5;
/// A module that depends on a matched symbol outranks one that merely shares a name.
const DEPENDENT_BONUS: i64 = 4;
/// An exact symbol match outranks a dependent, which outranks a name-sharer.
const EXACT_SYMBOL: i64 = 5;
/// A second-hop dependent (calls something that calls the symbol).
const DEPENDENT2_BONUS: i64 = 3;
const MAX_DEPENDENT_MODULES: usize = 40;
/// At most this many "right now" lines: who is live, not the whole roster.
const MAX_RIGHT_NOW: usize = 5;
/// The repo on one page, at session start: never more than this many bytes,
/// never more than a third of the budget, skipped when the budget is tiny.
const SHAPE_CAP: usize = 1_500;
const MIN_SHAPE: usize = 200;
const SHAPE_ITEMS: usize = 6;
const SHAPE_IDLE_S: f64 = 900.0;
/// After a compaction the working set may take up to half the budget.
const MAX_RESUME_SHARE: usize = 2;

pub struct BriefInput<'a> {
    pub repo_id: &'a str,
    pub since: f64,
    pub budget: usize,
    pub identifiers: &'a [String],
    pub exclude: &'a [String],
    /// The hook's session, so a resume can lead with its working set.
    pub session: &'a str,
    /// SessionStart after a compaction or a resume: the context the agent had
    /// is gone, so the briefing leads with what this session was working on.
    pub resume: bool,
    /// The roster's idle threshold, for the "right now" lines: an agent
    /// quiet longer than this is not working right now.
    pub idle_after_s: f64,
    /// The workspace scopes this caller may read (`access::visible_scopes`):
    /// recipes are offered across the workspace, through this allowlist.
    pub visible: &'a std::collections::BTreeSet<String>,
    /// Who is reading, and whether the plan shares teammates' knowledge
    /// (limits.knowledge_sharing): when it does not, the notes and recipes
    /// rendered are the reader's own — the modules, signatures and who
    /// changed them are the radar and always shown.
    pub viewer: &'a str,
    pub share_knowledge: bool,
}

struct Changed {
    ts: f64,
    user: String,
    agent: String,
    /// What changed, per symbol, newest first: `def serve(x) → def serve(x, region)`.
    changes: Vec<String>,
    /// Why, in the author's words, when they said: the newest reason on
    /// the path, from the edit row or a rationale row that followed it.
    why: String,
    why_by: String,
}
const MAX_CHANGES: usize = 4;
const MAX_CHANGE_LINE: usize = 300;

/// path -> latest write in the window (who, when)
fn recent_writes(store: &Store, scope: &str, since: f64) -> BTreeMap<String, Changed> {
    let mut rows = store.ledger_since(scope, since);
    rows.retain(|row| row.kind == "edit_reported" || row.kind == "rationale");
    rows.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: BTreeMap<String, Changed> = BTreeMap::new();
    let mut seen_symbols: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let path = row.payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() || crate::codegraph::is_collide_artifact(&path) {
            continue;
        }
        // the signature changes this row recorded, newest row first, one line per symbol
        let seen = seen_symbols.entry(path.clone()).or_default();
        let mut changes: Vec<String> = Vec::new();
        for event in row.payload.get("events").and_then(Value::as_array).into_iter().flatten() {
            let symbol = event.get("symbol").and_then(Value::as_str).unwrap_or("");
            if symbol.is_empty() || seen.contains(symbol) {
                continue;
            }
            let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
            let before = event.get("signature_before").and_then(Value::as_str).unwrap_or("");
            let after = event.get("signature_after").and_then(Value::as_str).unwrap_or("");
            let new_name = event.get("new_name").and_then(Value::as_str).unwrap_or("");
            let line = if kind == "renamed" && !new_name.is_empty() {
                format!("{symbol} renamed → {new_name}")
            } else if kind == "removed" {
                format!("{symbol} removed")
            } else if !before.is_empty() && !after.is_empty() && before != after {
                format!("{before} → {after}")
            } else if kind == "added" && !after.is_empty() {
                format!("+ {after}")
            } else {
                continue; // a body-only edit: the signature the agent has is still right
            };
            seen.insert(symbol.to_string());
            changes.push(line);
        }
        let why = row.payload.get("why").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let user = row.payload.get("user").and_then(Value::as_str).unwrap_or("").to_string();
        match out.get_mut(&path) {
            Some(existing) => {
                for line in changes {
                    if existing.changes.len() < MAX_CHANGES {
                        existing.changes.push(line);
                    }
                }
                if existing.why.is_empty() && !why.is_empty() {
                    existing.why = why;
                    existing.why_by = user.clone();
                }
                // a rationale row came first (newest): the write behind it is
                // still the latest write, with its author and time
                if existing.ts == 0.0 && row.kind == "edit_reported" {
                    existing.ts = row.ts;
                    existing.user = user;
                    existing.agent = row.payload.get("agent").and_then(Value::as_str).unwrap_or("").to_string();
                }
            }
            None => {
                changes.truncate(MAX_CHANGES);
                // a rationale row alone is not a write: it only carries the reason
                let is_write = row.kind == "edit_reported";
                out.insert(
                    path,
                    Changed {
                        ts: if is_write { row.ts } else { 0.0 },
                        user: if is_write { user.clone() } else { String::new() },
                        agent: if is_write { row.payload.get("agent").and_then(Value::as_str).unwrap_or("").to_string() } else { String::new() },
                        changes,
                        why,
                        why_by: user,
                    },
                );
            }
        }
    }
    out
}

/// The `changed:` line a recency block carries, and the author's reason
/// under it when they gave one, or nothing.
fn change_lines(recent: &BTreeMap<String, Changed>) -> BTreeMap<String, Vec<String>> {
    recent
        .iter()
        .filter(|(_, c)| !c.changes.is_empty() || !c.why.is_empty())
        .map(|(path, c)| {
            let mut lines = Vec::new();
            if !c.changes.is_empty() {
                lines.push(clip(&format!("  changed: {}", c.changes.join("; ")), MAX_CHANGE_LINE));
            }
            if !c.why.is_empty() {
                lines.push(clip(&format!("  why ({}): {}", c.why_by, c.why), MAX_CHANGE_LINE));
            }
            (path.clone(), lines)
        })
        .collect()
}

pub fn brief(store: &Store, scope: &str, input: &BriefInput) -> Value {
    let stamp = now();
    let since = if input.since > 0.0 { input.since } else { stamp - DEFAULT_WINDOW_S };
    let budget = if input.budget == 0 { DEFAULT_BUDGET } else { input.budget.min(MAX_BUDGET) };
    let exclude: HashSet<&str> = input.exclude.iter().map(String::as_str).collect();
    let recent = recent_writes(store, scope, since);

    if input.identifiers.is_empty() {
        // session start: the repo's shape on one page, then — after a
        // compaction — this session's working set, then what changed recently
        let shape = if budget / 3 >= MIN_SHAPE { render_shape(store, scope, SHAPE_CAP.min(budget / 3)) } else { String::new() };
        let mut used = if shape.is_empty() { 0 } else { shape.len() + 1 };
        let changed = change_lines(&recent);
        let mut resume_blocks: Vec<String> = Vec::new();
        let mut resume_paths: Vec<String> = Vec::new();
        if input.resume && !input.session.is_empty() {
            let working: Vec<(String, f64, String, String, String)> = crate::deltas::working_set(store, scope, input.session)
                .into_iter()
                .filter(|path| !exclude.contains(path.as_str()) && !crate::codegraph::is_collide_artifact(path))
                .map(|path| match recent.get(&path) {
                    Some(c) => (path.clone(), c.ts, c.user.clone(), c.agent.clone(), String::new()),
                    None => (path, 0.0, String::new(), String::new(), String::new()),
                })
                .collect();
            if !working.is_empty() {
                let (blocks, _rest, shown) = render_all(store, scope, &working, budget / MAX_RESUME_SHARE, stamp, &changed, input);
                used += blocks.iter().map(|b| b.len() + 1).sum::<usize>();
                resume_blocks = blocks;
                resume_paths = shown;
            }
        }
        // by recency: newest write first, minus what the resume already showed
        let mut order: Vec<(&String, &Changed)> = recent.iter().collect();
        order.sort_by(|a, b| b.1.ts.partial_cmp(&a.1.ts).unwrap_or(std::cmp::Ordering::Equal));
        let candidates: Vec<(String, f64, String, String, String)> = order
            .into_iter()
            .filter(|(path, _)| !exclude.contains(path.as_str()) && !resume_paths.contains(path))
            .map(|(path, c)| (path.clone(), c.ts, c.user.clone(), c.agent.clone(), String::new()))
            .collect();
        if candidates.is_empty() && shape.is_empty() && resume_blocks.is_empty() {
            return json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": []});
        }
        let total = candidates.len();
        // what the page and the resume did not use; the newest module always fits
        let remaining = budget.saturating_sub(used);
        let (blocks, rest, shown_paths) = if candidates.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            render_all(store, scope, &candidates, remaining, stamp, &changed, input)
        };
        let shown = blocks.len();
        let mut text = String::new();
        if !resume_blocks.is_empty() {
            text.push_str(&format!(
                "Resuming your session — {} module(s) in your working set, current facts first:",
                resume_blocks.len()
            ));
            for block in &resume_blocks {
                text.push('\n');
                text.push_str(block);
            }
            text.push('\n');
        }
        if !shape.is_empty() {
            text.push_str(&shape);
            text.push('\n');
        }
        if total > 0 {
            text.push_str(&format!(
                "Collide briefing — {}: {shown} of {total} module(s) changed recently, newest first. \
Signatures and docstrings are exact (parsed from the code); notes are what earlier agents recorded. \
Build on these instead of reading the files. get_symbol(repo_id, path) gives a module's full facts; \
blast_radius the dependents before you change a shared symbol.",
                input.repo_id
            ));
            let live = right_now(store, scope, input, stamp);
            if !live.is_empty() {
                text.push('\n');
                text.push_str(&live);
            }
            for block in blocks {
                text.push('\n');
                text.push_str(&block);
            }
            if !rest.is_empty() {
                let listed: Vec<&str> = rest.iter().take(MAX_REST).map(String::as_str).collect();
                text.push_str(&format!("\n… {} more changed: {}", rest.len(), listed.join(", ")));
            }
        } else {
            text.push_str(&format!(
                "Collide briefing — {}: no module changed recently. get_symbol(repo_id, path) gives any module's exact facts; \
blast_radius the dependents before you change a shared symbol.",
                input.repo_id
            ));
            let live = right_now(store, scope, input, stamp);
            if !live.is_empty() {
                text.push('\n');
                text.push_str(&live);
            }
        }
        // the one rule that makes the difference visible: the agent says,
        // in the terminal, what of this it acted on and whose it was
        text.push('\n');
        text.push_str(RULE);
        let mut paths = resume_paths.clone();
        paths.extend(shown_paths.iter().cloned());
        // the modules the tail only NAMED: the hook credits a write to one of
        // them as found, the briefing having said where to look
        let named: Vec<String> = rest.iter().take(MAX_REST).cloned().collect();
        return json!({"ok": true, "text": text, "modules": shown, "total": total, "paths": paths, "named": named,
                      "resumed": resume_paths.len(), "shape": !shape.is_empty()});
    }

    // scoped to the prompt: rank every module the identifiers touch
    let idents: Vec<String> = input
        .identifiers
        .iter()
        .map(|i| i.trim().to_string())
        .filter(|i| i.len() >= 2)
        .take(MAX_IDENTIFIERS)
        .collect();
    let lowered: Vec<String> = idents.iter().map(|i| i.to_lowercase()).collect();
    let mut scores: BTreeMap<String, (i64, BTreeSet<String>, f64)> = BTreeMap::new(); // path -> (score, why, record ts)
    let mut matched_ids: BTreeSet<String> = BTreeSet::new();
    let mut matched_symbols: Vec<(String, String)> = Vec::new(); // (path, symbol)
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    let records: Vec<Value> = store.kv_list(crate::codegraph::GRAPH_BUCKET, &format!("{scope}:")).into_iter().map(|(_, r)| r).collect();
    let mut defined_in: BTreeMap<String, usize> = BTreeMap::new();
    for record in &records {
        if let Some(symbols) = record.get("symbols").and_then(Value::as_object) {
            for ident_l in &lowered {
                if symbols.keys().any(|name| name.to_lowercase() == *ident_l) {
                    *defined_in.entry(ident_l.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    for record in &records {
        let path = record.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        all_dirs.insert(crate::graphview::top_dir(&path));
        let path_l = path.to_lowercase();
        let basename = path_l.rsplit('/').next().unwrap_or(&path_l).to_string();
        let stem = basename.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(basename.clone());
        let module_doc = record.get("doc").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let symbols = record.get("symbols").and_then(Value::as_object);
        let mut score = 0i64;
        let mut weak = 0i64;
        let mut why: BTreeSet<String> = BTreeSet::new();
        for (ident, ident_l) in idents.iter().zip(lowered.iter()) {
            // a path or module name
            if ident_l.contains('/') || ident_l.contains('.') {
                let as_path = ident_l.trim_start_matches("./").to_string();
                // the identifier IS the path, or the repo path is its tail —
                // and the reverse, because an agent pastes the absolute path
                // its editor shows it and the map stores the relative one
                if path_l == as_path
                    || path_l.ends_with(&format!("/{as_path}"))
                    || as_path.ends_with(&format!("/{path_l}"))
                {
                    score += 6;
                    why.insert(ident.clone());
                    matched_ids.insert(ident.clone());
                    continue;
                }
                let dotted = ident_l.replace('.', "/");
                if path_l.trim_end_matches(".py").ends_with(&dotted) || path_l.contains(&format!("/{dotted}/")) {
                    score += 4;
                    why.insert(ident.clone());
                    matched_ids.insert(ident.clone());
                    continue;
                }
            }
            if stem == *ident_l {
                score += 3;
                why.insert(ident.clone());
                matched_ids.insert(ident.clone());
            }
            if let Some(symbols) = symbols {
                for (name, entry) in symbols {
                    let name_l = name.to_lowercase();
                    if name_l == *ident_l {
                        // a name defined in many modules is common, not a match
                        if defined_in.get(ident_l).copied().unwrap_or(0) <= COMMON_NAME {
                            score += EXACT_SYMBOL;
                        } else {
                            weak += 1;
                        }
                        why.insert(ident.clone());
                        matched_ids.insert(ident.clone());
                        matched_symbols.push((path.clone(), name.clone()));
                    } else if ident_l.len() >= 4 && name_l.contains(ident_l.as_str()) {
                        weak += 1;
                        why.insert(ident.clone());
                        matched_ids.insert(ident.clone());
                    }
                    if ident_l.len() >= 4 {
                        if let Some(doc) = entry.get("doc").and_then(Value::as_str) {
                            if doc.to_lowercase().contains(ident_l.as_str()) {
                                weak += 1;
                                why.insert(ident.clone());
                                matched_ids.insert(ident.clone());
                            }
                        }
                    }
                }
            }
            if ident_l.len() >= 4 && module_doc.contains(ident_l.as_str()) {
                weak += 1;
                why.insert(ident.clone());
                matched_ids.insert(ident.clone());
            }
        }
        score += weak.min(WEAK_CAP);
        if score > 0 {
            let ts = record.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            scores.insert(path, (score, why, ts));
        }
    }
    // a docstring word or a partial name alone is a guess, not a match
    scores.retain(|_, (score, _, _)| *score >= STRONG_MATCH);
    matched_symbols.retain(|(path, _)| scores.contains_key(path));
    // the modules that depend on a matched symbol break when it changes —
    // and where nothing does is the negative fact that saves the search.
    // Dependents come from the resolved snapshot (the same graph blast_radius
    // walks), so an aliased import (`serve as serve_a`) counts as a caller.
    let graph = crate::graphview::snapshot(store, scope);
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dependent_modules: BTreeSet<String> = BTreeSet::new();
    let mut capped = false;
    let mut with_callers: BTreeSet<String> = BTreeSet::new();
    for (path, symbol) in &matched_symbols {
        let nid = format!("{path}::{symbol}");
        let levels = crate::graphview::blast_radius(&graph, &nid, 2);
        let mut caller_dirs: BTreeSet<String> = BTreeSet::new();
        let mut direct = 0usize;
        for (depth, level) in levels.iter().enumerate() {
            for node in level {
                let dep_path = node.get("path").and_then(Value::as_str).unwrap_or("");
                if dep_path.is_empty() || dep_path == path {
                    continue;
                }
                if depth == 0 {
                    direct += 1;
                    caller_dirs.insert(crate::graphview::top_dir(dep_path));
                }
                if dependent_modules.len() >= MAX_DEPENDENT_MODULES && !dependent_modules.contains(dep_path) {
                    capped = true;
                    continue;
                }
                dependent_modules.insert(dep_path.to_string());
                let slot = scores.entry(dep_path.to_string()).or_insert((0, BTreeSet::new(), 0.0));
                if depth == 0 {
                    slot.0 += DEPENDENT_BONUS;
                    slot.1.insert(format!("calls {symbol}"));
                } else {
                    slot.0 += DEPENDENT2_BONUS;
                    slot.1.insert(format!("calls a caller of {symbol}"));
                }
            }
        }
        if direct > 0 {
            with_callers.insert(symbol.clone());
        }
        let absent: Vec<String> = all_dirs.difference(&caller_dirs).cloned().collect();
        let line = if direct == 0 {
            format!("  {symbol} ← no callers anywhere in the map")
        } else if absent.is_empty() {
            format!("  {symbol} ← called from {}", caller_dirs.iter().cloned().collect::<Vec<_>>().join(", "))
        } else {
            format!("  {symbol} ← called from {}; none under {}",
                caller_dirs.iter().cloned().collect::<Vec<_>>().join(", "), absent.join(", "))
        };
        extra.entry(path.clone()).or_default().push(line);
    }
    // what changed on a matched module, and why its author said, under the
    // dependents lines: the prompt briefing used to carry only the shape
    for (path, lines) in change_lines(&recent) {
        if scores.contains_key(&path) {
            extra.entry(path).or_default().extend(lines);
        }
    }
    let matched_ids: BTreeSet<String> = scores
        .values()
        .flat_map(|(_, why, _)| why.iter().cloned())
        .filter(|w| !w.starts_with("calls "))
        .collect();
    let unmatched: Vec<&str> = idents
        .iter()
        .filter(|i| !matched_ids.contains(*i) && looks_like_code(i))
        .map(String::as_str)
        .collect();
    if scores.is_empty() {
        return json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": [], "unmatched": unmatched});
    }
    let mut ranked: Vec<(String, i64, BTreeSet<String>, f64)> = scores
        .into_iter()
        .filter(|(path, _)| !exclude.contains(path.as_str()))
        .map(|(path, (score, why, ts))| {
            let recency = recent.get(&path).map(|c| c.ts).unwrap_or(ts);
            (path, score, why, recency)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal)));
    let candidates: Vec<(String, f64, String, String, String)> = ranked
        .iter()
        .map(|(path, _score, why, _)| {
            let (ts, user, agent) = match recent.get(path) {
                Some(c) => (c.ts, c.user.clone(), c.agent.clone()),
                None => (0.0, String::new(), String::new()),
            };
            let why_text = why.iter().cloned().collect::<Vec<_>>().join(", ");
            (path.clone(), ts, user, agent, why_text)
        })
        .collect();
    if candidates.is_empty() {
        return json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": [], "unmatched": unmatched});
    }
    let total = candidates.len();
    let (blocks, rest, shown_paths) = render_all(store, scope, &candidates, budget, stamp, &extra, input);
    let shown = blocks.len();
    let matched_list: Vec<&str> = matched_ids.iter().map(String::as_str).collect();
    let mut text = format!(
        "Collide briefing — {}, scoped to your prompt: {shown} of {total} module(s) match {}. \
Signatures and docstrings are exact (parsed from the code); notes are what earlier agents recorded. \
Build on these instead of reading the files. get_symbol(repo_id, path) gives a module's full facts; \
the Coverage line below says whether the dependents listed are complete.",
        input.repo_id,
        if matched_list.is_empty() { "nothing".to_string() } else { matched_list.join(", ") }
    );
    let live = right_now(store, scope, input, stamp);
    if !live.is_empty() {
        text.push('\n');
        text.push_str(&live);
    }
    for block in blocks {
        text.push('\n');
        text.push_str(&block);
    }
    let mut coverage: Vec<String> = Vec::new();
    if rest.is_empty() && !capped && !with_callers.is_empty() {
        // every dependent within two hops is on the page: the agent has the
        // whole blast radius and a call to fetch it again would return the same
        coverage.push(format!(
            "complete: every module that depends on {} within 2 hops is listed ({} dependent module(s)); no blast_radius call needed",
            with_callers.iter().cloned().collect::<Vec<_>>().join(", "),
            dependent_modules.len()
        ));
    }
    if !rest.is_empty() {
        let listed: Vec<&str> = rest.iter().take(MAX_REST).map(String::as_str).collect();
        coverage.push(format!("not expanded ({}): {}; blast_radius(path, symbol) lists the rest", rest.len(), listed.join(", ")));
    } else if capped {
        coverage.push(format!("dependents capped at {MAX_DEPENDENT_MODULES}; blast_radius(path, symbol) lists the rest"));
    }
    if !unmatched.is_empty() {
        coverage.push(format!("matched nothing in the map: {}", unmatched.join(", ")));
    }
    if !coverage.is_empty() {
        text.push_str(&format!("\nCoverage — {}.", coverage.join("; ")));
    }
    // a transformation another agent already made on this symbol: one
    // command replays it — the work is reused, not just the facts
    let recipes = crate::recipes::matching(store, scope, input.visible, &idents, input.viewer, input.share_knowledge);
    for line in crate::recipes::brief_lines(&recipes, stamp) {
        text.push('\n');
        text.push_str(&line);
    }
    text.push('\n');
    text.push_str(PROMPT_TAIL);
    let named: Vec<String> = rest.iter().take(MAX_REST).cloned().collect();
    // the shown modules whose matched symbol had its callers listed above:
    // a write to one of them is credited the callers search it did not need
    let mut covered: Vec<String> = Vec::new();
    for (path, symbol) in &matched_symbols {
        if with_callers.contains(symbol) && shown_paths.contains(path) && !covered.contains(path) {
            covered.push(path.clone());
        }
    }
    json!({"ok": true, "text": text, "modules": shown, "total": total, "paths": shown_paths, "named": named, "covered": covered,
           "unmatched": unmatched})
}

/// snake_case, a path, a dotted name, or CamelCase — not a capitalized word.
/// Is this prompt asking for work on the code, or talking about something
/// else? A briefing costs the agent its tokens on every message it rides;
/// on "which database should we use?" it is noise. A prompt that names code
/// or asks for a change is briefed; so is a statement (a bug report reads as
/// one: "french customers are getting free shipping by accident"). What is
/// left, a question with neither, is briefed only when its meaning lands
/// close to real code (`meaning_close`, decided by the caller).
pub fn about_the_code(prompt: &str, names_code: bool, meaning_close: bool) -> bool {
    if names_code || meaning_close {
        return true;
    }
    const TASK: [&str; 32] = [
        "fix", "add", "build", "implement", "create", "write", "change", "update", "rename", "remove", "delete",
        "refactor", "move", "replace", "debug", "test", "deploy", "ship", "handle", "support", "integrate",
        "migrate", "port", "optimize", "improve", "clean", "wire", "bump", "revert", "patch", "edit", "make",
    ];
    const ASK: [&str; 18] = [
        "what", "whats", "why", "how", "which", "who", "where", "when", "should", "can", "could", "would", "is",
        "are", "do", "does", "did", "will",
    ];
    const FILLER: [&str; 10] = ["ok", "okay", "so", "also", "now", "then", "and", "yes", "yea", "yeah"];
    const REQUEST: [&str; 6] = ["can you", "could you", "can u", "please", "pls", "i need you to"];
    let lower = prompt.to_lowercase();
    let words_of = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
            .filter(|w| !w.is_empty())
            .collect()
    };
    let is_task = |word: &str| {
        TASK.iter().any(|t| word == *t || word.strip_suffix('s') == Some(t) || word.strip_suffix("ed") == Some(t))
    };
    // each sentence's first word that is not filler ("Ok build that")
    let sentences: Vec<&str> = lower.split_inclusive(|c| matches!(c, '.' | '!' | '?' | '\n')).collect();
    let openers: Vec<String> = sentences
        .iter()
        .filter_map(|sentence| words_of(sentence).into_iter().find(|w| !FILLER.contains(&w.as_str())))
        .collect();
    // "When it is done, push" opens with a clause, not a question: "when" and
    // "where" ask only in a sentence that ends in a question mark
    let asks = |sentence: &str| {
        words_of(sentence).into_iter().find(|w| !FILLER.contains(&w.as_str())).is_some_and(|w| {
            ASK.contains(&w.as_str()) && (!matches!(w.as_str(), "when" | "where") || sentence.trim_end().ends_with('?'))
        })
    };
    let question = lower.contains('?') || sentences.iter().any(|s| asks(s));
    if !question {
        return true; // a statement: a request, or a bug report
    }
    // a question is work when it asks for it
    openers.iter().any(|w| is_task(w))
        || (REQUEST.iter().any(|r| lower.contains(r)) && words_of(&lower).iter().any(|w| is_task(w)))
}

/// A task that adds code (a feature, a function, a report), as opposed to
/// one that fixes or explains existing code: the one that may rewrite a
/// helper the repo already has.
pub fn adds_code(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| matches!(w, "add" | "adds" | "build" | "create" | "implement" | "write" | "new" | "wants" | "want" | "need" | "needs" | "support"))
}

/// The briefing's line for `embed::reuse_candidates`.
pub fn reuse_line(helpers: &[Value]) -> Option<String> {
    if helpers.is_empty() {
        return None;
    }
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let named: Vec<String> = helpers
        .iter()
        .map(|h| {
            let signature = text(h, "signature");
            let head = if signature.is_empty() { text(h, "symbol") } else { signature };
            let doc = text(h, "doc");
            let first = doc.split(". ").next().unwrap_or("").trim().trim_end_matches('.');
            let used = h.get("used_by").and_then(Value::as_u64).unwrap_or(0);
            let used = if used == 1 { "used in 1 other file".to_string() } else { format!("used in {used} other files") };
            if first.is_empty() {
                format!("`{head}` in {} ({used})", text(h, "path"))
            } else {
                format!("`{head}` in {} ({used}): {}", text(h, "path"), clip(first, 140))
            }
        })
        .collect();
    Some(format!(
        "Already in this repo for parts of your task (reuse them, do not write your own; they keep the house rules): {}.",
        named.join("; ")
    ))
}

/// A task that ends in sharing the work (commit, push, a PR): how to do it in
/// one step. Agents otherwise spend three or four messages on it (test, add,
/// commit, push, and a pull and retry when the push is rejected), each one
/// replaying the whole context, while the push hook already lands a chained
/// command: it rebases onto teammates' commits, reruns the tests and pushes.
pub fn finish_line(prompt: &str) -> Option<&'static str> {
    let lower = prompt.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let shares = words.iter().any(|w| matches!(w.as_str(), "commit" | "push" | "pr" | "ship" | "land"))
        || lower.contains("pull request");
    shares.then_some(
        "Finish in one command when your part is done: run the tests && git add <your files> && git commit -m \"...\" && git push. \
Collide lands the push itself (it rebases onto teammates' commits, reruns the tests and pushes), so a rejected push needs no pull or retry from you.",
    )
}

fn looks_like_code(ident: &str) -> bool {
    if ident.contains('_') || ident.contains('/') || ident.contains('.') {
        return true;
    }
    let humps = ident.chars().filter(|c| c.is_ascii_uppercase()).count();
    humps >= 2 && ident.chars().any(|c| c.is_ascii_lowercase())
}

/// The repo on one page: size, subsystems, the symbols most of the repo
/// leans on, and the conventions its directories follow. Built from the
/// graph, so it costs the agent nothing to learn; capped so it stays a page.
/// Empty when the graph is cold — "not known yet" must not print as zeroes.
fn render_shape(store: &Store, scope: &str, cap: usize) -> String {
    let map = crate::graphview::briefing_map(store, scope, SHAPE_IDLE_S, SHAPE_ITEMS * 3);
    if map.is_null() {
        return String::new();
    }
    let count = |key: &str| map.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut langs: Vec<(String, i64)> = map
        .get("languages")
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect())
        .unwrap_or_default();
    langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let langs: Vec<String> = langs.iter().take(3).map(|(k, v)| format!("{k} {v}")).collect();
    let mut head = format!("Repo shape — {} files, {} symbols, {} edges", count("files"), count("symbols"), count("edges"));
    if !langs.is_empty() {
        head.push_str(&format!("; {}", langs.join(", ")));
    }
    head.push('.');
    let mut used = head.len() + 1;
    let mut out = vec![head];
    let rows = |key: &str| -> Vec<Value> { map.get(key).and_then(Value::as_array).cloned().unwrap_or_default() };
    let field = |row: &Value, key: &str| -> String { row.get(key).and_then(Value::as_str).unwrap_or("").to_string() };
    let num = |row: &Value, key: &str| -> u64 { row.get(key).and_then(Value::as_u64).unwrap_or(0) };
    let subsystems: Vec<String> = rows("subsystems")
        .iter()
        .filter_map(|s| {
            let dir = field(s, "dir");
            let name = if dir.is_empty() { field(s, "label") } else { dir };
            if name.is_empty() {
                return None;
            }
            let hubs: Vec<&str> = s
                .get("hubs")
                .and_then(Value::as_array)
                .map(|h| h.iter().filter_map(Value::as_str).take(3).collect())
                .unwrap_or_default();
            Some(if hubs.is_empty() {
                format!("{name} ({} files, {} symbols)", num(s, "files"), num(s, "symbols"))
            } else {
                format!("{name} ({} files, {} symbols; hubs {})", num(s, "files"), num(s, "symbols"), hubs.join(", "))
            })
        })
        .take(SHAPE_ITEMS)
        .collect();
    push_line(&mut out, &mut used, cap, "  subsystems: ", &subsystems);
    let all_hubs: Vec<Value> = rows("hubs").into_iter().filter(|h| !field(h, "symbol").is_empty()).collect();
    let code_hubs: Vec<Value> = all_hubs.iter().filter(|h| !is_test_path(&field(h, "path"))).cloned().collect();
    let picked = if code_hubs.len() >= 2 { code_hubs } else { all_hubs };
    let hubs: Vec<String> = picked
        .iter()
        .take(SHAPE_ITEMS)
        .map(|h| format!("{}::{} ({} dependents)", field(h, "path"), field(h, "symbol"), num(h, "dependents")))
        .collect();
    push_line(&mut out, &mut used, cap, "  most depended on: ", &hubs);
    let rules: Vec<String> = rows("conventions")
        .iter()
        .take(SHAPE_ITEMS)
        .map(|r| {
            let exceptions: Vec<&str> = r
                .get("exceptions")
                .and_then(Value::as_array)
                .map(|e| e.iter().filter_map(Value::as_str).take(3).collect())
                .unwrap_or_default();
            let tail = if exceptions.is_empty() { String::new() } else { format!(", except {}", exceptions.join(", ")) };
            format!("{} {} in {} of {}{tail}", field(r, "dir"), field(r, "rule"), num(r, "holds"), num(r, "of"))
        })
        .collect();
    push_line(&mut out, &mut used, cap, "  conventions: ", &rules);
    out.join("\n")
}

/// `prefix` + items joined by " · ", dropping trailing items until the line
/// fits what is left of the cap; nothing when none fit.
fn push_line(out: &mut Vec<String>, used: &mut usize, cap: usize, prefix: &str, items: &[String]) {
    let mut n = items.len();
    while n > 0 {
        let line = format!("{prefix}{}", items[..n].join(" · "));
        if *used + line.len() + 1 <= cap {
            *used += line.len() + 1;
            out.push(line);
            return;
        }
        n -= 1;
    }
}

/// Render candidates in order until the budget is spent; the first always fits.
fn render_all(
    store: &Store, scope: &str, candidates: &[(String, f64, String, String, String)], budget: usize, stamp: f64,
    extra: &BTreeMap<String, Vec<String>>, input: &BriefInput,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut blocks: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (path, ts, user, agent, why) in candidates {
        let view = crate::memory::get_symbol(store, scope, path, "");
        if !view.get("found").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let ts = if *ts > 0.0 { *ts } else { view.get("provenance").and_then(|p| p.get("ts")).and_then(Value::as_f64).unwrap_or(stamp) };
        let cap = (budget / MAX_BLOCK_SHARE).max(MIN_BLOCK);
        let verdict = tests_verdict(store, scope, path);
        let mut block =
            render_module(path, ts, user, agent, why, &view, stamp, cap, input.viewer, input.share_knowledge, &verdict);
        if let Some(lines) = extra.get(path) {
            for line in lines {
                block.push('\n');
                block.push_str(line);
            }
        }
        if !blocks.is_empty() && used + block.len() > budget {
            rest.push(path.clone());
            continue;
        }
        used += block.len();
        blocks.push(block);
        shown.push(path.clone());
    }
    (blocks, rest, shown)
}

#[allow(clippy::too_many_arguments)]
fn render_module(
    path: &str, ts: f64, user: &str, agent: &str, why: &str, view: &Value, stamp: f64, cap: usize, viewer: &str,
    share_knowledge: bool, verdict: &str,
) -> String {
    let who = if user.is_empty() {
        "observed".to_string()
    } else if agent.is_empty() {
        user.to_string()
    } else {
        format!("{user} via {agent}")
    };
    let mut line = if why.is_empty() {
        format!("{path} — {who}, {}", age_text(stamp - ts))
    } else {
        format!("{path} — matches {why}; {who}, {}", age_text(stamp - ts))
    };
    if let Some(cmd) = view.pointer("/provenance/verified/command").and_then(Value::as_str) {
        let ok = view.pointer("/provenance/verified/ok").and_then(Value::as_bool).unwrap_or(true);
        line.push_str(&if ok { format!(", verified by `{cmd}`") } else { format!(", check FAILED (`{cmd}`)") });
    }
    if !verdict.is_empty() {
        line.push_str(", ");
        line.push_str(verdict);
    }
    let mut out = vec![line];
    if let Some(doc) = view.get("doc").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        out.push(format!("  {}", clip(doc, MAX_MODULE_DOC_CHARS)));
    }
    let mut used: usize = out.iter().map(|l| l.len() + 1).sum();
    // symbols leave room for the notes when there are any: on a resume the
    // notes are what the agent most needs back
    let has_notes = view.get("anchored_memories").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
    let symbol_cap = if has_notes { cap * SYMBOL_SHARE / 10 } else { cap };
    if let Some(symbols) = view.get("symbols").and_then(Value::as_object) {
        let mut skipped = 0usize;
        for (index, (name, entry)) in symbols.iter().enumerate() {
            let signature = entry.get("signature").and_then(Value::as_str).unwrap_or("");
            let signature = if signature.is_empty() { name.as_str() } else { signature };
            let line = match entry.get("doc").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                Some(doc) => format!("  {signature} — {}", clip(doc, MAX_SYMBOL_DOC_CHARS)),
                None => format!("  {signature}"),
            };
            if index >= MAX_SYMBOL_LINES || (index > 0 && used + line.len() > symbol_cap) {
                skipped += 1;
                continue;
            }
            used += line.len() + 1;
            out.push(line);
        }
        if skipped > 0 {
            out.push(format!("  … {skipped} more symbol(s): get_symbol(repo_id, \"{path}\")"));
        }
    }
    if let Some(notes) = view.get("anchored_memories").and_then(Value::as_array) {
        // notes share the block's cap with the symbols: a module with long
        // memories still renders as a page, and the same fact twice is once
        let mut seen: HashSet<&str> = HashSet::new();
        let mut shown = 0usize;
        let mut skipped = 0usize;
        for note in notes {
            // teammates' notes are knowledge, and knowledge is Team: without
            // sharing, a note is rendered only when the reader wrote it
            if !share_knowledge && !crate::access::own_knowledge(note, viewer) {
                continue;
            }
            let fact = note.get("fact").and_then(Value::as_str).unwrap_or("").trim();
            if fact.is_empty() || !seen.insert(fact) {
                continue;
            }
            // a note reality has scored down is not served: the cache stays
            // small enough to trust, and stale confident advice stops arriving
            if note.get("confidence").and_then(Value::as_f64).unwrap_or(1.0) < SERVE_CONFIDENCE {
                continue;
            }
            let owner = note.get("owner").and_then(Value::as_str).unwrap_or("");
            let flag = if note.get("stale").and_then(Value::as_bool).unwrap_or(false) {
                " [stale]"
            } else if note.get("possibly_stale").is_some() {
                " [possibly stale]"
            } else {
                ""
            };
            let line = format!("  note ({owner}){flag}: {}", clip(fact, MAX_NOTE_CHARS));
            if shown >= MAX_NOTES || used + line.len() > cap {
                skipped += 1;
                continue;
            }
            used += line.len() + 1;
            out.push(line);
            shown += 1;
        }
        if skipped > 0 {
            out.push(format!("  … {skipped} more note(s): get_symbol(repo_id, \"{path}\")"));
        }
    }
    out.join("\n")
}

/// The one rule that makes Collide visible in the terminal: appended to every
/// session-start briefing, so the agent says what of the briefing it acted
/// on and whose work that was, in one sentence, before acting.
const RULE: &str = "Say what you use: when something here changes what you do (a teammate's live work or change, a note, a recipe, a verified version, a claim on a file), tell the user in one plain sentence before acting, naming who and what. For example: Becca changed save_doc's signature 3m ago, so I'm updating the callers to match. Say nothing about what did not matter.";

/// The prompt briefing's shorter reminder of the same rule.
const PROMPT_TAIL: &str = "Say so to the user, in one sentence naming who, when any of this changes what you do.";

/// Other agents working in this repo right now, one line each, for the
/// briefing: who (by nickname when the reader has one for them), what they
/// are doing, their declared task, how long ago. Empty when nobody else is
/// live, so a solo session pays nothing for it. Another session of the
/// reader's own is named as such.
fn right_now(store: &Store, scope: &str, input: &BriefInput, stamp: f64) -> String {
    let view = crate::activity::list_activity(store, scope, input.repo_id, input.idle_after_s, input.visible);
    let rows: Vec<&Value> = view
        .get("workspaces")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    // rows name the scope's own repo id, which a renamed repo keeps while
    // answering to its new name too; the id the caller asked with may be
    // either, so the scope's is the one to match
    let scope_repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(scope);
    let live: Vec<&Value> = rows
        .into_iter()
        .filter(|row| row.get("online").and_then(Value::as_bool).unwrap_or(false))
        .filter(|row| row.get("repo_id").and_then(Value::as_str) == Some(scope_repo))
        .filter(|row| {
            let user = row.get("user").and_then(Value::as_str).unwrap_or("");
            let session = row.get("session").and_then(Value::as_str).unwrap_or("");
            !(user == input.viewer && session == input.session)
        })
        .take(MAX_RIGHT_NOW)
        .collect();
    if live.is_empty() {
        return String::new();
    }
    let users: BTreeSet<String> = live
        .iter()
        .filter_map(|row| row.get("user").and_then(Value::as_str).map(str::to_string))
        .filter(|user| user != input.viewer)
        .collect();
    let labels = crate::activity::identity_labels(store, input.viewer, &users);
    let mut out = vec!["Right now, other agents in this repo:".to_string()];
    for row in live {
        let user = row.get("user").and_then(Value::as_str).unwrap_or("");
        let who = if user == input.viewer {
            "another session of yours".to_string()
        } else {
            labels.get(user).and_then(|l| l.get("label")).and_then(Value::as_str).unwrap_or(user).to_string()
        };
        let kind = row.pointer("/last_action/kind").and_then(Value::as_str).unwrap_or("");
        let doing = match kind {
            "editing" | "reading" | "running" | "searching" => kind,
            _ => "working on",
        };
        let path = row.get("current_path").and_then(Value::as_str).unwrap_or("");
        let task = row.get("task").and_then(Value::as_str).unwrap_or("");
        let task = if task.is_empty() { String::new() } else { format!(", task \"{}\"", clip(task, 80)) };
        let idle = row.get("last_activity_s").and_then(Value::as_f64).unwrap_or(0.0);
        let _ = stamp;
        // no file yet: the agent has a prompt and has not opened anything
        if path.is_empty() {
            out.push(format!("  {who}: starting a task{task}, {}", age_text(idle)));
        } else {
            out.push(format!("  {who}: {doing} {path}{task}, {}", age_text(idle)));
        }
    }
    out.join("\n")
}

/// What the dependents' tests said about the newest live change to `path`,
/// from its hot marker: "tests passed after the change", "tests FAILING
/// after the change (2 failed)", or "" when nothing has been verified.
fn tests_verdict(store: &Store, scope: &str, path: &str) -> String {
    let prefix = format!("hot:{scope}:{}:", crate::repo::path_key(path));
    let newest = store
        .eph_scan(&prefix)
        .into_iter()
        .map(|(_key, marker)| marker)
        .max_by(|a, b| {
            let ta = a.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            let tb = b.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(marker) = newest else { return String::new() };
    let Some(verified) = marker.get("verified").filter(|v| v.is_object()) else { return String::new() };
    let count = |key: &str| verified.get(key).and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    match verified.get("status").and_then(Value::as_str).unwrap_or("") {
        "clear" => "tests passed after the change".to_string(),
        "failing" => format!("tests FAILING after the change ({} failed)", count("failed")),
        "partial" => "tests passed after the change, partly covered".to_string(),
        _ => String::new(),
    }
}

/// The first `max` characters of a fact, on a word boundary, with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head.rfind(' ').filter(|&i| i > max / 2).unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

/// Test scaffolding is what depends on everything; it is not what an agent
/// writing code leans on.
pub(crate) fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/") || path.contains("/test/") || path.starts_with("tests/") || path.starts_with("test/")
        || name.starts_with("test_") || name == "conftest.py" || name.ends_with("_test.rs")
        || name.ends_with(".test.ts") || name.ends_with(".test.tsx") || name.ends_with(".spec.ts")
}

fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7_200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 172_800.0 {
        format!("{}h ago", (s / 3_600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::{about_the_code, age_text, clip, is_test_path};

    #[test]
    fn a_prompt_about_the_code_is_briefed_and_a_conversation_is_not() {
        // Study 5's tickets, vague as they are, are all work on the code
        for task in [
            "the revenue number on the dashboard is way too high, i think refunds are getting counted in it. fix it",
            "french customers are getting free shipping by accident",
            "we arent adding tax for british customers when they check out",
            "the WELCOME10 code is supposed to be one time use per customer but ppl keep reusing it",
            "uk customers arent getting charged vat at checkout?? it should be 20%. can u fix that",
            "loyalty points are way too high, people get points for orders that were cancelled or never paid",
            "Ok build that. Also what do you suggest for embeddings?",
            // a task that says what to do after it: "when" opens a clause here
            "the orders export for accounting should also have the customers email in it. When it is done and the tests pass, commit and push to origin main.",
            "accounting also wants each customer's region as a column in the orders export. When it is done, push. Where the file has a header, keep it.",
        ] {
            assert!(about_the_code(task, false, false), "{task}");
        }
        // this session's strategy questions are not
        for talk in [
            "What about what cursor does? The turbopuffer",
            "Keep it concise. Is collide and its hooks saving me tokens in this session right now",
            "What’s the absolute best option",
            "So it wouldn’t be faster adding this? Then what makes cursor so fast?",
            "Should I make people add a card?",
            "When does the digest go out?",
        ] {
            assert!(!about_the_code(talk, false, false), "{talk}");
        }
        // a question that names code, or means it closely, is still briefed
        assert!(about_the_code("why does levy_for return 0 for UK?", true, false));
        assert!(about_the_code("why do UK customers pay no VAT?", false, true));
    }

    #[test]
    fn a_task_that_adds_code_is_told_which_helpers_exist() {
        assert!(super::adds_code("add `find_customer(email)` in tillhouse/support/lookup_tool.py"));
        assert!(super::adds_code("accounting wants a monthly revenue csv"));
        assert!(!super::adds_code("the revenue number on the dashboard is way too high. fix it"));
        let line = super::reuse_line(&[serde_json::json!({"path": "tillhouse/core/csvout.py", "symbol": "to_csv",
            "signature": "def to_csv(header: list[str], rows: list[list]) -> str",
            "doc": "CSV text with a header row. Fields are quoted when they need it.", "used_by": 2})]).unwrap();
        assert!(line.contains("`def to_csv(header: list[str], rows: list[list]) -> str` in tillhouse/core/csvout.py (used in 2 other files): CSV text with a header row"), "{line}");
        assert!(super::reuse_line(&[]).is_none());
    }

    #[test]
    fn a_task_that_ends_in_a_push_is_told_to_finish_in_one_command() {
        assert!(super::finish_line("fix it. When it is done, commit and push to origin main").is_some());
        assert!(super::finish_line("open a pull request for the fix").is_some());
        assert!(super::finish_line("fix the refunds on the dashboard").is_none());
        assert!(super::finish_line("the pushback on the proposal was fair").is_none());
    }

    #[test]
    fn a_note_clips_on_a_word_boundary_with_an_ellipsis() {
        assert_eq!(clip("short fact", 240), "short fact");
        let long = "word ".repeat(100);
        let clipped = clip(&long, 40);
        assert!(clipped.ends_with('…') && clipped.chars().count() <= 41, "{clipped}");
        assert!(!clipped.contains("  "), "no dangling space before the ellipsis");
    }

    #[test]
    fn test_scaffolding_is_recognised_by_path() {
        assert!(is_test_path("server/tests/conftest.py"));
        assert!(is_test_path("tests/spec.py"));
        assert!(is_test_path("src/app/test_routes.py"));
        assert!(is_test_path("web/src/Button.test.tsx"));
        assert!(!is_test_path("server/collide-rs/src/store.rs"));
        assert!(!is_test_path("src/collide/contest.py"));
    }

    #[test]
    fn ages_read_coarsely() {
        assert_eq!(age_text(5.0), "just now");
        assert_eq!(age_text(300.0), "5m ago");
        assert_eq!(age_text(10_000.0), "2h ago");
        assert_eq!(age_text(300_000.0), "3d ago");
    }
}
