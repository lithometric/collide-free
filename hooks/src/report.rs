//! PostToolUse / Stop: report EVERY file touch to Collide, mechanically.
//!
//! Writes post the file's current content to /report, which hashes, diffs and
//! discards it. Reads post only a path to /presence — no content ever leaves
//! the machine for a read. Shell calls are reported as writes when the
//! filesystem says they actually changed a file, otherwise as presence.
//!
//! HARD FAIL-OPEN: 500ms budget, always exits 0.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::{self, Env};
use crate::git;
use crate::http;
use crate::shell;
use crate::transcript;

pub const HOOK_VERSION: u32 = 48; // must match blocks.HOOK_ARTIFACT_VERSION
pub const TOTAL_BUDGET: Duration = Duration::from_millis(500);
const MAX_FILE_BYTES: u64 = 512 * 1024;

pub(crate) const WRITE_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "apply_patch"];
const READ_TOOLS: [&str; 4] = ["Read", "Grep", "Glob", "Bash"];
const SHELL_TOOLS: [&str; 3] = ["Bash", "shell", "run_terminal_cmd"];

const SENSITIVE_PATH_MARKERS: [&str; 11] = [
    ".ssh", ".gnupg", ".aws", ".collide", "credential", "secret", ".env", ".pem", ".key",
    "id_rsa", "token",
];

pub(crate) fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The server keeps 300 characters of a presence path; a longer command is
/// cut there and says so, instead of ending mid-word. Byte-identical to
/// the Python hook's `_shown_command`.
const SHOWN_COMMAND_MAX: usize = 300;

fn shown_command(command: &str) -> String {
    let command = redact_command(command);
    if command.chars().count() <= SHOWN_COMMAND_MAX {
        return command;
    }
    format!("{}\u{2026}", take_chars(&command, SHOWN_COMMAND_MAX - 1))
}

/// Secrets never leave the machine inside a command line. The dashboard
/// still shows `npm run seed`; the key in front of it is gone. Applied in
/// order: known token shapes, then every `NAME=value` assignment's value,
/// then auth headers, then passwords inside URLs. The Python hook's
/// `_redact_command`, pattern for pattern.
pub(crate) fn redact_command(command: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        REDACTIONS.iter().map(|(pattern, with)| (regex::Regex::new(pattern).expect("static regex"), *with)).collect()
    });
    let mut out = command.to_string();
    for (pattern, with) in rules {
        out = pattern.replace_all(&out, *with).into_owned();
    }
    out
}

/// (pattern, replacement) — kept byte-identical with report_hook.py's
/// _REDACTIONS; `${n}` is a capture group in both engines' syntax once the
/// Python side maps it to `\g<n>`.
const REDACTIONS: [(&str, &str); 7] = [
    (r"(sk-[A-Za-z0-9_-]{16,}|sk_(?:live|test)_[A-Za-z0-9]{8,}|rk_(?:live|test)_[A-Za-z0-9]{8,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[abposr]-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|cat_[A-Za-z0-9_]{16,}|re_[A-Za-z0-9_]{16,}|AIza[0-9A-Za-z_-]{30,})", "[redacted]"),
    (r#"(^|[\s;&|(])([A-Z_][A-Z0-9_]*)=("[^"]*"|'[^']*'|[^\s;&|]+)"#, "${1}${2}=[redacted]"),
    (r"(?i)(authorization:\s*(?:bearer|basic|token)\s+)[^\s'\x22]+", "${1}[redacted]"),
    (r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}", "${1}[redacted]"),
    (r"(://[^/\s:@]+:)[^@\s/]+@", "${1}[redacted]@"),
    (r"(?i)(--(?:password|passwd|token|secret|api-key|apikey)[=\s]+)[^\s;&|]+", "${1}[redacted]"),
    (r"(?i)([?&](?:token|key|secret|password|sig|signature|access_token|api_key|apikey)=)[^&\s'\x22]+", "${1}[redacted]"),
];

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// This machine, as a random id made once and kept beside the credential:
/// no hostname, nothing that names the person. Lets the server see one login
/// at work on several machines. Twin of report_hook.py `_machine_id`.
fn machine_id(env: &Env) -> String {
    let Some(dir) = config::credentials_path(env).parent().map(Path::to_path_buf) else { return String::new() };
    let path = dir.join("machine-id");
    if let Ok(found) = std::fs::read_to_string(&path) {
        let found = found.trim();
        if !found.is_empty() {
            return found.chars().take(32).collect();
        }
    }
    let mut bytes = [0u8; 8];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| { use std::io::Read; f.read_exact(&mut bytes) }).is_ok();
    if !filled {
        let seed = format!("{:?}{}{:p}", std::time::SystemTime::now(), std::process::id(), &bytes);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes());
        bytes.copy_from_slice(&digest[..8]);
    }
    let made: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(&path, format!("{made}\n")).is_err() {
        return String::new();
    }
    made
}

pub(crate) fn user_agent() -> String {
    format!("collide-report-hook/{HOOK_VERSION}")
}

/// Out-of-repo READS are presence too — the path travels, content never does.
/// Anything that smells like keys or credentials stays unreported.
fn external_label(file_path: &str, env: &Env) -> Option<String> {
    let lowered = file_path.to_lowercase();
    if SENSITIVE_PATH_MARKERS.iter().any(|m| lowered.contains(m)) {
        return None;
    }
    let home = config::get(env, "HOME");
    let shown = if !home.is_empty() && file_path.starts_with(home) {
        format!("~{}", &file_path[home.len()..])
    } else {
        file_path.to_string()
    };
    Some(take_chars(&shown, 160))
}

fn block_version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"cross-agent collision awareness \(v(\d+)\)").expect("block version pattern")
    })
}

/// The Collide block version this repo's AGENTS.md actually carries; 0 when
/// there is no block. Sent with every edit so the server can compare ground
/// truth against the version it ships.
fn agents_block_version(root: &Path) -> u32 {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let Ok(body) = std::fs::read_to_string(root.join(name)) else { continue };
        let head: String = body.chars().take(200_000).collect();
        if let Some(caps) = block_version_re().captures(&head) {
            if let Some(m) = caps.get(1) {
                if let Ok(version) = m.as_str().parse() {
                    return version;
                }
            }
        }
    }
    0
}

/// Cross-agent awareness, pushed INTO the session: teammates' certain
/// interface changes since this agent last talked to Collide, so it adapts
/// within one tool call of a teammate's save.
/// The server asked for one line of rationale: the symbol just written is
/// hot (other agents on the file) and the write carried no reason. Mirrors
/// `_ask_why_note`, byte for byte.
/// The server predicted the file this agent touches next and sent its
/// facts: printed once per session per file, and remembered as briefed.
/// Mirrors `_prefetch_note`.
fn prefetch_note(response: &Value, session: &str, env: &Env) -> String {
    let Some(next) = response.get("prefetch").filter(|n| n.is_object()) else { return String::new() };
    let path = text(next, "path");
    let note = text(next, "text");
    if path.is_empty() || note.is_empty() {
        return String::new();
    }
    if !session.is_empty() {
        let state = config::load_json(&crate::check::state_path(session, env));
        let briefed = state.get("briefed_paths").and_then(Value::as_array).map(|a| a.iter().any(|v| v.as_str() == Some(path.as_str()))).unwrap_or(false);
        if briefed {
            return String::new();
        }
        crate::prompt::remember_briefed(session, &[json!(path)], &[], &note, &[], env);
    }
    note
}

fn ask_why_note(response: &Value) -> String {
    let Some(ask) = response.get("ask_why").filter(|a| a.is_object()) else { return String::new() };
    let path = text(ask, "path");
    if path.is_empty() {
        return String::new();
    }
    let symbol = text(ask, "symbol");
    let where_ = if symbol.is_empty() { path.clone() } else { format!("{path}::{symbol}") };
    let others = ask.get("others").and_then(Value::as_i64).unwrap_or(0);
    format!(
        "Collide: {where_} is hot ({others} other agent(s) on this file). One line, why this change? \
Say it to the user and record it: report_edit(repo_id, \"{path}\", why=\"...\")."
    )
}

