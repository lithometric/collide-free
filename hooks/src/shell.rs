//! Deciding which files a tool call touched.
//!
//! Harnesses hand shell calls over as one opaque command string with no
//! file_path, and some steer agents to edit with `sed`, heredocs and inline
//! scripts. Treating those as reads let an agent rewrite a whole repo while
//! the workspace tree stood still, so the command's tokens are matched
//! against files the filesystem says just changed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use fancy_regex::Regex as FancyRegex;
use regex::Regex;

use crate::config::{absolute, normpath, rel_path};

pub const MAX_SHELL_GATE_FILES: usize = 5;
pub const MAX_SHELL_WRITE_FILES: usize = 5;
pub const SHELL_WRITE_WINDOW_S: f64 = 20.0;

pub const PATCH_MARKERS: [&str; 4] = [
    "*** Add File:",
    "*** Update File:",
    "*** Delete File:",
    "*** Move to:",
];

/// Shell idioms that can write a file. Deliberately over-inclusive: this only
/// decides whether it is worth stat-ing anything. `2>&1` and `>&2` are
/// excluded so redirecting diagnostics doesn't look like an edit.
fn write_idiom() -> &'static FancyRegex {
    static RE: OnceLock<FancyRegex> = OnceLock::new();
    RE.get_or_init(|| {
        FancyRegex::new(concat!(
            r"(?<![0-9&])>>?(?!&)",
            r"|\bsed\b[^|;]*\s-[A-Za-z]*i",
            r"|\b(?:tee|cp|mv|touch|install|dd|truncate|patch|rsync|ln)\b",
            r"|\bgit\s+(?:apply|checkout|restore|stash|mv|rm|clean)\b",
            r"|\b(?:python[0-9.]*|py|node|deno|bun|perl|ruby|php)\b[^|;]*\s-[ce]\b",
            r"|\b(?:python[0-9.]*|py)\s+-m\b",
            r"|<<-?\s*['\x22]?\w+",
            // a program that was RUN: a script writes files that appear
            // nowhere on its command line, so the window and git's view of
            // what changed decide
            r"|(?:^|[;&|(]\s*)(?:\w+=[^\s;&|]*\s+)*(?:(?:sudo|env|time|exec|nice)\s+)?",
            r"(?:python[0-9.]*|py|node|deno|bun|perl|ruby|php|bash|sh|zsh|make|npm|npx|pnpm|yarn|cargo|go)\s+[^\s|;&<>]+",
            r"|(?:^|[;&|(]\s*)\.{1,2}/\S+",
        ))
        .expect("write idiom pattern")
    })
}

/// Anything in the command that could name a file: a run containing a path
/// separator, or a bare `name.ext`. Quotes fall outside the class on purpose,
/// which is what finds the path inside `python -c "open('src/a.py','w')"`.
fn path_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\w.~@+-]*(?:[/\\][\w.~@+-]+)+|[\w~@+-]+\.[A-Za-z0-9_]{1,12}")
            .expect("path token pattern")
    })
}

fn cd_re() -> &'static FancyRegex {
    static RE: OnceLock<FancyRegex> = OnceLock::new();
    RE.get_or_init(|| FancyRegex::new(r"\b(?:cd|pushd)\s+(?!-)([^\s;&|]+)").expect("cd pattern"))
}

fn git_c_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bgit\s+-C\s+([^\s;&|]+)").expect("git -C pattern"))
}

pub fn has_write_idiom(command: &str) -> bool {
    write_idiom().is_match(command).unwrap_or(false)
}

fn tokens(command: &str) -> Vec<String> {
    path_token()
        .find_iter(command)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn expanduser(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home.trim_end_matches('/'), rest)
    } else {
        path.to_string()
    }
}

/// True when a shell command's work belongs to a DIFFERENT repo: it cd/pushd-es
/// outside the root, or aims git elsewhere with `git -C`. Conservative by
/// design — a target that cannot be resolved to a concrete in-repo path (an
/// unexpanded `$VAR`) counts as leaving.
pub fn command_leaves_repo(command: &str, cwd: &Path, root: &Path, home: &str) -> bool {
    let mut targets: Vec<String> = Vec::new();
    for caps in cd_re().captures_iter(command).flatten() {
        if let Some(m) = caps.get(1) {
            targets.push(m.as_str().to_string());
        }
    }
    for caps in git_c_re().captures_iter(command) {
        if let Some(m) = caps.get(1) {
            targets.push(m.as_str().to_string());
        }
    }
    for target in targets {
        let target = target.trim().trim_matches(|c| c == '\'' || c == '"');
        if target.is_empty() || target == "-" {
            continue;
        }
        if target.contains('$') {
            return true;
        }
        let expanded = expanduser(target, home);
        let resolved = if Path::new(&expanded).is_absolute() {
            PathBuf::from(&expanded)
        } else {
            cwd.join(&expanded)
        };
        if rel_path(&normpath(&resolved), root).is_none() {
            return true;
        }
    }
    false
}

