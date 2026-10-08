//! Repo-workspace routing: which workspace a repo belongs to, and the tools
//! that place, move, remove, and watch a repo there.
//!
//! One credential is bound to one workspace, but its holder can have several
//! editor windows open on several repos in several DIFFERENT workspaces at
//! once. [`workspace_for`] is what makes that work: every call gets routed to
//! the workspace that actually knows the repo, not blindly to the
//! credential's own default. An explicit dashboard watch (this module's
//! [`register_watch`]) is the human's binding and always outranks a stray
//! scope an agent happened to report into; everything else falls back to
//! sensible defaults (the token's workspace, then the one other workspace
//! that knows the repo, then the GitHub-org workspace for a brand new repo).
//!
//! [`register_watch`] is also the auto-relink point for a GitHub rename:
//! watches are keyed by the repo's numeric GitHub id, which survives a
//! rename even though the slug doesn't, so seeing a new slug under a
//! familiar id is unambiguous evidence of a rename rather than a new repo —
//! [`crate::rename::rename_repo`] does the actual relinking.
//!
//! [`move_repo`] and [`remove_repo`] are the two ways a repo's place in the
//! workspace list changes after the fact; both carry the repo's binding,
//! live trees and setup state with them (never the ledger itself — that
//! chain is global and append-only, so history is re-appended under the new
//! scope rather than moved).
//!
//! [`ensure_watched`] is the third way a repo gets bound, and the one that
//! needs no human: an agent's FIRST call naming a repo the workspace has
//! never bound writes the binding itself (marked `auto`, so an explicit
//! dashboard watch always outranks it), and holds the plan's repo cap at
//! that door. `main::authorize_bound` runs it on every hot route and the
//! MCP dispatch runs it per tool — Python's `scope_of` -> `_ensure_watched`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::access::has_repo_access;
use crate::auth::{member, AuthUser};
use crate::compat::{python_repr, truthy};
use crate::repo::{path_key, repo_key, repo_org, Aliases};
use crate::store::{now, Store};
use crate::workspaces::{self, RepoScope};

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A truthy `setup_state` record — Python's `if await self.setup_state(scope):`.
fn has_setup_state(store: &Store, scope: &str) -> bool {
    truthy(store.kv_get("setup_state", scope).as_ref())
}

// ---------------------------------------------------------------- routing

/// Two editor windows, two repos, two workspaces, ONE credential — the
/// credential's workspace can't be right for both. Route each call to the
/// workspace that actually knows the repo. "Knows" includes an explicit
/// dashboard WATCH (the human's binding, valid before any agent reports) —
/// and an unambiguous watch outranks a stray scope in the token's
/// workspace, so setup in a fresh checkout lands where the human connected
/// the repo instead of minting a parallel scope. Otherwise: token's
/// workspace first, else the ONE other workspace of the caller that knows
/// it (membership still gates everything). Fresh repos stay with the
/// token's workspace — that choice belongs to the human at OAuth time.
///
/// Fail-open: `auth.uid` empty (no real identity to route by) returns the
/// credential's own workspace, same as any lookup trouble would.
///
/// Python caches this (300s, bumped by a route epoch on every new watch);
/// that cache is pure performance — nothing else reads its ephemeral key —
/// so it is not reproduced here. [`register_watch`], [`move_repo`] and
/// [`remove_repo`] still write the route-epoch stamp the Python instance's
/// own cache depends on, since both servers share one database.
pub fn workspace_for(store: &Store, aliases: &Aliases, auth: &AuthUser, repo_id: &str) -> String {
    if auth.uid.is_empty() {
        return auth.workspace.clone();
    }
    let allowed: BTreeSet<String> = auth.workspaces.iter().cloned().collect();

    // An EXPLICIT dashboard watch — the human's binding of this repo to a
    // workspace, valid BEFORE any agent ever reports. Auto bindings
    // (first-call registration) deliberately don't count here: they make
    // the repo known, but must never pin routing against a human's
    // explicit choice elsewhere.
    let watched_in = |workspace_id: &str| -> bool {
        let scope = aliases.scope_for(workspace_id, repo_id);
        store.kv_list("ghrepo", &format!("{workspace_id}:")).into_iter().any(|(_, rec)| {
            let watched = text(&rec, "repo_id");
            !watched.is_empty()
                && !truthy(rec.get("auto"))
                && aliases.scope_for(workspace_id, &watched) == scope
        })
    };
    // A repo moved to another workspace leaves its history behind; that
    // history must not pull routing back here.
    let moved_away = |workspace_id: &str| -> bool {
        truthy(store.kv_get("repo_moved", &format!("{workspace_id}:{}", repo_key(repo_id))).as_ref())
    };
    let active_in = |workspace_id: &str| -> bool {
        if moved_away(workspace_id) {
            return false;
        }
        let scope = aliases.scope_for(workspace_id, repo_id);
        if has_setup_state(store, &scope) {
            return true;
        }
        if store.list_scopes(&format!("{workspace_id}:")).contains(&scope) {
            return true;
        }
        // any binding — explicit or auto — means the workspace knows it
        store.kv_list("ghrepo", &format!("{workspace_id}:")).into_iter().any(|(_, rec)| {
            let watched = text(&rec, "repo_id");
            !watched.is_empty() && aliases.scope_for(workspace_id, &watched) == scope
        })
    };

    let mut chosen = auth.workspace.clone();
    if !watched_in(&auth.workspace) {
        let candidates: Vec<String> = workspaces::list_for(store, &auth.uid)
            .into_iter()
            .map(|w| text(&w, "id"))
            .filter(|id| *id != auth.workspace && (allowed.is_empty() || allowed.contains(id)))
            .collect();
        if active_in(&auth.workspace) {
            // the token's workspace knows the repo only incidentally (a
            // stray scope from a misrouted report) — an unambiguous
            // explicit watch elsewhere is the human's decision; follow it
            let watched_hits: Vec<&String> = candidates.iter().filter(|id| watched_in(id)).collect();
            if watched_hits.len() == 1 {
                chosen = watched_hits[0].clone();
            }
        } else {
            let mut hits: Vec<String> = candidates.iter().filter(|id| active_in(id)).cloned().collect();
            if hits.len() > 1 {
                // several know it: an explicit watch disambiguates
                let watched_hits: Vec<String> = hits.iter().filter(|id| watched_in(id)).cloned().collect();
                if watched_hits.len() == 1 {
                    hits = watched_hits;
                }
            }
            if hits.len() == 1 {
                chosen = hits[0].clone();
            } else if hits.is_empty() && !active_in(&auth.workspace) {
                // a brand-new repo nobody knows: the workspace named after
                // its GitHub org is the accurate home; the token's
                // workspace only when no org workspace exists
                let org = repo_org(repo_id);
                let allowed_ref = if allowed.is_empty() { None } else { Some(&allowed) };
                if let Some(org_ws) = workspaces::org_workspace_for(store, &auth.uid, &org, allowed_ref) {
                    chosen = text(&org_ws, "id");
                }
            }
        }
    }
    chosen
}

