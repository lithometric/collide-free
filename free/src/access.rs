//! Membership is workspace-wide; ACCESS is per repo.
//!
//! There is no ghost mode. The rest of the team reports on a repo through
//! Collide, so working on it unobserved is exactly what Collide exists to
//! prevent — which is why a refusal here is a refusal to record, not a
//! refusal to read.
//!
//! Fails OPEN on infrastructure trouble and CLOSED only on an explicit
//! denial, matching the Python original: a GitHub lookup that cannot run
//! must not lock a legitimate member out of reporting their work.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::auth::member;
use crate::billing::{self, Limit, PlanDef, PLANS};
use crate::presence::agent_id;
use crate::repo::repo_key;
use crate::store::{now, Store};

// ---- the plan gate: what a plan sells, said once, the same on both halves ----
//
// "Gate on how much you use it and who you use it with, never on how well it
// works." Every notice below names a count or a window and the billing page;
// none of them ever stops a write. The strings are byte-identical to the
// Python half's (service.py) — an agent reads whichever server answered.

/// A VIEWER (effective access "read": a member on Free, or one the owner
/// set to read on a live plan) sees the dashboard; their agents get
/// nothing. The refusal is the shape `enforce_repo_access` already uses — a
/// refusal to RECORD, never a refusal to read — plus this one plain line,
/// which names the room, who IS covered and who to ask. `{covered}` is
/// "{owner} and {players} are" or, when the owner is the only player,
/// "{owner} is" (see [`viewer_notice`]).
pub const VIEWER_NOTICE: &str = "Collide can't see this because you don't have a seat in {workspace}. \
{covered} covered, you aren't: nothing you write is recorded, briefed or collision-checked. Ask {owner} for \
a seat at {billing_url}.";
/// The player-side half of the same fact, told once per session to every
/// player's agent while the workspace has viewers: the teammates whose
/// work Collide cannot see, and where to add them. `{are}`/`{viewers}`
/// follow the count: "bob is … They're a viewer" for one, "erin and frank
/// are … They're viewers" for more.
pub const VIEWERS_NUDGE: &str = "Collide can't see what {names} {are} doing in this repo. They're {viewers}, \
so their agents don't report, and their edits won't show up in your collision checks. Add them at \
{billing_url}.";
/// How many people a notice names before "and N more".
pub const NAMES_SHOWN: usize = 5;
/// The fourth agent at once on Free: recorded and on the dashboard (the
/// owner must SEE it — that is the account-sharing lockdown), gated as the
/// gate gates everyone, but briefed nothing until another agent finishes.
pub const AGENT_CAP_NOTICE: &str = "{cap} agents at once is the {plan} plan's limit — this one is recorded \
and shows on the dashboard, but gets no briefing, notes or recipes until another of your agents finishes. \
Tell the human: unlimited agents at once is Team, at {billing_url}";
pub const MESSAGING_NOTICE: &str = "messaging reaches only your own agents on the {plan} plan — a message to \
{to}'s agents is Team. Tell the human: the owner can move up in about a minute at {billing_url}";
pub const COMPLIANCE_NOTICE: &str = "the compliance export (the full ledger of every agent action, \
hash-chained) is Business and up — this workspace is on {plan}. The owner can move up at {billing_url}";
/// How long "this session was told" and "this session is over the cap"
/// stand: a session is a conversation, and a conversation rarely outlives
/// an hour of silence; after that the count is taken afresh.
pub const TOLD_ONCE_TTL_S: f64 = 6.0 * 3600.0;
/// The viewers nudge is an upsell, not news: once a week per person per
/// workspace, whatever the number of sessions in between.
pub const VIEWERS_NUDGE_EVERY_S: f64 = 7.0 * 86_400.0;
pub const AGENT_CAP_TTL_S: f64 = 3600.0;

/// The billing page every notice points at — the dashboard origin plus
/// /dashboard/billing, the string `BillingService._billing_url` builds. The
/// Python app hands its service the ORIGIN of COLLIDE_DASHBOARD_URL; the
/// Rust `App` carries the raw value, so it is reduced to the origin here.
pub fn billing_url(dashboard_url: &str) -> String {
    let trimmed = dashboard_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "/dashboard/billing".to_string();
    }
    let origin = match trimmed.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.split('/').next().unwrap_or("")),
        None => trimmed.to_string(),
    };
    format!("{origin}/dashboard/billing")
}

