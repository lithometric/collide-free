//! Workspace tenancy: invite-only sections of the shared database.
//!
//! A workspace is the partition every scoped row lives under
//! (`scope = "{workspace_id}:{repo_id}"`), not a separate table or database.
//! Metadata sits in the shared tier's bucketed KV:
//!
//!   ws       `<ws_id>`          -> {id, name, owner, created, kind, github_org}
//!   member   `<ws_id>/<uid>`    -> {uid, email, name, role, added, added_by, access, repos}
//!   uws      `<uid>/<ws_id>`    -> {workspace}          (membership index)
//!   invite   `<code>`           -> {workspace, created_by, created, expires, max_uses, uses,
//!                                   repos, email}
//!
//! A workspace holds many repos. `kind` is "personal" (the one every account
//! gets, named "<name>'s workspace"), "org" (named after a GitHub org — repos
//! under `github.com/<org>/...` route here automatically) or "custom". A
//! member's `repos` is "*" (everything, the owner always) or the list of repo
//! ids they may work in; an invite carries the same list so redeeming it
//! grants exactly that. Repo ids in those lists are stored as keys (lowercase,
//! no `#branch`) so any spelling of the same repo matches — see
//! [`crate::repo::repo_key`], reused rather than duplicated here.
//!
//! Joining requires an invite code from an existing member; a user without
//! one can only create a fresh workspace of their own.
//!
//! Nothing in the crate calls into this module yet (it is not wired to an
//! HTTP handler), hence the blanket allow below — every item here is dead
//! code until that wiring lands, the same position `store.rs` was in before
//! its first caller.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::io::Read;

use serde_json::{json, Value};

use crate::access::has_repo_access;
use crate::compat::{new_id, python_repr, truthy};
use crate::repo::repo_key;
use crate::store::{now, Store};

/// Unambiguous, human-typable invite alphabet — no 0/O/1/I/L — matching
/// Python's `INVITE_ALPHABET`.
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const INVITE_TTL_S: f64 = 7.0 * 86400.0;
const INVITE_MAX_USES: i64 = 20;

/// An 8-character invite code drawn from OS randomness, matching Python's
/// `secrets.choice`.
///
/// Rejection sampling, not a plain modulo: 256 is not a multiple of the
/// alphabet's 31 characters, so `byte % 31` would hand out the first eight
/// letters slightly more often than the rest. This code gates who can join a
/// private workspace, so that bias is worth the extra branch.
fn invite_code() -> String {
    let ceiling = 256 - (256 % INVITE_ALPHABET.len());
    let mut source = std::fs::File::open("/dev/urandom").ok();
    let mut code = String::with_capacity(8);
    let mut byte = [0u8; 1];
    while code.len() < 8 {
        let drawn = source.as_mut().is_some_and(|f| f.read_exact(&mut byte).is_ok());
        if !drawn {
            // OS randomness unavailable (should not happen in practice): fall
            // back to compat::new_id's process-unique, unpredictably-seeded
            // churn rather than block invite creation entirely.
            byte[0] = new_id().as_bytes()[code.len() % 12];
        }
        if (byte[0] as usize) < ceiling {
            code.push(INVITE_ALPHABET[byte[0] as usize % INVITE_ALPHABET.len()] as char);
        }
    }
    code
}

/// The caller identity a workspace write needs: who owns the write, and what
/// name lands in a freshly created member record.
///
/// Deliberately not `auth::Principal` — that type does not carry a display
/// name on this side yet (the OAuth exchange that fills it in is a separate,
/// in-flight port), and every member-creating call here needs one. A local,
/// complete shape beats a partial borrow of a type that is still catching up;
/// once `auth::Principal` gains `name`, a caller can build one of these from
/// it in one line.
#[derive(Debug, Clone)]
pub struct Principal {
    pub uid: String,
    pub email: String,
    pub name: String,
}

impl Principal {
    /// The identity written into records and shown on dashboards.
    pub fn label(&self) -> &str {
        if self.email.is_empty() { &self.uid } else { &self.email }
    }
}

/// Which repos a member (or invite) may work in — Python's `list[str] | str`
/// union, made concrete so a caller cannot pass a stray string other than
/// `"*"`.
#[derive(Debug, Clone)]
pub enum RepoScope {
    All,
    Some(Vec<String>),
}

