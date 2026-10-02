//! The native binary keeps itself current.
//!
//! The binary is per machine and gitignored, so no `git pull` ever updates
//! it. It used to wait for the committed Python script to outrank it
//! (`supersede`) and then for Python to fetch its replacement — which made
//! Python a requirement just to stay current. Now the binary asks the server
//! itself: CI publishes every hook version as a GitHub release, the server
//! mirrors those builds at `/artifacts/hooks`, and a binary older than the
//! server's version fetches its own platform's build, in a detached process
//! so no session waits.
//!
//! Nothing is trusted on the way in: the index must carry CI's Ed25519
//! signature over "collide-hook <target> v<N> sha256:<digest>", checked
//! against the public key compiled into THIS binary — so the server, or
//! anyone who takes it over, cannot hand out a build CI did not sign. The
//! download must then match that digest, and the new file must run and
//! report that version, before it replaces anything. The swap is a rename (on Windows,
//! where a running exe can't be overwritten, the old one is renamed aside
//! first). Any doubt leaves the working binary exactly as it was, and the
//! next attempt waits a few hours.
//!
//! Only the managed install updates itself — the copy under a repo's
//! `.collide/bin/` — never a collide-hook someone put on PATH by hand.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};
use crate::report::HOOK_VERSION;

/// How long after an attempt the next one may run: a server without a
/// build for this platform is asked again in hours, not every session.
const RETRY_S: u64 = 6 * 3600;
const INDEX_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// CI's release-signing key, public half. The private half exists only as a
/// GitHub Actions secret; blocks.py/blocks.rs HOOK_SIGNING_KEY is the same.
pub const SIGNING_KEY: &str = "17092e6d5632783f1d9b08fc134b0343c7e67b4a4b42feb52df6ca295f325ef8";

/// What CI signs for one build — artifacts.rs::signed_statement's twin.
pub fn statement(target: &str, version: u32, sha256: &str) -> String {
    statement_for("collide-hook", target, version, sha256)
}

/// The same for any of the programs a release carries (`collide-hook`, the
/// free version's `collide`).
pub fn statement_for(program: &str, target: &str, version: u32, sha256: &str) -> String {
    format!("{program} {target} v{version} sha256:{sha256}")
}

/// Whether `sig_b64` is CI's signature over this build's statement.
pub fn signed_by_ci(target: &str, version: u32, sha256: &str, sig_b64: &str) -> bool {
    signed_for("collide-hook", target, version, sha256, sig_b64)
}

/// Whether `sig_b64` is CI's signature over `program`'s statement.
pub fn signed_for(program: &str, target: &str, version: u32, sha256: &str, sig_b64: &str) -> bool {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let Ok(key_bytes) = (0..SIGNING_KEY.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&SIGNING_KEY[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>() else { return false };
    let Ok(key_array) = <[u8; 32]>::try_from(key_bytes.as_slice()) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&key_array) else { return false };
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()) else { return false };
    let Ok(sig_array) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else { return false };
    key.verify(statement_for(program, target, version, sha256).as_bytes(), &Signature::from_bytes(&sig_array)).is_ok()
}

/// This build's release name, as CI publishes it.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// The managed install is the one under `.collide/bin/`.
fn managed(exe: &Path) -> bool {
    let parent = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    parent.file_name().is_some_and(|name| name == "bin")
        && parent.parent().and_then(Path::file_name).is_some_and(|name| name == ".collide")
}