/// The workspace's seat roster: its display name, the owner, the other
/// PLAYERS (effective access "write") and the VIEWERS, in member order.
/// Python's `_seat_roster`, member for member.
pub struct Roster {
    pub name: String,
    pub owner: Option<Value>,
    pub players: Vec<Value>,
    pub viewers: Vec<Value>,
}

/// The identity string a member's agents report under — the email, or the
/// uid when the account has none — the same value `Principal::label` gives
/// a token and `recent_events[].user` carries.
pub fn member_label(member: &Value) -> String {
    let email = member.get("email").and_then(Value::as_str).unwrap_or("").trim();
    if !email.is_empty() {
        return email.to_string();
    }
    member.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string()
}

pub fn seat_roster(store: &Store, workspace_id: &str) -> Roster {
    let plan = billing::plan_of(store, workspace_id);
    let name = crate::workspaces::get(store, workspace_id)
        .and_then(|ws| ws.get("name").and_then(Value::as_str).map(|n| n.trim().to_string()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| workspace_id.to_string());
    let mut roster = Roster { name, owner: None, players: Vec::new(), viewers: Vec::new() };
    for member in crate::workspaces::members(store, workspace_id) {
        if member.get("role").and_then(Value::as_str) == Some("owner") {
            roster.owner = Some(member);
        } else if billing::effective_access(&plan, &member) == "write" {
            roster.players.push(member);
        } else {
            roster.viewers.push(member);
        }
    }
    roster
}

/// Display names for a notice, in the order `identity_labels` resolves
/// them: the caller's own nickname for the person, else their claimed
/// username, else the identity itself (the email). Nicknames are private to
/// `caller`, so two people reading the same notice may see different names.
pub fn display_names(store: &Store, caller: &str, users: &[String]) -> Vec<String> {
    let nicks = store.kv_get("nicknames", caller).unwrap_or(Value::Null);
    users
        .iter()
        .map(|user| {
            let nick = nicks.get(user).and_then(Value::as_str).unwrap_or("").trim().to_string();
            if !nick.is_empty() {
                return nick;
            }
            let username = store
                .kv_get("username_by_email", user)
                .and_then(|claim| claim.get("username").and_then(Value::as_str).map(|u| u.trim().to_string()))
                .unwrap_or_default();
            if !username.is_empty() { username } else { user.clone() }
        })
        .collect()
}

/// "a, b and c" — up to [`NAMES_SHOWN`] names, the last joined with "and";
/// past that, "a, b, c, d, e and N more" (commas only, so the two "and"s
/// never meet). Python's `name_list`.
pub fn name_list(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(NAMES_SHOWN).map(String::as_str).collect();
    if names.len() > NAMES_SHOWN {
        return format!("{} and {} more", shown.join(", "), names.len() - NAMES_SHOWN);
    }
    match shown.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => shown.join(", "),
    }
}

/// The viewer's line, for `user` (the viewer, whose nicknames name the
/// others): the workspace by name, the owner and the other players as the
/// ones covered, the owner as the one to ask. Python's `viewer_notice`.
pub fn viewer_notice(store: &Store, workspace_id: &str, user: &str, dashboard_url: &str) -> String {
    let roster = seat_roster(store, workspace_id);
    let owner_id = roster.owner.as_ref().map(member_label).unwrap_or_default();
    let owner = if owner_id.is_empty() {
        "the workspace owner".to_string()
    } else {
        display_names(store, user, &[owner_id]).remove(0)
    };
    let players: Vec<String> = roster.players.iter().map(member_label).filter(|p| !p.is_empty()).collect();
    let covered = if players.is_empty() {
        format!("{owner} is")
    } else {
        let mut all = vec![owner.clone()];
        all.extend(display_names(store, user, &players));
        format!("{} are", name_list(&all))
    };
    VIEWER_NOTICE
        .replace("{workspace}", &roster.name)
        .replace("{covered}", &covered)
        .replace("{owner}", &owner)
        .replace("{billing_url}", &billing_url(dashboard_url))
}