/// The ghrepo record binding this repo to the workspace, if any — matched
/// through the alias map, so every spelling of one repo finds its binding
/// (Python's `_binding_of`).
pub(crate) fn binding_of(store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str) -> Option<(String, Value)> {
    let scope = aliases.scope_for(workspace_id, repo_id);
    store.kv_list("ghrepo", &format!("{workspace_id}:")).into_iter().find(|(_, rec)| {
        let watched = text(rec, "repo_id");
        !watched.is_empty() && aliases.scope_for(workspace_id, &watched) == scope
    })
}

/// The person who placed a repo can obviously work in it.
fn grant_repo(store: &Store, workspace_id: &str, uid: &str, repo_id: &str) {
    let Some(member) = member(store, workspace_id, uid) else { return };
    if has_repo_access(Some(&member), repo_id) {
        return;
    }
    let mut repos: Vec<String> = member
        .get("repos")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    repos.push(repo_id.to_string());
    let _ = workspaces::set_member_repos(store, workspace_id, uid, RepoScope::Some(repos));
}

// -------------------------------------------------- the first call IS the watch

/// How long a scope's "already bound" answer stands in ephemeral — the
/// steady-state cost of the auto-bind is one cache hit.
const AUTOWATCH_TTL_S: f64 = 3600.0;

/// First call IS the watch: a repo an agent works in gets a binding
/// automatically, so it shows on the dashboard, survives reloads, and keeps
/// routing stable — no human ceremony. Auto bindings are marked `auto: true`
/// so an explicit dashboard watch always outranks them (in routing and
/// `primary_repo`). Memoized in ephemeral under `autowatch:{scope}` so the
/// steady-state cost is one cache hit.
///
/// The door: this first call would BIND the (N+1)th repo, so the plan's
/// repo cap holds HERE and never on a call about a repo already inside —
/// `Err` is `billing::repo_cap_error`'s refusal payload, for the caller to
/// return as-is (/report, /presence, MCP) or carry (/gate allows). Python's
/// `CollideService._ensure_watched`, record for record: the `ghrepo` key is
/// `{workspace}:name:{repo_id}` (the request's own spelling, as a
/// GitHub-id-less dashboard watch writes it) and the record is
/// `{repo_id, github_id: "", auto: true, updated, by}`.
///
/// Fail open, like every store write on the hot path: a store that cannot
/// be written binds nothing and refuses nothing; the next call tries again.
pub fn ensure_watched(
    store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str, scope: &str, user: &str, explicit: bool,
) -> Result<(), Value> {
    if repo_id.is_empty() {
        return Ok(());
    }
    let memo = format!("autowatch:{scope}");
    if store.eph_get(&memo).is_some() {
        return Ok(());
    }
    let already_bound = store.kv_list("ghrepo", &format!("{workspace_id}:")).into_iter().any(|(_, rec)| {
        let watched = text(&rec, "repo_id").trim().to_string();
        !watched.is_empty() && aliases.scope_for(workspace_id, &watched) == scope
    });
    if already_bound {
        let _ = store.eph_set(&memo, &json!({"t": now()}), Some(AUTOWATCH_TTL_S));
        return Ok(());
    }
    // a folder on one machine never joins: no one else could be working in
    // it (the free version's local server keeps every git repo it is given)
    if !crate::local::active() && crate::repo::is_machine_folder(repo_id) {
        return Err(not_shared(repo_id));
    }
    if !explicit {
        if let Some(question) = new_repo_question(store, workspace_id, repo_id, scope) {
            return Err(question);
        }
    }
    if let Some(refused) = crate::billing::repo_cap_error(store, workspace_id, repo_id) {
        return Err(refused);
    }
    let stamp = now();
    let _ = store.kv_put(
        "ghrepo", &format!("{workspace_id}:name:{repo_id}"),
        &json!({"repo_id": repo_id, "github_id": "", "auto": true, "updated": stamp, "by": user}),
        stamp,
    );
    let _ = store.eph_set(&memo, &json!({"t": stamp}), Some(AUTOWATCH_TTL_S));
    Ok(())
}

pub const NEW_REPOS_BUCKET: &str = "new_repos";