fn awareness_context(response: &Value) -> String {
    let deltas = response
        .get("since_your_last_call")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    for delta in &deltas {
        let kind = text(delta, "kind");
        if kind == "symbol" && text(delta, "confidence") == "certain" {
            let detail = delta.get("detail");
            let symbol = text(delta, "symbol");
            let what = if let Some(map) = detail.and_then(Value::as_object) {
                let new_name = map.get("new_name").and_then(Value::as_str).unwrap_or("");
                if new_name.is_empty() {
                    continue;
                }
                let signature = map.get("signature").and_then(Value::as_str).unwrap_or("");
                format!("renamed {symbol} -> {new_name} ({signature})")
            } else {
                let change = text(delta, "change");
                // what can break a caller: a removal or a new signature. An
                // addition or a body edit broke nothing, and Study 5's agents
                // answered each one "unrelated, no action needed"
                let resigned = change == "modified"
                    && detail.and_then(Value::as_str).is_some_and(|s| s.starts_with("signature changed"));
                if change != "removed" && !resigned {
                    continue;
                }
                let mut what = format!("{change} {symbol}");
                if let Some(Value::String(s)) = detail {
                    if !s.is_empty() && change != "removed" {
                        what.push_str(&format!(" ({})", take_chars(s, 120)));
                    }
                }
                what
            };
            let who = {
                let named = text(delta, "who");
                if named.is_empty() {
                    let user = text(delta, "user");
                    if user.is_empty() { "a teammate".to_string() } else { user }
                } else {
                    named
                }
            };
            let path = {
                let p = text(delta, "path");
                if p.is_empty() { "?".to_string() } else { p }
            };
            lines.push(format!("- {who} {what} in {path}"));
        } else if kind == "tripwire" {
            let note = text(delta, "note");
            if note.is_empty() {
                continue;
            }
            let by = {
                let b = text(delta, "by");
                if b.is_empty() { "?".to_string() } else { b }
            };
            let path = {
                let p = text(delta, "path");
                if p.is_empty() { "?".to_string() } else { p }
            };
            lines.push(format!("- tripwire from {by} on {path}: {}", take_chars(&note, 160)));
        }
        if lines.len() >= 4 {
            break;
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "Collide: teammates changed code you may depend on since your last sync. Adapt to the \
NEW names/signatures before writing against the old ones (verify with \
check_collisions/get_symbol if unsure), and say so to the user:\n{}",
        lines.join("\n")
    )
}

/// The settings file this harness installs the hooks into.
fn settings_file(harness: &str) -> &'static str {
    match harness {
        "codex" => ".codex/hooks.json",
        "cursor" => ".cursor/hooks.json",
        // Hermes keeps its hooks in the user's own config, outside the repo,
        // so there is nothing here to fingerprint and nothing is claimed
        "hermes" => "",
        _ => ".claude/settings.json",
    }
}

/// A shell command that runs one of Collide's hooks, in any dialect. Exact
/// twin of `blocks.py::_is_collide_command`.
fn is_collide_command(text: &str) -> bool {
    text.contains("collide")
        && (text.contains(".collide/report_hook.py")
            || text.contains(".collide/gate_hook.py")
            || text.contains(".collide/bin/collide-hook")
            || text.contains("collide-hook")
            || text.contains("collide-report-hook")
            || text.contains("collide-gate-hook"))
}

/// Every Collide command, tagged with the event it is wired under — the
/// nearest enclosing key that is not the dialect's own "hooks"/"command"
/// plumbing — so wiring the same script to one more event changes the
/// digest. Exact twin of `blocks::walk_commands`.
fn walk_commands(node: &Value, event: &str, found: &mut std::collections::BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                let under = if key == "hooks" || key == "command" { event } else { key.as_str() };
                walk_commands(value, under, found);
            }
        }
        Value::Array(items) => {
            for value in items {
                walk_commands(value, event, found);
            }
        }
        Value::String(text) if is_collide_command(text) => {
            found.insert(format!("{event}\t{text}"));
        }
        _ => {}
    }
}

/// Fingerprint of the Collide hooks this repo's settings file installs, or
/// empty when there is no such file or it installs none of ours.
///
/// The SCRIPTS carry a version of their own. The settings block does not,
/// and it is what decides whether those scripts are ever reached — which
/// events are wired, and whether the native-binary branch exists at all. So
/// a repo can be perfectly current on the scripts and years behind here,
/// which nothing could see before. Empty is the safe answer everywhere: the
/// server only compares a digest it was actually given.
fn settings_digest(root: &Path) -> String {
    let name = settings_file(crate::harness::current());
    if name.is_empty() {
        return String::new();
    }
    let Ok(body) = std::fs::read_to_string(root.join(name)) else { return String::new() };
    let Ok(parsed) = serde_json::from_str::<Value>(&body) else { return String::new() };
    let mut found = std::collections::BTreeSet::new();
    walk_commands(&parsed, "", &mut found);
    if found.is_empty() {
        return String::new();
    }
    let joined = found.into_iter().collect::<Vec<_>>().join("\n");
    sha256_hex(joined.as_bytes())[..12].to_string()
}

/// The staleness notice, said once rather than on every single edit.
///
/// It used to ride every response unfiltered, so a repo that could not fix
/// itself (the shadowing binary, before `supersede`) repeated it forever.
/// The stamp carries this hook's own version too, so a LATER version that is
/// still behind gets to speak again instead of inheriting the old silence.
fn artifacts_note(response: &Value, root: &Path) -> String {
    let notice = text(response, "artifacts_outdated");
    if notice.is_empty() {
        return String::new();
    }
    let stamp = git::stamp_path(root, "artifacts");
    let stored = config::load_json(&stamp);
    let told = stored.get("notified").and_then(Value::as_str).unwrap_or("");
    let told_at = stored.get("version").and_then(Value::as_u64).unwrap_or(0);
    if told == notice && told_at == HOOK_VERSION as u64 {
        return String::new();
    }
    let _ = std::fs::write(
        &stamp,
        json!({"notified": notice, "version": HOOK_VERSION, "ts": 0}).to_string(),
    );
    notice
}

/// The setup check rides the CONSTANT call, so a repo whose protocol block is
/// missing or behind is caught within one edit. Repeats only when the notice
/// CHANGES — told once, never nagged.
fn setup_note(response: &Value, root: &Path) -> String {
    let hint = text(response, "setup_pending");
    if hint.is_empty() {
        return String::new();
    }
    let stamp = git::stamp_path(root, "setup");
    let stored = config::load_json(&stamp);
    if stored.get("notified").and_then(Value::as_str) == Some(hint.as_str()) {
        return String::new();
    }
    let _ = std::fs::write(&stamp, json!({"notified": hint, "ts": 0}).to_string());
    format!(
        "Collide: {hint} Act on this unprompted at your next clean moment: call the collide tool \
setup, apply its agents_md_block with sync_agents_block, and commit the result — until then \
the whole team is following an out-of-date protocol."
    )
}

/// Where a teammate's new name is, as this checkout sees it: in the working
/// tree ("here"), only on origin ("origin"), or not landed at all. Study 5's
/// agents switched to a rename still in flight, then waited on failing tests.
fn name_state(root: &Path, name: &str) -> &'static str {
    let found = |args: &[&str]| crate::git::git(root, args, Duration::from_secs(2)).is_some();
    // no git to ask: the name is taken as here, the advice as it always was
    if name.is_empty() || !found(&["rev-parse", "--git-dir"]) || found(&["grep", "-q", "-w", "-e", name, "--", ":/"]) {
        "here"
    } else if found(&["grep", "-q", "-w", "-e", name, "@{u}", "--", ":/"]) {
        "origin"
    } else {
        "nowhere"
    }
}

