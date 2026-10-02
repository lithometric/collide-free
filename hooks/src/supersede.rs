//! Stepping aside when the repo's committed hook is newer than this binary.
//!
//! The native binary is an optional speedup: per-platform, gitignored, and
//! installed once by hand. The committed Python hooks are the opposite —
//! tracked files that every `git pull` updates. So the two drift in only one
//! direction, and it is always the binary that falls behind.
//!
//! That would be harmless if the binary were the fallback. It is the
//! opposite: the generated settings command finds the binary FIRST and exits
//! before the script branch is reachable, so a stale binary silently shadows
//! a current script. The server notices the old version and says so, the
//! agent re-runs setup and commits fresh scripts, the binary keeps shadowing
//! them, and the notice repeats with nothing able to clear it.
//!
//! The fix is for the binary to check, and to hand the work to the script
//! whenever the script knows more than it does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{self, Env};
use crate::report::HOOK_VERSION;

/// Interpreters to try, in the same order and for the same reason as the
/// generated shell command's `_PY_RESOLVE`: on Windows `python3` is often a
/// Store alias stub that exits without running anything.
const INTERPRETERS: [&str; 3] = ["python3", "python", "py"];

/// The version the committed reporter declares, or 0 when it is missing or
/// silent. Only the reporter carries the marker; the gate ships from the same
/// `setup` call in the same commit, so the reporter's number speaks for both.
pub fn script_version(script: &Path) -> u32 {
    let Ok(body) = std::fs::read_to_string(script) else { return 0 };
    for line in body.lines().take(400) {
        let Some(rest) = line.trim_start().strip_prefix("HOOK_VERSION") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('=') else { continue };
        let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(found) = digits.parse::<u32>() {
            return found;
        }
    }
    0
}

fn repo_root(env: &Env) -> Option<PathBuf> {
    let project = config::get(env, "CLAUDE_PROJECT_DIR");
    config::find_repo_root(&[
        (!project.is_empty()).then(|| PathBuf::from(project)),
        std::env::current_dir().ok(),
    ])
}

/// `Some(exit code)` when the committed script outranks this binary and ran
/// in its place; `None` to carry on natively.
///
/// Everything here fails toward running natively: an unreadable script, no
/// interpreter, a spawn that never starts. Being one version behind is a
/// far smaller problem than not reporting at all.
pub fn delegate(command: &str, args: &[String], stdin_data: &str, env: &Env) -> Option<i32> {
    let root = repo_root(env)?;
    if script_version(&root.join(".collide").join("report_hook.py")) <= HOOK_VERSION {
        return None;
    }
    // this binary is behind the repo's hooks: fetch its replacement (in the
    // background, throttled) while the script does this call
    crate::selfupdate::spawn_if_due(&crate::config::config(Some(&root), env).server, env);
    let script = root
        .join(".collide")
        .join(if command == "gate" { "gate_hook.py" } else { "report_hook.py" });
    if !script.is_file() {
        return None;
    }
    for interpreter in INTERPRETERS {
        // argv after the subcommand carries `--harness <name>`, which the
        // script reads exactly as the generated command passes it
        let spawned = Command::new(interpreter)
            .arg(&script)
            .args(args)
            .stdin(Stdio::piped())
            .spawn();
        let Ok(mut child) = spawned else { continue };
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(stdin_data.as_bytes());
        }
        if let Ok(status) = child.wait() {
            return Some(status.code().unwrap_or(0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join(".collide")).unwrap();
        std::fs::write(dir.join(".collide").join(name), body).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("collide-supersede-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_the_version_the_committed_script_declares() {
        let dir = tmpdir("version");
        write(&dir, "report_hook.py", "#!/usr/bin/env python3\n\nHOOK_VERSION = 41  # a note\n");
        assert_eq!(script_version(&dir.join(".collide").join("report_hook.py")), 41);
    }

    #[test]
    fn a_script_with_no_marker_or_no_file_reads_as_zero() {
        let dir = tmpdir("silent");
        write(&dir, "report_hook.py", "print('hello')\n");
        assert_eq!(script_version(&dir.join(".collide").join("report_hook.py")), 0);
        assert_eq!(script_version(&dir.join(".collide").join("absent.py")), 0);
    }

    /// The whole point: an equal-or-older script must NOT take over, or every
    /// machine would pay Python's startup for nothing.
    #[test]
    fn only_a_strictly_newer_script_supersedes_the_binary() {
        let dir = tmpdir("rank");
        let env = Env::new();
        let path = dir.join(".collide").join("report_hook.py");

        write(&dir, "report_hook.py", &format!("HOOK_VERSION = {}\n", HOOK_VERSION));
        assert!(script_version(&path) <= HOOK_VERSION);

        write(&dir, "report_hook.py", &format!("HOOK_VERSION = {}\n", HOOK_VERSION - 1));
        assert!(script_version(&path) <= HOOK_VERSION);

        write(&dir, "report_hook.py", &format!("HOOK_VERSION = {}\n", HOOK_VERSION + 1));
        assert!(script_version(&path) > HOOK_VERSION);

        // and with no repo root to stand in, nothing is delegated at all
        assert_eq!(delegate("report", &[], "{}", &env).is_none(), true);
    }
}