fn mtime_secs(path: &Path) -> Option<f64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Absolute paths a shell command just wrote: every token naming a real file
/// inside the repo whose mtime lands in this command's window. A wrong guess
/// costs nothing — a file merely read has an old mtime and drops out.
pub fn shell_write_targets(command: &str, cwd: &Path, root: &Path) -> Vec<PathBuf> {
    if command.is_empty() || !has_write_idiom(command) {
        return Vec::new();
    }
    let cutoff = now_secs() - SHELL_WRITE_WINDOW_S;
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for token in tokens(command) {
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        for base in [cwd, root] {
            let candidate = if Path::new(&token).is_absolute() {
                PathBuf::from(&token)
            } else {
                base.join(&token)
            };
            let candidate = normpath(&candidate);
            if seen.contains(&candidate) {
                continue;
            }
            seen.push(candidate.clone());
            // a directory is no write target: `cd repo && pytest 2>&1` after
            // pytest made its cache there is a test run, not a write
            if rel_path(&candidate, root).is_none() || !candidate.is_file() {
                continue;
            }
            match mtime_secs(&candidate) {
                Some(mtime) if mtime >= cutoff => {
                    out.push(candidate);
                    if out.len() >= MAX_SHELL_WRITE_FILES {
                        return out;
                    }
                }
                _ => continue,
            }
        }
    }
    // what the command never named — see `recently_written_in_repo`
    for candidate in recently_written_in_repo(root, cutoff) {
        if seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate.clone());
        out.push(candidate);
        if out.len() >= MAX_SHELL_WRITE_FILES {
            break;
        }
    }
    out
}