/// Findings from the semantic linter, phrased for the agent's context.
fn lint_context(response: &Value, skip_mechanical: bool, root: &Path) -> String {
    let findings = response.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();
    if findings.is_empty() {
        return String::new();
    }
    let mode = {
        let m = text(response, "autofix_mode");
        if m.is_empty() { "findings".to_string() } else { m }
    };
    let mut lines = vec!["Collide lint — teammates' in-flight changes affect this file:".to_string()];
    let mut judgment_present = false;
    for finding in findings.iter().take(5) {
        let fix = finding.get("fix").cloned().unwrap_or(Value::Null);
        if skip_mechanical && text(&fix, "class") == "mechanical" {
            continue;
        }
        let lines_at = finding
            .get("lines")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "None".into());
        if text(finding, "rule") == "stale_name" {
            let by = {
                let b = text(finding, "by");
                if b.is_empty() { "a teammate".to_string() } else { b }
            };
            let (old, new) = (text(finding, "symbol"), text(finding, "renamed_to"));
            lines.push(match name_state(root, &new) {
                "here" => format!(
                    "- `{old}` was renamed to `{new}` by {by}; this file still uses the old name (lines {lines_at}). \
Mechanical fix: replace the identifier."
                ),
                "origin" => format!(
                    "- `{old}` was renamed to `{new}` by {by}; it is on origin but not in your checkout (lines {lines_at} use \
the old name). Pull or rebase onto it first, then replace the identifier."
                ),
                _ => format!(
                    "- `{old}` was renamed to `{new}` by {by}, not on origin yet (lines {lines_at} use the old name). Keep the \
old name until it lands: your checkout does not have the new one, so switching now breaks your tests. Once you pull or \
rebase onto it, replace the identifier."
                ),
            });
        } else {
            judgment_present = true;
            lines.push(format!(
                "- {}: `{}` (lines {}) — {}",
                text(finding, "rule"),
                text(finding, "symbol"),
                lines_at,
                take_chars(&text(&fix, "prompt"), 220)
            ));
        }
    }
    if lines.len() == 1 {
        return String::new();
    }
    if judgment_present && (mode == "propose" || mode == "auto") {
        lines.push(
            "Dispatch rule: for judgment-class findings, spawn ONE scoped fixer per finding if \
your harness allows it (Claude Code: Task tool or `claude -p '<fix>' &`; Codex: `codex exec \
'<fix>' &`; otherwise fix inline). Fixers must verify with tests/differential_check and must \
NEVER spawn further fixers."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Certified applier, auto mode only: word-boundary rename substitutions,
/// applied ONLY if the file on disk is byte-identical to what was just
/// reported (compare-and-swap — never guess on a moved file). The fix is
/// re-reported under agent=collide-autofix and its response discarded: fixers
/// never chain, which is the loop guard.
#[allow(clippy::too_many_arguments)]
fn apply_mechanical(
    response: &Value,
    root: &Path,
    target: &Path,
    rel: &str,
    reported_content: &str,
    cfg: &config::Config,
    meta: &Map<String, Value>,
    budget: Duration,
) -> String {
    if text(response, "autofix_mode") != "auto" {
        return String::new();
    }
    let renames: Vec<Value> = response
        .get("findings")
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(|f| f.get("fix").cloned())
                .filter(|fix| text(fix, "class") == "mechanical" && text(fix, "kind") == "rename")
                // only a rename this checkout already has: one still in flight would break it
                .filter(|fix| name_state(root, &text(fix, "to")) == "here")
                .collect()
        })
        .unwrap_or_default();
    if renames.is_empty() || budget.is_zero() {
        return String::new();
    }
    let Ok(on_disk) = std::fs::read_to_string(target) else { return String::new() };
    if on_disk != reported_content {
        return String::new();
    }
    let mut fixed = on_disk;
    let mut applied: Vec<String> = Vec::new();
    for fix in &renames {
        let from = text(fix, "from");
        let to = text(fix, "to");
        if from.is_empty() {
            continue;
        }
        let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(&from))) else { continue };
        let replaced = pattern.replace_all(&fixed, to.as_str()).into_owned();
        if replaced != fixed {
            applied.push(format!("{from} -> {to}"));
            fixed = replaced;
        }
    }
    if applied.is_empty() || std::fs::write(target, &fixed).is_err() {
        return String::new();
    }
    let mut payload = meta.clone();
    payload.insert("repo_id".into(), json!(cfg.repo_id));
    payload.insert("path".into(), json!(rel));
    payload.extend(cfg.source_fields(&rel, &fixed));
    payload.insert("agent".into(), json!("collide-autofix"));
    let _ = http::post(
        &cfg.server,
        "/report",
        &cfg.token,
        &user_agent(),
        &Value::Object(payload),
        budget,
    );
    format!(
        "Collide AUTOFIX rewrote {rel} on disk ({}) — the file changed since your last read; \
re-read it before editing again.",
        applied.join("; ")
    )
}

/// A repo renamed on the dashboard rides back on /report as
/// preferred_repo_id. Adopt it — but ONLY when this checkout's own git origin
/// already points at that repo. The origin match is the whole safety story: a
/// stale or wrong checkout can never be hijacked onto another scope.
fn adopt_preferred_repo_id(response: &Value, root: &Path, current_repo_id: &str) -> String {
    let preferred = text(response, "preferred_repo_id").trim().to_string();
    if preferred.is_empty() || preferred == current_repo_id {
        return String::new();
    }
    let origin = git::origin_repo_id(root);
    if origin.is_empty() || origin != git::normalize_repo_url(&preferred) {
        return String::new(); // can't confirm this checkout is the renamed repo
    }
    let cfg_path = root.join(".collide").join("config.json");
    let mut cfg = config::load_json(&cfg_path);
    if cfg.get("repo_id").and_then(Value::as_str) == Some(preferred.as_str()) {
        return String::new();
    }
    let Some(map) = cfg.as_object_mut() else { return String::new() };
    map.insert("repo_id".into(), json!(preferred));
    let Ok(body) = serde_json::to_string_pretty(&cfg) else { return String::new() };
    if std::fs::write(&cfg_path, format!("{body}\n")).is_err() {
        return String::new();
    }
    format!(
        "Collide: this repo was renamed to {preferred} — updated .collide/config.json to match. \
Commit it so teammates follow."
    )
}

/// A failed tool call (PostToolUseFailure): presence only. It reports that
/// the agent is running and what it tried, and prints nothing: the event has
/// no context channel we rely on, and whatever is pending waits for the next.
static FAILED_CALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn emit(note: &str) -> i32 {
    if FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    let note = crate::check::with_pending(note);
    // the context lands in the agent's session: awareness arrives with the
    // write, not at the next voluntary check. A harness that reads nothing
    // back queues it for the next turn instead.
    let rendered = crate::harness::render_context("PostToolUse", &note);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    0
}

/// Stop hook: the turn ended. If it ended because the agent ran out of quota,
/// tell Collide so the team sees "hit its limit" on the Now panel instead of a
/// teammate that silently went dark mid-task.
/// ConfigChange: the settings file just changed, and THIS hook firing is
/// proof Collide's hooks are loaded in this session.
///
/// Until now the setup nag cleared only when a file write was reported, so a
/// session that installed the hooks and then only read kept being told to
/// install them. Says nothing: the event discards context, so the only output
/// is the exit code.
fn run_config_change(hook_input: &Value, env: &Env) -> i32 {
    let file_path = text(hook_input, "file_path");
    let settings = if file_path.is_empty() { Value::Null } else { config::load_json(Path::new(&file_path)) };
    let hooks = settings.get("hooks").cloned().unwrap_or(Value::Null).to_string();
    if !hooks.contains("report_hook.py") && !hooks.contains("collide-hook") && !hooks.contains("collide-report-hook") {
        return 0; // an unrelated settings edit is not evidence about Collide
    }
    let session = text(hook_input, "session_id");
    let state_file = crate::check::state_path(&session, env);
    let mut state = config::load_json(&state_file).as_object().cloned().unwrap_or_default();
    if state.get("config_alive_sent").and_then(Value::as_bool).unwrap_or(false) {
        return 0; // ConfigChange fires on every settings write; report once
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(from_input) }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[Some(cwd.clone()), project]).unwrap_or(cwd);
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let payload = json!({"repo_id": cfg.repo_id, "session": session, "event": "ConfigChange",
                         "source": text(hook_input, "source"), "hook_version": HOOK_VERSION});
    if http::post(&cfg.server, "/hook-alive", &cfg.token, &user_agent(), &payload, Duration::from_secs(3)).is_err() {
        return 0;
    }
    state.insert("config_alive_sent".into(), json!(true));
    save_json(&state_file, &Value::Object(state));
    0
}

fn run_stop(hook_input: &Value, env: &Env, started: Instant) -> i32 {
    // the turn's last message replayed the context too; then the briefing's
    // tally for the turn, then the limit note
    crate::prompt::note_call(&text(hook_input, "session_id"), env);
    crate::prompt::brief_outcome(hook_input, env, started, false);
    let path = text(hook_input, "transcript_path");
    let note = transcript::limit_note(&path);
    if note.is_empty() {
        return 0;
    }
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    if remaining.is_zero() {
        return 0;
    }
    let mut meta = transcript::turn_meta(&path);
    for key in transcript::USAGE_KEYS {
        meta.remove(key); // a limit hit spends nothing
    }
    let session = text(hook_input, "session_id");
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    meta.insert("repo_id".into(), json!(cfg.repo_id));
    meta.insert("path".into(), json!(note));
    meta.insert("action".into(), json!("limit"));
    let _ = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(meta),
        remaining,
    );
    0
}

