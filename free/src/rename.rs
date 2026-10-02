//! Repo aliasing and renaming.
//!
//! A repo that gets renamed on GitHub must keep its history. The ledger is a
//! single global hash chain, so re-keying a scope would invalidate every row
//! after it — the rename therefore never moves data. It records two things
//! instead: an alias so future reports under the new id resolve onto the
//! scope that already holds the history, and a preferred id so the server can
//! hand the new name back for clients to adopt.
//!
//! Both operations refuse, rather than force, when the target already owns
//! its own history. A rename that silently buried a live scope would be
//! unrecoverable, and two repos briefly sharing a name is a normal thing that
//! must not cost anyone their record.

use serde_json::{json, Value};

use crate::repo::{normalize_repo_id, repo_key, Aliases};
use crate::store::{now, Store};

/// True when a repo id already owns a persisted tree — the signal that
/// aliasing it away would orphan real history.
fn scope_has_data(store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str) -> bool {
    let scope = aliases.scope_for(workspace_id, repo_id);
    store.list_scopes(&format!("{workspace_id}:")).contains(&scope)
}

/// Point every future reference to `from_id` at the scope that holds `to_id`.
/// Idempotent.
pub fn alias_repo(
    store: &Store, aliases: &Aliases, workspace_id: &str, from_id: &str, to_id: &str,
    by: &str, force: bool,
) -> Value {
    let from_id = from_id.trim();
    let to_id = to_id.trim();
    if from_id.is_empty() || to_id.is_empty() {
        return json!({"ok": false, "error": "need both from and to repo ids"});
    }
    // resolve the target to its terminal canonical so aliases stay one hop
    let canonical = aliases.resolve(workspace_id, to_id);
    if from_id == canonical {
        return json!({"ok": false, "error": "from and to resolve to the same repo"});
    }
    if let Some(existing) = store.kv_get("repo_alias", &format!("{workspace_id}:{from_id}")) {
        if existing.get("canonical").and_then(Value::as_str) == Some(canonical.as_str()) {
            return json!({"ok": true, "from": from_id, "canonical": canonical, "already": true});
        }
    }
    if !force && scope_has_data(store, aliases, workspace_id, from_id) {
        return json!({
            "ok": false, "shadows_data": true,
            "error": format!("'{from_id}' already has its own history; pass force to alias anyway"),
        });
    }
    let stamp = now();
    let record = json!({"canonical": canonical, "from": from_id, "created": stamp, "by": by});
    let _ = store.kv_put("repo_alias", &format!("{workspace_id}:{from_id}"), &record, stamp);
    aliases.add_rename(workspace_id, from_id, &canonical);
    json!({"ok": true, "from": from_id, "canonical": canonical})
}

/// Present `new_id` as this repo's id everywhere, while its ledger stays
/// physically under the scope that already holds it. Idempotent.
pub fn rename_repo(
    store: &Store, aliases: &Aliases, workspace_id: &str, old_id: &str, new_id: &str,
    by: &str, force: bool,
) -> Value {
    let old_id = old_id.trim();
    let new_id = new_id.trim();
    if old_id.is_empty() || new_id.is_empty() {
        return json!({"ok": false, "error": "need both old and new repo ids"});
    }
    let physical = aliases.resolve(workspace_id, old_id);
    let pkey = format!("{workspace_id}:{physical}");
    if new_id == physical {
        return json!({"ok": false, "error": "new id is already the canonical scope"});
    }
    if let Some(record) = store.kv_get("repo_preferred", &pkey) {
        if record.get("preferred").and_then(Value::as_str) == Some(new_id) {
            return json!({
                "ok": true, "canonical": physical, "preferred": new_id, "already": true,
            });
        }
    }
    // both steps must hold: alias new->old so future reports land on the real
    // history, then record the preferred id so it can be handed back
    let aliased = alias_repo(store, aliases, workspace_id, new_id, old_id, by, force);
    if aliased.get("ok").and_then(Value::as_bool) != Some(true) {
        return aliased;
    }
    let stamp = now();
    let record = json!({
        "preferred": new_id, "canonical": physical, "from": old_id,
        "created": stamp, "by": by,
    });
    let _ = store.kv_put("repo_preferred", &pkey, &record, stamp);

    // also bridge the bare basename of the new name onto the same scope, so a
    // client reporting the short form does not fragment into its own scope.
    // Best-effort and never forced: if the short name already owns data, leave
    // it — workspace-wide recall keeps those notes findable anyway.
    let base = normalize_repo_id(new_id);
    if !base.is_empty() && base != new_id && base != physical {
        let _ = alias_repo(store, aliases, workspace_id, &base, old_id, by, false);
    }
    json!({"ok": true, "canonical": physical, "preferred": new_id})
}

/// The hook reports the id it was configured with AND the id its git origin
/// implies. When they differ, and the origin is not already a repo of its own
/// here, the repo was renamed on GitHub.
///
/// Throttled to one check an hour per repo pair, because the hook sends the
/// origin on every single edit and this is the only expensive path it can
/// reach. Returns the new id when something changed, else empty.
///
/// `member` is the caller's own workspace record: adopting an origin
/// pre-registers alias origin->repo, which redirects every later read on the
/// origin's id onto this repo's scope, so an origin outside the caller's
/// per-repo allowlist is refused (before the throttle, so a refused attempt
/// never blocks a legitimate one for the hour). The owner sees every repo.
pub fn adopt_origin(
    store: &Store, aliases: &Aliases, workspace_id: &str, repo_id: &str, origin: &str, by: &str,
    member: Option<&Value>,
) -> String {
    let origin = origin.trim().to_lowercase();
    let repo_id = repo_id.trim();
    // a folder on the reporting machine is no hosted repo (older hooks sent them)
    if origin.is_empty() || repo_id.is_empty() || !origin.contains('/') || origin.starts_with('/')
        || origin.starts_with('.') || origin.starts_with('~') || origin.starts_with("file")
        || origin.chars().nth(1) == Some(':')
    {
        return String::new();
    }
    if repo_key(&origin) == repo_key(repo_id) {
        return String::new();
    }
    // a #branch-scoped id keeps its suffix through the rename
    let suffix = repo_id.find('#').map(|index| &repo_id[index..]).unwrap_or("");
    let new_id = format!("{origin}{suffix}");
    if !crate::access::has_repo_access(member, &new_id) {
        return String::new();
    }
    let stamp_key = format!(
        "originchk:{workspace_id}:{}:{}", repo_key(repo_id), repo_key(&new_id));
    if store.eph_get(&stamp_key).is_some() {
        return String::new();
    }
    let _ = store.eph_set(&stamp_key, &json!({"t": now()}), Some(3600.0));

    if crate::envelope::preferred_repo_id(store, aliases, workspace_id, repo_id) == new_id {
        return new_id;
    }
    if scope_has_data(store, aliases, workspace_id, &new_id) {
        return String::new(); // two real repos, not a rename: leave both alone
    }
    let renamed = rename_repo(store, aliases, workspace_id, repo_id, &new_id, by, false);
    if renamed.get("ok").and_then(Value::as_bool) != Some(true) {
        return String::new();
    }
    let _ = store.ledger_append(
        &aliases.scope_for(workspace_id, repo_id),
        "repo_renamed",
        &json!({"user": by, "from": repo_id, "to": new_id, "source": "git-origin"}),
        now(),
    );
    new_id
}