/// Files under the repo written inside this command's window that the
/// command never named, sorted by path: git's view of what is modified or
/// new (ignored files excluded), stat-ed against the window.
///
/// `python3 fix.py` names only the script; the files it wrote appear nowhere
/// on its command line, so the token scan cannot see them. Without this a
/// scripted edit went unreported, the server's baseline for the file went
/// days stale, and the next report of it read as a rename that never
/// happened. Bounded and fail-open: no git, no repo, or too slow means
/// nothing extra, never a wrong answer. Mirrors `_recently_written_in_repo`
/// in report_hook.py, sorted the same way (by the path's text).
pub fn recently_written_in_repo(root: &Path, cutoff: f64) -> Vec<PathBuf> {
    let Some(listing) = crate::git::git(
        root,
        &["ls-files", "-m", "-o", "--exclude-standard", "-z"],
        std::time::Duration::from_millis(250),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for rel in listing.split('\0') {
        if rel.is_empty() {
            continue;
        }
        let candidate = normpath(&root.join(rel));
        if !candidate.is_file() {
            continue;
        }
        if mtime_secs(&candidate).is_some_and(|mtime| mtime >= cutoff) {
            out.push(candidate);
        }
    }
    out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    out
}

/// Shell commands that DISPLAY a file: what they show joins the map too.
const READ_CMDS: [&str; 8] = ["cat", "head", "tail", "bat", "less", "more", "nl", "sed"];
const MAX_SHELL_READ_FILES: usize = 4;

/// Absolute paths of repo source files a shell command displayed, so they
/// join the map the way a Read does. Only plain display idioms — cat, head,
/// tail, sed -n and friends — and only tokens naming an existing source file
/// inside the repo; a command that also writes is the write path's business.
/// Capped at four: a `cat` over a glob is an index, not a read. Mirrors
/// `_shell_read_targets` in report_hook.py.
pub fn shell_read_targets(command: &str, cwd: &Path, root: &Path) -> Vec<PathBuf> {
    if command.is_empty() || has_write_idiom(command) {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let joined = command.replace("||", "\u{1}").replace("&&", "\u{1}");
    for segment in joined.split(|c| c == ';' || c == '|' || c == '\u{1}') {
        let mut words = segment.split_whitespace().peekable();
        while words.peek().is_some_and(|w| w.contains('=') && !w.starts_with('-')) {
            words.next(); // leading VAR=value assignments
        }
        let Some(first) = words.next() else { continue };
        let base = Path::new(first).file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !READ_CMDS.contains(&base) {
            continue;
        }
        let rest: Vec<&str> = words.collect();
        if base == "sed"
            && (rest.iter().any(|a| a.starts_with("-i")) || !rest.iter().any(|a| a.starts_with("-n")))
        {
            continue;
        }
        for token in tokens(segment) {
            if token.is_empty() || token.starts_with('-') || token.starts_with('~') {
                continue;
            }
            let candidate = if Path::new(&token).is_absolute() {
                PathBuf::from(&token)
            } else {
                cwd.join(&token)
            };
            let candidate = normpath(&candidate);
            let Some(rel) = rel_path(&candidate, root) else { continue };
            if crate::report::is_map_source(&rel) && candidate.is_file() && !out.contains(&candidate) {
                out.push(candidate);
                if out.len() >= MAX_SHELL_READ_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// A command's simple commands, split on `;`, `&&`, `||`, `|` and newlines
/// outside quotes, so a separator inside `python -c "a; b"` stays put.
pub fn shell_segments(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    current.push(c);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                    continue;
                }
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                ';' | '\n' => out.push(std::mem::take(&mut current)),
                '|' | '&' if chars.peek() == Some(&c) => {
                    chars.next();
                    out.push(std::mem::take(&mut current));
                }
                '|' => out.push(std::mem::take(&mut current)),
                _ => current.push(c),
            },
        }
    }
    out.push(current);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// A simple command that copies (cp, install, rsync, ln) and runs nothing else
/// that writes: only its last path is written.
fn copies_only_to_last(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some(first) = words.iter().find(|w| !w.contains('=')) else { return false };
    let base = first.rsplit('/').next().unwrap_or(first);
    matches!(base, "cp" | "install" | "rsync" | "ln")
        && !segment.contains('>')
        && !segment.contains("<<")
}

/// PreToolUse runs BEFORE the command, so unlike the reporter this cannot lean
/// on mtime — there is no evidence yet, only the command text. It gates the
/// conservative subset: tokens naming a file that already exists, in a part of
/// the command that itself writes. `cat a.py && python -m pytest` reads a.py;
/// only the part with the write idiom can name what is written. Anything it
/// misses is simply not blocked, which is the right direction for a gate whose
/// whole contract is to fail open.
pub fn shell_gate_paths(command: &str, cwd: &Path) -> Vec<PathBuf> {
    if command.is_empty() || !has_write_idiom(command) {
        return Vec::new();
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for segment in shell_segments(command) {
        if !has_write_idiom(&segment) {
            continue;
        }
        let mut named = tokens(&segment);
        // `cp a b` (and install, rsync, ln) writes only its last path; the
        // sources are read. The staging demo's agent ran `cp daybook.py /tmp/x`
        // to diff it and was blocked on daybook.py. mv removes its sources,
        // so they stay gated.
        if copies_only_to_last(&segment) && named.len() > 1 {
            named = named.split_off(named.len() - 1);
        }
        for token in named {
            if token.is_empty() || token.starts_with('-') {
                continue;
            }
            let candidate = if Path::new(&token).is_absolute() {
                PathBuf::from(&token)
            } else {
                cwd.join(&token)
            };
            let candidate = normpath(&candidate);
            if seen.contains(&candidate) {
                continue;
            }
            seen.push(candidate.clone());
            if candidate.is_file() {
                out.push(candidate);
                if out.len() >= MAX_SHELL_GATE_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// Repo-relative paths a Codex `apply_patch` touches, read from its headers —
/// Codex passes only the patch text, never a file_path.
pub fn apply_patch_paths(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in command.lines() {
        let stripped = line.trim();
        for marker in PATCH_MARKERS {
            if let Some(rest) = stripped.strip_prefix(marker) {
                let path = rest.trim();
                if !path.is_empty() && !out.iter().any(|p| p == path) {
                    out.push(path.to_string());
                }
                break;
            }
        }
    }
    out
}

pub fn abs_targets(paths: &[String], cwd: &Path) -> Vec<PathBuf> {
    paths.iter().map(|p| absolute(&cwd.join(p))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_only_gates_the_part_of_a_command_that_writes() {
        let dir = std::env::temp_dir().join(format!("collide-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fmt.py"), "x\n").unwrap();
        std::fs::write(dir.join("notes.md"), "x\n").unwrap();
        let read_then_test = "cat fmt.py && echo '--- a; b ---' && find . -o -iname x -print && python -m pytest -q";
        assert!(shell_gate_paths(read_then_test, &dir).is_empty());
        assert_eq!(shell_gate_paths("cat fmt.py | tee notes.md", &dir), vec![dir.join("notes.md")]);
        // a copy reads its source: only the destination is written
        assert!(shell_gate_paths("cp fmt.py /tmp/check.py && diff /tmp/check.py fmt.py", &dir).is_empty());
        assert_eq!(shell_gate_paths("cp /tmp/x notes.md", &dir), vec![dir.join("notes.md")]);
        assert_eq!(shell_gate_paths("mv fmt.py notes.md", &dir), vec![dir.join("fmt.py"), dir.join("notes.md")]);
        assert_eq!(shell_gate_paths("python -c \"x=1; open('fmt.py','w')\"", &dir), vec![dir.join("fmt.py")]);
        assert_eq!(shell_gate_paths("cat > notes.md <<'EOF'\nhi; there\nEOF", &dir), vec![dir.join("notes.md")]);
        assert_eq!(shell_segments("a 2>&1 | b && c || d; e"), vec!["a 2>&1", "b", "c", "d", "e"]);
        assert_eq!(shell_segments("python -c \"a; b | c\" && d"), vec!["python -c \"a; b | c\"", "d"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
