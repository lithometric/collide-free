//! The prompt-scoped briefing and the grep companion — the Rust twins of
//! the Python hook's `_run_user_prompt` / `_grep_companion`, byte for byte.
//!
//! At prompt submit the task is known: the hook extracts the identifiers
//! the prompt names (paths, snake_case, CamelCase, dotted names, backticked
//! tokens — never the prose) and asks `/brief` for the modules they touch.
//! After a grep, the name the agent searched for goes to `/attribute` and
//! the map's structured answer rides the same tool result.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::check::state_path;
use crate::config::{self, Env};
use crate::http;
use crate::report::{save_json, text, user_agent, BRIEF_BUDGET, BRIEF_TIMEOUT, TOTAL_BUDGET};

const MAX_PROMPT_IDENTIFIERS: usize = 40;
const MAX_GREP_SYMBOLS: usize = 5;
const GREP_NOTE_CHARS: usize = 1500;
const IDENT_STOP: [&str; 15] = [
    "e.g", "i.e", "etc", "vs", "python3", "python", "node", "npm", "npx", "git", "cd", "ls", "pytest", "cargo",
    "tests.check",
];
const IDENT_STOP_MORE: [&str; 2] = ["readme.md", "package.json"];
/// Function words, shell verbs and primitive type names. Reachable in practice
/// only through a bare backtick span, where nothing else can tell `run` the
/// verb from `run` the symbol. FROZEN at this size by policy: it is cheap
/// insurance against the long tail, never an English dictionary, and it
/// deliberately omits doc, recap, remember, session, name, call, auto, since,
/// setup and auth — those are real Collide symbols and briefing them is right.
const IDENT_PROSE: [&str; 135] = [
    "add", "added", "all", "also", "and", "any", "are", "as", "at", "awk", "be", "been", "bool",
    "build", "but", "by", "can", "cat", "change", "changed", "clone", "commit", "curl", "debug",
    "delete", "dict", "did", "diff", "do", "does", "done", "echo", "else", "error", "false",
    "fetch", "fix", "fixed", "for", "from", "grep", "had", "has", "have", "he", "help", "her",
    "his", "if", "in", "info", "int", "into", "is", "it", "its", "just", "kill", "len", "list",
    "log", "make", "me", "merge", "must", "my", "new", "no", "none", "not", "now", "null", "of",
    "off", "ok", "okay", "old", "on", "one", "only", "open", "or", "our", "out", "over", "per",
    "print", "pull", "push", "rebase", "remove", "removed", "return", "run", "sed", "self", "set",
    "she", "should", "start", "stash", "status", "still", "stop", "str", "test", "tests", "that",
    "the", "them", "then", "these", "they", "this", "those", "to", "true", "two", "type", "under",
    "up", "update", "updated", "via", "warn", "was", "we", "wget", "when", "while", "will", "with",
    "yes", "you", "your"
];

/// Every pattern that proposes a candidate. The ASCII-only ones carry `(?-u)`
/// so Rust's `\w` and `\b` mean what Python's `re.ASCII` means; the two that
/// must accept arbitrary text (the backtick span, the URL) stay Unicode in
/// both twins, which is also agreement. Anything non-ASCII is rejected by the
/// shape guard downstream regardless.
struct IdentRes {
    file: Regex,
    tick: Regex,
    tick_token: Regex,
    word: Regex,
    shape: Regex,
    url: Regex,
}

/// Source extensions, plus the repo-data and doc extensions a tracked file
/// really carries (CLAUDE.md, Cargo.toml, blocks_golden.json). Image and
/// binary extensions are deliberately absent: that is what keeps a pasted
/// `Screenshot 2026-09-17 at 12.42.34 PM.png` from ever proposing a token.
const CODE_EXT: &str = "py|pyi|ts|tsx|js|jsx|mjs|cjs|go|rs|java|cs|rb|c|h|cc|cpp|hpp|php";
const DATA_EXT: &str =
    "md|txt|json|jsonl|toml|yaml|yml|sql|html|css|scss|sh|lock|cfg|ini|rst|xml|proto|graphql|env";