/// SessionEnd: the conversation is over — Claude Code's exit, /clear or
/// logout; Codex's close, or 30 idle minutes; Cursor's sessionEnd. Tell
/// Collide, so the Now panel stops counting this agent the moment it is gone
/// instead of an hour later, when silence is finally taken for absence. Fire
/// and forget: nothing is printed and nothing can block. The turns already
/// paid for themselves at Stop, so no usage rides along. Then the briefing
/// tally's last report for the session. Mirrors `_run_session_end`.
fn run_session_end(hook_input: &Value, env: &Env, started: Instant) -> i32 {
    let cwd = {
        let from_input = text(hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });
    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }
    let remaining = TOTAL_BUDGET.checked_sub(started.elapsed()).unwrap_or_default();
    if remaining.is_zero() {
        return 0;
    }
    let mut meta = transcript::turn_meta(&text(hook_input, "transcript_path"));
    for key in transcript::USAGE_KEYS {
        meta.remove(key);
    }
    let session = text(hook_input, "session_id");
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    meta.insert("repo_id".into(), json!(cfg.repo_id));
    meta.insert("action".into(), json!("ended"));
    let _ = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(meta),
        remaining,
    );
    // then the briefing's final word for this session: every briefed path
    // still only read is a miss now, and the cost of carrying the briefing
    // is reported whether or not a path ever settled. Its own budget: the
    // retirement above is the post that must not be starved.
    crate::prompt::brief_outcome(hook_input, env, Instant::now(), true);
    0
}

// ------------------------------------------------------------ the map
//
// The map — what blast_radius and the repo map answer from — is built from
// file CONTENT, and it used to learn a file only when someone wrote it, so a
// symbol's callers stayed invisible until an earlier agent happened to edit
// them. Two more doors: the tracked tree is indexed once at session start
// (then only what changed), and a Read posts the file it read. Only the
// languages the parser knows are worth sending. Mirrors report_hook.py.

pub(crate) const MAP_EXTS: [&str; 24] = [
    ".py", ".pyi", ".ts", ".mts", ".cts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".go", ".rs",
    ".java", ".cs", ".rb", ".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx", ".php",
];
const OBSERVE_MAX_BYTES: u64 = 512_000;
/// A session start may wait this long; the rest continues detached.
const INDEX_BUDGET: Duration = Duration::from_secs(20);
const INDEX_BATCH_FILES: usize = 64;
const INDEX_BATCH_BYTES: usize = 3_000_000;

pub fn is_map_source(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    MAP_EXTS.iter().any(|ext| lower.ends_with(ext)) && !is_collide_artifact(rel)
}

/// Collide's own installed files (`.collide/report_hook.py`, the gate, the
/// config) are tracked by git in every repo setup touches. They are not the
/// user's code: never indexed, reported, or counted as the session's work.
pub fn is_collide_artifact(rel: &str) -> bool {
    rel.starts_with(".collide/") || rel.contains("/.collide/")
}

/// The file's text, or None when it is too large or unreadable.
pub(crate) fn source_of(full: &Path) -> Option<String> {
    let meta = std::fs::metadata(full).ok()?;
    if meta.len() > OBSERVE_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(full).ok()?;
    Some(text_of_source(&bytes))
}

/// A source file's text as every OS reports it: UTF-8 (lossy), Windows line
/// endings as \n, the way git's autocrlf and Python's text mode see it. A
/// file checked out on Windows would otherwise differ, line for line, from
/// the same file on a teammate's Mac. Twin of report_hook.py's
/// `_text_of_source`.
pub(crate) fn text_of_source(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text.into_owned() }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Per-checkout memory of what the map already holds: path -> content
/// hash. Lives beside the credentials, never in the repo.
/// Keyed by the checkout AND where it reports: a checkout moved to another
/// server, workspace or repo id starts a fresh index there instead of
/// believing the new map already holds its files.
fn index_state_path(root: &Path, env: &Env, server: &str, repo_id: &str) -> PathBuf {
    let home = {
        let collide_home = config::get(env, "COLLIDE_HOME");
        let home = config::get(env, "HOME");
        if !collide_home.is_empty() { collide_home.to_string() }
        else if !home.is_empty() { home.to_string() }
        else { "~".to_string() }
    };
    let key = format!("{}\n{server}\n{repo_id}", config::absolute(root).to_string_lossy());
    let digest = sha256_hex(key.as_bytes());
    PathBuf::from(home).join(".collide").join("index").join(format!("{}.json", &digest[..16]))
}

pub(crate) fn save_json(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Every git-tracked file in a language the parser knows.
pub(crate) fn tracked_sources(root: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "ls-files", "-z"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty() && is_map_source(p))
        .map(str::to_string)
        .collect()
}

/// (path, content, sha) for every tracked source the map has not seen in
/// this exact form — the whole tree the first time, then only changes.
fn index_plan(root: &Path, state: &Value) -> Vec<(String, String, String)> {
    let known = state.get("files").and_then(Value::as_object);
    let mut plan = Vec::new();
    for rel in tracked_sources(root) {
        let Some(content) = source_of(&root.join(&rel)) else { continue };
        let sha = sha256_hex(content.as_bytes());
        if known.and_then(|k| k.get(&rel)).and_then(Value::as_str) == Some(sha.as_str()) {
            continue;
        }
        plan.push((rel, content, sha));
    }
    plan
}