/// Optional arguments to [`create_invite`], mirroring the keyword arguments
/// `app.py` builds up conditionally before calling the Python method.
pub struct InviteOptions {
    pub ttl_s: f64,
    pub max_uses: i64,
    pub repos: RepoScope,
    pub email: String,
    pub github_login: String,
}

/// A seat-limit lookup: workspace id -> the cap, or `None` for unlimited.
/// Billing isn't ported yet, so [`redeem_invite`] takes this as an optional
/// callback rather than calling into a `billing` module directly.
///
/// Carries its own lifetime rather than defaulting to `'static`: a bare
/// `dyn Fn(&str) -> Option<i64>` alias with no lifetime parameter gets
/// `'static` as its default trait-object bound wherever it's substituted,
/// which silently forces every caller's closure to be `'static` too — one
/// couldn't borrow `&app.store` without first cloning it into an `Arc`.
/// With `<'a>` here and `SeatLimiter<'_>` at the call site, the bound
/// follows the reference's own lifetime instead, so a closure that borrows
/// something short-lived (a `&Store` included) just works.
pub type SeatLimiter<'a> = dyn Fn(&str) -> Option<i64> + 'a;

impl Default for InviteOptions {
    fn default() -> Self {
        Self {
            ttl_s: INVITE_TTL_S,
            max_uses: INVITE_MAX_USES,
            repos: RepoScope::All,
            email: String::new(),
            github_login: String::new(),
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn int_or(value: &Value, key: &str, default: i64) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// Python's `s[:n]` counts Unicode code points, not bytes; `&str[..n]` would
/// panic mid-codepoint on anything outside ASCII.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Parse the `"*" | [repo, ...]` union Python stores for a `repos` field.
/// Anything that isn't a JSON array — `"*"`, missing, or null — reads as
/// every repo, matching every default this module ever wrote (`.get(k, "*")`).
fn repo_scope_of(value: Option<&Value>) -> RepoScope {
    match value.and_then(Value::as_array) {
        Some(items) => {
            RepoScope::Some(items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        }
        None => RepoScope::All,
    }
}

fn repo_scope_to_value(scope: &RepoScope) -> Value {
    match scope {
        RepoScope::All => json!("*"),
        RepoScope::Some(list) => json!(list),
    }
}

/// Clean a caller-supplied repo list into the canonical stored form: keys
/// normalized (so any spelling of the same repo collapses to one entry),
/// blanks dropped, deduped and sorted — matching
/// `sorted({repo_key(str(r)) for r in repos if str(r).strip()})`.
fn normalize_repos(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(repo_key)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// The name a fresh personal workspace gets when the caller didn't choose
/// one — every account's default workspace.
pub fn personal_name(principal: &Principal) -> String {
    format!("{}'s workspace", principal.label())
}

// ---------------------------------------------------------------- workspaces

/// Workspace names are unique per account: with automatic per-repo routing
/// and name-based switching, two same-named workspaces on one account would
/// be indistinguishable.
fn name_taken(store: &Store, uid: &str, name: &str, ignore_id: &str) -> bool {
    let wanted = name.trim().to_lowercase();
    list_for(store, uid)
        .iter()
        .any(|w| text(w, "id") != ignore_id && text(w, "name").trim().to_lowercase() == wanted)
}

/// `kind`: "personal" (no name given, or the personal name), "org"
/// (`github_org` set, or the name looks like a GitHub org slug the caller
/// passes explicitly) or "custom". One personal workspace per account.
pub fn create(
    store: &Store, principal: &Principal, name: &str, kind: &str, github_org: &str,
) -> Result<Value, String> {
    let now_ts = now();
    let mut cleaned = truncate_chars(name.trim(), 120);
    let mut kind = kind.to_string();
    if cleaned.is_empty() {
        cleaned = personal_name(principal);
        kind = "personal".to_string();
    }
    if kind == "personal" && personal_for(store, &principal.uid).is_some() {
        return Err("you already have a personal workspace".to_string());
    }
    if !matches!(kind.as_str(), "personal" | "org" | "custom") {
        kind = if !github_org.is_empty() { "org".to_string() } else { "custom".to_string() };
    }
    if name_taken(store, &principal.uid, &cleaned, "") {
        return Err(format!("you already have a workspace named {}", python_repr(&cleaned)));
    }
    let id = new_id();
    let ws = json!({
        "id": id,
        "name": cleaned,
        "owner": principal.uid,
        "created": now_ts,
        "kind": kind,
        "github_org": github_org.trim().to_lowercase(),
    });
    let _ = store.kv_put("ws", &id, &ws, now_ts);
    add_member(store, &id, principal, "owner", &principal.uid, now_ts);
    crate::analytics::signed_up(store, &principal.uid, &principal.email, &principal.name, "site");
    crate::analytics::capture(&principal.uid, "workspace_created", &id, json!({"kind": kind}));
    Ok(ws)
}

pub fn owned_by(store: &Store, uid: &str) -> Vec<Value> {
    list_for(store, uid).into_iter().filter(|w| text(w, "owner") == uid).collect()
}

/// The account's personal workspace: kind == personal, else (for accounts
/// predating kinds) the oldest one it owns whose name is the personal name.
pub fn personal_for(store: &Store, uid: &str) -> Option<Value> {
    let owned = owned_by(store, uid);
    if let Some(hit) = owned.iter().find(|w| text(w, "kind") == "personal") {
        return Some(hit.clone());
    }
    owned.into_iter().find(|w| text(w, "name").ends_with("'s workspace"))
}

/// The workspace that represents GitHub org `org` for this account:
/// `github_org` set to it, else a workspace NAMED like it. First match by
/// creation order; membership is already implied by [`list_for`].
pub fn org_workspace_for(
    store: &Store, uid: &str, org: &str, allowed: Option<&BTreeSet<String>>,
) -> Option<Value> {
    let wanted = org.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let filter_active = allowed.is_some_and(|a| !a.is_empty());
    let mine: Vec<Value> = list_for(store, uid)
        .into_iter()
        .filter(|w| !filter_active || allowed.unwrap().contains(&text(w, "id")))
        .collect();
    if let Some(hit) = mine.iter().find(|w| text(w, "github_org").to_lowercase() == wanted) {
        return Some(hit.clone());
    }
    mine.into_iter().find(|w| text(w, "name").trim().to_lowercase() == wanted)
}

pub fn set_kind(store: &Store, workspace_id: &str, kind: &str, github_org: Option<&str>) -> Option<Value> {
    let mut ws = get(store, workspace_id)?;
    if matches!(kind, "personal" | "org" | "custom") {
        ws["kind"] = json!(kind);
    }
    if let Some(org) = github_org {
        ws["github_org"] = json!(org.trim().to_lowercase());
    }
    let _ = store.kv_put("ws", workspace_id, &ws, now());
    Some(ws)
}

pub fn get(store: &Store, workspace_id: &str) -> Option<Value> {
    store.kv_get("ws", workspace_id)
}

pub fn rename(store: &Store, workspace_id: &str, name: &str) -> Result<Option<Value>, String> {
    let Some(mut ws) = get(store, workspace_id) else { return Ok(None) };
    let cleaned = truncate_chars(name.trim(), 120);
    if !cleaned.is_empty() {
        // same uniqueness rule as create — two same-named workspaces on one
        // account would make every by-name reference ambiguous
        let owner = text(&ws, "owner");
        if name_taken(store, &owner, &cleaned, workspace_id) {
            return Err(format!("you already have a workspace named {}", python_repr(&cleaned)));
        }
        ws["name"] = json!(cleaned);
        let _ = store.kv_put("ws", workspace_id, &ws, now());
    }
    Ok(Some(ws))
}

pub fn list_for(store: &Store, uid: &str) -> Vec<Value> {
    let mut out: Vec<Value> = store
        .kv_list("uws", &format!("{uid}/"))
        .into_iter()
        .filter_map(|(_, entry)| get(store, &text(&entry, "workspace")))
        .collect();
    out.sort_by(|a, b| num(a, "created").partial_cmp(&num(b, "created")).unwrap());
    out
}

// ---------------------------------------------------------------- membership

/// The membership record. Same bucket and key shape as [`crate::auth::member`]
/// — reused there rather than duplicated here, since the hot-path auth
/// resolution needs the identical lookup.
pub use crate::auth::member;

pub fn members(store: &Store, workspace_id: &str) -> Vec<Value> {
    let mut out: Vec<Value> =
        store
            .kv_list("member", &format!("{workspace_id}/"))
            .into_iter()
            .map(|(_, v)| crate::auth::with_repo_renames(store, workspace_id, v))
            .collect();
    out.sort_by(|a, b| num(a, "added").partial_cmp(&num(b, "added")).unwrap());
    out
}

fn add_member(
    store: &Store, workspace_id: &str, principal: &Principal, role: &str, added_by: &str, now_ts: f64,
) -> Value {
    let record = json!({
        "uid": principal.uid,
        "email": principal.email,
        "name": principal.name,
        "role": role,
        "added": now_ts,
        "added_by": added_by,
    });
    let _ = store.kv_put("member", &format!("{workspace_id}/{}", principal.uid), &record, now_ts);
    let _ = store.kv_put(
        "uws", &format!("{}/{workspace_id}", principal.uid), &json!({"workspace": workspace_id}), now_ts,
    );
    record
}

/// Which repos this member may work in: every repo, or an explicit set. The
/// owner is always every repo.
pub fn set_member_repos(store: &Store, workspace_id: &str, uid: &str, repos: RepoScope) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    let owner = text(&member, "role") == "owner";
    let normalized = if owner {
        RepoScope::All
    } else {
        match repos {
            RepoScope::All => RepoScope::All,
            RepoScope::Some(list) => RepoScope::Some(normalize_repos(&list)),
        }
    };
    member["repos"] = repo_scope_to_value(&normalized);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

pub fn members_with_access(store: &Store, workspace_id: &str, repo_id: &str) -> Vec<Value> {
    members(store, workspace_id).into_iter().filter(|m| has_repo_access(Some(m), repo_id)).collect()
}

pub fn repo_seats_used(store: &Store, workspace_id: &str, repo_id: &str) -> i64 {
    members_with_access(store, workspace_id, repo_id).len() as i64
}

/// `access`: "write" (default for every member) or "read" — a read-only
/// member observes the workspace but MCP write tools (declare_intent,
/// report_edit, ...) reject their calls.
pub fn set_access(store: &Store, workspace_id: &str, uid: &str, access: &str) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    member["access"] = json!(access);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

// ------------------------------------------------------------------ invites

/// `opts.repos`: every repo, or the repo ids the joiner may work in — the
/// per-repo permission checked off on the invite form. `opts.email` /
/// `opts.github_login`: who it was sent to (the greyed-out contributor row).
pub fn create_invite(store: &Store, workspace_id: &str, principal: &Principal, opts: InviteOptions) -> Value {
    let now_ts = now();
    let code = invite_code();
    let repos = match opts.repos {
        RepoScope::All => RepoScope::All,
        RepoScope::Some(list) => RepoScope::Some(normalize_repos(&list)),
    };
    let invite = json!({
        "code": code,
        "workspace": workspace_id,
        "created_by": principal.uid,
        "created": now_ts,
        "expires": now_ts + opts.ttl_s.max(60.0),
        "max_uses": opts.max_uses.max(1),
        "uses": 0,
        "repos": repo_scope_to_value(&repos),
        "email": opts.email.trim().to_lowercase(),
        "github_login": opts.github_login.trim().to_lowercase(),
    });
    let _ = store.kv_put("invite", &code, &invite, now_ts);
    crate::analytics::capture(
        &principal.uid, "invite_sent", workspace_id,
        json!({"by_email": !text(&invite, "email").is_empty(), "by_github": !text(&invite, "github_login").is_empty()}),
    );
    invite
}

/// Returns `{"ok": true, "workspace": {...}, "repos": ..., "access":
/// "write"|"read"[, "viewer": true][, "share_play": {...}]}` or
/// `{"ok": false, "error": ...}`.
///
/// Who gets in, and as what (Figma/Xbox: viewers are free, players pay) —
/// decided from the workspace's plan record, in this order:
///   1. Free, trial never used, and the invite is the OWNER's: SHARE PLAY —
///      `billing::start_trial` puts the workspace on a 14-day Team trial
///      first, then the invitee joins as a PLAYER (access "write"). Both
///      get Team.
///   2. Paid or trialing (after step 1): the seat gate — join as a player
///      while the purchased seats allow, else the `seats_full` refusal (a
///      paid workspace with no seat left is still a refusal; the owner buys
///      a seat). A trial sells no seats, so it never refuses.
///   3. Free with the trial used or expired: join as a VIEWER (access
///      "read") — never `seats_full`, unlimited. Their agents get nothing;
///      they watch the radar and read the briefings.
/// The response's `access` is `billing::effective_access` — computed, not
/// stored: once a trial or plan lapses every non-owner reads as a viewer
/// whatever their stored access says.
///
/// `seat_limit_for` caps PLAYERS at the seats the plan bought;
/// `repo_seat_limit_for` caps PEOPLE PER REPO when a plan sets it (none does
/// today). The invite's repos are granted one by one — a repo already at its
/// cap is skipped, and when none can be granted the join bounces with
/// `seats_full`. Existing members always get through; re-redeeming widens
/// their repo list. Twin of Python's `WorkspaceStore.redeem_invite`.
pub fn redeem_invite(
    store: &Store,
    code: &str,
    principal: &Principal,
    seat_limit_for: Option<&SeatLimiter<'_>>,
    repo_seat_limit_for: Option<&SeatLimiter<'_>>,
) -> Value {
    let code = code.trim().to_uppercase();
    let now_ts = now();
    let Some(mut invite) = (if code.is_empty() { None } else { store.kv_get("invite", &code) }) else {
        return json!({"ok": false, "error": "invalid invite code"});
    };
    if num(&invite, "expires") <= now_ts {
        return json!({"ok": false, "error": "invite expired"});
    }
    if int_or(&invite, "uses", 0) >= int_or(&invite, "max_uses", 1) {
        return json!({"ok": false, "error": "invite already used up"});
    }
    let Some(ws) = get(store, &text(&invite, "workspace")) else {
        return json!({"ok": false, "error": "workspace no longer exists"});
    };
    let ws_id = text(&ws, "id");
    let wanted = repo_scope_of(invite.get("repos"));
    let existing = member(store, &ws_id, &principal.uid);

    let mut plan = crate::billing::plan_of(store, &ws_id);
    let mut viewer = false;
    let mut share_play = false;
    if existing.is_none() && !crate::billing::is_active_status(plan.get("status")) {
        let stored = store.kv_get(crate::billing::BILLING_BUCKET, &ws_id);
        let trial_spent = stored.as_ref().is_some_and(|record| truthy(record.get("trial_used")));
        if !trial_spent && text(&invite, "created_by") == text(&ws, "owner") {
            // SHARE PLAY: the owner's first invite — both get 14 days of Team
            plan = crate::billing::start_trial(store, &ws_id, crate::billing::TRIAL_DAYS, "invite");
            share_play = true;
        } else {
            // Free, trial spent: viewers are free and unlimited
            viewer = true;
        }
    }

    if existing.is_none() && !viewer {
        if let Some(seat_limit_for) = seat_limit_for {
            if let Some(limit) = seat_limit_for(&ws_id) {
                // a member's own seat (selfseat.rs) uses none of the plan's
                let taken = members(store, &ws_id).iter().filter(|m| !crate::selfseat::is_self_paid(m)).count();
                if taken as i64 >= limit {
                    // the joiner isn't the payer: leave a trace the OWNER sees
                    // (dashboard pressure pill, agent notices, digest)
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false,
                        "seats_full": true,
                        "error": format!(
                            "this workspace's plan holds {limit} seat{} and all are taken — the \
owner can add seats on the billing page",
                            if limit != 1 { "s" } else { "" }
                        ),
                    });
                }
            }
        }
    }

    let per_repo = repo_seat_limit_for.and_then(|f| f(&ws_id));

    let (granted, skipped): (RepoScope, Vec<String>) = if let Some(limit) = per_repo {
        match wanted {
            RepoScope::Some(want_list) => {
                let mut keep = Vec::new();
                let mut skipped = Vec::new();
                for rid in &want_list {
                    if existing.as_ref().is_some_and(|m| has_repo_access(Some(m), rid)) {
                        keep.push(rid.clone());
                        continue;
                    }
                    if repo_seats_used(store, &ws_id, rid) >= limit {
                        skipped.push(rid.clone());
                    } else {
                        keep.push(rid.clone());
                    }
                }
                if keep.is_empty() && existing.is_none() {
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false, "seats_full": true, "repos_full": skipped,
                        "error": format!(
                            "every repo on this invite already has {limit} people on it (the free \
plan's limit per repo) — the owner can upgrade on the billing page or free a seat"
                        ),
                    });
                }
                (RepoScope::Some(keep), skipped)
            }
            RepoScope::All => {
                // "*" on a per-repo-capped plan: everything, as long as every
                // existing repo still has room for one more person (a
                // wildcard holder counts on every repo); otherwise just the
                // repos with room
                let mut room = Vec::new();
                let mut skipped = Vec::new();
                for rid in repos(store, &ws_id, false) {
                    let has_room = existing.as_ref().is_some_and(|m| has_repo_access(Some(m), &rid))
                        || repo_seats_used(store, &ws_id, &rid) < limit;
                    if has_room {
                        room.push(repo_key(&rid));
                    } else {
                        skipped.push(repo_key(&rid));
                    }
                }
                let granted = if skipped.is_empty() { RepoScope::All } else { RepoScope::Some(room) };
                if matches!(&granted, RepoScope::Some(v) if v.is_empty()) && existing.is_none() {
                    record_blocked_join(store, &ws_id, principal, now_ts);
                    return json!({
                        "ok": false, "seats_full": true, "repos_full": skipped,
                        "error": format!(
                            "every repo in this workspace already has {limit} people on it (the \
free plan's limit per repo) — the owner can upgrade on the billing page or free a seat"
                        ),
                    });
                }
                (granted, skipped)
            }
        }
    } else {
        (wanted, Vec::new())
    };

    let newly_joined = existing.is_none();
    let member_record = if let Some(existing) = existing {
        // widen, never narrow: an invite can add repos to someone already in
        let current = repo_scope_of(existing.get("repos"));
        let merged = match (granted, current) {
            (RepoScope::All, _) | (_, RepoScope::All) => RepoScope::All,
            (RepoScope::Some(g), RepoScope::Some(c)) => {
                let set: BTreeSet<String> = c.into_iter().chain(g).collect();
                RepoScope::Some(set.into_iter().collect())
            }
        };
        set_member_repos(store, &ws_id, &principal.uid, merged).unwrap_or(existing)
    } else {
        let created = add_member(store, &ws_id, principal, "member", &text(&invite, "created_by"), now_ts);
        let with_repos = set_member_repos(store, &ws_id, &principal.uid, granted).unwrap_or(created);
        if viewer {
            set_access(store, &ws_id, &principal.uid, "read").unwrap_or(with_repos)
        } else {
            with_repos
        }
    };

    invite["uses"] = json!(int_or(&invite, "uses", 0) + 1);
    let _ = store.kv_put("invite", &code, &invite, now_ts);

    let access = crate::billing::effective_access(&plan, &member_record);
    let mut result = json!({
        "ok": true,
        "workspace": ws,
        "repos": member_record.get("repos").cloned().unwrap_or(json!("*")),
        "access": access,
    });
    if access == "read" {
        result["viewer"] = json!(true);
    }
    if share_play {
        result["share_play"] = json!({
            "trial_ends": plan.get("trial_ends").cloned().unwrap_or(Value::Null),
            "days": crate::billing::TRIAL_DAYS,
        });
    }
    if !skipped.is_empty() {
        result["repos_full"] = json!(skipped);
    }
    if newly_joined {
        crate::analytics::signed_up(store, &principal.uid, &principal.email, &principal.name, "invite");
        crate::analytics::capture(
            &principal.uid, "invite_joined", &ws_id,
            json!({"access": access, "share_play": share_play, "invited_by": text(&invite, "created_by")}),
        );
    }
    result
}

// ---------------------------------------------------------- leaving & removal

/// Membership ends immediately: the member record and the reverse index both
/// go, so every workspace-bound credential this person holds fails its
/// membership check on the next call. Their edits, notes, and rationales stay
/// in the ledger — history is not rewritten.
pub fn remove_member(store: &Store, workspace_id: &str, uid: &str) {
    let _ = store.kv_delete("member", &format!("{workspace_id}/{uid}"));
    let _ = store.kv_delete("uws", &format!("{uid}/{workspace_id}"));
}

/// A member's role, kept in step with the workspace's `owner` by the caller.
pub fn set_role(store: &Store, workspace_id: &str, uid: &str, role: &str) -> Option<Value> {
    let mut member = member(store, workspace_id, uid)?;
    member["role"] = json!(role);
    let _ = store.kv_put("member", &format!("{workspace_id}/{uid}"), &member, now());
    Some(member)
}

pub fn transfer_ownership(store: &Store, workspace_id: &str, new_owner_uid: &str) -> Option<Value> {
    let mut ws = get(store, workspace_id)?;
    let mut member = member(store, workspace_id, new_owner_uid)?;
    let now_ts = now();
    ws["owner"] = json!(new_owner_uid);
    member["role"] = json!("owner");
    let _ = store.kv_put("ws", workspace_id, &ws, now_ts);
    let _ = store.kv_put("member", &format!("{workspace_id}/{new_owner_uid}"), &member, now_ts);
    Some(ws)
}

// ------------------------------------------------------------- seat pressure

/// A join that bounced off the seat limit, kept so the owner finds out (the
/// joiner isn't the one who can fix it). Capped, best-effort: a failed write
/// here must never be the reason a join itself fails.
pub fn record_blocked_join(store: &Store, workspace_id: &str, principal: &Principal, now_ts: f64) {
    let rec = store.kv_get("join_blocked", workspace_id);
    let mut attempts: Vec<Value> = rec
        .as_ref()
        .and_then(|r| r.get("attempts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    attempts.push(json!({"uid": principal.uid, "email": principal.email, "ts": now_ts}));
    let start = attempts.len().saturating_sub(20);
    let rec = json!({"attempts": attempts[start..].to_vec()});
    let _ = store.kv_put("join_blocked", workspace_id, &rec, now_ts);
    // an upgrade trigger: the owner's plan turned someone away
    let owner = get(store, workspace_id).map(|ws| text(&ws, "owner")).unwrap_or_default();
    // the count only: the person turned away is not the one being tracked
    crate::analytics::capture(&owner, "seats_full", workspace_id, json!({"attempts": attempts.len()}));
}

/// Recent joins that bounced off the seat cap — EXCLUDING anyone who has
/// since made it in (a resolved bounce is history, not pressure).
pub fn blocked_joins(store: &Store, workspace_id: &str, within_s: f64) -> Vec<Value> {
    let rec = store.kv_get("join_blocked", workspace_id).unwrap_or(json!({}));
    let cutoff = now() - within_s;
    let current: BTreeSet<String> = members(store, workspace_id).iter().map(|m| text(m, "uid")).collect();
    rec.get("attempts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| num(a, "ts") >= cutoff && !current.contains(&text(a, "uid")))
        .collect()
}

// ------------------------------------------------------------ repos in a workspace

/// Repo ids with any activity — persisted trees plus, when
/// `include_live_intents`, in-flight intents — plus explicitly watched repos,
/// so a freshly watched repo with no reports yet still shows as connected
/// after a reload. A repo moved to another workspace is hidden here even
/// though its pre-move history stays under the old scope.
///
/// `include_live_intents` mirrors Python's `storage` parameter: `app.py`
/// always passes a live storage handle (`true`); `redeem_invite`'s seat count
/// passes `None` (`false`) — a repo known only through an uncommitted intent
/// has no proof any seat is actually used on it yet.
pub fn repos(store: &Store, workspace_id: &str, include_live_intents: bool) -> Vec<String> {
    let prefix = format!("{workspace_id}:");
    let mut found: BTreeSet<String> = BTreeSet::new();
    for scope in store.list_scopes(&prefix) {
        found.insert(scope[prefix.len()..].to_string());
    }
    for (_, rec) in store.kv_list("ghrepo", &prefix) {
        let watched = text(&rec, "repo_id").trim().to_string();
        if !watched.is_empty() {
            found.insert(watched);
        }
    }
    if include_live_intents {
        let intent_prefix = format!("intent:{workspace_id}:");
        for (key, _) in store.eph_scan(&intent_prefix) {
            // key = "intent:<ws>:<repo>:<intent_id>"
            let middle = &key[intent_prefix.len()..];
            let repo = match middle.rfind(':') {
                Some(idx) => &middle[..idx],
                None => middle,
            };
            if !repo.is_empty() {
                found.insert(repo.to_string());
            }
        }
    }
    let moved: BTreeSet<String> = store
        .kv_list("repo_moved", &prefix)
        .into_iter()
        .map(|(_, rec)| repo_key(&text(&rec, "repo_id")))
        .collect();
    found.into_iter().filter(|r| !moved.contains(&repo_key(r))).collect()
}