fn stamp_path(env: &Env) -> PathBuf {
    config::credentials_path(env).with_file_name("native").join("selfupdate.json")
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Whether an attempt may start now: the managed binary, updates not turned
/// off (`COLLIDE_NATIVE_INSTALL=0`, which the test suites set), a platform
/// with a build, and no attempt in the last few hours.
fn due(exe: &Path, env: &Env) -> bool {
    if config::get(env, "COLLIDE_NATIVE_INSTALL").trim() == "0" || target().is_none() || !managed(exe) {
        return false;
    }
    let last = config::load_json(&stamp_path(env)).get("ts").and_then(Value::as_u64).unwrap_or(0);
    now_s().saturating_sub(last) >= RETRY_S
}

/// Start an update in the background when one may be due; returns at once.
/// Called at session start (`check-credentials`) and whenever this binary
/// hands a hook to a newer committed script — the moment it knows it is
/// behind.
pub fn spawn_if_due(server: &str, env: &Env) {
    let Ok(exe) = std::env::current_exe() else { return };
    if server.is_empty() || !due(&exe, env) {
        return;
    }
    // stamped before spawning: two hooks firing together start one update
    crate::report::save_json(&stamp_path(env), &json!({"ts": now_s(), "from": HOOK_VERSION}));
    let mut command = std::process::Command::new(&exe);
    command
        .arg("self-update")
        .arg(server)
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

/// The version a binary reports (`collide-hook X (hook artifacts vN)`).
fn reported_version(exe: &Path) -> u32 {
    let Ok(output) = std::process::Command::new(exe).arg("--version").output() else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    text.split("hook artifacts v")
        .nth(1)
        .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// `self-update <server>`: fetch, verify, swap. Always exits 0 — this runs
/// detached and nothing is listening for its failure.
pub fn run(server: &str, _env: &Env) -> i32 {
    let _ = update(server.trim_end_matches('/'));
    0
}

fn update(server: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = target().ok_or("no build for this platform")?;
    // a previous Windows swap left the old binary aside; it is not running now
    let _ = std::fs::remove_file(exe.with_extension("old"));

    let agent = ureq::AgentBuilder::new().timeout(INDEX_TIMEOUT).build();
    let index: Value = serde_json::from_str(
        &agent
            .get(&format!("{server}/artifacts/hooks"))
            .set("User-Agent", &format!("collide-hook/{HOOK_VERSION}"))
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let offered = index.get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
    if offered <= HOOK_VERSION {
        return Ok(()); // current, or the server is behind this binary
    }
    // the free version's one program updates from its own builds
    let program = if exe.file_stem().and_then(|s| s.to_str()) == Some(crate::machine::PROGRAM) { "collide" } else { "collide-hook" };
    let list = if program == "collide" { "free_builds" } else { "builds" };
    let build = index
        .get(list)
        .and_then(Value::as_array)
        .and_then(|builds| builds.iter().find(|b| b.get("target").and_then(Value::as_str) == Some(target)))
        .ok_or("the server has no build for this platform")?;
    let want = build.get("sha256").and_then(Value::as_str).unwrap_or("").to_lowercase();
    if want.len() != 64 {
        return Err("the index advertised no digest".into());
    }
    // the server says what the digest is; only CI's key says it is ours
    let sig = build.get("sig").and_then(Value::as_str).unwrap_or("");
    if !signed_for(program, target, offered, &want, sig) {
        return Err("the build is not signed by Collide's release key".into());
    }

    let response = ureq::AgentBuilder::new()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .get(&build.get("url").and_then(Value::as_str).map(str::to_string)
            .unwrap_or_else(|| format!("{server}/artifacts/hooks/{target}")))
        .set("User-Agent", &format!("collide-hook/{HOOK_VERSION}"))
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_BYTES).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let got = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != want {
        return Err("digest mismatch".into());
    }

    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("collide-hook");
    let part = exe.with_file_name(format!("{name}.part"));
    std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755));
    }
    if reported_version(&part) != offered {
        let _ = std::fs::remove_file(&part);
        return Err("the download does not run as the advertised version".into());
    }
    swap(&part, &exe)?;
    Ok(())
}

/// Put `part` in place of `exe`. A running binary on Windows can only be
/// renamed aside, not replaced.
fn swap(part: &Path, exe: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(|e| e.to_string())?;
        if let Err(problem) = std::fs::rename(part, exe) {
            let _ = std::fs::rename(&old, exe); // put the working one back
            return Err(problem.to_string());
        }
    } else {
        std::fs::rename(part, exe).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_managed_install_updates_itself() {
        assert!(managed(Path::new("/work/repo/.collide/bin/collide-hook")));
        assert!(managed(Path::new("C:/work/repo/.collide/bin/collide-hook.exe")));
        assert!(!managed(Path::new("/usr/local/bin/collide-hook")));
        assert!(!managed(Path::new("/work/repo/bin/collide-hook")));
    }

    #[test]
    fn only_a_signature_from_the_release_key_is_accepted() {
        let sha = "ab".repeat(32);
        // no signature, garbage, and a well-formed signature by another key
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, ""));
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, "not base64 !!"));
        use base64::Engine;
        use ed25519_dalek::{Signer, SigningKey};
        let stranger = SigningKey::from_bytes(&[7u8; 32]);
        let forged = stranger.sign(statement("aarch64-apple-darwin", 34, &sha).as_bytes());
        let forged = base64::engine::general_purpose::STANDARD.encode(forged.to_bytes());
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &sha, &forged));
        // a signature the release key made (with openssl, exactly as CI
        // signs): accepted for its own statement, refused for any other
        let real = "gzGFIfJdBaI+qk09bctIgv8zlvicRnvtlzX0LoVkpmL8ft4ykBFcjXNk0sfVefuMokx9+mdtikPBfpasjQz+DQ==";
        assert!(signed_by_ci("aarch64-apple-darwin", 34, &sha, real));
        assert!(!signed_by_ci("x86_64-apple-darwin", 34, &sha, real));
        assert!(!signed_by_ci("aarch64-apple-darwin", 35, &sha, real));
        assert!(!signed_by_ci("aarch64-apple-darwin", 34, &"cd".repeat(32), real));
    }

    #[test]
    fn this_build_knows_its_release_name() {
        // every platform CI builds maps to its release; the test host is one
        assert!(target().is_some());
    }

    #[test]
    fn turned_off_or_unmanaged_never_runs() {
        let mut env = Env::new();
        env.insert("COLLIDE_NATIVE_INSTALL".into(), "0".into());
        assert!(!due(Path::new("/r/.collide/bin/collide-hook"), &env));
        let home = std::env::temp_dir().join("collide-selfupdate-test");
        let _ = std::fs::remove_dir_all(&home);
        let mut env = Env::new();
        env.insert("HOME".into(), home.to_string_lossy().into_owned());
        assert!(!due(Path::new("/usr/local/bin/collide-hook"), &env));
        assert!(due(Path::new("/r/.collide/bin/collide-hook"), &env));
        // a recent attempt holds the next one off
        crate::report::save_json(&stamp_path(&env), &json!({"ts": now_s()}));
        assert!(!due(Path::new("/r/.collide/bin/collide-hook"), &env));
    }
}