/// Send what the map lacks, in batches, remembering each batch as it lands
/// so an interrupted run never repeats itself. Returns (sent, left).
pub fn index_repo(root: &Path, cfg: &config::Config, env: &Env, budget: Duration) -> (usize, usize) {
    let started = Instant::now();
    let state_path = index_state_path(root, env, &cfg.server, &cfg.repo_id);
    let state = config::load_json(&state_path);
    let plan = index_plan(root, &state);
    let mut files = state.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
    let (mut sent, mut i) = (0usize, 0usize);
    while i < plan.len() {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining <= Duration::from_secs(1) {
            break;
        }
        let start = i;
        let mut batch = Vec::new();
        let mut size = 0usize;
        while i < plan.len() && batch.len() < INDEX_BATCH_FILES && size < INDEX_BATCH_BYTES {
            let (rel, content, _sha) = &plan[i];
            let mut file = cfg.source_fields(rel, content);
            file.insert("path".into(), json!(rel));
            batch.push(Value::Object(file));
            size += content.len();
            i += 1;
        }
        let payload = json!({"repo_id": cfg.repo_id, "files": batch});
        if http::post(&cfg.server, "/observe", &cfg.token, &user_agent(), &payload,
                      remaining.min(Duration::from_secs(30))).is_err() {
            i = start;
            break; // unreachable server: stop here, the next session start resumes
        }
        for (rel, _content, sha) in &plan[start..i] {
            files.insert(rel.clone(), json!(sha));
        }
        sent += i - start;
        save_json(&state_path, &json!({"files": files, "ts": now_s()}));
    }
    (sent, plan.len() - i)
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// SessionStart: put the tracked tree in the map before the first tool
/// call, so the first agent's blast_radius is as complete as the fourth's.
/// stdout becomes session context, so it says one line and only when it
/// did something.
pub(crate) const BRIEF_BUDGET: u64 = 6_000;
pub(crate) const BRIEF_TIMEOUT: Duration = Duration::from_secs(10);
/// SessionStart sources after which the agent's context is gone.
pub(crate) const RESUME_SOURCES: [&str; 2] = ["compact", "resume"];

/// The dashboard behind a server address: COLLIDE_DASHBOARD_URL when set,
/// else the site the mcp.* (or api.*) host belongs to. Any other server —
/// localhost, a self-hosted box — has no known dashboard, so none opens.
/// Twin of the Python hook's `_dashboard_url`.
pub(crate) fn dashboard_url(server: &str, env: &Env) -> String {
    let explicit = config::get(env, "COLLIDE_DASHBOARD_URL").trim_end_matches('/');
    if !explicit.is_empty() {
        return format!("{explicit}/dashboard");
    }
    static HOST: OnceLock<Regex> = OnceLock::new();
    let host = HOST.get_or_init(|| Regex::new(r"^(https?)://(?:mcp|api)\.([^/:]+)").expect("valid regex"));
    host.captures(server).map(|c| format!("{}://{}/dashboard", &c[1], &c[2])).unwrap_or_default()
}

/// A browser opens only for a person at a desktop: never in CI, over SSH,
/// in a test run, on a headless Linux box, or with COLLIDE_NO_BROWSER=1.
pub(crate) fn may_open_browser(env: &Env) -> bool {
    let off = config::get(env, "COLLIDE_NO_BROWSER");
    if !off.is_empty() && off != "0" {
        return false;
    }
    let markers = ["CI", "GITHUB_ACTIONS", "SSH_CONNECTION", "SSH_TTY", "CODESPACES", "PYTEST_CURRENT_TEST"];
    if markers.iter().any(|m| !config::get(env, m).is_empty()) {
        return false;
    }
    if cfg!(target_os = "linux") && config::get(env, "DISPLAY").is_empty() && config::get(env, "WAYLAND_DISPLAY").is_empty() {
        return false;
    }
    true
}

/// Open the workspace's dashboard the first time a session starts in it on
/// this machine; the URL when it did, else empty. Remembered per workspace
/// in ~/.collide/dashboard_opened.json BEFORE opening, so a browser that
/// fails to launch is never retried every session. Twin of the Python
/// hook's `_open_dashboard_once`.
fn open_dashboard_once(root: &Path, server: &str, repo_id: &str, env: &Env) -> String {
    let url = dashboard_url(server, env);
    if url.is_empty() || !may_open_browser(env) {
        return String::new();
    }
    let repo_cfg = config::load_json(&root.join(".collide").join("config.json"));
    let workspace = repo_cfg.get("workspace").and_then(Value::as_str).unwrap_or("");
    let key = if workspace.is_empty() { format!("{server}|{repo_id}") } else { workspace.to_string() };
    let url = if workspace.is_empty() { url } else { format!("{url}?workspace={workspace}") };
    let home = [config::get(env, "COLLIDE_HOME"), config::get(env, "HOME"), config::get(env, "USERPROFILE")]
        .into_iter()
        .find(|h| !h.is_empty())
        .unwrap_or("")
        .to_string();
    let path = PathBuf::from(home).join(".collide").join("dashboard_opened.json");
    let mut opened = config::load_json(&path);
    if opened.get(&key).is_some() {
        return String::new();
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !opened.is_object() {
        opened = json!({});
    }
    opened[key.as_str()] = json!(stamp);
    save_json(&path, &opened);
    let mut command = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", &url]);
        c
    } else {
        let mut c = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        c.arg(&url);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        Ok(_) => url,
        Err(_) => String::new(),
    }
}

fn run_session_start(hook_input: &Value, env: &Env) -> i32 {
    // a signed-in free machine whose workspace went paid moves over first,
    // so this session already starts on Collide's servers
    let moved = crate::machine::plan_check(env);
    // a machine install updates itself (both binaries) from Collide's
    // releases, now and then; the local server has none to offer
    if crate::machine::installed(env) {
        crate::selfupdate::spawn_if_due(&crate::machine::update_server(env), env);
    }
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
    let (sent, left) = index_repo(&root, &cfg, env, INDEX_BUDGET);
    if cfg.machine_local {
        crate::machine::mark_indexed(&root, env);
    }
    let mut said: Vec<String> = moved.into_iter().collect();
    // the free version: the other agents are on this machine, and the way
    // to reach them is a command (the MCP tools may not be connected)
    if cfg.machine_local && !RESUME_SOURCES.contains(&text(hook_input, "source").as_str()) {
        said.push(format!(
            "Collide (free, on this machine): other agents working in this repo on this machine see your edits as you make them, \
and you see theirs. To tell them something, run: {} \"...\" (add --to agent-xxxx for one agent, \
or --about path.py::symbol for whoever is on that code).",
            crate::machine::message_command(env)
        ));
    }
    // a repo covered through this machine's install: the agent may offer
    // the user Collide's AGENTS.md section (it never writes it unasked)
    if !RESUME_SOURCES.contains(&text(hook_input, "source").as_str()) && crate::machine::installed(env) {
        if let Some(ask) = crate::machine::agents_md_ask(&root, env) {
            said.push(ask);
        }
    }
    // the first session in a workspace on this machine opens its live
    // dashboard, so the human watches the agents from the first call
    let source = text(hook_input, "source");
    if !RESUME_SOURCES.contains(&source.as_str()) {
        let opened = if cfg.machine_local { String::new() } else { open_dashboard_once(&root, &cfg.server, &cfg.repo_id, env) };
        if !opened.is_empty() {
            said.push(format!(
                "Collide: opened the live dashboard for this workspace in the browser ({opened}). \
Tell the user in one line that it shows every agent's edits here as they happen."
            ));
        }
    }
    if left > 0 {
        // a large tree finishes detached rather than holding the session
        if let Ok(exe) = std::env::current_exe() {
            let mut command = std::process::Command::new(exe);
            command
                .arg("index")
                .arg(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let _ = command.spawn();
        }
    }
    if sent > 0 || left > 0 {
        let tail = if left > 0 { format!(", {left} more in the background") } else { String::new() };
        said.push(format!("Collide: map indexed {sent} source file(s){tail}."));
    }
    // the briefing: the repo's shape on one page and the recently changed
    // modules as exact facts, printed into the session so the first message
    // starts oriented. After a compaction the context the agent had is gone:
    // the briefing then leads with this session's working set, the last check
    // verdict is said first, and what was briefed before counts as unseen.
    let session = text(hook_input, "session_id");
    let resume = RESUME_SOURCES.contains(&text(hook_input, "source").as_str());
    let mut payload = json!({"repo_id": cfg.repo_id, "budget": BRIEF_BUDGET});
    if resume {
        if let Some(map) = payload.as_object_mut() {
            map.insert("session".into(), json!(session));
            map.insert("resume".into(), json!(true));
        }
        crate::prompt::forget_briefed(&session, env);
        let line = crate::check::resume_line(&root, &session, env);
        if !line.is_empty() {
            said.push(line);
        }
    }
    if let Ok(brief) = http::post(&cfg.server, "/brief", &cfg.token, &user_agent(), &payload, BRIEF_TIMEOUT) {
        let text = brief.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            said.push(text.to_string());
        }
        let paths = brief.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
        let named = brief.get("named").and_then(Value::as_array).cloned().unwrap_or_default();
        let covered = brief.get("covered").and_then(Value::as_array).cloned().unwrap_or_default();
        crate::prompt::remember_briefed(&session, &paths, &named, text, &covered, env);
    }
    // once a day on the free version: what Collide did, shown to the person
    let summary = crate::machine::daily_summary(&cfg, env);
    // one emission in whatever shape this harness reads back
    match summary {
        Some(line) if crate::harness::current() == "claude" => {
            // Claude Code shows a systemMessage to the person directly
            let mut out = json!({"systemMessage": line});
            if !said.is_empty() {
                out["hookSpecificOutput"] = json!({"hookEventName": "SessionStart", "additionalContext": said.join("\n")});
            }
            println!("{out}");
        }
        other => {
            if let Some(line) = other {
                said.push(format!("{line} Tell the user this in one line."));
            }
            let rendered = crate::harness::render_context("SessionStart", &said.join("\n"));
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
    }
    // what already fails before the first write, so later failures can be
    // told apart from it — detached, the session does not wait
    crate::check::spawn_baseline(&root, hook_input, env);
    0
}

/// `index <root>`: the detached tail of a session-start index — no output,
/// long budget.
pub fn index_command(root: &str, env: &Env) -> i32 {
    if root.is_empty() {
        return 0;
    }
    let root = PathBuf::from(root);
    let cfg = config::config(Some(&root), env);
    if cfg.usable() {
        let _ = index_repo(&root, &cfg, env, Duration::from_secs(600));
    }
    0
}

pub fn run(stdin_data: &str, env: &Env) -> i32 {
    let code = run_event(stdin_data, env);
    // a verdict nothing else carried: emit it alone, in the event's own shape
    let leftover = crate::check::leftover();
    if !leftover.is_empty() {
        let event = serde_json::from_str::<Value>(stdin_data)
            .map(crate::harness::normalize)
            .ok()
            .map(|v| text(&v, "hook_event_name"))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "PostToolUse".to_string());
        // ConfigChange reads nothing back, so emitting here would lose the
        // verdict rather than defer it; render_context already returns empty
        // for any event this harness cannot inject into.
        if event != "ConfigChange" {
            let rendered = crate::harness::render_context(&event, &leftover);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
    }
    code
}

fn run_event(stdin_data: &str, env: &Env) -> i32 {
    let started = Instant::now();
    let Ok(hook_input) = serde_json::from_str::<Value>(stdin_data) else { return 0 };
    let hook_input = crate::harness::normalize(hook_input);
    // ConfigChange must return before this: Claude Code discards
    // additionalContext on that event, so a verdict moved into the pending
    // slot here would be emitted into a channel that throws it away — lost,
    // not deferred.
    if text(&hook_input, "hook_event_name") == "ConfigChange" {
        return run_config_change(&hook_input, env);
    }
    // likewise SessionEnd: nothing printed here reaches anyone
    if text(&hook_input, "hook_event_name") == "SessionEnd" {
        return run_session_end(&hook_input, env, started);
    }
    // a call that failed changed nothing: the agent is running, and that is
    // all it reports (Claude Code sends no PostToolUse for it, so a session
    // whose commands failed looked like no agent at all)
    let failed = text(&hook_input, "hook_event_name") == "PostToolUseFailure";
    if failed {
        FAILED_CALL.store(true, std::sync::atomic::Ordering::Relaxed);
    } else {
        crate::check::deliver_pending(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "PostToolBatch" {
        return crate::check::run_post_tool_batch(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "UserPromptSubmit" {
        // a free machine whose workspace just went paid (`collide upgrade`)
        // moves over at the next prompt, not only at the next session
        let moved = crate::machine::plan_check(env);
        let code = crate::prompt::run_user_prompt(&hook_input, env);
        // plain text joins the briefing; a JSON harness gets one object per
        // event, so it hears at its next session start instead
        if let Some(line) = moved.filter(|_| !matches!(crate::harness::current(), "cursor" | "hermes")) {
            let rendered = crate::harness::render_context("UserPromptSubmit", &line);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
        return code;
    }
    if text(&hook_input, "hook_event_name") == "SessionStart" {
        return run_session_start(&hook_input, env);
    }
    if text(&hook_input, "hook_event_name") == "Stop" {
        return run_stop(&hook_input, env, started);
    }
    let tool = text(&hook_input, "tool_name");
    if !WRITE_TOOLS.contains(&tool.as_str()) && !READ_TOOLS.contains(&tool.as_str()) {
        return 0;
    }
    let cwd = {
        let from_input = text(&hook_input, "cwd");
        if from_input.is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(from_input)
        }
    };
    let tool_input = hook_input.get("tool_input").cloned().unwrap_or(Value::Null);
    let session_id = text(&hook_input, "session_id");
    crate::prompt::note_call(&session_id, env);

    // locate the repo root: the edited file's directory is the strongest hint,
    // then the cwd, then the harness-provided project dir. Paths are reported
    // relative to this root so a drifted cwd cannot fragment a file's identity.
    let mut is_write = !failed && WRITE_TOOLS.contains(&tool.as_str());
    let mut targets: Vec<PathBuf> = Vec::new();
    let hint = if is_write {
        targets = {
            let direct = {
                let file_path = text(&tool_input, "file_path");
                if file_path.is_empty() { text(&tool_input, "notebook_path") } else { file_path }
            };
            if !direct.is_empty() {
                vec![PathBuf::from(direct)]
            } else {
                let command = text(&tool_input, "command");
                if command.contains("*** ") {
                    shell::abs_targets(&shell::apply_patch_paths(&command), &cwd)
                } else {
                    Vec::new()
                }
            }
        };
        targets.first().and_then(|t| t.parent().map(Path::to_path_buf))
    } else {
        let read_path = {
            let file_path = text(&tool_input, "file_path");
            if file_path.is_empty() { text(&tool_input, "path") } else { file_path }
        };
        (!read_path.is_empty())
            .then(|| PathBuf::from(&read_path).parent().map(Path::to_path_buf))
            .flatten()
    };

    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let root = config::find_repo_root(&[
        hint,
        Some(cwd.clone()),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ])
    .unwrap_or_else(|| {
        if project_dir.is_empty() { cwd.clone() } else { PathBuf::from(project_dir) }
    });

    let cfg = config::config(Some(&root), env);
    if !cfg.usable() {
        return 0;
    }

    // The pull runs on EVERY hook fire, not only writes: reads are the
    // majority of an agent's tool calls, and a session that only read used to
    // drift arbitrarily far behind with no signal. Cost is bounded — the
    // network fetch is throttled and the local fast-forward is microseconds.
    let sync_note = git::sync_note(&root, git::auto_pull_enabled(&root, &config::get(env, "COLLIDE_AUTO_PULL")));

    let mut meta = transcript::turn_meta(&text(&hook_input, "transcript_path"));
    // Which conversation made this edit. Claude Code hands every hook a
    // session_id, and a subagent has its own — so forwarding it is what lets
    // the ledger tell one agent's spend from another's when several work
    // under one login. Without it every agent on a machine collapses into a
    // single row and the per-agent economics are unmeasurable.
    let session = text(&hook_input, "session_id");
    if !session.is_empty() {
        meta.insert("session".into(), json!(session));
    }
    // a tool call made inside a subagent carries the parent's session_id and
    // its own agent_id: the worker. Forwarded so the server can count how
    // many hands one conversation has on the repo right now.
    let worker = text(&hook_input, "agent_id");
    if !worker.is_empty() {
        meta.insert("worker".into(), json!(worker));
        let worker_type = text(&hook_input, "agent_type");
        if !worker_type.is_empty() {
            meta.insert("worker_type".into(), json!(worker_type));
        }
    }
    let branch = git::branch(&root);
    if !branch.is_empty() {
        meta.insert("branch".into(), json!(branch));
    }
    let machine = machine_id(env);
    if !machine.is_empty() {
        meta.insert("machine".into(), json!(machine));
        // which working copy this is: two sessions in one checkout share
        // every change on disk; two checkouts only share what is pushed
        let checkout = sha256_hex(format!("{machine}\n{}", root.display()).as_bytes());
        meta.insert("checkout".into(), json!(checkout.get(..16).unwrap_or("")));
    }
    // the git origin as a repo id: when it disagrees with the committed config
    // the server treats it as a rename and hands back preferred_repo_id
    let origin = git::origin_repo_id(&root);
    if !origin.is_empty() {
        meta.insert("origin".into(), json!(origin));
    }

    // a shell command that actually changed files is a WRITE, whatever the
    // harness labelled it
    if !is_write && SHELL_TOOLS.contains(&tool.as_str()) {
        let shell_targets = shell::shell_write_targets(&text(&tool_input, "command"), &cwd, &root);
        if !shell_targets.is_empty() {
            is_write = true;
            targets = shell_targets;
        }
    }

    // local work is not Collide's to spend: git (the fetch, and every spawn
    // that reads the branch, the origin or the ignore rules, 50-150 ms each
    // on Windows) and the disk are discounted, so the budget bounds the
    // network alone and a slow machine can never starve the post that follows
    let local = std::cell::Cell::new(started.elapsed());
    let budget_left = || {
        TOTAL_BUDGET
            .checked_sub(started.elapsed().saturating_sub(local.get()))
            .unwrap_or_default()
    };

    if is_write {
        let mut context = String::new();
        let agents_version = agents_block_version(&root);
        let digest = settings_digest(&root);
        let payload_for = |rel: &str, content: &str, meta: &Map<String, Value>| -> Value {
            let mut payload = meta.clone();
            payload.insert("repo_id".into(), json!(cfg.repo_id));
            payload.insert("path".into(), json!(rel));
            payload.extend(cfg.source_fields(rel, content));
            payload.insert("hook_version".into(), json!(HOOK_VERSION));
            payload.insert("agents_version".into(), json!(agents_version));
            payload.insert("settings_digest".into(), json!(digest));
            Value::Object(payload)
        };
        // reports an earlier run could not deliver go first, as the files
        // are now, with a third of this run's budget at most
        local.set(started.elapsed());
        let resend_budget = budget_left() / 3;
        crate::spool::resend(env, &cfg.server, &cfg.repo_id, &cfg.token, &user_agent(), &root, resend_budget,
            &|rel, content, meta| (!is_collide_artifact(rel)).then(|| payload_for(rel, content, meta)));
        for target in &targets {
            let Some(rel) = config::rel_path(target, &root) else { continue };
            if is_collide_artifact(&rel) {
                continue;
            }
            // before note_touch as well: a scratch file is not the session's
            // work either, and counting it would flatter the hit rate
            let git_started = Instant::now();
            let ignored = git::is_ignored(&root, &rel);
            local.set(local.get() + git_started.elapsed());
            if ignored {
                continue;
            }
            crate::prompt::note_touch(&session_id, "written", &rel, env);
            match std::fs::metadata(target) {
                Ok(meta) if meta.len() > MAX_FILE_BYTES => continue,
                Err(_) => continue, // a deleted file (apply_patch delete) or unreadable
                _ => {}
            }
            let Ok(bytes) = std::fs::read(target) else { continue };
            let content = text_of_source(&bytes);
            if budget_left().is_zero() {
                break;
            }
            let payload = payload_for(&rel, &content, &meta);
            let response = match http::post(&cfg.server, "/report", &cfg.token, &user_agent(), &payload, budget_left()) {
                Ok(response) => {
                    crate::spool::forget(env, &cfg.server, &cfg.repo_id, &rel);
                    response
                }
                // the server did not get it: owed, and sent again next run
                Err(()) => {
                    crate::spool::remember(env, &cfg.server, &cfg.repo_id, &rel, &meta);
                    Value::Object(Default::default())
                }
            };

            // a certain interface change: its dependents' tests run detached
            // and grade the change on the graph; nothing of it reaches the agent
            if let Some(plan) = response.get("verify").filter(|p| p.get("tests").and_then(Value::as_array).is_some_and(|t| !t.is_empty())) {
                crate::verify::spawn(&root, &session_id, plan);
            }
            let fix_note =
                apply_mechanical(&response, &root, target, &rel, &content, &cfg, &meta, budget_left());
            let lint_note = lint_context(&response, !fix_note.is_empty(), &root);
            let rename_note = adopt_preferred_repo_id(&response, &root, &cfg.repo_id);
            let awareness = if context.is_empty() { awareness_context(&response) } else { context.clone() };
            let pieces = [
                fix_note,
                lint_note,
                rename_note,
                setup_note(&response, &root),
                sync_note.clone(),
                artifacts_note(&response, &root),
                ask_why_note(&response),
                prefetch_note(&response, &session_id, env),
                awareness,
                crate::machine::inbox(&response),
            ];
            let mut kept: Vec<String> = Vec::new();
            for piece in pieces.into_iter().filter(|p| !p.is_empty()) {
                if !kept.contains(&piece) {
                    kept.push(piece);
                }
            }
            if !kept.is_empty() {
                context = kept.join("\n");
            }
        }
        // a test run that also left files behind (`pytest 2>&1 | tail` just
        // after an edit reads as a shell write): its verdict still counts,
        // and it goes after the writes it judged
        if SHELL_TOOLS.contains(&tool.as_str()) && !budget_left().is_zero() {
            let command = text(&tool_input, "command");
            let failed = FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(ok) = test_verdict(&command, &shell_output(&hook_input), failed) {
                let mut payload = meta.clone();
                payload.insert("repo_id".into(), json!(cfg.repo_id));
                payload.insert("path".into(), json!(shown_command(command.trim())));
                payload.insert("action".into(), json!("running"));
                payload.insert("tests_ok".into(), json!(ok));
                let _ = http::post(&cfg.server, "/presence", &cfg.token, &user_agent(), &Value::Object(payload), budget_left());
            }
        }
        // additionalContext lands in the agent's session: awareness arrives
        // with the write, not at the next voluntary check
        return emit(&context);
    }

    // presence only: where the agent is looking, or what it is RUNNING.
    // Commands are presence too, but scoped to THIS repo — a command that cd's
    // into another repo is that repo's activity, not ours.
    if tool == "Bash" {
        let command = text(&tool_input, "command").trim().to_string();
        if command.is_empty()
            || shell::command_leaves_repo(&command, &cwd, &root, config::get(env, "HOME"))
            || budget_left().is_zero()
        {
            return emit(&sync_note);
        }
        // the map's answer to the name the agent just searched for
        let sync_note = crate::prompt::join_notes(&[&sync_note, &crate::prompt::grep_companion(&command, &cfg, budget_left(), &session_id)]);
        // what the command DISPLAYED joins the map, the way a Read does:
        // one file as a single observation, several as a batch
        let shown: Vec<Value> = shell::shell_read_targets(&command, &cwd, &root)
            .iter()
            .filter_map(|full| {
                let rel = config::rel_path(full, &root)?;
                if is_collide_artifact(&rel) {
                    return None;
                }
                let content = source_of(full)?;
                crate::prompt::note_touch(&session_id, "read", &rel, env);
                let mut file = cfg.source_fields(&rel, &content);
                file.insert("path".into(), json!(rel));
                Some(Value::Object(file))
            })
            .collect();
        if !shown.is_empty() {
            let mut payload = meta.clone();
            payload.insert("repo_id".into(), json!(cfg.repo_id));
            if shown.len() == 1 {
                if let Some(file) = shown[0].as_object() {
                    payload.extend(file.clone());
                }
            } else {
                payload.insert("files".into(), json!(shown));
            }
            let _ = http::post(
                &cfg.server, "/observe", &cfg.token, &user_agent(), &Value::Object(payload),
                budget_left(),
            );
            return emit(&sync_note);
        }
        let mut payload = meta.clone();
        payload.insert("repo_id".into(), json!(cfg.repo_id));
        payload.insert("path".into(), json!(shown_command(&command)));
        payload.insert("action".into(), json!("running"));
        // the repo's tests ran here: their verdict is how Collide knows a
        // task's work passed, with no commit involved
        let failed = FAILED_CALL.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(ok) = test_verdict(&command, &shell_output(&hook_input), failed) {
            payload.insert("tests_ok".into(), json!(ok));
        }
        let answer = http::post(
            &cfg.server,
            "/presence",
            &cfg.token,
            &user_agent(),
            &Value::Object(payload),
            budget_left(),
        );
        let inbox = answer.map(|a| crate::machine::inbox(&a)).unwrap_or_default();
        return emit(&crate::prompt::join_notes(&[&sync_note, &inbox]));
    }

    let read_path = {
        let file_path = text(&tool_input, "file_path");
        if file_path.is_empty() { text(&tool_input, "path") } else { file_path }
    };
    let (focus, action) = if !read_path.is_empty() {
        // a source file inside the repo joins the map with its content —
        // the same in-memory parse-and-discard as a write; presence rides
        // along server-side. Anything else stays a path-only presence ping.
        if let Some(inside) = config::rel_path(Path::new(&read_path), &root) {
            if is_map_source(&inside) && !budget_left().is_zero() {
                crate::prompt::note_touch(&session_id, "read", &inside, env);
                if let Some(content) = source_of(&root.join(&inside)) {
                    let mut payload = meta.clone();
                    payload.insert("repo_id".into(), json!(cfg.repo_id));
                    payload.insert("path".into(), json!(inside));
                    payload.extend(cfg.source_fields(&inside, &content));
                    let answer = http::post(
                        &cfg.server, "/observe", &cfg.token, &user_agent(),
                        &Value::Object(payload), budget_left(),
                    );
                    let (note, inbox) = answer
                        .map(|a| (crate::prompt::recent_write_note(&a, &inside), crate::machine::inbox(&a)))
                        .unwrap_or_default();
                    return emit(&crate::prompt::join_notes(&[&sync_note, &note, &inbox]));
                }
            }
        }
        let labelled = config::rel_path(Path::new(&read_path), &root)
            .or_else(|| external_label(&read_path, env));
        let Some(focus) = labelled else { return emit(&sync_note) };
        (focus, "reading")
    } else {
        let pattern = take_chars(&text(&tool_input, "pattern"), 200);
        if pattern.is_empty() {
            return emit(&sync_note);
        }
        (pattern, "searching")
    };
    if budget_left().is_zero() {
        return emit(&sync_note);
    }
    let mut payload = meta.clone();
    payload.insert("repo_id".into(), json!(cfg.repo_id));
    payload.insert("path".into(), json!(focus));
    payload.insert("action".into(), json!(action));
    let answer = http::post(
        &cfg.server,
        "/presence",
        &cfg.token,
        &user_agent(),
        &Value::Object(payload),
        budget_left(),
    );
    let inbox = answer.map(|a| crate::machine::inbox(&a)).unwrap_or_default();
    emit(&crate::prompt::join_notes(&[&sync_note, &inbox]))
}

/// `add-credentials [path]`: mechanically MERGE a credentials fragment into
/// ~/.collide/credentials.json — never overwrite, so one machine keeps every
/// workspace's tokens.
pub fn add_credentials(source: &str, env: &Env) -> i32 {
    let raw = if source.is_empty() {
        let mut buffer = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut buffer);
        buffer
    } else {
        match std::fs::read_to_string(source) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("add-credentials failed: {error}");
                return 1;
            }
        }
    };
    let fragment: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("add-credentials failed: {error}");
            return 1;
        }
    };
    let Some(fragment) = fragment.as_object().filter(|m| !m.is_empty()) else {
        eprintln!("add-credentials: fragment must be a non-empty JSON object");
        return 1;
    };
    let target = config::credentials_path(env);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // missing or corrupt: start fresh, keeping only the fragment
    let existing = config::load_json(&target);
    let existing = existing.as_object().cloned().unwrap_or_default();
    let mut merged = existing.clone();
    for (key, value) in fragment {
        merged.insert(key.clone(), value.clone());
    }
    let body = match serde_json::to_string_pretty(&Value::Object(merged)) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("add-credentials failed: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::write(&target, format!("{body}\n")) {
        eprintln!("add-credentials failed: {error}");
        return 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }
    let added = fragment
        .iter()
        .filter(|(key, value)| existing.get(*key) != Some(*value))
        .count();
    println!("merged {added} credential key(s) into {}", target.display());
    0
}

const INSTALL_NOTICE: &str = "Collide: this machine has no usable hook credential for {server} \
yet, so NOTHING you edit is being reported and every teammate's collision check goes stale \
against your work. Before any other task: call the collide MCP tool `setup` (repo_id {repo_id}), \
save the returned user_files JSON content to a temp file, and run `{install}` — a mechanical \
merge into ~/.collide/credentials.json that leaves other workspaces' tokens intact. One-time; \
every edit auto-reports afterwards. Tell the human when it is done.";

/// `check-credentials`: print a session-start notice when this machine cannot
/// actually report, silent when it can. Resolves the credential exactly the
/// way the hook does, so what it reports is what the hook will do.
pub fn check_credentials(python_cmd: &str, env: &Env) -> i32 {
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let Some(root) = config::find_repo_root(&[
        std::env::current_dir().ok(),
        (!project_dir.is_empty()).then(|| PathBuf::from(project_dir)),
    ]) else {
        return 0; // not a Collide repo: nothing to say
    };
    let cfg = config::config(Some(&root), env);
    if cfg.server.is_empty() || cfg.repo_id.is_empty() {
        return 0;
    }
    // once per session start: a binary older than the server's hook
    // version fetches its replacement in the background
    crate::selfupdate::spawn_if_due(&cfg.server, env);
    if cfg.token.is_empty() {
        // when the session-start shim found the native binary it passes its
        // own path, so the instruction names the tool that will actually run
        let install = if python_cmd.ends_with("collide-hook") {
            format!("{python_cmd} add-credentials <that file>")
        } else {
            format!("{python_cmd} .collide/report_hook.py --add-credentials <that file>")
        };
        println!(
            "{}",
            INSTALL_NOTICE
                .replace("{server}", &cfg.server)
                .replace("{repo_id}", &cfg.repo_id)
                .replace("{install}", &install)
        );
    }
    0 // never disturb a session start
}

/// What a shell command printed, as one text: the tool response's streams
/// and a failed call's error.
fn shell_output(hook_input: &Value) -> String {
    let mut out = String::new();
    match hook_input.get("tool_response") {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Object(map)) => {
            for key in ["stdout", "stderr", "output", "error"] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
    if let Some(s) = hook_input.get("error").and_then(Value::as_str) {
        out.push_str(s);
    }
    out
}

/// A test run's verdict from what the shell printed: Some(true) when the
/// repo's tests ran and passed, Some(false) when they ran and failed, None
/// when the command was no test run or printed nothing a verdict rests on.
/// Twin of report_hook.py's `_test_verdict`.
pub fn test_verdict(command: &str, output: &str, failed: bool) -> Option<bool> {
    let runs = Regex::new(
        r"(?:^|[\s;&|(])(?:pytest|py\.test|python3? -m (?:pytest|unittest)|npm (?:run )?test|yarn test|pnpm (?:run )?test|npx (?:jest|vitest)|jest|vitest|cargo test|go test|rspec|mvn test|gradle test|\./gradlew test|phpunit|dotnet test|mix test)\b",
    )
    .expect("static regex");
    if !runs.is_match(command) {
        return None;
    }
    if failed {
        return Some(false);
    }
    let fails = Regex::new(
        r"(?m)\b[1-9]\d* (?:failed|errors?)\b|^FAILED\b|^FAIL\b|test result: FAILED|Traceback \(most recent call last\)|panicked at",
    )
    .expect("static regex");
    if fails.is_match(output) {
        return Some(false);
    }
    let passes = Regex::new(r"(?m)\b\d+ passed\b|test result: ok\.|^ok\s+\S|Tests:\s+\d+ passed|\bOK \(\d+ tests?\)|^OK$")
        .expect("static regex");
    passes.is_match(output).then_some(true)
}

#[cfg(test)]
mod verdict_tests {
    use super::test_verdict;

    #[test]
    fn a_test_run_s_verdict_is_read_from_what_it_printed() {
        assert_eq!(test_verdict("python3 -m pytest -q 2>&1 | tail -30", "......\n14 passed in 0.02s\n", false), Some(true));
        assert_eq!(test_verdict("cd x && pytest -q", "1 failed, 13 passed in 0.03s", false), Some(false));
        assert_eq!(test_verdict("cargo test", "test result: ok. 5 passed; 0 failed; 0 ignored", false), Some(true));
        assert_eq!(test_verdict("npm test", "", true), Some(false));
        assert_eq!(test_verdict("go test ./...", "ok  \texample.com/m\t0.01s", false), Some(true));
        assert_eq!(test_verdict("git status", "14 passed", false), None);
        assert_eq!(test_verdict("pytest -q", "no tests ran in 0.01s", false), None);
    }
}

#[cfg(test)]
mod settings_digest_tests {
    use super::*;

    /// The digest is a handshake across three implementations: this hook, the
    /// Python hook, and the server that decides what "current" means. The
    /// fixture and its expected value were produced by `blocks.settings_digest`
    /// on the Python side, so a drift in any one of them fails here rather than
    /// reporting every correct install as stale.
    #[test]
    fn matches_the_python_digest_for_the_same_settings() {
        let dir = std::env::temp_dir().join("collide-settings-digest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude").join("settings.json"),
            r#"{"hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "sh -c \"$CLAUDE_PROJECT_DIR/.collide/report_hook.py\" # collide"}]}, {"matcher": "Read", "hooks": [{"type": "command", "command": "npm run lint"}]}]}, "other": ["collide-gate-hook --harness codex # collide"]}"#,
        )
        .unwrap();
        assert_eq!(settings_digest(&dir), "47fb8f9ef655");

        // no settings file: silence, never a wrong answer
        assert_eq!(settings_digest(&dir.join("nowhere")), "");
    }

    /// Somebody else's hooks are not ours, so merging Collide into a settings
    /// file that already had hooks must not read as drift.
    #[test]
    fn foreign_hooks_do_not_enter_the_digest() {
        let dir = std::env::temp_dir().join("collide-settings-digest-foreign");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude").join("settings.json"),
            r#"{"hooks": {"PostToolUse": [{"hooks": [{"command": "npm run lint"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(settings_digest(&dir), "");
    }
}

#[cfg(test)]
mod dashboard_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_dashboard_is_the_site_behind_the_mcp_host() {
        assert_eq!(dashboard_url("https://mcp.collidemcp.com", &env(&[])), "https://collidemcp.com/dashboard");
        assert_eq!(dashboard_url("https://api.example.io/", &env(&[])), "https://example.io/dashboard");
        assert_eq!(dashboard_url("http://localhost:8000", &env(&[])), "");
        assert_eq!(
            dashboard_url("http://localhost:8000", &env(&[("COLLIDE_DASHBOARD_URL", "http://localhost:3000/")])),
            "http://localhost:3000/dashboard"
        );
    }

    #[test]
    fn no_browser_in_ci_over_ssh_or_when_turned_off() {
        let desktop = [("DISPLAY", ":0")];
        assert!(may_open_browser(&env(&desktop)));
        for (key, value) in [("CI", "true"), ("SSH_CONNECTION", "1 2 3 4"), ("COLLIDE_NO_BROWSER", "1"), ("PYTEST_CURRENT_TEST", "x")] {
            assert!(!may_open_browser(&env(&[desktop[0], (key, value)])), "{key}");
        }
        assert!(may_open_browser(&env(&[desktop[0], ("COLLIDE_NO_BROWSER", "0")])));
    }
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn secrets_never_leave_in_a_command() {
        let cases = [
            ("STRIPE_KEY=sk_live_abcdef1234567890 npm run seed", "STRIPE_KEY=[redacted] npm run seed"),
            ("curl -H \"Authorization: Bearer ghp_0123456789abcdefghijABCDEFGHIJ\" https://x", "curl -H \"Authorization: Bearer [redacted]\" https://x"),
            ("git clone https://kevin:hunter22@github.com/a/b", "git clone https://kevin:[redacted]@github.com/a/b"),
            ("psql --password s3cret -h db", "psql --password [redacted] -h db"),
            ("npm test", "npm test"),
            ("git log --format=%an -n 3", "git log --format=%an -n 3"),
            ("export OPENAI_API_KEY='abc123'; python x.py", "export OPENAI_API_KEY=[redacted]; python x.py"),
            ("curl 'https://api.x.com/v1?id=4&token=abcdef'", "curl 'https://api.x.com/v1?id=4&token=[redacted]'"),
            ("python3 -m tests.check", "python3 -m tests.check"),
        ];
        for (raw, want) in cases {
            assert_eq!(redact_command(raw), want, "{raw}");
        }
    }
}