fn ident_res() -> IdentRes {
    IdentRes {
        file: Regex::new(&format!(r"(?-u)[\w./-]*[\w-]+\.(?:{CODE_EXT}|{DATA_EXT})\b")).expect("static regex"),
        tick: Regex::new(r"`([^`\n]{1,200})`").expect("static regex"),
        tick_token: Regex::new(r"(?-u)[A-Za-z_][\w./:-]*").expect("static regex"),
        word: Regex::new(r"(?-u)[A-Za-z_][A-Za-z0-9_]*").expect("static regex"),
        shape: Regex::new(r"(?-u)^[\w./:-]+$").expect("static regex"),
        url: Regex::new(r"(?:https?://|www\.)\S+").expect("static regex"),
    }
}

fn ascii_lower(t: &str) -> String {
    t.chars().map(|c| if c.is_ascii_uppercase() { ((c as u8) + 32) as char } else { c }).collect()
}

fn alnum_underscore(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A token whose SHAPE is a code marker English prose cannot produce. Written
/// with explicit byte comparisons, no regex and no locale, so the Python twin
/// transcribes character for character.
fn is_code(t: &str) -> bool {
    if t.is_empty() || !t.is_ascii() {
        return false;
    }
    let uppers = t.chars().filter(char::is_ascii_uppercase).count();
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
    // snake_case, _leading, SCREAMING_SNAKE, FACTOR_5
    if t.contains('_') && t.len() >= 3 && has_alpha && alnum_underscore(t) {
        return true;
    }
    // CamelCase: ToolSearch, BriefInput, CollideMCP
    !t.contains('_') && t.len() >= 4 && alnum_underscore(t) && uppers >= 2 && has_lower
}

/// A bare word that only a backtick makes an identifier. Keeps `compute` and
/// `recap`; refuses `Bash`, `Collide`, `REGION`, `EU`.
fn is_bare_name(t: &str) -> bool {
    if t.is_empty() || !t.is_ascii() || !alnum_underscore(t) || t.len() < 3 {
        return false;
    }
    let first = t.chars().next().expect("non-empty");
    let uppers = t.chars().filter(char::is_ascii_uppercase).count();
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    if first.is_ascii_uppercase() && has_lower && uppers < 2 {
        return false; // Bash, Edit, Collide, Session
    }
    if !has_lower && uppers >= 1 {
        return false; // bare ALL-CAPS: PM, JSON, EU
    }
    !first.is_ascii_digit()
}

/// A pasted git sha, not a name.
fn is_hexish(t: &str) -> bool {
    t.len() >= 7
        && ascii_lower(t).chars().all(|c| "0123456789abcdef".contains(c))
        && t.chars().any(|c| c.is_ascii_digit())
}

/// How much of a prompt's own words the briefing may match by meaning.
pub const PROMPT_CHARS: usize = 2_000;

/// The identifiers a prompt names, never its prose.
///
/// Three producing rules in a load-bearing order — paths, then the author's
/// explicit backticks, then shape — each candidate routed through one guard
/// chain. Shape is the only evidence a bare word gets: a capitalized English
/// word is not a name, because `Likely` exact-matching a constant named
/// `LIKELY` scores above the briefing's strong-match threshold and spends a
/// module block on nothing. Measured over 312 recorded prompts against the
/// real index: precision 0.54 before, 0.65 after, with more productive
/// identifiers than the stricter alternative that deletes snake_case.
pub fn prompt_identifiers(prompt: &str) -> Vec<String> {
    let res = ident_res();
    // a URL is a path-shaped thing that names no file in this repo
    let text = res.url.replace_all(prompt, " ").into_owned();
    let mut out: Vec<String> = Vec::new();

    let take = |raw: &str, span: Option<(usize, usize)>, out: &mut Vec<String>| {
        if let Some((start, end)) = span {
            let before = if start > 0 { text[..start].chars().last() } else { None };
            let after = text[end..].chars().next();
            if matches!(before, Some('/') | Some('-') | Some('\\'))
                || matches!(after, Some('/') | Some('-') | Some('\\'))
            {
                return; // a path segment or a slug, not a name
            }
        }
        // trailing punctuation goes; a LEADING dot stays on a path, so
        // `.collide/report_hook.py` keeps the name the map stores it under
        let mut tok = raw.trim().trim_matches(|c| ",;:()".contains(c)).to_string();
        while tok.ends_with('.') {
            tok.pop();
        }
        if !tok.contains('/') {
            tok = tok.trim_start_matches('.').to_string();
        }
        if tok.is_empty() {
            return;
        }
        let first = tok.chars().next().expect("non-empty");
        let last = tok.chars().last().expect("non-empty");
        if first == '-' || first == '\\' || "/-\\.".contains(last) {
            return;
        }
        if tok.contains("://") || !tok.is_ascii() || !res.shape.is_match(&tok) {
            return;
        }
        if is_hexish(&tok) {
            return;
        }
        if tok.len() > 128 || (tok.len() > 32 && !tok.contains(['_', '/', '.'])) {
            return;
        }
        let lower = ascii_lower(&tok);
        if IDENT_STOP.contains(&lower.as_str())
            || IDENT_STOP_MORE.contains(&lower.as_str())
            || IDENT_PROSE.contains(&lower.as_str())
        {
            return;
        }
        // the briefing lowercases every identifier, so the dedupe must too
        if out.iter().any(|seen| {
            let seen_l = ascii_lower(seen);
            seen_l == lower || seen_l.ends_with(&format!("/{lower}"))
        }) {
            return;
        }
        out.push(tok);
    };

    // a path with a known extension, taken whole and without the neighbour
    // guard: its own slashes must not disqualify it
    for m in res.file.find_iter(&text) {
        take(m.as_str(), None, &mut out);
    }

    // what the author marked with backticks. One token that IS the whole span
    // is a name they chose; several tokens is a code fragment, so each is
    // judged on shape alone and a dotted or `::` name is split rather than
    // sent whole, because the briefing can match neither.
    let segments = |t: &str| -> Vec<String> {
        t.split("::").flat_map(|part| part.split('.')).map(str::to_string).collect()
    };
    for caps in res.tick.captures_iter(&text) {
        let Some(span) = caps.get(1).map(|m| m.as_str().trim()) else { continue };
        let tokens: Vec<&str> = res.tick_token.find_iter(span).map(|m| m.as_str()).collect();
        if tokens.len() == 1 && tokens[0] == span {
            let tok = tokens[0];
            if res.file.find(tok).map(|m| m.as_str() == tok).unwrap_or(false) || is_code(tok) {
                take(tok, None, &mut out);
            } else if is_bare_name(tok) {
                take(tok, None, &mut out);
            } else {
                for seg in segments(tok) {
                    if is_code(&seg) {
                        take(&seg, None, &mut out);
                    }
                }
            }
        } else {
            for tok in tokens {
                if res.file.find(tok).map(|m| m.as_str() == tok).unwrap_or(false) || is_code(tok) {
                    take(tok, None, &mut out);
                    continue;
                }
                for seg in segments(tok) {
                    if is_code(&seg) {
                        take(&seg, None, &mut out);
                    }
                }
            }
        }
    }

    // the prose scan: shape is the only evidence. No word boundary and no
    // lowercase-start test, so `_run_user_prompt` and `FACTOR_5` arrive; the
    // scan splits on '.', so `region.startswith` never forms.
    for m in res.word.find_iter(&text) {
        if is_code(m.as_str()) {
            take(m.as_str(), Some((m.start(), m.end())), &mut out);
        }
    }

    out.truncate(MAX_PROMPT_IDENTIFIERS);
    out
}

fn strings(state: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    state
        .get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// How much context a briefing is: its bytes over four, the usual token
/// estimate. Both hooks count the same bytes so the two post the same cost.
pub fn brief_tokens(text: &str) -> u64 {
    (text.len() as u64 + 3) / 4
}

/// The briefing just landed in the session: the modules it rendered with
/// facts are BRIEFED, the ones it only named (the "N more changed" tail) are
/// NAMED, the briefed ones whose callers it listed in full are COVERED, and
/// the text itself is context the session now pays to carry on every message
/// until a compaction drops it. Mirrors `_remember_briefed`.
pub fn remember_briefed(session: &str, paths: &[Value], named: &[Value], text: &str, covered: &[Value], env: &Env) {
    let named: Vec<String> = named.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let covered: Vec<String> = covered.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let tokens = brief_tokens(text);
    if paths.is_empty() && named.is_empty() && covered.is_empty() && tokens == 0 {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let mut briefed = strings(&state, "briefed_paths");
    for p in paths {
        if let Some(p) = p.as_str() {
            if !briefed.iter().any(|s| s == p) {
                briefed.push(p.to_string());
            }
        }
    }
    let mut known = strings(&state, "named_paths");
    for p in named {
        if !known.contains(&p) && !briefed.contains(&p) {
            known.push(p);
        }
    }
    state.insert("briefed_paths".into(), json!(briefed));
    state.insert("named_paths".into(), json!(known));
    let mut listed = strings(&state, "covered_paths");
    for p in covered {
        if !listed.contains(&p) {
            listed.push(p);
        }
    }
    state.insert("covered_paths".into(), json!(listed));
    if tokens > 0 {
        let total = state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0);
        state.insert("brief_tokens".into(), json!(total + tokens));
        let injected = state.get("brief_injected").and_then(Value::as_u64).unwrap_or(0);
        state.insert("brief_injected".into(), json!(injected + tokens));
    }
    save_json(&path, &Value::Object(state));
}

/// A compaction dropped the context: a briefed module the session never
/// touched is unknown again, and the briefing text is no longer carried. A
/// path already read or written keeps its place, so a read made before the
/// compaction still settles as NEEDED (or a miss) after it, instead of the
/// credit vanishing with the context. Mirrors `_forget_briefed`.
pub fn forget_briefed(session: &str, env: &Env) {
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let touched: BTreeSet<String> =
        strings(&state, "read_paths").into_iter().chain(strings(&state, "written_paths")).collect();
    let mut changed = false;
    for key in ["briefed_paths", "named_paths", "covered_paths"] {
        let before = strings(&state, key);
        let after: Vec<String> = before.iter().filter(|p| touched.contains(*p)).cloned().collect();
        if after != before {
            state.insert(key.into(), json!(after));
            changed = true;
        }
    }
    if state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0) > 0 {
        state.insert("brief_tokens".into(), json!(0));
        changed = true;
    }
    if changed {
        save_json(&path, &Value::Object(state));
    }
}

/// One more message replayed the context. The briefing text riding in it is
/// re-read at the cache-read rate every time, and that is the briefing's own
/// running cost, counted at every tool call Collide sees and at Stop.
pub fn note_call(session: &str, env: &Env) {
    if session.is_empty() {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let carried = state.get("brief_tokens").and_then(Value::as_u64).unwrap_or(0);
    if carried == 0 {
        return;
    }
    let so_far = state.get("brief_carried").and_then(Value::as_u64).unwrap_or(0);
    state.insert("brief_carried".into(), json!(so_far + carried));
    save_json(&path, &Value::Object(state));
}

/// Remember that this session read or wrote a path — the other half of the
/// briefing's hit/miss ledger (the briefed half is `remember_briefed`).
pub fn note_touch(session: &str, kind: &str, rel: &str, env: &Env) {
    if session.is_empty() || rel.is_empty() {
        return;
    }
    let path = state_path(session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let key = format!("{kind}_paths");
    let mut seen: Vec<String> = state
        .get(&key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if seen.iter().any(|s| s == rel) {
        return;
    }
    seen.push(rel.to_string());
    if seen.len() > 400 {
        seen.drain(..seen.len() - 400);
    }
    state.insert(key, json!(seen));
    save_json(&path, &Value::Object(state));
}

/// What the briefing was worth, settled once per path. A briefed path the
/// session WROTE is a HIT when it was never read (the facts were enough) and
/// NEEDED when it was read first (the Edit tool insists on its own Read). A
/// path the briefing only NAMED, in its "N more changed" tail, that the
/// session then wrote is FOUND: the briefing said where to look. A written
/// briefed path whose CALLERS the briefing listed in full is one more search
/// not sent, the callers search. A briefed path read and never written is a
/// MISS, known only when the session ends
/// (`last`): a read this turn is very often the read before an edit a few
/// turns later, and calling it a miss one turn later, as this used to, wrote
/// most of them off. Counts only leave the machine, never paths, plus what
/// the briefing itself cost to carry since the last report. Mirrors
/// `_brief_outcome`.
pub fn brief_outcome(hook_input: &Value, env: &Env, started: std::time::Instant, last: bool) {
    let session = text(hook_input, "session_id");
    if session.is_empty() {
        return;
    }
    let path = state_path(&session, env);
    let mut state = config::load_json(&path).as_object().cloned().unwrap_or_default();
    let briefed = strings(&state, "briefed_paths");
    let named: Vec<String> = strings(&state, "named_paths").into_iter().filter(|p| !briefed.contains(p)).collect();
    if briefed.is_empty() && named.is_empty() {
        return;
    }
    let read: BTreeSet<String> = strings(&state, "read_paths").into_iter().collect();
    let written: BTreeSet<String> = strings(&state, "written_paths").into_iter().collect();
    let covered: BTreeSet<String> = strings(&state, "covered_paths").into_iter().collect();
    let mut done: BTreeSet<String> = strings(&state, "outcome_seen").into_iter().collect();
    let (mut hits, mut misses, mut needed, mut found, mut callers) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut newly: Vec<String> = Vec::new();
    for p in &briefed {
        if done.contains(p) {
            continue;
        }
        if written.contains(p) && read.contains(p) {
            needed += 1;
        } else if written.contains(p) {
            hits += 1;
        } else if read.contains(p) && last {
            misses += 1;
        } else {
            continue;
        }
        if written.contains(p) && covered.contains(p) {
            callers += 1;
        }
        newly.push(p.clone());
    }
    for p in &named {
        if done.contains(p) || !written.contains(p) {
            continue;
        }
        found += 1;
        newly.push(p.clone());
    }
    let injected = state.get("brief_injected").and_then(Value::as_u64).unwrap_or(0);
    let carried = state.get("brief_carried").and_then(Value::as_u64).unwrap_or(0);
    if newly.is_empty() && !(last && (injected > 0 || carried > 0)) {
        return;
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[Some(cwd.clone()), project]).unwrap_or(cwd);
    let cfg = config::config(Some(&root), env);
    let remaining = TOTAL_BUDGET.saturating_sub(started.elapsed());
    let mut posted = false;
    if cfg.usable() && !remaining.is_zero() {
        // what a message was worth: the context this session was carrying (a
        // message it did not send would have replayed all of it), and the
        // model's rate; and what the briefing cost to inject and carry
        let meta = crate::transcript::turn_meta(&text(hook_input, "transcript_path"));
        let mut payload = json!({"repo_id": cfg.repo_id, "session": session, "hits": hits, "misses": misses, "needed": needed,
                                 "found": found, "callers": callers, "brief_injected": injected, "brief_carried": carried});
        if let Some(map) = payload.as_object_mut() {
            if let Some(context) = meta.get("context").and_then(Value::as_i64).filter(|c| *c > 0) {
                map.insert("context_tokens".into(), json!(context));
            }
            if let Some(model) = meta.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
                map.insert("model".into(), json!(model));
            }
        }
        posted = http::post(&cfg.server, "/brief/outcome", &cfg.token, &crate::report::user_agent(), &payload,
            remaining.min(std::time::Duration::from_secs(5))).is_ok();
    }
    if posted {
        // the cost is reported once; a post that failed keeps it for the next
        state.insert("brief_injected".into(), json!(0));
        state.insert("brief_carried".into(), json!(0));
    }
    done.extend(newly);
    state.insert("outcome_seen".into(), json!(done.into_iter().collect::<Vec<_>>()));
    state.remove("outcome_pending");
    save_json(&path, &Value::Object(state));
}

/// UserPromptSubmit: the briefing narrows to what the prompt names.
pub fn run_user_prompt(hook_input: &Value, env: &Env) -> i32 {
    let prompt = text(hook_input, "prompt");
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let Some(root) = config::find_repo_root(&[Some(cwd), project]) else { return 0 };
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let identifiers = prompt_identifiers(&prompt);
    // the prompt's words too, capped: a request that names no code ("the
    // revenue on the dashboard is too high") is matched by meaning on the
    // server, used for that lookup and dropped. Twin of report_hook.py.
    let meaning: String = prompt.chars().take(PROMPT_CHARS).collect();
    if identifiers.is_empty() && meaning.trim().is_empty() {
        return 0;
    }
    let session = text(hook_input, "session_id");
    let state = config::load_json(&state_path(&session, env));
    let exclude: Vec<String> = state
        .get("briefed_paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let payload = json!({"repo_id": cfg.repo_id, "identifiers": identifiers, "exclude": exclude, "budget": BRIEF_BUDGET, "session": session, "prompt": meaning});
    let Ok(brief) = http::post(&cfg.server, "/brief", &cfg.token, &user_agent(), &payload, BRIEF_TIMEOUT) else { return 0 };
    let rendered = crate::harness::render_context("UserPromptSubmit", &text(&brief, "text"));
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    let paths = brief.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
    let named = brief.get("named").and_then(Value::as_array).cloned().unwrap_or_default();
    let covered = brief.get("covered").and_then(Value::as_array).cloned().unwrap_or_default();
    remember_briefed(&session, &paths, &named, &text(&brief, "text"), &covered, env);
    0
}

fn grep_identifiers(command: &str) -> Vec<String> {
    let cmd_re = Regex::new(r"(?:^|[;&|(]\s*)(?:git\s+grep|rg|grep|ag|ack)\b").expect("static regex");
    if !cmd_re.is_match(command) {
        return Vec::new();
    }
    let split_re = Regex::new(r"(?:git\s+grep|rg|grep|ag|ack)\b(.*)$").expect("static regex");
    let rest = split_re.captures(command).and_then(|c| c.get(1)).map(|m| m.as_str()).unwrap_or("");
    let quoted_re = Regex::new(r#"'([^']+)'|"([^"]+)""#).expect("static regex");
    let mut candidates: Vec<String> = quoted_re
        .captures_iter(rest)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
        .take(1)
        .collect();
    if candidates.is_empty() {
        let stripped = quoted_re.replace_all(rest, " ");
        candidates = stripped
            .split_whitespace()
            .filter(|t| !t.starts_with('-') && !t.contains('/') && !t.starts_with('.') && !matches!(*t, "|" | "&&" | ";"))
            .take(1)
            .map(str::to_string)
            .collect();
    }
    let word_re = Regex::new(r"[A-Za-z_]\w{2,}").expect("static regex");
    let mut out: Vec<String> = Vec::new();
    for cand in &candidates {
        for m in word_re.find_iter(cand) {
            let w = m.as_str().to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out.truncate(MAX_GREP_SYMBOLS);
    out
}

/// The map's structured answer to the name the agent just searched for.
pub fn grep_companion(command: &str, cfg: &config::Config, budget: Duration, session: &str) -> String {
    let symbols = grep_identifiers(command);
    if symbols.is_empty() || budget.is_zero() {
        return String::new();
    }
    let payload = {
        let mut payload = json!({"repo_id": cfg.repo_id, "symbols": symbols});
        if !session.is_empty() {
            // the answer is a read not sent, priced against this session
            payload["session"] = json!(session);
        }
        payload
    };
    let Ok(answer) = http::post(&cfg.server, "/attribute", &cfg.token, &user_agent(), &payload, budget.min(Duration::from_secs(3))) else {
        return String::new();
    };
    let Some(facts) = answer.get("facts").and_then(Value::as_object) else { return String::new() };
    if facts.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    for name in &symbols {
        let Some(fact) = facts.get(name).and_then(Value::as_object) else { continue };
        let sig = fact.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(name);
        let doc = fact.get("doc").and_then(Value::as_str).unwrap_or("");
        let path = fact.get("path").and_then(Value::as_str).unwrap_or("");
        let callers: Vec<&str> = fact
            .get("callers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut line = format!("Collide map — `{name}` ({path}): {sig}");
        if !doc.is_empty() {
            line.push_str(&format!(" — {doc}"));
        }
        if !callers.is_empty() {
            let shown: Vec<&str> = callers.iter().take(8).cloned().collect();
            line.push_str(&format!(" · callers: {}", shown.join(", ")));
            if callers.len() > 8 {
                line.push_str(&format!(" (+{})", callers.len() - 8));
            }
        }
        lines.push(line);
    }
    let joined = lines.join("\n");
    if joined.chars().count() <= GREP_NOTE_CHARS { joined } else { joined.chars().take(GREP_NOTE_CHARS).collect() }
}

/// The read answered that another agent wrote this file moments ago.
pub fn recent_write_note(resp: &Value, path: &str) -> String {
    let Some(who) = resp.get("recent_write").and_then(Value::as_object) else { return String::new() };
    if who.get("mine").and_then(Value::as_bool).unwrap_or(false) {
        return String::new();
    }
    let whom = if who.get("same_user").and_then(Value::as_bool).unwrap_or(false) {
        "another session of yours"
    } else {
        who.get("who")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .or_else(|| who.get("user").and_then(Value::as_str).filter(|u| !u.is_empty()))
            .unwrap_or("a teammate")
    };
    let agent = who.get("agent").and_then(Value::as_str).filter(|a| !a.is_empty()).unwrap_or("an agent");
    let age = age_text(who.get("age_s").and_then(Value::as_f64).unwrap_or(0.0));
    // each agent has its own checkout: the server says whether their change
    // is in what was just read, and whether it is pushed yet
    let in_copy = who.get("in_copy").and_then(Value::as_bool).unwrap_or(true);
    let pushed = who.get("pushed").and_then(Value::as_bool).unwrap_or(false);
    if in_copy {
        format!(
            "Collide: {whom} ({agent}) wrote {path} {age}; what you just read includes their change. Build on it: keep their lines and make your own edit now, there is nothing to wait for. Say so to the user if it changes what you do."
        )
    } else if pushed {
        format!(
            "Collide: {whom} ({agent}) changed {path} {age} and pushed it, but your copy does not have it yet. Pull first (git pull --rebase), re-read {path}, then make your edit on top of theirs. Say so to the user if it changes what you do."
        )
    } else {
        format!(
            "Collide: {whom} ({agent}) is changing {path} in their own checkout ({age}); it is not pushed, so your copy does not have it. Do not redo their change: make your edit fit theirs; you merge when you push (Collide rebases for you). Say so to the user if it changes what you do."
        )
    }
}

fn age_text(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 120.0 {
        "just now".to_string()
    } else if s < 7200.0 {
        format!("{}m ago", (s / 60.0) as i64)
    } else if s < 172_800.0 {
        format!("{}h ago", (s / 3600.0) as i64)
    } else {
        format!("{}d ago", (s / 86_400.0) as i64)
    }
}

pub fn join_notes(parts: &[&str]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("\n")
}