/// The player-side nudge, once a week per person (`viewersnudge:{ws}:{user}`,
/// VIEWERS_NUDGE_EVERY_S) while the workspace has viewers; `None` otherwise.
/// The stamp lands on the first check, so a workspace without viewers costs
/// one ephemeral read a week, not a member walk per call. Never blocks
/// anything: it rides a response the agent already receives. Python's
/// `viewers_nudge`.
pub fn viewers_nudge(store: &Store, workspace_id: &str, user: &str, session: &str, dashboard_url: &str) -> Option<String> {
    let _ = session;
    let key = format!("viewersnudge:{workspace_id}:{user}");
    if !tell_once(store, &key, VIEWERS_NUDGE_EVERY_S) {
        return None;
    }
    let roster = seat_roster(store, workspace_id);
    let viewers: Vec<String> = roster.viewers.iter().map(member_label).filter(|v| !v.is_empty()).collect();
    if viewers.is_empty() {
        return None;
    }
    let names = display_names(store, user, &viewers);
    let one = names.len() == 1;
    Some(
        VIEWERS_NUDGE
            .replace("{names}", &name_list(&names))
            .replace("{are}", if one { "is" } else { "are" })
            .replace("{viewers}", if one { "a viewer" } else { "viewers" })
            .replace("{billing_url}", &billing_url(dashboard_url)),
    )
}

/// The plan a workspace is on, resolved through the same alias-aware lookup
/// billing.rs uses for every read (`plan_of_record`, which is private there:
/// a retired name lands on the tier it maps to, anything unknown on Free).
pub struct Plan {
    pub def: &'static PlanDef,
}

fn plan_def_of(record: &Value) -> &'static PlanDef {
    let name = record.get("plan").and_then(Value::as_str).unwrap_or("free");
    let name = match name {
        "pro" => "team",
        other => other,
    };
    PLANS
        .iter()
        .find(|plan| plan.name == name)
        .or_else(|| PLANS.iter().find(|plan| plan.name == "free"))
        .expect("free plan always defined")
}

impl Plan {
    /// A count limit: `None` for unmetered, for a missing key, and for a
    /// limit that is a word or a flag.
    pub fn count(&self, key: &str) -> Option<i64> {
        match self.def.limits.iter().find(|(k, _)| *k == key) {
            Some((_, Limit::Count(value))) => *value,
            _ => None,
        }
    }

    pub fn flag(&self, key: &str, default: bool) -> bool {
        match self.def.limits.iter().find(|(k, _)| *k == key) {
            Some((_, Limit::Flag(value))) => *value,
            _ => default,
        }
    }

    pub fn word(&self, key: &str, default: &'static str) -> &'static str {
        match self.def.limits.iter().find(|(k, _)| *k == key) {
            Some((_, Limit::Text(value))) => value,
            _ => default,
        }
    }

    /// ledger_days as a read window in seconds; `None` = unlimited.
    pub fn history_window_s(&self) -> Option<f64> {
        self.count("ledger_days").map(|days| days as f64 * 86_400.0)
    }
}

/// `plan_of`'s answer plus its table entry: a lapsed trial already reads as
/// Free. (billing::plan_of never fails — a store it cannot read answers the
/// Free default — so the Python "billing unreadable → unlimited" branch has
/// no Rust twin to fail open through.)
pub fn plan_for(store: &Store, workspace_id: &str) -> Plan {
    Plan { def: plan_def_of(&billing::plan_of(store, workspace_id)) }
}

/// `billing::effective_access` for this member on this workspace's live plan
/// — "write" (a player) or "read" (a viewer). Fails CLOSED: no member is a
/// viewer.
pub fn access_of(store: &Store, workspace_id: &str, member: Option<&Value>) -> &'static str {
    let plan = billing::plan_of(store, workspace_id);
    billing::effective_access(&plan, member.unwrap_or(&Value::Null))
}

/// The days a caller asked for, no more than the plan's window.
pub fn clamp_days(days: f64, window_s: Option<f64>) -> f64 {
    match window_s {
        Some(window) => days.min(window / 86_400.0),
        None => days,
    }
}

pub fn clamp_since(since_s: f64, window_s: Option<f64>) -> f64 {
    match window_s {
        Some(window) => since_s.min(window),
        None => since_s,
    }
}

/// The oldest timestamp a read may reach on this plan; 0 = all of history.
pub fn oldest_ts(plan: &Plan) -> f64 {
    plan.history_window_s().map(|window| now() - window).unwrap_or(0.0)
}

