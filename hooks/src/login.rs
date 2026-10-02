//! `collide-hook login`: connect this repo from the terminal, no agent in the
//! loop. The Claude Code plugin's front door.
//!
//! 1. the browser signs in (the same OAuth the MCP clients use: dynamic
//!    registration, PKCE, a loopback redirect to a port this process holds);
//! 2. `POST /plugin/connect` turns that sign-in into what `setup` would have
//!    left on disk for this repo: `.collide/config.json` and a hook
//!    credential;
//! 3. the credential is merged into `~/.collide/credentials.json` the way
//!    `add-credentials` merges one (other servers' and workspaces' keys stay).
//!
//! Nothing here is sent to a model, and the token never touches the repo.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{self, Env};

const DEFAULT_SERVER: &str = "https://mcp.collidemcp.com";
const WAIT: Duration = Duration::from_secs(300);

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok();
    if !filled {
        // no /dev/urandom (Windows): the clock, the process and a stack
        // address, hashed. The verifier only has to outlive this one sign-in.
        let seed = format!("{:?}{}{:p}", std::time::SystemTime::now(), std::process::id(), &bytes);
        bytes.copy_from_slice(&Sha256::digest(seed.as_bytes()));
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| decode(v))
        .unwrap_or_default()
}

pub(crate) fn open_browser(url: &str) {
    let tried = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    let _ = tried;
}

/// Wait for the browser's redirect; answer it with a page that says what
/// happened. Returns the query string of the callback.
fn wait_for_callback(listener: &TcpListener) -> Option<String> {
    let deadline = Instant::now() + WAIT;
    listener.set_nonblocking(true).ok()?;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buffer = [0u8; 8192];
                let n = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let Some((path, query)) = target.split_once('?') else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
                    continue;
                };
                if path != "/callback" {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
                    continue;
                }
                let ok = !query_param(query, "code").is_empty();
                let body = if ok {
                    "<!doctype html><meta charset=utf-8><title>Collide</title><body style=\"font:16px system-ui;margin:3em\">\
<h1 style=\"font-size:20px\">Collide is connected.</h1><p>You can close this tab and go back to your terminal.</p>"
                } else {
                    "<!doctype html><meta charset=utf-8><title>Collide</title><body style=\"font:16px system-ui;margin:3em\">\
<h1 style=\"font-size:20px\">Sign-in did not finish.</h1><p>Go back to your terminal and run the login again.</p>"
                };
                let _ = stream.write_all(
                    format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                );
                return Some(query.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return None,
        }
    }
    None
}

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).current_dir(cwd).output().ok()?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !top.is_empty()).then(|| PathBuf::from(top))
}

/// The browser sign-in, the same OAuth the MCP clients use (dynamic
/// registration, PKCE, a loopback redirect to a port this process holds).
/// Returns the access token.
pub fn sign_in(server: &str, env: &Env) -> Result<String, String> {
    sign_in_as(server, env, "Collide for Claude Code")
}