/// The answer for a repo that is not on GitHub (or another git host): it is
/// not added, and its agents keep working with Collide on their machine.
pub fn not_shared(repo_id: &str) -> Value {
    json!({
        "ok": false,
        "not_shared": true,
        "repo_id": repo_id,
        "error": format!(
            "{} is not a repository on GitHub (or another git host), so it is not shared with the team: \
             a teammate could not be working in it. Collide keeps working for it on this machine. \
             To share it, push it to GitHub first.",
            if repo_id.trim().is_empty() { "this folder" } else { repo_id.trim() }
        ),
    })
}

/// The owner's setting: "ask" (the default) or "add" (an agent's first call
/// adds a repo, as every workspace did before the question existed).
pub fn new_repos_mode(store: &Store, workspace_id: &str) -> String {
    store
        .kv_get(NEW_REPOS_BUCKET, workspace_id)
        .map(|v| text(&v, "mode"))
        .filter(|m| m == "add")
        .unwrap_or_else(|| "ask".to_string())
}

/// Under "ask", a repo the workspace does not have yet is the user's to add, never the
/// agent's: a call from it is answered with the question to ask and binds
/// nothing, and `setup` (the explicit way in) adds it once they say yes.
/// Binding on first sight filled workspaces with every spelling an agent
/// used for one repo (a Windows working directory, `owner/name` beside
/// `github.com/owner/name`) and with repos nobody meant to share. The one
/// exception is a workspace with no repo at all: its first is what the
/// person set it up for.
pub fn new_repo_question(store: &Store, workspace_id: &str, repo_id: &str, scope: &str) -> Option<Value> {
    if new_repos_mode(store, workspace_id) == "add" {
        return None;
    }
    let repo_id = repo_id.trim();
    let bound = crate::billing::bound_repos(store, workspace_id);
    // a repo the workspace already has in any form (its data, a watch, a
    // #branch of it) is known: it binds as before
    let known = bound.contains(&repo_key(repo_id))
        || store.list_scopes(&format!("{workspace_id}:")).iter().any(|s| s == scope);
    if repo_id.is_empty() || bound.is_empty() || known {
        return None;
    }
    let name = workspaces::get(store, workspace_id)
        .map(|ws| text(&ws, "name"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| workspace_id.to_string());
    Some(json!({
        "ok": false,
        "new_repo": true,
        "repo_id": repo_id,
        "workspace": workspace_id,
        "error": format!(
            "{repo_id} is not in the Collide workspace {name}, so nothing from it is shared yet. Ask the user \
             whether to add it to {name}. If they say yes, run ~/.collide/bin/collide add in this repo (or, with \
             Collide's MCP tools, call setup with repo_id \"{repo_id}\"). It is also waiting on their Collide \
             dashboard, where they can add it to any of their workspaces or to a new one. If they say no, carry \
             on: Collide stays out of this repo."
        ),
    }))
}

// --------------------------------------------------------- waiting repos

const PENDING_BUCKET: &str = "pending_repo";

/// An agent worked in a repo with a git remote that none of this person's
/// workspaces has: it waits on their dashboard, where they pick the
/// workspace to watch it from (or make a new one), instead of adding each
/// repo again from GitHub. A folder with no remote is never shared, so it
/// never waits.
pub fn note_pending(store: &Store, uid: &str, repo_id: &str, workspace_id: &str) {
    let repo_id = repo_id.trim();
    let host = repo_id.split('/').next().unwrap_or("");
    if uid.is_empty() || repo_id.matches('/').count() < 2 || !host.contains('.') {
        return;
    }
    // a repo they said never to ask about does not come back when an agent
    // works there again, and a tool's own checkout is never offered at all
    if is_tool_checkout(repo_id)
        || store.kv_get(PENDING_IGNORED_BUCKET, &format!("{uid}:{}", repo_key(repo_id))).is_some()
    {
        return;
    }
    let key = format!("{uid}:{}", repo_key(repo_id));
    let now = crate::store::now();
    let first = store.kv_get(PENDING_BUCKET, &key).and_then(|v| v.get("first_seen").and_then(Value::as_f64)).unwrap_or(now);
    let _ = store.kv_put(PENDING_BUCKET, &key, &json!({
        "repo_id": repo_id, "workspace": workspace_id, "first_seen": first, "last_seen": now,
    }), now);
}

/// This person's waiting repos, newest first (a tool's checkout noted
/// before the tool list existed is left out).
pub fn pending_for(store: &Store, uid: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = store.kv_list(PENDING_BUCKET, &format!("{uid}:")).into_iter().map(|(_, v)| v)
        .filter(|v| !is_tool_checkout(v.get("repo_id").and_then(Value::as_str).unwrap_or("")))
        .collect();
    rows.sort_by(|a, b| {
        let at = |v: &Value| v.get("last_seen").and_then(Value::as_f64).unwrap_or(0.0);
        at(b).partial_cmp(&at(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

const PENDING_IGNORED_BUCKET: &str = "pending_repo_ignored";

/// Tools that install themselves as a git checkout of their own repo
/// (Homebrew in /opt/homebrew, nvm in ~/.nvm, oh-my-zsh, pyenv, ...): an
/// agent that runs one of them is "working" in that checkout, which is
/// nobody's to add to a workspace.
const TOOL_CHECKOUTS: &[&str] = &[
    "homebrew/brew", "homebrew/homebrew-core", "homebrew/homebrew-cask", "homebrew/install",
    "nvm-sh/nvm", "ohmyzsh/ohmyzsh", "robbyrussell/oh-my-zsh", "pyenv/pyenv", "pyenv/pyenv-virtualenv",
    "rbenv/rbenv", "rbenv/ruby-build", "asdf-vm/asdf", "tmux-plugins/tpm", "junegunn/fzf",
    "zsh-users/zsh-autosuggestions", "zsh-users/zsh-syntax-highlighting", "romkatv/powerlevel10k",
    "rust-lang/rustup", "flutter/flutter", "volta-cli/volta", "jdx/mise", "spaceship-prompt/spaceship-prompt",
];

pub fn is_tool_checkout(repo_id: &str) -> bool {
    let repo = repo_id.trim().to_lowercase();
    let path = repo.split_once('/').map(|(_, p)| p).unwrap_or("");
    let path = path.trim_end_matches(".git").trim_end_matches('/');
    TOOL_CHECKOUTS.contains(&path) || path.starts_with("homebrew/")
}

/// "Never": the repo stops waiting and is never offered again.
pub fn ignore_pending(store: &Store, uid: &str, repo_id: &str) {
    let key = format!("{uid}:{}", repo_key(repo_id.trim()));
    let _ = store.kv_put(PENDING_IGNORED_BUCKET, &key, &json!({"repo_id": repo_id.trim(), "ts": crate::store::now()}), crate::store::now());
    clear_pending(store, uid, repo_id);
}

/// The repo was added (or dismissed): it stops waiting.
pub fn clear_pending(store: &Store, uid: &str, repo_id: &str) {
    let _ = store.kv_delete(PENDING_BUCKET, &format!("{uid}:{}", repo_key(repo_id.trim())));
}

// --------------------------------------------------------------- watching

/// Record that this workspace watches `repo_id`, keyed by the GitHub
/// NUMERIC id (stable across renames) — the durable link between a
/// workspace and a GitHub repo. If the same numeric id was last watched
/// under a DIFFERENT repo_id, that is a GitHub rename: leverage the last id
/// (`prior_id`) to auto-relink it with a FULL rename — history carries
/// over, `repo_id` becomes the display id everyone adopts, and the bare
/// basename is aligned — so nothing fragments and the two stay linked.
pub fn register_watch(
    store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str, github_id: &str, by: &str,
) -> Value {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() {
        return json!({"ok": false, "error": "missing repo_id"});
    }
    // the door: an explicit watch binds the repo; the (N+1)th is refused in
    // the seat cap's shape, a repo already here never is
    if let Some(refused) = crate::billing::repo_cap_error(store, workspace_id, repo_id) {
        return refused;
    }
    let mut result = json!({"ok": true, "repo_id": repo_id});
    let gid = github_id.trim();
    if !gid.is_empty() {
        let key = format!("{workspace_id}:{gid}");
        let prior = store.kv_get("ghrepo", &key).unwrap_or(json!({}));
        let prior_id = text(&prior, "repo_id");
        if !prior_id.is_empty() && prior_id != repo_id {
            // full auto-relink (alias + preferred + basename), not just a
            // resolution alias, so the new slug propagates to every client
            let renamed = crate::rename::rename_repo(store, aliases, workspace_id, &prior_id, repo_id, by, false);
            if renamed.get("ok").and_then(Value::as_bool) == Some(true) {
                if let Some(map) = result.as_object_mut() {
                    map.insert("renamed_from".into(), json!(prior_id));
                    map.insert("canonical".into(), renamed.get("canonical").cloned().unwrap_or(Value::Null));
                    map.insert("preferred".into(), renamed.get("preferred").cloned().unwrap_or(Value::Null));
                }
            } else if let Some(error) = renamed.get("error") {
                // both names carry data (or similar) — don't guess; surface it
                if let Some(map) = result.as_object_mut() {
                    map.insert("conflict".into(), error.clone());
                }
            }
        }
        let stamp = now();
        let _ = store.kv_put(
            "ghrepo", &key,
            &json!({"repo_id": repo_id, "github_id": gid, "updated": stamp, "by": by}),
            stamp,
        );
    } else {
        // no GitHub id (non-GitHub repo, or the user skipped connecting):
        // persist the watch keyed by name so it survives reloads and shows
        // in the workspace's repo list — it just can't auto-relink renames
        let stamp = now();
        let _ = store.kv_put(
            "ghrepo", &format!("{workspace_id}:name:{repo_id}"),
            &json!({"repo_id": repo_id, "github_id": "", "updated": stamp, "by": by}),
            stamp,
        );
    }
    // an explicit watch resurrects a repo removed/moved away from here
    let _ = store.kv_delete("repo_moved", &format!("{workspace_id}:{}", repo_key(repo_id)));
    // a new binding must re-route agents NOW, not when caches expire
    let _ = store.eph_set(&format!("wsrouteepoch:{}", path_key(repo_id)), &json!({"t": now()}), Some(3600.0));
    result
}

// -------------------------------------------------------- moving a repo

/// Re-home a repo: its binding, its live trees, its ledger history
/// (re-appended under the new scope — the chain is global and keyed by
/// scope, so rows are copied, never re-keyed) and its setup state move to
/// `to_workspace`; the old scope is marked moved so nothing routes back to
/// it. Calling with the repo's current workspace pins an automatic
/// placement as the human's explicit choice. Caller must be a member of
/// both workspaces (checked here for the destination; the source repo's
/// access is the calling layer's job — MCP's `enforce_repo_access` or the
/// dashboard route's own check).
pub fn move_repo(
    store: &Store, aliases: &Aliases, auth: &AuthUser, repo_id: &str, to_workspace: &str, by: &str, via: &str,
) -> Value {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() {
        return json!({"ok": false, "error": "missing repo_id"});
    }
    let effective_by = if by.is_empty() { auth.uid.as_str() } else { by };
    let target = match workspaces::get(store, to_workspace) {
        Some(target) => Some(target),
        None => {
            let wanted = to_workspace.trim().to_lowercase();
            workspaces::list_for(store, &auth.uid)
                .into_iter()
                .find(|w| text(w, "name").trim().to_lowercase() == wanted)
        }
    };
    let Some(target) = target else {
        return json!({"ok": false, "error": format!("no workspace {} on this account", python_repr(to_workspace))});
    };
    let to_ws = text(&target, "id");
    let Some(to_member) = member(store, &to_ws, &auth.uid) else {
        return json!({"ok": false, "error": format!("you are not a member of '{}'", text(&target, "name"))});
    };
    if crate::access::access_of(store, &to_ws, Some(&to_member)) == "read" {
        return json!({"ok": false,
            "error": format!("read-only in '{}': cannot add repos there", text(&target, "name"))});
    }
    let from_ws = workspace_for(store, aliases, auth, repo_id);
    let physical = aliases.resolve(&from_ws, repo_id);
    let old_scope = format!("{from_ws}:{physical}");
    // the door: a move BINDS the repo to `to_ws`, so the destination's repo
    // cap holds here (a repo already bound there is never refused)
    if let Some(refused) = crate::billing::repo_cap_error(store, &to_ws, &physical) {
        return refused;
    }
    let stamp = now();

    if from_ws == to_ws {
        // pin: the automatic binding becomes an explicit one
        match binding_of(store, aliases, &from_ws, repo_id) {
            Some((key, mut rec)) if truthy(rec.get("auto")) => {
                rec["auto"] = json!(false);
                rec["updated"] = json!(stamp);
                rec["by"] = json!(effective_by);
                let _ = store.kv_put("ghrepo", &key, &rec, stamp);
            }
            None => {
                let _ = register_watch(store, aliases, &from_ws, &physical, "", effective_by);
            }
            _ => {}
        }
        grant_repo(store, &to_ws, &auth.uid, &physical);
        return json!({
            "ok": true, "workspace": to_ws, "name": text(&target, "name"), "pinned": true,
            "note": "placement confirmed; agents keep routing here",
        });
    }

    let new_scope = format!("{to_ws}:{physical}");
    aliases.force_alias(&to_ws, &physical, &physical);
    let (mut copied_files, mut copied_trees, mut copied_rows) = (0i64, 0i64, 0i64);
    for entry in store.list_workspaces(&old_scope) {
        let user = text(&entry, "user");
        if user.is_empty() {
            continue;
        }
        for (path, record) in store.list_files(&old_scope, &user) {
            let _ = store.put_file(&new_scope, &user, &path, &record, stamp);
            copied_files += 1;
        }
        if let Some(tree) = store.get_tree(&old_scope, &user) {
            let _ = store.put_tree(&new_scope, &user, &tree, stamp);
            copied_trees += 1;
        }
    }
    let rows = store.ledger_since(&old_scope, 0.0);
    let start = rows.len().saturating_sub(5000);
    for row in &rows[start..] {
        // copied verbatim — the mover's address is not stamped onto history
        let _ = store.ledger_append_raw(&new_scope, &row.kind, &row.payload, row.ts);
        copied_rows += 1;
    }
    for bucket in ["setup_state", "semlint"] {
        if let Some(rec) = store.kv_get(bucket, &old_scope) {
            let _ = store.kv_put(bucket, &new_scope, &rec, stamp);
        }
    }
    for (key, rec) in store.kv_list("hookseen", &format!("{old_scope}:")) {
        let suffix = &key[old_scope.len() + 1..];
        let _ = store.kv_put("hookseen", &format!("{new_scope}:{suffix}"), &rec, stamp);
    }

    // bindings: out of the old workspace, explicit in the new one
    let mut github_id = String::new();
    for (key, rec) in store.kv_list("ghrepo", &format!("{from_ws}:")) {
        let watched = text(&rec, "repo_id");
        if !watched.is_empty() && aliases.scope_for(&from_ws, &watched) == old_scope {
            if github_id.is_empty() {
                github_id = text(&rec, "github_id");
            }
            let _ = store.kv_delete("ghrepo", &key);
        }
    }
    let _ = register_watch(store, aliases, &to_ws, &physical, &github_id, effective_by);
    let _ = store.kv_put(
        "repo_moved", &format!("{from_ws}:{}", repo_key(&physical)),
        &json!({"repo_id": physical, "to": to_ws, "ts": stamp, "by": effective_by}), stamp,
    );
    grant_repo(store, &to_ws, &auth.uid, &physical);
    let _ = store.eph_set(&format!("wsrouteepoch:{}", path_key(repo_id)), &json!({"t": stamp}), Some(3600.0));
    store.eph_delete(&format!("autowatch:{old_scope}"));

    let payload = json!({
        "user": auth.user_id, "repo_id": physical, "from": from_ws, "to": to_ws,
        "files": copied_files, "trees": copied_trees, "rows": copied_rows,
    });
    let _ = store.ledger_append(&old_scope, "repo_moved", &payload, stamp);
    let _ = store.ledger_append(&new_scope, "repo_moved", &payload, stamp);
    let mut event = payload.clone();
    if let Some(map) = event.as_object_mut() {
        map.insert("kind".into(), json!("repo_moved"));
    }
    crate::events::publish(store, &old_scope, event.clone(), via);
    crate::events::publish(store, &new_scope, event, via);

    json!({
        "ok": true, "workspace": to_ws, "name": text(&target, "name"), "from": from_ws,
        "copied": {"files": copied_files, "trees": copied_trees, "rows": copied_rows},
        "note": "moved; agents route here from their next call. Hook configs (.collide/config.json) \
carrying the old workspace id keep working — routing follows the repo, not the id in the file.",
    })
}

/// Take a repo OUT of a workspace: its bindings, live trees and file
/// records, live intents and hot markers go; the ledger keeps the history
/// (append-only) but the scope is marked so it is hidden from the repo list
/// and never routes here again. A later explicit watch (dashboard "Add
/// repo") brings it back. `workspace_id` (id or name) targets a specific
/// workspace's copy — the stray one — instead of the repo's routed home.
/// Caller must have write access to the repo in the target workspace.
/// `move_repo(repo_id, new_workspace=<name>)`: the placement answer "put it
/// in a NEW workspace" in one call — create the workspace (the caller owns
/// it), then move the repo there with its history. Held to the same
/// workspace cap as the dashboard's create, with the dashboard's words; a
/// refusal changes nothing. Python's `create_and_move_repo`.
pub fn create_and_move(
    store: &Store, aliases: &Aliases, auth: &AuthUser, email: &str, name: &str,
    repo_id: &str, new_name: &str, by: &str, via: &str,
) -> Value {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return json!({"ok": false, "error": "new_workspace needs a name"});
    }
    let owned = workspaces::owned_by(store, &auth.uid);
    if let Some(cap) = crate::billing::workspace_limit_for(store, &owned) {
        if owned.len() as i64 >= cap {
            let first = owned.first().map(|w| text(w, "name")).filter(|n| !n.is_empty())
                .unwrap_or_else(|| "your workspace".to_string());
            return json!({
                "ok": false, "workspace_limit": true,
                "error": format!(
                    "the free plan includes one workspace ('{first}') with unlimited repos — \
add repos to it, or upgrade to create more workspaces"
                ),
            });
        }
    }
    let principal = workspaces::Principal { uid: auth.uid.clone(), email: email.to_string(), name: name.to_string() };
    match workspaces::create(store, &principal, new_name, "", "") {
        Ok(created) => {
            let id = text(&created, "id");
            let mut moved = move_repo(store, aliases, auth, repo_id, &id, by, via);
            if let Some(map) = moved.as_object_mut() {
                map.insert("created".into(), created);
            }
            moved
        }
        Err(problem) => json!({"ok": false, "error": problem}),
    }
}

pub fn remove_repo(store: &Store, aliases: &Aliases, auth: &AuthUser, repo_id: &str, workspace_id: &str, by: &str) -> Value {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() {
        return json!({"ok": false, "error": "missing repo_id"});
    }
    let effective_by = if by.is_empty() { auth.uid.as_str() } else { by };
    let (ws, target) = if !workspace_id.is_empty() {
        let target = match workspaces::get(store, workspace_id) {
            Some(target) => Some(target),
            None => {
                let wanted = workspace_id.trim().to_lowercase();
                workspaces::list_for(store, &auth.uid)
                    .into_iter()
                    .find(|w| text(w, "name").trim().to_lowercase() == wanted)
            }
        };
        let Some(target) = target else {
            return json!({"ok": false, "error": format!("no workspace {} on this account", python_repr(workspace_id))});
        };
        let ws = text(&target, "id");
        (ws, target)
    } else {
        let ws = workspace_for(store, aliases, auth, repo_id);
        let target = workspaces::get(store, &ws).unwrap_or_else(|| json!({"id": ws, "name": ws}));
        (ws, target)
    };
    let Some(member) = member(store, &ws, &auth.uid) else {
        return json!({"ok": false, "error": format!("you are not a member of '{}'", text(&target, "name"))});
    };
    let read_only = crate::access::access_of(store, &ws, Some(&member)) == "read";
    if read_only || !has_repo_access(Some(&member), repo_id) {
        return json!({"ok": false, "error": "removing a repo needs write access to it"});
    }
    let physical = aliases.resolve(&ws, repo_id);
    let scope = format!("{ws}:{physical}");
    let stamp = now();

    let mut removed_bindings = 0i64;
    for (key, rec) in store.kv_list("ghrepo", &format!("{ws}:")) {
        let watched = text(&rec, "repo_id");
        if !watched.is_empty() && aliases.scope_for(&ws, &watched) == scope {
            let _ = store.kv_delete("ghrepo", &key);
            removed_bindings += 1;
        }
    }
    let removed_trees = store.list_workspaces(&scope).len() as i64;
    let _ = store.delete_scope(&scope);

    for prefix in [
        format!("intent:{scope}:"), format!("hot:{scope}:"),
        format!("draft:{scope}:"), format!("lastedit:{scope}:"),
        format!("recent:{scope}"),
    ] {
        for (key, _) in store.eph_scan(&prefix) {
            store.eph_delete(&key);
        }
    }
    // presence is keyed by workspace and agent, with the repo in the record
    crate::presence::forget_scope(store, &scope);
    store.eph_delete(&format!("autowatch:{scope}"));
    let _ = store.eph_set(&format!("wsrouteepoch:{}", path_key(repo_id)), &json!({"t": stamp}), Some(3600.0));
    for bucket in ["setup_state", "semlint"] {
        let _ = store.kv_delete(bucket, &scope);
    }
    let _ = store.kv_put(
        "repo_moved", &format!("{ws}:{}", repo_key(&physical)),
        &json!({"repo_id": physical, "to": "", "removed": true, "ts": stamp, "by": effective_by}), stamp,
    );
    let _ = store.ledger_append(
        &scope, "repo_removed",
        &json!({"user": auth.user_id, "repo_id": physical, "workspace": ws,
                "bindings": removed_bindings, "trees": removed_trees}),
        stamp,
    );

    json!({
        "ok": true, "workspace": ws, "name": text(&target, "name"), "repo_id": physical,
        "removed": {"bindings": removed_bindings, "trees": removed_trees},
        "note": "gone from this workspace's repo list and routing; the ledger keeps its history. \
Adding the repo again on the dashboard brings it back.",
    })
}

// -------------------------------------------------------- switching workspace

/// Without a target: list every workspace this account belongs to and which
/// one THIS credential is currently bound to. With a workspace id or name:
/// re-bind this credential to it immediately (membership-checked, no
/// re-authentication) — only a `cat_…` hook credential can be re-bound this
/// way; anything else must re-authenticate. `authorization` is the raw
/// `Authorization` header value the caller presented (e.g. `"Bearer
/// cat_…"`), exactly as `auth::principal_for_bearer` reads it.
pub fn switch_workspace(store: &Store, uid: &str, current_workspace: &str, authorization: &str, workspace: &str) -> Value {
    let mine = workspaces::list_for(store, uid);
    let listing: Vec<Value> = mine
        .iter()
        .map(|w| {
            json!({
                "id": text(w, "id"), "name": text(w, "name"),
                "kind": w.get("kind").and_then(Value::as_str).unwrap_or("custom"),
                "current": text(w, "id") == current_workspace,
            })
        })
        .collect();
    if workspace.is_empty() {
        return json!({
            "current": current_workspace, "workspaces": listing,
            "how": "call switch_workspace(workspace=<id or name>) to re-bind",
        });
    }
    let wanted = workspace.trim().to_lowercase();
    let Some(target) = mine.iter().find(|w| text(w, "id") == workspace || text(w, "name").trim().to_lowercase() == wanted)
    else {
        return json!({"ok": false, "workspaces": listing,
            "error": format!("no workspace {} on this account", python_repr(workspace))});
    };
    let target_id = text(target, "id");
    let target_name = text(target, "name");
    if target_id == current_workspace {
        return json!({"ok": true, "workspace": target_id, "name": target_name, "note": "already bound here"});
    }
    let raw = authorization.split_once(' ').map(|(_, rest)| rest).unwrap_or("").trim();
    if !raw.starts_with(crate::auth::ACCESS_TOKEN_PREFIX) {
        let label = if target_name.is_empty() { &target_id } else { &target_name };
        return json!({"ok": false, "error": format!(
            "this credential type cannot be re-bound; re-authenticate (in Claude Code: /mcp -> \
collide -> authenticate) and pick {} on the approval screen", python_repr(label))});
    }
    let Some(mut record) = store.kv_get(crate::auth::TOKEN_BUCKET, &crate::auth::token_key(raw)) else {
        return json!({"ok": false, "error": "credential record not found; re-authenticate"});
    };
    record["workspace"] = json!(target_id);
    if let Some(list) = record.get("workspaces").and_then(Value::as_array).cloned() {
        if !list.is_empty() && !list.iter().any(|w| w.as_str() == Some(target_id.as_str())) {
            let mut updated = list;
            updated.push(json!(target_id));
            record["workspaces"] = Value::Array(updated);
        }
    }
    let _ = store.kv_put(crate::auth::TOKEN_BUCKET, &crate::auth::token_key(raw), &record, now());
    json!({
        "ok": true, "workspace": target_id, "name": target_name,
        "note": "credential re-bound; established repos keep routing to their home workspace — \
this changes where fresh repos and defaults land",
    })
}

// ---------------------------------------------------------- dashboard routes

/// `POST /api/workspaces/{workspace_id}/watch` handler logic. Passing the
/// GitHub numeric id lets a rename (same id, new slug) be recognized and
/// the old history aliased onto the new name — see [`register_watch`].
pub fn api_watch(store: &Store, aliases: &Aliases, workspace_id: &str, auth: &AuthUser, body: &Value) -> Value {
    let result = register_watch(store, aliases, workspace_id, &text(body, "repo_id"), &text(body, "github_id"), &auth.uid);
    if crate::compat::truthy(result.get("ok")) {
        clear_pending(store, &auth.uid, &text(body, "repo_id"));
    }
    result
}

/// `POST /api/workspaces/{workspace_id}/repos/move` handler logic. Same
/// operation the `move_repo` MCP tool performs; `via` is the request tag
/// (host/path) events should carry, or empty when none is known.
///
/// Bodies here match the Python route exactly, which means the two
/// pre-checks below deliberately have no `ok` key (unlike every other
/// result this module returns) — Python answers them with a bare
/// `{"error": ...}` before `service.move_repo` is ever called. The route
/// layer owns status codes; the caller here needs 400 for the first
/// pre-check and 403 for the second, and otherwise 200 when the deeper
/// `move_repo` result carries `"ok": true`, else 400.
pub fn api_move_repo(store: &Store, aliases: &Aliases, workspace_id: &str, auth: &AuthUser, body: &Value, via: &str) -> Value {
    let repo_id = text(body, "repo_id").trim().to_string();
    let to = text(body, "to").trim().to_string();
    if repo_id.is_empty() || to.is_empty() {
        return json!({"error": "repo_id and to are required"}); // -> 400
    }
    let current_member = member(store, workspace_id, &auth.uid);
    if !has_repo_access(current_member.as_ref(), &repo_id) {
        return json!({"error": "no access to this repo"}); // -> 403
    }
    move_repo(store, aliases, auth, &repo_id, &to, &auth.user_id, via)
}

/// `POST /api/workspaces/{workspace_id}/repos/remove` handler logic.
/// Removes from THIS workspace (the URL's `workspace_id`), same as the
/// `remove_repo` MCP tool. The pre-check below is bodied like Python's
/// (no `ok` key) and needs status 400; otherwise 200/400 on the deeper
/// `remove_repo` result's `ok` field, as with every other tool here.
pub fn api_remove_repo(store: &Store, aliases: &Aliases, workspace_id: &str, auth: &AuthUser, body: &Value) -> Value {
    let repo_id = text(body, "repo_id").trim().to_string();
    if repo_id.is_empty() {
        return json!({"error": "repo_id is required"}); // -> 400
    }
    remove_repo(store, aliases, auth, &repo_id, workspace_id, &auth.user_id)
}

/// `GET /api/workspaces/{workspace_id}/repos` handler logic: the display
/// mapping — a renamed repo's ledger stays under its old scope id, so hand
/// the UI the preferred (post-rename) id and collapse ids that resolve to
/// the same scope, never listing old AND new names.
pub fn api_list_repos(store: &Store, aliases: &Aliases, workspace_id: &str) -> Value {
    let shown = |rid: &str| {
        let canonical = aliases.resolve(workspace_id, rid);
        let preferred = crate::envelope::preferred_repo_id(store, aliases, workspace_id, &canonical);
        if preferred.is_empty() { canonical } else { preferred }
    };
    let mut display: BTreeSet<String> = BTreeSet::new();
    for rid in workspaces::repos(store, workspace_id, true) {
        display.insert(shown(&rid));
    }
    // who added each repo, and how: the earliest binding of it wins, since
    // that is the one that brought it into the workspace
    let mut added: BTreeMap<String, Value> = BTreeMap::new();
    for (_, rec) in store.kv_list("ghrepo", &format!("{workspace_id}:")) {
        let rid = text(&rec, "repo_id");
        let id = shown(rid.trim());
        let ts = rec.get("updated").and_then(Value::as_f64).unwrap_or(0.0);
        if !display.contains(&id) || added.get(&id).is_some_and(|prior| prior["ts"].as_f64().unwrap_or(0.0) <= ts) {
            continue;
        }
        added.insert(id, json!({"by": who_added(store, &text(&rec, "by")), "auto": truthy(rec.get("auto")), "ts": ts}));
    }
    json!({"repos": display.into_iter().collect::<Vec<_>>(), "added": added})
}

/// A binding's `by` (a uid from the dashboard, an email from an agent) as
/// the name people know: the Collide username, else the email.
fn who_added(store: &Store, by: &str) -> String {
    let by = by.trim();
    let profile = if by.contains('@') {
        store.kv_get("username_by_email", &by.to_lowercase())
    } else {
        store.kv_get("profile", by)
    };
    profile
        .map(|p| text(&p, "username"))
        .filter(|name| !name.is_empty())
        .or_else(|| store.kv_get("profile", by).map(|p| text(&p, "email")).filter(|e| !e.is_empty()))
        .unwrap_or_else(|| by.to_string())
}

/// `POST /api/workspaces/{workspace_id}/repo-alias` handler logic: manual
/// repo migration, the escape hatch when a rename wasn't auto-detected (no
/// GitHub id).
pub fn api_repo_alias(store: &Store, aliases: &Aliases, workspace_id: &str, auth: &AuthUser, body: &Value) -> Value {
    crate::rename::alias_repo(
        store, aliases, workspace_id, &text(body, "from"), &text(body, "to"), &auth.uid,
        truthy(body.get("force")),
    )
}

/// `POST /api/workspaces/{workspace_id}/repo-rename` handler logic: unlike
/// `/repo-alias` (a one-way redirect for merging spellings), this also
/// records the forward preference the client write-back reads.
pub fn api_repo_rename(store: &Store, aliases: &Aliases, workspace_id: &str, auth: &AuthUser, body: &Value) -> Value {
    crate::rename::rename_repo(
        store, aliases, workspace_id, &text(body, "old"), &text(body, "new"), &auth.uid,
        truthy(body.get("force")),
    )
}


/// The workspace's CURRENT repo: its most recently watched one, as a display
/// id. This is the server-side truth every dashboard converges on — a watch
/// made in one browser moves the others, whose per-origin localStorage copy
/// is just a cache. Empty when nothing is watched.
///
/// An agent merely touching repo B must not flip every dashboard off the repo
/// the human explicitly watched, so an auto binding only wins when no
/// explicit watch exists at all.
///
/// `member` is the VIEWER's workspace record: membership is workspace-wide
/// but access is per repo, so the bindings go through the same allowlist
/// every other workspace-wide read does. A one-repo member converges on the
/// newest watch among the repos they can see — never on the id (or the
/// last-watch time) of one their invite excludes.
pub fn primary_repo(store: &Store, aliases: &Aliases, workspace_id: &str, member: Option<&Value>) -> Value {
    let (mut best_ts, mut best) = (0.0_f64, String::new());
    let (mut auto_ts, mut auto_best) = (0.0_f64, String::new());
    for (_key, rec) in store.kv_list("ghrepo", &format!("{workspace_id}:")) {
        let ts = rec.get("updated").and_then(Value::as_f64).unwrap_or(0.0);
        let rid = rec.get("repo_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if rid.is_empty() || !crate::access::has_repo_access(member, &rid) {
            continue;
        }
        if crate::compat::truthy(rec.get("auto")) {
            if ts >= auto_ts {
                auto_ts = ts;
                auto_best = rid;
            }
        } else if ts >= best_ts {
            best_ts = ts;
            best = rid;
        }
    }
    let (chosen, chosen_ts) = if best.is_empty() { (auto_best, auto_ts) } else { (best, best_ts) };
    if chosen.is_empty() {
        return json!({});
    }
    let display = crate::envelope::preferred_repo_id(store, aliases, workspace_id, &chosen);
    let display = if display.is_empty() { chosen } else { display };
    json!({"repo": display, "updated": chosen_ts})
}