/// True the first time a notice is asked about under `key`, false after —
/// the server-side told-once stamp (an ephemeral row, the mechanism
/// workspace_hint and workspace_choice_notice use). Fails to true: better
/// twice than never.
pub fn tell_once(store: &Store, key: &str, ttl_s: f64) -> bool {
    if store.eph_get(key).is_some() {
        return false;
    }
    let _ = store.eph_set(key, &json!({"ts": now()}), Some(ttl_s));
    true
}

/// `(over the cap, the notice to carry — "" once this session was told)`.
///
/// agents_concurrent counts a player's LIVE agents: the distinct
/// `focus:{ws}:{user}#…` markers that carry `repo_id` (the per-agent format
/// the hooks and the MCP session write). A session with no marker yet that
/// arrives when the count is at the cap is over it, and stays over it for
/// AGENT_CAP_TTL_S — its own reports still land and the owner SEES the extra
/// agent on Now; what it does not get is the briefing. Python's
/// `agent_cap_state`, step for step.
pub fn agent_cap_state(store: &Store, workspace_id: &str, user: &str, session: &str, dashboard_url: &str) -> (bool, String) {
    let plan = plan_for(store, workspace_id);
    let Some(cap) = plan.count("agents_concurrent") else { return (false, String::new()) };
    let agent = agent_id(user, session);
    let stamp_key = format!("agentcap:{workspace_id}:{agent}");
    let notice = AGENT_CAP_NOTICE
        .replace("{cap}", &cap.to_string())
        .replace("{plan}", plan.def.label)
        .replace("{billing_url}", &billing_url(dashboard_url));
    if let Some(mut stamp) = store.eph_get(&stamp_key) {
        if crate::compat::truthy(stamp.get("told")) {
            return (true, String::new());
        }
        if let Some(map) = stamp.as_object_mut() {
            map.insert("told".into(), json!(true));
        }
        let _ = store.eph_set(&stamp_key, &stamp, Some(AGENT_CAP_TTL_S));
        return (true, notice);
    }
    if store.eph_get(&format!("focus:{workspace_id}:{agent}")).is_some() {
        return (false, String::new()); // one of the counted agents
    }
    let live = store
        .eph_scan(&format!("focus:{workspace_id}:{user}#"))
        .into_iter()
        .filter(|(_, marker)| marker.get("repo_id").map(|r| !r.is_null()).unwrap_or(false))
        .count();
    if (live as i64) < cap {
        return (false, String::new());
    }
    let _ = store.eph_set(&stamp_key, &json!({"ts": now(), "told": true}), Some(AGENT_CAP_TTL_S));
    (true, notice)
}

/// Whether a note, scar, rationale or recipe is the caller's OWN. Notes and
/// rationales carry `owner` (memory::save: the saver's user id); recipes
/// carry `by`; a mined scar's `owner` is the miner ("collide"), so it is the
/// caller's when they were the intent's owner (`intent_owner`) or the one
/// who reverted it (`reverted_by`). Anything unattributable is not own.
pub fn own_knowledge(record: &Value, user: &str) -> bool {
    if user.is_empty() {
        return false;
    }
    ["owner", "by", "intent_owner", "reverted_by"]
        .iter()
        .any(|key| record.get(key).and_then(Value::as_str) == Some(user))
}

/// Whether teammates' notes, scars, rationales and recipes reach this
/// caller (limits.knowledge_sharing).
pub fn knowledge_shared(store: &Store, workspace_id: &str) -> bool {
    plan_for(store, workspace_id).flag("knowledge_sharing", true)
}

/// limits.messaging "own": a message may only address the sender's own
/// agents (recipient user == sender). The notice names Team.
pub fn messaging_refusal(store: &Store, workspace_id: &str, sender: &str, to: &str, dashboard_url: &str) -> Option<String> {
    let plan = plan_for(store, workspace_id);
    if plan.word("messaging", "team") != "own" {
        return None;
    }
    if to.trim().to_lowercase() == sender.trim().to_lowercase() {
        return None;
    }
    Some(
        MESSAGING_NOTICE
            .replace("{plan}", plan.def.label)
            .replace("{to}", to.trim())
            .replace("{billing_url}", &billing_url(dashboard_url)),
    )
}