/// The same, registering as `client_name`: Collide's server tells a
/// `collide upgrade` from a `collide login` from the plugin by it.
pub fn sign_in_as(server: &str, env: &Env, client_name: &str) -> Result<String, String> {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return Err("could not open a local port for the browser to return to".into());
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let redirect = format!("http://127.0.0.1:{port}/callback");
    // registered portless: a native client arrives on whatever port it got (RFC 8252 s7.3)
    let registered = http()
        .post(&format!("{server}/register"))
        .send_json(json!({"client_name": client_name, "redirect_uris": ["http://127.0.0.1/callback"],
                          "grant_types": ["authorization_code"], "token_endpoint_auth_method": "none"}));
    let Some(client_id) = registered.ok().and_then(|r| r.into_json::<Value>().ok())
        .and_then(|v| v.get("client_id").and_then(Value::as_str).map(str::to_string))
    else {
        return Err(format!("could not reach {server}. Check the address, or your connection, and try again"));
    };
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token();
    let url = format!(
        "{server}/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        encode(&client_id), encode(&redirect)
    );
    println!("Opening your browser to sign in to Collide. If it does not open, visit:\n  {url}");
    if config::get(env, "COLLIDE_NO_BROWSER") != "1" {
        open_browser(&url);
    }
    let Some(query) = wait_for_callback(&listener) else {
        return Err("no answer from the browser within five minutes. Run it again when you are ready".into());
    };
    if query_param(&query, "state") != state {
        return Err("the browser's answer did not match this sign-in, so it was ignored. Run it again".into());
    }
    let code = query_param(&query, "code");
    if code.is_empty() {
        return Err(format!("sign-in was cancelled ({})", query_param(&query, "error")));
    }
    let exchanged = http().post(&format!("{server}/token")).send_form(&[
        ("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect),
        ("client_id", &client_id), ("code_verifier", &verifier),
    ]);
    let Some(access) = exchanged.ok().and_then(|r| r.into_json::<Value>().ok())
        .and_then(|v| v.get("access_token").and_then(Value::as_str).map(str::to_string))
    else {
        return Err("the sign-in could not be completed. Run it again".into());
    };
    Ok(access)
}

pub fn run(args: &[String], env: &Env) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    // Collide installed for the machine: the login is the machine's, from
    // anywhere, unless asked for one repo (--repo) inside a repo set up alone
    if crate::machine::installed(env) && arg("--repo").is_empty() && !args.iter().any(|a| a == "--this-repo") {
        return crate::machine::login(args, env);
    }
    let Some(root) = repo_root(&cwd) else {
        println!("Collide login: run this inside the git repository you want to connect.");
        return 1;
    };
    let existing = config::load_json(&root.join(".collide").join("config.json"));
    let server = {
        let given = arg("--server");
        let from_repo = existing.get("server_url").and_then(Value::as_str).unwrap_or("").to_string();
        let chosen = if !given.is_empty() { given } else if !from_repo.is_empty() { from_repo } else { DEFAULT_SERVER.to_string() };
        chosen.trim_end_matches('/').to_string()
    };
    let repo_id = {
        let given = arg("--repo");
        let from_repo = existing.get("repo_id").and_then(Value::as_str).unwrap_or("").to_string();
        if !given.is_empty() { given } else if !from_repo.is_empty() { from_repo } else { crate::git::origin_repo_id(&root) }
    };
    if repo_id.is_empty() {
        println!("Collide login: this repository has no origin remote to name it by. Run again with --repo github.com/you/project.");
        return 1;
    }

    let access = match sign_in(&server, env) {
        Ok(access) => access,
        Err(problem) => {
            println!("Collide login: {problem}.");
            return 1;
        }
    };
    let connected = http()
        .post(&format!("{server}/plugin/connect"))
        .set("Authorization", &format!("Bearer {access}"))
        .send_json(json!({"repo_id": repo_id}))
        .ok()
        .and_then(|r| r.into_json::<Value>().ok())
        .unwrap_or(Value::Null);
    if connected.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = connected.get("reason").or_else(|| connected.get("access_denied")).and_then(Value::as_str).unwrap_or("");
        println!("Collide login: this account cannot connect {repo_id}{}.", if why.is_empty() { String::new() } else { format!(" ({why})") });
        return 1;
    }

    // the credential: merged, never replacing another server's or workspace's keys
    // only this server's keys: a bare "token" would become every other
    // repo's fallback and mix this workspace's credential into theirs
    let fragment: serde_json::Map<String, Value> = connected
        .get("credentials")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter(|(k, _)| k.starts_with(&server)).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let fragment = Value::Object(fragment);
    let scratch = std::env::temp_dir().join(format!("collide-login-{}.json", std::process::id()));
    if std::fs::write(&scratch, fragment.to_string()).is_err() || crate::report::add_credentials(&scratch.to_string_lossy(), env) != 0 {
        let _ = std::fs::remove_file(&scratch);
        println!("Collide login: signed in, but the credential could not be saved to ~/.collide/credentials.json.");
        return 1;
    }
    let _ = std::fs::remove_file(&scratch);

    // the repo config: written when there is none; an existing one is the team's and stays
    let config_path = root.join(".collide").join("config.json");
    let wrote_config = if existing.as_object().map_or(true, |m| m.is_empty()) {
        let _ = std::fs::create_dir_all(root.join(".collide"));
        let body = connected.get("config").cloned().unwrap_or(Value::Null);
        std::fs::write(&config_path, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default())).is_ok()
    } else {
        false
    };
    let who = connected.get("user").and_then(Value::as_str).unwrap_or("");
    let workspace = connected.get("workspace_name").and_then(Value::as_str).filter(|w| !w.is_empty())
        .or_else(|| connected.get("workspace").and_then(Value::as_str)).unwrap_or("");
    println!(
        "Collide is on for {repo_id}{}, workspace {workspace}.{}",
        if who.is_empty() { String::new() } else { format!(": signed in as {who}") },
        if wrote_config {
            " Wrote .collide/config.json; commit it so teammates' agents join the same repo."
        } else {
            ""
        }
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_round_trip_through_the_encoder() {
        let value = "http://127.0.0.1:5555/callback?x=a b&y=é";
        assert_eq!(decode(&encode(value)), value);
        assert_eq!(query_param("code=abc%2Fd&state=s%20t", "code"), "abc/d");
        assert_eq!(query_param("code=abc&state=s+t", "state"), "s t");
        assert_eq!(query_param("code=abc", "state"), "");
    }

    #[test]
    fn verifiers_are_long_and_different() {
        let (a, b) = (random_token(), random_token());
        assert!(a.len() >= 43 && a != b);
    }
}