/// limits.messaging "own": teammates' messages stay in the box unread — the
/// sender was told at send time; a message that landed while the plan was
/// live simply waits for the plan. Python's `inbox`, filter for filter.
pub fn own_messages_only(store: &Store, workspace_id: &str, user_id: &str, mut inbox: Value, dashboard_url: &str) -> Value {
    let plan = plan_for(store, workspace_id);
    if plan.word("messaging", "team") != "own" {
        return inbox;
    }
    let Some(map) = inbox.as_object_mut() else { return inbox };
    let messages = map.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let mine = user_id.trim().to_lowercase();
    let kept: Vec<Value> = messages
        .iter()
        .filter(|m| m.get("from").and_then(Value::as_str).unwrap_or("").trim().to_lowercase() == mine)
        .cloned()
        .collect();
    let withheld = messages.len() - kept.len();
    map.insert("messages".into(), Value::Array(kept));
    if withheld > 0 {
        map.insert("withheld".into(), json!(withheld));
        map.insert("notice".into(), json!(MESSAGING_NOTICE
            .replace("{plan}", plan.def.label)
            .replace("{to}", "a teammate")
            .replace("{billing_url}", &billing_url(dashboard_url))));
    }
    inbox
}

/// The notice below Business (limits.compliance_export false); `None` when
/// the export is allowed.
pub fn compliance_refusal(store: &Store, workspace_id: &str, dashboard_url: &str) -> Option<Value> {
    let plan = plan_for(store, workspace_id);
    if plan.flag("compliance_export", true) {
        return None;
    }
    let url = billing_url(dashboard_url);
    Some(json!({
        "ok": false, "gated": "compliance_export", "plan": plan.def.label,
        "notice": COMPLIANCE_NOTICE.replace("{plan}", plan.def.label).replace("{billing_url}", &url),
        "billing": url,
    }))
}

/// `(players, viewers)`: members whose effective access is "write", and
/// the rest. A Free workspace with four viewers is 1 player.
pub fn player_counts(store: &Store, workspace_id: &str) -> (usize, usize) {
    let plan = billing::plan_of(store, workspace_id);
    let members = crate::workspaces::members(store, workspace_id);
    let players = members.iter().filter(|m| billing::effective_access(&plan, m) == "write").count();
    (players, members.len() - players)
}

/// The receipt's seat phrase: "seats 1/1 · 4 viewers" on a capped plan,
/// "3 players · 1 viewer" on an uncapped one.
pub fn seats_line(players: usize, viewers: usize, limit: Option<i64>) -> String {
    let mut line = match limit {
        Some(limit) => format!("seats {players}/{limit}"),
        None => format!("{players} player{}", if players != 1 { "s" } else { "" }),
    };
    if viewers > 0 {
        line.push_str(&format!(" · {viewers} viewer{}", if viewers != 1 { "s" } else { "" }));
    }
    line
}

pub enum Access {
    Allowed,
    Denied(String),
}

/// Owner and "*" members see every repo; others only their list. A member
/// record without `repos` predates per-repo access and keeps full access, so
/// nothing regresses for existing teams.
pub fn has_repo_access(member: Option<&Value>, repo_id: &str) -> bool {
    let Some(member) = member else { return false };
    if member.get("role").and_then(Value::as_str) == Some("owner") {
        return true;
    }
    match member.get("repos") {
        None | Some(Value::Null) => true,
        Some(Value::String(all)) if all == "*" => true,
        Some(Value::Array(repos)) => {
            let wanted = repo_key(repo_id);
            repos.iter().any(|entry| {
                let text = entry.as_str().map(str::to_string).unwrap_or_else(|| entry.to_string());
                repo_key(&text) == wanted
            })
        }
        _ => true,
    }
}

/// The subset of `<workspace>:<repo>` scopes this member may read.
///
/// Membership is workspace-wide but ACCESS is per repo, so anything that fans
/// OUT across a workspace's scopes — recall today, more coming — has to put
/// every contributing scope through the same allowlist a direct call to that
/// repo would hit. Without it a workspace-wide read is a side door into the
/// repos an invite deliberately excludes.
pub fn scopes_visible_to<I, S>(member: Option<&Value>, scopes: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    scopes
        .into_iter()
        .filter(|scope| {
            // "<ws>:<repo>"; a scope with no separator has no repo half, and
            // Python's str.partition(":")[2] reads that as the empty repo too
            let repo = scope.as_ref().split_once(':').map(|(_ws, repo)| repo).unwrap_or("");
            has_repo_access(member, repo)
        })
        .map(|scope| scope.as_ref().to_string())
        .collect()
}

/// THE permission chokepoint for every workspace-wide read.
///
/// The workspace is the unit of people, the repo the unit of code — so
/// presence, recipes and recall read across a whole workspace, and every one
/// of those reads must start here: the scopes this member may see, no more.
/// Callers take the result as a NON-OPTIONAL argument, which turns "forgot
/// the allowlist" from a data leak into a compile error.
///
/// Scopes come from the scope table (`shared_trees`, the table `delete_scope`
/// actually clears), never from a bucket's own key prefix: removal leaves
/// bucket rows behind, so enumerating from them would resurrect removed
/// repos. A scope with no tree yet is therefore invisible here even to its
/// own members — the caller adds the one scope it was itself authorised for.
pub fn visible_scopes(store: &Store, workspace_id: &str, member: Option<&Value>) -> BTreeSet<String> {
    scopes_visible_to(member, store.list_scopes(&format!("{workspace_id}:")))
}

/// "ok" | "removed" | "unverified" | "off".
///
/// Only the cached collaborator list is consulted — this half never calls
/// GitHub. A cold cache reads as "off", which is the same fail-open answer
/// the Python side gives when the lookup cannot run.
fn github_verdict(store: &Store, workspace_id: &str, repo_id: &str, uid: &str) -> &'static str {
    let Some(integration) = store.kv_get("ghintegration", workspace_id) else { return "off" };
    if integration.get("enabled").and_then(Value::as_bool) == Some(false) {
        return "off";
    }
    let login = store
        .kv_get("profile", uid)
        .and_then(|profile| {
            profile.get("github_login").and_then(Value::as_str).map(|s| s.trim().to_lowercase())
        })
        .unwrap_or_default();
    if login.is_empty() {
        return "unverified";
    }
    let cached = store.eph_get(&format!("ghcollab:{workspace_id}:{}", repo_key(repo_id)));
    let Some(cached) = cached else { return "off" };
    let Some(logins) = cached.get("logins").and_then(Value::as_array) else { return "off" };
    let present = logins
        .iter()
        .filter_map(Value::as_str)
        .any(|candidate| candidate.trim().to_lowercase() == login);
    if present { "ok" } else { "removed" }
}

/// The invite's half of the rule. The PLAN's half — a member whose
/// EFFECTIVE access (`access_of`: the stored toggle read through the live
/// plan) is "read" is a viewer whose agents may look but never record — is
/// computed once per request beside this, in `authorize_full`
/// (`Caller::read_only`), and read by every endpoint that would write.
pub fn enforce_repo_access(store: &Store, workspace_id: &str, uid: &str, repo_id: &str) -> Access {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() || uid.is_empty() {
        return Access::Allowed;
    }
    let record = member(store, workspace_id, uid);
    let Some(record) = record.as_ref() else {
        return Access::Denied(format!(
            "you are not a member of the workspace that owns '{repo_id}'; ask its owner for an \
invite — without one you cannot see or report on your teammates' work"
        ));
    };
    if !has_repo_access(Some(record), repo_id) {
        let name = store
            .kv_get("workspace", workspace_id)
            .and_then(|ws| ws.get("name").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| workspace_id.to_string());
        return Access::Denied(format!(
            "no access to '{repo_id}' in workspace '{name}': your invite covers other repos. \
There is no ghost mode — the rest of the team reports on this repo through Collide, so working \
on it unobserved isn't available. Tell the human to ask the workspace owner to check this repo \
off on their access list (dashboard Settings > Members)."
        ));
    }
    if github_verdict(store, workspace_id, repo_id, uid) == "removed" {
        return Access::Denied(format!(
            "your GitHub account is no longer a collaborator on '{repo_id}', so Collide will not \
record work on it — nothing you edit locally appears to the team. Tell the human: regain access \
on GitHub (or have the owner reconnect), then retry."
        ));
    }
    Access::Allowed
}
