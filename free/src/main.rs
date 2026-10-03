//! Collide's server, in Rust.
//!
//! This is a migration in progress, not a finished replacement. The strategy
//! is a strangler: the Rust server speaks to the SAME SQLite database with the
//! same schema, key shapes and JSON encodings, so endpoints can move across
//! one at a time while both halves run. Nothing is cut over until a
//! differential test proves the Rust path produces byte-identical state.
//!
//! What is here now:
//!   * the three storage tiers, schema-compatible with the Python driver
//!   * the ledger's hash chain and the merkle tree, which MUST agree byte for
//!     byte or the record reads as tampered and collision checks go wrong
//!   * `/health`
//!   * `parity`, a self-check the Python test suite drives to prove the above
//!
//! Since then the report pipeline, MCP, OAuth, billing, the dashboard routes
//! and the first-call auto-bind have moved across behind the same proof. What
//! is not here yet: per-call workspace ROUTING on the hot path (Python's
//! `workspace_for`; `authorize_full` still takes the credential's workspace)
//! and the live `tools/list_changed` notification.

// Without the cloud, what only the cloud calls is unused; the cloud build
// still checks it.
#![cfg_attr(not(feature = "cloud"), allow(dead_code))]

mod access;
mod overlap;
mod examples;
#[cfg(feature = "cloud")]
mod selfseat;
#[cfg(not(feature = "cloud"))]
#[path = "free/selfseat.rs"]
mod selfseat;
mod freshness;
#[cfg(feature = "cloud")]
mod vectors;
#[cfg(not(feature = "cloud"))]
#[path = "free/vectors.rs"]
mod vectors;
#[cfg(feature = "cloud")]
mod engine;
#[cfg(not(feature = "cloud"))]
#[path = "free/engine.rs"]
mod engine;
#[cfg(feature = "cloud")]
mod s3;
#[cfg(not(feature = "cloud"))]
#[path = "free/s3.rs"]
mod s3;
mod routestats;
#[cfg(feature = "cloud")]
mod routes_account;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_account.rs"]
mod routes_account;
#[cfg(feature = "cloud")]
mod routes_observe;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_observe.rs"]
mod routes_observe;
#[cfg(feature = "cloud")]
mod routes_workspaces;
#[cfg(not(feature = "cloud"))]
#[path = "free/routes_workspaces.rs"]
mod routes_workspaces;
mod watch;
mod agenttools;
#[cfg(feature = "cloud")]
mod crm;
#[cfg(not(feature = "cloud"))]
#[path = "free/crm.rs"]
mod crm;
mod blocks;
#[cfg(feature = "cloud")]
mod oauth;
#[cfg(not(feature = "cloud"))]
#[path = "free/oauth.rs"]
mod oauth;
mod workspaces;
mod activity;
#[cfg(feature = "cloud")]
mod admin;
#[cfg(not(feature = "cloud"))]
#[path = "free/admin.rs"]
mod admin;
mod claims;
#[cfg(feature = "cloud")]
mod setupone;
#[cfg(not(feature = "cloud"))]
#[path = "free/setupone.rs"]
mod setupone;
mod advisory;
#[cfg(feature = "cloud")]
mod artifacts;
#[cfg(not(feature = "cloud"))]
#[path = "free/artifacts.rs"]
mod artifacts;
mod traffic;
mod replay;
#[cfg(feature = "cloud")]
mod foresight;
#[cfg(not(feature = "cloud"))]
#[path = "free/foresight.rs"]
mod foresight;
mod auth;
#[cfg(feature = "cloud")]
mod analytics;
#[cfg(not(feature = "cloud"))]
#[path = "free/analytics.rs"]
mod analytics;
#[cfg(feature = "cloud")]
mod email;
#[cfg(not(feature = "cloud"))]
#[path = "free/email.rs"]
mod email;
#[cfg(feature = "cloud")]
mod billing;
#[cfg(not(feature = "cloud"))]
#[path = "free/billing.rs"]
mod billing;
mod brief;
mod briefstat;
mod check;
mod deltas;
#[cfg(feature = "cloud")]
mod embed;
#[cfg(not(feature = "cloud"))]
#[path = "free/embed.rs"]
mod embed;
mod inflight;
mod land;
#[cfg(feature = "cloud")]
mod ratelimit;
#[cfg(not(feature = "cloud"))]
#[path = "free/ratelimit.rs"]
mod ratelimit;
#[cfg(feature = "cloud")]
mod sharing;
#[cfg(not(feature = "cloud"))]
#[path = "free/sharing.rs"]
mod sharing;
mod recipes;
mod briefing;
mod codegraph;
mod graphview;
mod compat;
mod collisions;
mod diff;
mod envelope;
mod events;
mod gate;
mod hashing;
mod insights;
mod intents;
mod knows;
mod lint;
mod local;
#[cfg(feature = "cloud")]
mod machines;
#[cfg(not(feature = "cloud"))]
#[path = "free/machines.rs"]
mod machines;
#[cfg(feature = "cloud")]
mod downloads;
#[cfg(not(feature = "cloud"))]
#[path = "free/downloads.rs"]
mod downloads;
mod mcp;
mod memory;
mod merge;
mod operations;
mod presence;
mod rename;
mod repo;
mod report;
mod semantics;
mod store;
mod supervise;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use repo::Aliases;
use store::Store;

pub(crate) struct App {
    /// Shared with the OAuth router, which the SDK builds as its own axum
    /// tree: one connection, one ephemeral-wipe-at-startup, one truth.
    pub(crate) store: Arc<Store>,
    pub(crate) aliases: Aliases,
    pub(crate) started: std::time::Instant,
    pub(crate) dashboard_url: String,
    /// The address this server is reachable at, and the one agents connect
    /// to for MCP — the setup artifacts embed both, so they must be what a
    /// client can actually dial rather than what the process bound.
    pub(crate) public_url: String,
    pub(crate) mcp_url: String,
    /// How dashboard callers are identified: test tokens under the suite,
    /// Firebase ID tokens everywhere else.
    pub(crate) dashboard_auth: auth::DashboardAuth,
    pub(crate) hot_ttl_s: f64,
    pub(crate) idle_after_s: f64,
    pub(crate) intent_ttl_s: f64,
}

pub(crate) fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn number(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Which address this call came in on — the classic api.* host or the mcp.*
/// root, with or without the /mcp suffix. Rides every record so the dashboard
/// can show how an agent is actually reaching the server.
fn via_tag(headers: &HeaderMap, path: &str) -> String {
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    format!("{host} {}", if path.is_empty() { "/" } else { path })
}

/// The shared preamble of every hot endpoint: resolve the credential, confirm
/// membership, resolve the scope. Returns the scope and the caller's display
/// id, or the fail-open body to return instead.
pub(crate) struct Caller {
    pub(crate) scope: String,
    /// The repo id exactly as the request spelled it — what the auto-bind
    /// records (`watch::ensure_watched`), as Python binds `repo_id` raw.
    pub(crate) repo_id: String,
    pub(crate) user_id: String,
    pub(crate) workspace: String,
    /// The credential's workspace allowlist (`Principal.workspaces`) —
    /// empty means every workspace of the account; Python's `auth.workspaces`.
    pub(crate) workspaces: Vec<String>,
    pub(crate) uid: String,
    pub(crate) email: String,
    pub(crate) name: String,
    /// What this member may DO right now, as `billing::effective_access`
    /// answers it — the stored toggle read through the live plan (on Free
    /// every non-owner is a viewer). Computed once per request, here, and
    /// read everywhere a record would be made: a viewer may look but never
    /// record — nor mint, since the setup artifacts embed a credential.
    pub(crate) read_only: bool,
}

/// The viewer refusal, in the shape `authorize_full` refuses any other
/// access denial: the endpoint's fail-open body plus `access_denied` and the
/// one plain line. A refusal to RECORD, never to read.
fn viewer_refusal(app: &App, caller: &Caller, open: &Value) -> Json<Value> {
    let mut body = open.clone();
    if let Some(map) = body.as_object_mut() {
        map.insert("access_denied".into(), json!(true));
        map.insert("reason".into(), json!(viewer_notice(app, caller)));
    }
    Json(body)
}

/// The viewer's one line, addressed to this caller: it names the workspace,
/// who is covered and who to ask, through the caller's own nicknames.
fn viewer_notice(app: &App, caller: &Caller) -> String {
    access::viewer_notice(&app.store, &caller.workspace, &caller.user_id, &app.dashboard_url)
}

/// Python's `set_via`: the address this request came in on, in scope for
/// every ledger row the handler writes (`store::VIA`). Applied to the hot
/// routes — the hooks' and agents' surface — and not to the dashboard's
/// `/api/*`, exactly where Python's `service.set_via` is and is not called.
async fn via_scope(request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let host = request.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let via = format!("{host} {}", if path.is_empty() { "/" } else { path.as_str() });
    let started = std::time::Instant::now();
    let response = store::VIA.scope(via, next.run(request)).await;
    // the route's first segment: /gate, /report, /mcp — never a repo id
    let route = format!("/{}", path.trim_start_matches('/').split('/').next().unwrap_or(""));
    crate::routestats::record(&route, started.elapsed().as_secs_f64() * 1000.0);
    response
}

/// A request answers once the engine's log holds every write it made: the
/// writes themselves return at local commit, and this waits once, here.
async fn durable(State(app): State<Arc<App>>, request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    store::DURABLE
        .scope(std::cell::Cell::new(0), async move {
            let response = next.run(request).await;
            let seq = store::DURABLE.with(|mark| mark.get());
            if seq > 0 {
                if let Some(engine) = app.store.engine() {
                    engine.wait_acked_async(seq).await;
                }
            }
            response
        })
        .await
}

pub(crate) fn authorize_full(
    app: &App, headers: &HeaderMap, body: &Value, open: Value,
) -> Result<Caller, Json<Value>> {
    let header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let Some(principal) = auth::principal_for_bearer(&app.store, header, store::now()) else {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("unauthorized"));
        }
        return Err(Json(body));
    };
    if principal.workspace.is_empty() {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("unauthorized"));
        }
        return Err(Json(body));
    }
    // Route the call to the workspace that watches this repo — an explicit
    // dashboard watch, else the one workspace that already knows it, else
    // the workspace named after its GitHub org — and only then the
    // credential's own (its default: the first workspace ticked at
    // approval). Python's `scope_of`. Without it, one connection granted
    // several workspaces read every repo through its default: an agent in
    // another org's repo got the default's (empty) view, and its first call bound the
    // repo into the default. Candidates are the caller's memberships within the
    // credential's allowlist, so routing never widens what it may touch.
    let repo_for_routing = text(body, "repo_id");
    let workspace = if repo_for_routing.trim().is_empty() {
        principal.workspace.clone()
    } else {
        let routing = auth::AuthUser {
            user_id: principal.label().to_string(),
            workspace: principal.workspace.clone(),
            uid: principal.uid.clone(),
            workspaces: principal.workspaces.clone(),
        };
        watch::workspace_for(&app.store, &app.aliases, &routing, repo_for_routing.trim())
    };
    let Some(member) = auth::member(&app.store, &workspace, &principal.uid) else {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("not a member"));
        }
        return Err(Json(body));
    };
    // the EFFECTIVE access: the owner's toggle read through the live plan
    // (a lapsed trial already reads as Free, where every non-owner is a
    // viewer). Computed once here; every recording endpoint reads it.
    let access = billing::effective_access(&billing::plan_of(&app.store, &workspace), &member);
    let read_only = access == "read";
    let repo_id = text(body, "repo_id");
    if let access::Access::Denied(reason) =
        access::enforce_repo_access(&app.store, &workspace, &principal.uid, &repo_id)
    {
        // an explicit denial is NOT fail-open: it is the one case where the
        // answer is a refusal rather than a shrug
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("access_denied".into(), json!(true));
            map.insert("reason".into(), json!(reason));
        }
        return Err(Json(body));
    }
    // over the plan's rate: the endpoint's fail-open answer, never a block
    if !crate::ratelimit::allow_call(&app.store, &workspace) {
        let mut body = open.clone();
        if let Some(map) = body.as_object_mut() {
            map.insert("degraded".into(), json!(true));
            map.insert("reason".into(), json!("rate_limited"));
        }
        return Err(Json(body));
    }
    let scope = app.aliases.scope_for(&workspace, &repo_id);
    // on one machine every agent is the same person, and everything that
    // tells agents about each other compares people: locally, each agent
    // session is its own identity, so your agents are each other's teammates
    let user_id = if local::active() {
        local::agent_identity(&principal.label(), &text(body, "session"))
    } else {
        principal.label()
    };
    Ok(Caller {
        scope, repo_id, user_id, workspace, read_only, workspaces: principal.workspaces.clone(),
        email: principal.email.clone(), name: principal.name.clone(), uid: principal.uid,
    })
}

/// Python's `scope_of`, second half: the first call IS the watch. Binds the
/// caller's repo to the workspace when nothing has yet (`auto: true`, so a
/// dashboard watch outranks it) and holds the plan's repo cap at that door —
/// `Err` is the refusal payload (`billing::repo_cap_error`'s shape). Called
/// where Python's service methods call `scope_of`: after the identity and
/// access checks, and only for a caller who may RECORD — a viewer's agents
/// record nothing, a binding included.
pub(crate) fn bind_scope(app: &App, caller: &Caller) -> Result<(), Value> {
    bind_scope_as(app, caller, false)
}

/// `bind_scope`, saying whether the call is the explicit way in (`setup`):
/// only that adds a repo to a workspace that already has one.
pub(crate) fn bind_scope_as(app: &App, caller: &Caller, explicit: bool) -> Result<(), Value> {
    if caller.read_only {
        return Ok(());
    }
    watch::ensure_watched(
        &app.store, &app.aliases, &caller.workspace, &caller.repo_id, &caller.scope, &caller.user_id, explicit,
    )
}

/// `authorize_full` plus the auto-bind, for every hot route whose Python
/// twin reaches `scope_of` with no refusal shape of its own: the (N+1)th repo
/// is refused as `open` plus `workspace_full` and the one plain line — the
/// same way this layer already refuses an access denial — and is not
/// recorded, so it is never observed. /gate, /presence and /report answer
/// in the shapes their Python twins do (allow-and-carry, the payload, the
/// payload) and so call `authorize_full` + `bind_scope` themselves, at the
/// point their Python twins reach `scope_of`.
pub(crate) fn authorize_bound(
    app: &App, headers: &HeaderMap, body: &Value, open: Value,
) -> Result<Caller, Json<Value>> {
    let caller = authorize_full(app, headers, body, open.clone())?;
    if let Err(refused) = bind_scope(app, &caller) {
        let mut body = open;
        if let Some(map) = body.as_object_mut() {
            if refused.get("new_repo").and_then(Value::as_bool) == Some(true) {
                // the briefing tells it (brief_endpoint); every other route
                // just declines, so none of them spends the telling
                map.insert("new_repo".into(), json!(true));
                map.insert("stamp".into(), json!(format!(
                    "newrepo:{}:{}:{}", caller.workspace, repo::repo_key(&caller.repo_id), caller.user_id)));
            } else if refused.get("not_shared").and_then(Value::as_bool) == Some(true) {
                map.insert("not_shared".into(), json!(true));
            } else {
                map.insert("workspace_full".into(), json!(true));
            }
            map.insert("reason".into(), refused.get("error").cloned().unwrap_or(Value::Null));
        }
        return Err(Json(body));
    }
    Ok(caller)
}

/// The scopes of the caller's workspace this caller may read — the one
/// argument every workspace-wide read (roster, recipes) takes. Computed from
/// the membership record, never from the request.
pub(crate) fn visible_for(app: &App, caller: &Caller) -> std::collections::BTreeSet<String> {
    let member = auth::member(&app.store, &caller.workspace, &caller.uid);
    access::visible_scopes(&app.store, &caller.workspace, member.as_ref())
}

async fn health(State(app): State<Arc<App>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = app.store.ping();
    let mut body = json!({
        "local": local::active(),
        "local_id": if local::active() { local::instance_id() } else { String::new() },
        "status": if ok { "ok" } else { "degraded" },
        "uptime_s": app.started.elapsed().as_secs_f64(),
        "tiers": {"sqlite": {"driver": "rusqlite", "ok": ok}},
        "engine": "rust",
        "store": app.store.write_stats(),
        "routes": crate::routestats::view(),
    });
    if let Some(state) = crate::embed::health() {
        body["embeddings"] = state;
    }
    // which build answered: a deploy can be checked instead of guessed at
    let commit: String = std::env::var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_default().chars().take(7).collect();
    if !commit.is_empty() {
        body["commit"] = json!(commit);
    }
    // a machine still restoring takes no traffic: Railway keeps the old
    // deployment serving until this says ready
    if let Some(engine) = app.store.engine() {
        body["engine"] = engine.view(&app.store);
        if !engine.ready() {
            body["status"] = json!("restoring");
            return (http::StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
        }
    }
    Json(body).into_response()
}

/// PreToolUse: may this write proceed? Fails open on everything except a
/// certain collision on the exact path.
async fn gate_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"allow": true});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let path = text(&body, "path");
    if path.is_empty() || text(&body, "repo_id").is_empty() {
        return Json(json!({"allow": true, "degraded": true, "reason": "missing repo_id or path"}));
    }
    let session = text(&body, "session");
    if caller.read_only {
        // a viewer's agent: the gate ALLOWS (fail-open contract) and records
        // nothing; the one line rides the first gate of the session and no other
        let mut body = json!({"allow": true, "access_denied": true, "viewer": true});
        let stamp = format!("viewertold:{}:{}", caller.workspace, presence::agent_id(&caller.user_id, &session));
        if access::tell_once(&app.store, &stamp, access::TOLD_ONCE_TTL_S) {
            body["reason"] = json!(viewer_notice(&app, &caller));
        }
        return Json(body);
    }
    // the first call IS the watch — and the (N+1)th repo is not bound, so it
    // is not observed, and never blocked: the gate allows and carries the line
    if let Err(refused) = bind_scope(&app, &caller) {
        let new_repo = refused.get("new_repo").and_then(Value::as_bool) == Some(true);
        let mut answer = json!({"allow": true, "reason": refused.get("error").cloned().unwrap_or(Value::Null)});
        answer[if new_repo { "new_repo" } else { "workspace_full" }] = json!(true);
        return Json(answer);
    }
    let via = via_tag(&headers, "/gate");
    // a new numbered file (a migration, an ADR): the number is claimed for
    // this agent right here, or the write is steered to the next free one
    if let Some(claim) = body.get("claim").filter(|c| c.is_object()) {
        let after = claim.get("after").and_then(Value::as_i64).unwrap_or(0);
        if let Some(reason) = crate::claims::gate_check(
            &app.store, &caller.scope, &caller.user_id, &session, &text(&body, "agent"), &path, after, &via,
        ) {
            return Json(json!({"allow": false, "path": path, "reason": reason, "claim": true}));
        }
    }
    let mut decision = gate::gate(&app.store, &caller.scope, &caller.user_id, &session, &path, &app.dashboard_url, &via).body;
    // a player's agent hears once per session who Collide cannot see —
    // riding the first gate of the session, as the viewer's line does
    if let Some(nudge) = access::viewers_nudge(&app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url) {
        decision["viewers_notice"] = json!(nudge);
    }
    Json(decision)
}

/// PostToolUse read side: where an agent is looking. Never content.
async fn presence_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // a refused call is still a call: counted before the viewer and
    // validation checks, where Python's /presence counts it
    billing::count_call(&app.store, &caller.workspace);
    // presence is a RECORD: a viewer (effective access "read") is refused
    // in the same shape as any other access denial
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    crate::sharing::note(&app.store, &caller.workspace, &caller.user_id, &text(&body, "machine"), crate::store::now());
    crate::freshness::note_checkout(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "checkout"));
    let path = text(&body, "path");
    let action = text(&body, "action");
    // a turn's end: its closing message becomes the note on what it changed
    if action == "settled" && !text(&body, "repo_id").is_empty() {
        if let Err(refused) = bind_scope(&app, &caller) {
            return Json(refused);
        }
        return Json(crate::intents::settle_turn(
            &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "text"), &text(&body, "agent")));
    }
    // a session end names no path: the agent is simply gone
    if text(&body, "repo_id").is_empty() || (path.is_empty() && action != "ended") {
        return Json(json!({"ok": false, "reason": "missing repo_id or path"}));
    }
    // the first call IS the watch — and the (N+1)th repo is refused here,
    // with the payload itself, as Python's `report_presence` unwinds it
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(refused);
    }
    let (scope, user_id) = (caller.scope, caller.user_id);
    presence::note_worker(&app.store, &scope, &user_id, &text(&body, "session"), &text(&body, "worker"), &text(&body, "worker_type"));
    // the hook saw the repo's tests run in the agent's shell, and their verdict
    if let Some(ok) = body.get("tests_ok").and_then(Value::as_bool) {
        crate::examples::note_tests(&app.store, &scope, &user_id, &text(&body, "session"), ok, &path);
    }
    if action == "ended" {
        crate::examples::session_ended(&app.store, &scope, &text(&body, "session"));
        crate::freshness::note_ended(&app.store, &scope, &text(&body, "session"));
    }
    let via = via_tag(&headers, "/presence");
    let mut answer = presence::report_presence(
        &app.store,
        &presence::PresenceInput {
            scope: &scope,
            user_id: &user_id,
            // the hook has always sent this; it names the agent
            session: &text(&body, "session"),
            path: &path,
            action: if action.is_empty() { "reading" } else { &action },
            agent: &text(&body, "agent"),
            model: &text(&body, "model"),
            branch: &text(&body, "branch"),
            tokens: number(&body, "tokens"),
            turn_id: &text(&body, "turn_id"),
            context: number(&body, "input_tokens") + number(&body, "cache_read_tokens") + number(&body, "cache_creation_tokens"),
            via: &via,
        },
    );
    // the hooks post a test verdict, a usage limit and a session's end
    // without reading the answer: a message handed back on those would be
    // marked delivered and never seen, so it waits for a call that prints it
    if !discards_answer(&action, &body) {
        local::attach_inbox(&app.store, &scope, &user_id, &text(&body, "session"), &mut answer);
    }
    Json(answer)
}

/// A presence post whose answer no hook prints.
fn discards_answer(action: &str, body: &Value) -> bool {
    matches!(action, "limit" | "ended") || body.get("tests_ok").is_some()
}

/// Hook traffic proves this user's machine auto-reports: remember it
/// durably, once, so the install nudge never fires for them again. Only the
/// hook paths (/report, /observe) call this — a report made by the model
/// through the MCP tool is exactly the case the nudge exists for.
fn mark_hooks_seen(store: &Store, scope: &str, user_id: &str) {
    let hook_key = format!("{scope}:{user_id}");
    if !compat::truthy(store.kv_get("hookseen", &hook_key).as_ref()) {
        let _ = store.kv_put("hookseen", &hook_key, &json!({"ts": store::now()}), store::now());
    }
}

/// Files the agent LOOKED at, and the tracked tree the session-start index
/// sweeps: folded into the map, nothing else. One file (`path`, `content`)
/// or a batch (`files: [{path, content}]`) so an index of a few hundred
/// modules is a handful of calls. A single read also registers as presence,
/// which is what the read side of the hook posted before it carried content.
fn string_list(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The hook ran the repo's check after a batch of writes: stamp the
/// caller's write records with the result.
async fn verified_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let paths = string_list(&body, "paths");
    let command = text(&body, "command");
    if paths.is_empty() || command.is_empty() {
        return Json(json!({"ok": false, "reason": "missing paths or command"}));
    }
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let agent = {
        let a = text(&body, "agent");
        if a.is_empty() { "collide-report-hook".to_string() } else { a }
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    billing::count_call(&app.store, &caller.workspace);
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    Json(crate::check::stamp_verified(
        &app.store, &caller.scope, &caller.user_id, &agent, &paths, &command, ok, crate::store::now(),
    ))
}

/// The hook ran the dependents' tests for an interface change it reported
/// (the /report response's `verify` plan) and says how they went; the server
/// grades the change on the graph. Fail open.
async fn verify_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let path = text(&body, "path");
    if text(&body, "repo_id").is_empty() || path.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id or path"}));
    }
    let via = via_tag(&headers, "/verify");
    Json(report::record_verification(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &path, &body, app.hot_ttl_s, &via,
    ))
}

/// A failure named files and symbols: who wrote the files, what the
/// symbols really are.
async fn attribute_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let paths = string_list(&body, "paths");
    let symbols = string_list(&body, "symbols");
    let session = text(&body, "session");
    billing::count_call(&app.store, &caller.workspace);
    let answer = crate::check::attribute(
        &app.store, &caller.scope, &caller.user_id, &session, &paths, &symbols, crate::store::now(),
    );
    // the grep companion: the map answered the name the agent searched for
    // with its signature, docstring and callers, in the same step — the read
    // of that file, not sent. One saving per answered grep, not per symbol.
    let answered = answer.get("facts").and_then(Value::as_object).map(|f| f.len()).unwrap_or(0);
    if !symbols.is_empty() && answered > 0 && !caller.read_only {
        let agent = if text(&body, "agent").is_empty() { "collide-report-hook".to_string() } else { text(&body, "agent") };
        crate::briefstat::record_saving(
            &app.store, &caller.scope, "map_answered", &caller.user_id, &agent, &session, 1,
            &json!({"symbols": answered}), crate::store::now(),
        );
    }
    Json(answer)
}

/// What other agents changed in this session's working set since its last
/// step — a few lines the hook prepends to the next event.
async fn deltas_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let session = text(&body, "session");
    let since = body.get("since_ts").and_then(Value::as_f64).unwrap_or(0.0);
    let budget = body.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
    let answer = crate::deltas::deltas(&app.store, &caller.scope, &caller.user_id, &session, since, budget);
    // each change delivered is the re-read that would have caught it, not sent
    let delivered = answer.get("count").and_then(Value::as_u64).unwrap_or(0);
    if delivered > 0 && !caller.read_only {
        let agent = if text(&body, "agent").is_empty() { "collide-report-hook".to_string() } else { text(&body, "agent") };
        crate::briefstat::record_saving(
            &app.store, &caller.scope, "deltas_delivered", &caller.user_id, &agent, &session, delivered,
            &json!({"count": delivered}), crate::store::now(),
        );
    }
    Json(answer)
}

/// `collide-hook login` (the Claude Code plugin): the browser sign-in gave
/// the hook an OAuth token; this turns it into what `setup` would have left
/// on disk for this repo, the repo config and a 90-day hook credential, so
/// the plugin needs no MCP call and no agent in the loop. Viewers get no
/// credential, as with setup.
async fn plugin_connect_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let identity = crate::blocks::HookIdentity {
        auth_uid: &caller.uid, email: &caller.email, name: &caller.name, workspace: &caller.workspace,
    };
    let credentials = crate::blocks::mint_hook_credential(&app.store, &identity, "collide-plugin", &app.public_url);
    crate::blocks::mark_hooks_installed(&app.store, &caller.scope, &caller.user_id);
    let workspace_name = crate::workspaces::get(&app.store, &caller.workspace)
        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    Json(json!({
        "ok": true,
        "config": crate::blocks::collide_config_json(&app.public_url, &caller.repo_id, &caller.workspace),
        "credentials": credentials,
        "user": caller.email, "workspace": caller.workspace, "workspace_name": workspace_name,
    }))
}

/// `collide-hook land`: the push turn (`acquire` / `release`) and the renames
/// that landed (`renames`). See land.rs.
async fn land_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let session = text(&body, "session");
    let holder = format!("{}#{}", caller.user_id, session);
    let now = crate::store::now();
    let agent = if text(&body, "agent").is_empty() { "collide-hook land".to_string() } else { text(&body, "agent") };
    let via = via_tag(&headers, "/land");
    Json(match text(&body, "action").as_str() {
        "acquire" => crate::land::acquire(&app.store, &caller.scope, &caller.user_id, &holder, now),
        "release" => crate::land::release(&app.store, &caller.scope, &caller.user_id, &session, &agent, &holder, &body, now, &via),
        "renames" => json!({"ok": true, "renames": crate::land::renames(&app.store, &caller.scope, now)}),
        _ => json!({"ok": false, "reason": "action is acquire, release or renames"}),
    })
}

/// `collide-hook apply` landed a batch: every file written and verified
/// without the agent reading or editing it, two messages a file less the one
/// command that did it. With a recipe id it was another agent's work replayed.
async fn apply_done_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let n = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000);
    let files = n("files");
    if files == 0 {
        return Json(json!({"ok": false, "reason": "files must be at least 1"}));
    }
    let recipe = text(&body, "recipe");
    let agent = if text(&body, "agent").is_empty() { "collide-hook/apply".to_string() } else { text(&body, "agent") };
    let answer = crate::briefstat::record_saving(
        &app.store, &caller.scope, "batch_applied", &caller.user_id, &agent, &text(&body, "session"), (2 * files).saturating_sub(1).max(1),
        &json!({"files": files, "ops": n("ops"), "recipe": recipe, "replayed": !recipe.is_empty()}), crate::store::now(),
    );
    Json(answer)
}

/// A verified typed-op batch, saved as a recipe the next agent can replay.
async fn recipe_save_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(crate::recipes::save(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &body, crate::store::now()))
}

async fn recipe_get_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let id = text(&body, "id");
    if text(&body, "repo_id").is_empty() || id.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id or id"}));
    }
    // no recipes for a viewer's agents, nor for the agent over the cap: the
    // notice instead of the content, once
    if caller.read_only {
        return Json(json!({"ok": false, "viewer": true, "reason": viewer_notice(&app, &caller)}));
    }
    let (over_cap, cap_notice) = access::agent_cap_state(
        &app.store, &caller.workspace, &caller.user_id, &text(&body, "session"), &app.dashboard_url);
    if over_cap {
        return Json(json!({"ok": false, "over_cap": true, "reason": cap_notice}));
    }
    let visible = visible_for(&app, &caller);
    let share = access::knowledge_shared(&app.store, &caller.workspace);
    Json(crate::recipes::get(&app.store, &caller.scope, &visible, &id, crate::store::now(), &caller.user_id, share))
}

/// Direct callers of a symbol, for expanding a recipe's `$caller` ops.
async fn callers_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let path = text(&body, "path");
    let symbol = text(&body, "symbol");
    if text(&body, "repo_id").is_empty() || path.is_empty() || symbol.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id, path or symbol"}));
    }
    Json(json!({"ok": true, "callers": crate::recipes::callers(&app.store, &caller.scope, &path, &symbol)}))
}

/// A question that names no code and asks for no change is briefed only when
/// its meaning lands this close to real code (the ranked score, the wired
/// bonus included): "why do UK customers pay no VAT?" does; "which database
/// should we use?" does not.
const QUESTION_MEANING_FLOOR: f64 = 0.75;

/// Worked examples in the briefing: OFF unless COLLIDE_EXAMPLES=1. The
/// three-round study (8 new people a round, similar features) showed them
/// firing on the right tasks (7/8 and 8/8 of rounds 2 and 3, none on bug
/// reports, every scored use good) but no clear saving: Collide already
/// brings a small feature to about ten steps, and rounds without them cost
/// the same within noise. Tasks are still recorded, so switching them on
/// starts from the team's history.
fn examples_on() -> bool {
    std::env::var("COLLIDE_EXAMPLES").map(|v| v == "1").unwrap_or(false)
}

/// The session-start briefing the hook prints into the agent's context:
/// recently changed modules as exact facts, under a byte budget.
async fn brief_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(Json(mut response)) => {
            // the hooks print a briefing's text: the question about a repo
            // the workspace lacks reaches the agent at session start or the
            // next prompt, twice a day at most
            let stamp = text(&response, "stamp");
            if let Some(map) = response.as_object_mut() {
                map.remove("stamp");
                if map.get("new_repo").and_then(Value::as_bool) == Some(true)
                    && !stamp.is_empty()
                    && access::tell_once(&app.store, &stamp, 12.0 * 3600.0)
                {
                    let said = map.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
                    map.insert("text".into(), json!(format!("Collide: {said}")));
                }
            }
            return Json(response);
        }
    };
    let repo_id = text(&body, "repo_id");
    if repo_id.is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    let since = body.get("since").and_then(Value::as_f64).unwrap_or(0.0);
    let budget = body.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut identifiers = string_list(&body, "identifiers");
    let exclude = string_list(&body, "exclude");
    let session = text(&body, "session");
    let resume = body.get("resume").and_then(Value::as_bool).unwrap_or(false);
    // the prompt's own words, when the hook sends them: used for this one
    // lookup by meaning (embed.rs) and dropped, never stored or logged
    let prompt = text(&body, "prompt");
    billing::count_call(&app.store, &caller.workspace);
    // a viewer's agents get no briefing: the notice IS the briefing text
    if caller.read_only {
        return Json(json!({"ok": true, "text": viewer_notice(&app, &caller),
                           "modules": 0, "total": 0, "paths": [], "viewer": true}));
    }
    // the fourth agent at once on Free: recorded and on Now, briefed nothing
    // — the notice once, then an empty briefing
    let (over_cap, cap_notice) = access::agent_cap_state(
        &app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url);
    if over_cap {
        return Json(json!({"ok": true, "text": cap_notice, "modules": 0, "total": 0, "paths": [], "over_cap": true}));
    }
    // only the hook asks for this: proof the hooks are live
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    // a prompt just arrived: the agent is running from this moment, before
    // its first tool call (a failed or backgrounded one never reports). The
    // prompt's text stays out of presence; it is the person's.
    if !text(&body, "prompt").trim().is_empty() && !session.is_empty() && !caller.read_only {
        let via = via_tag(&headers, "/brief");
        presence::report_presence(&app.store, &presence::PresenceInput {
            scope: &caller.scope, user_id: &caller.user_id, session: &session, path: "", action: "thinking",
            agent: &text(&body, "agent"), model: &text(&body, "model"), branch: &text(&body, "branch"),
            tokens: 0, turn_id: "", context: 0, via: &via,
        });
    }
    let visible = visible_for(&app, &caller);
    // the recent-writes window never reaches past the plan's history
    let plan = access::plan_for(&app.store, &caller.workspace);
    let since = match plan.history_window_s() {
        Some(window) => {
            let floor = crate::store::now() - window;
            if since > 0.0 { since.max(floor) } else { floor.max(crate::store::now() - 7.0 * 86_400.0) }
        }
        None => since,
    };
    let mut by_meaning: Vec<Value> = Vec::new();
    let mut named_for_overlap: Vec<String> = Vec::new();
    if !prompt.trim().is_empty() {
        let names_code = !identifiers.is_empty();
        let named = identifiers.clone();
        named_for_overlap = named.clone();
        // a repo indexed moments ago is still being embedded: the first
        // prompt of a session arrives in the same second as its index, and
        // matched names only. A short wait gets it the meaning match.
        // matching by meaning (the embedding model) is Team and up; Free
        // matches the names a prompt uses
        let (meant, shown) = if plan.flag("meaning_match", true) {
            let waited = std::time::Instant::now();
            while crate::embed::pending_for(&caller.scope) > 0 && waited.elapsed() < std::time::Duration::from_millis(2500) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            crate::embed::prompt_identifiers(&app.store, &caller.scope, &prompt, 6, Some((&caller.user_id, &session)))
        } else {
            (Vec::new(), Vec::new())
        };
        // a conversation, not a task: no briefing (the agent is still marked working above)
        let closest = shown.first().and_then(|h| h.get("score")).and_then(Value::as_f64).unwrap_or(0.0);
        if !crate::brief::about_the_code(&prompt, names_code, closest >= QUESTION_MEANING_FLOOR) {
            return Json(json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": [], "skipped": "not about the code"}));
        }
        for id in meant {
            if !identifiers.contains(&id) {
                identifiers.push(id);
            }
        }
        by_meaning = shown;
        // a prompt that named nothing and meant nothing gets no briefing, not
        // the session-start one again
        if identifiers.is_empty() {
            return Json(json!({"ok": true, "text": "", "modules": 0, "total": 0, "paths": []}));
        }
    }
    let input = crate::brief::BriefInput {
        repo_id: &repo_id, since, budget, identifiers: &identifiers, exclude: &exclude, session: &session, resume,
        idle_after_s: app.idle_after_s,
        visible: &visible, viewer: &caller.user_id, share_knowledge: plan.flag("knowledge_sharing", true),
    };
    let mut answer = crate::brief::brief(&app.store, &caller.scope, &input);
    if !by_meaning.is_empty() {
        answer["meant"] = json!(by_meaning);
    }
    // two tasks on the same code: say so to both now, before either writes;
    // and a task that ends in a push: how to finish it in one step
    if !prompt.trim().is_empty() {
        let overlap = crate::overlap::announce(&app.store, &caller.scope, &caller.user_id, &session, &named_for_overlap, &by_meaning);
        let adds = crate::brief::adds_code(&prompt);
        let meaning = plan.flag("meaning_match", true);
        let reuse = if adds && meaning { crate::embed::reuse_candidates(&app.store, &caller.scope, &prompt, 3) } else { Vec::new() };
        // a worked example: a finished, tested task like this one. A live
        // teammate in the same code comes first, and then there is none
        let task_q = if meaning { crate::embed::query_vector(&crate::examples::task_text(&prompt)).unwrap_or_default() } else { Vec::new() };
        crate::examples::start_task(&app.store, &caller.scope, &caller.user_id, &session, &task_q, adds);
        let example = if adds && overlap.is_none() && examples_on() {
            let mut points_at: BTreeSet<String> = BTreeSet::new();
            for list in [answer.get("paths"), answer.get("meant")] {
                for v in list.and_then(Value::as_array).into_iter().flatten() {
                    match v.as_str() {
                        Some(p) => { points_at.insert(p.to_string()); }
                        None => { points_at.insert(text(v, "path")); }
                    }
                }
            }
            for helper in &reuse {
                points_at.insert(text(helper, "path"));
                points_at.insert(format!("{}::{}", text(helper, "path"), text(helper, "symbol")));
            }
            let readable: BTreeSet<String> = crate::recipes::readable_scopes(&caller.scope, &visible).into_iter().collect();
            crate::examples::offer(&app.store, &crate::examples::Offer {
                scope: &caller.scope, readable: &readable, user: &caller.user_id, session: &session, q: &task_q,
                points_at: &points_at, history_s: plan.history_window_s(), share_knowledge: plan.flag("knowledge_sharing", true),
            })
        } else {
            None
        };
        let (example_line, example_meta) = match example {
            Some((line, meta)) => (Some(line), Some(meta)),
            None => (None, None),
        };
        if let Some(meta) = example_meta {
            answer["example"] = meta;
        }
        let reuse_line = crate::brief::reuse_line(&reuse);
        if !reuse.is_empty() {
            answer["reuse"] = json!(reuse);
        }
        for line in overlap.into_iter().chain(example_line).chain(reuse_line).chain(crate::brief::finish_line(&prompt).map(str::to_string)) {
            if let Some(text) = answer.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                let mut lines: Vec<&str> = text.lines().collect();
                let last = lines.pop().unwrap_or("");
                let joined = format!("{}\n{line}\n{last}", lines.join("\n"));
                answer["text"] = json!(joined);
            }
        }
    }
    if !identifiers.is_empty() {
        let shown = string_list(&answer, "paths");
        crate::deltas::touch(&app.store, &caller.scope, &session, &shown, crate::store::now());
    }
    // what the hooks never carried before: messages for this person (the
    // MCP tools deliver them on every response, but an agent that only has
    // hooks never saw one), each once per session; and the numbers others
    // have claimed, whenever there is a briefing to put them in
    let mut extra: Vec<String> = Vec::new();
    let mut own = json!({});
    local::attach_inbox(&app.store, &caller.scope, &caller.user_id, &session, &mut own);
    if let Some(note) = own.get("inbox_note").and_then(Value::as_str) {
        extra.push(note.to_string());
    }
    let inbox = if local::active() {
        json!({"messages": []})
    } else {
        let inbox = crate::agenttools::inbox_ack(&app.store, &caller.scope, &caller.user_id, &visible, &[]);
        access::own_messages_only(&app.store, &caller.workspace, &caller.user_id, inbox, &app.dashboard_url)
    };
    let mut told: Vec<String> = Vec::new();
    for message in inbox.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let id = text(message, "id");
        if session.is_empty() || !access::tell_once(&app.store, &format!("briefmsg:{session}:{id}"), access::TOLD_ONCE_TTL_S) {
            continue;
        }
        told.push(crate::agenttools::message_line(message));
    }
    if !told.is_empty() {
        extra.push(format!(
            "Messages for you from teammates' agents — act on them, then inbox_ack(repo_id, ids=[...]):\n{}",
            told.join("\n")));
    }
    if !text(&answer, "text").is_empty() || identifiers.is_empty() {
        extra.extend(crate::claims::brief_lines(&app.store, &caller.scope, &caller.user_id, &session, crate::store::now()));
    }
    // a Free login at work on several machines: told once a session
    if let Some(shared) = crate::sharing::notice(
        &app.store, &caller.scope, &caller.workspace, &caller.user_id, &session,
        &access::billing_url(&app.dashboard_url), crate::store::now(),
    ) {
        extra.push(shared);
    }
    if !extra.is_empty() {
        let shown = text(&answer, "text");
        let joined = extra.join("\n");
        answer["text"] = json!(if shown.is_empty() { joined } else { format!("{shown}\n{joined}") });
    }
    // once a week, who Collide cannot see: the hook prints `text`, so the
    // nudge rides inside it — but only inside a briefing that is being
    // shown anyway. A prompt that matched nothing injects nothing, and an
    // upsell is never the only thing an agent is handed.
    if !text(&answer, "text").is_empty() {
        if let Some(nudge) = access::viewers_nudge(&app.store, &caller.workspace, &caller.user_id, &session, &app.dashboard_url) {
            let shown = text(&answer, "text");
            answer["text"] = json!(format!("{shown}\n\n{nudge}"));
            answer["viewers_notice"] = json!(nudge);
        }
    }
    Json(answer)
}

/// A hook fired that proves the hooks are loaded on this machine, without a
/// file having been written. Claude Code's ConfigChange is the case that
/// matters: the agent writes the hooks into settings.json, the harness
/// reloads them, and this is the first thing that runs. Before it existed the
/// setup nag cleared only on the first reported WRITE, so a session that
/// installed the hooks and then only read kept being told to install them.
async fn hook_alive_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    app.store.ledger_append_later(
        &caller.scope,
        "hooks_alive",
        &json!({"user": caller.user_id, "session": text(&body, "session"),
                "event": text(&body, "event"), "source": text(&body, "source")}),
        crate::store::now(),
    );
    Json(json!({"ok": true, "seen": true}))
}

async fn brief_outcome_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let n = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000);
    let big = |key: &str| body.get(key).and_then(Value::as_u64).unwrap_or(0).min(10_000_000_000);
    let session = text(&body, "session");
    let mut context = body.get("context_tokens").and_then(Value::as_u64).unwrap_or(0).min(10_000_000);
    let mut model = text(&body, "model");
    if context == 0 || model.is_empty() {
        // a hook that could not read its transcript: the session's newest
        // reported usage stands in, so a hit is never worth nothing by accident
        let (known_context, known_model) = crate::briefstat::last_usage(&app.store, &caller.scope, &session, &caller.user_id);
        if context == 0 {
            context = known_context;
        }
        if model.is_empty() {
            model = known_model;
        }
    }
    let agent = {
        let named = text(&body, "agent");
        if named.is_empty() { "collide-report-hook".to_string() } else { named }
    };
    let outcome = crate::briefstat::Outcome {
        hits: n("hits"), misses: n("misses"), needed: n("needed"), found: n("found"), callers: n("callers"),
        context_tokens: context, model: &model,
        brief_injected: big("brief_injected"), brief_carried: big("brief_carried"),
    };
    let answer = crate::briefstat::record(
        &app.store, &caller.scope, &caller.user_id, &agent, &session, &outcome, crate::store::now(),
    );
    Json(answer)
}

async fn brief_stats_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    Json(crate::briefstat::stats(&app.store, &caller.scope))
}

async fn observe_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    // observing fills the map from a viewer's machine: a record, refused
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    // each file arrives as its content, or as the hook's own parse of it
    let mut files: Vec<(String, String, Option<Value>)> = Vec::new();
    let one = |item: &Value| -> Option<(String, String, Option<Value>)> {
        let path = item.get("path").and_then(Value::as_str)?.to_string();
        if let Some(structure) = item.get("structure") {
            return Some((path, String::new(), Some(structure.clone())));
        }
        Some((path, item.get("content").and_then(Value::as_str)?.to_string(), None))
    };
    if let Some(items) = body.get("files").and_then(Value::as_array) {
        files.extend(items.iter().take(200).filter_map(one));
    } else if let Some(file) = one(&body) {
        files.push(file);
    }
    let batch = body.get("files").is_some();
    if files.is_empty() {
        return Json(json!({"ok": false, "reason": "missing path or content"}));
    }
    billing::count_call(&app.store, &caller.workspace);
    mark_hooks_seen(&app.store, &caller.scope, &caller.user_id);
    presence::note_worker(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "worker"), &text(&body, "worker_type"),
    );

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut results: Vec<Value> = Vec::with_capacity(files.len());
    // parse and index the whole batch against one read of the graph
    let mut slots: Vec<Option<Value>> = Vec::with_capacity(files.len());
    let mut wanted: Vec<(&str, &str, Option<&Value>)> = Vec::new();
    for (path, content, structure) in &files {
        if path.is_empty() {
            continue;
        }
        if content.len() > 2_000_000 {
            slots.push(Some(json!({"path": path, "parse": "too large", "map": "skipped"})));
            continue;
        }
        wanted.push((path.as_str(), content.as_str(), structure.as_ref()));
        slots.push(None);
    }
    let mut indexed = report::observe_batch(&app.store, &caller.scope, &wanted, app.hot_ttl_s).into_iter();
    for slot in slots {
        let result = match slot {
            Some(skipped) => skipped,
            None => indexed.next().unwrap_or(Value::Null),
        };
        let outcome = result.get("map").and_then(Value::as_str).unwrap_or("skipped");
        *counts.entry(match outcome {
            "indexed" => "indexed", "unchanged" => "unchanged", "deferred" => "deferred", _ => "skipped",
        }).or_insert(0) += 1;
        results.push(result);
    }
    // any read is this agent's copy: one that already holds a change the gate
    // was blocking on clears that block (parsed only where a block could be)
    // and one file read is the agent's own copy, remembered so its next write
    // is told apart from a teammate's work it has not pulled
    let reader = text(&body, "session");
    if !reader.is_empty() {
        for (path, content, structure) in &files {
            let blocking = crate::gate::has_blocking_marker(&app.store, &caller.scope, path);
            if !blocking && files.len() != 1 {
                continue;
            }
            if let Some(symbols) = report::symbol_signatures(path, content, structure.as_ref()) {
                if blocking {
                    crate::gate::acknowledge_read(&app.store, &caller.scope, &caller.user_id, &reader, path, &symbols);
                }
                if files.len() == 1 {
                    report::remember_copy(&app.store, &caller.scope, &caller.user_id, &reader, path, &symbols);
                }
            }
        }
    }
    let mut response = json!({
        "ok": true, "files": results,
        "indexed": counts.get("indexed").copied().unwrap_or(0),
        "unchanged": counts.get("unchanged").copied().unwrap_or(0),
        "deferred": counts.get("deferred").copied().unwrap_or(0),
        "skipped": counts.get("skipped").copied().unwrap_or(0),
    });
    if !batch {
        // the agent is reading a file another agent (or another session of
        // the same user) wrote moments ago: say so with the read, so the
        // edit that follows expects the file to move under it
        let session = text(&body, "session");
        crate::freshness::note_checkout(&app.store, &caller.scope, &caller.user_id, &session, &text(&body, "checkout"));
        if let Some((path, content, structure)) = files.first() {
            crate::deltas::touch(&app.store, &caller.scope, &session, &[path.clone()], crate::store::now());
            // what this copy holds now: the gate and the notes compare against it
            let hashes: Vec<String> = body.get("line_hashes").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let fp = if !hashes.is_empty() {
                crate::freshness::fingerprint(&hashes)
            } else if !content.is_empty() {
                crate::freshness::fingerprint_content(content)
            } else {
                String::new()
            };
            crate::freshness::saw(&app.store, &caller.scope, &caller.user_id, &session, path, &fp, false);
            let _ = structure;
            let who = crate::check::attribute(
                &app.store, &caller.scope, &caller.user_id, &session, &[path.clone()], &[], crate::store::now(),
            );
            if let Some(entry) = who.get("files").and_then(|f| f.get(path.as_str())) {
                let mine = entry.get("mine").and_then(Value::as_bool).unwrap_or(true);
                let age = entry.get("age_s").and_then(Value::as_f64).unwrap_or(f64::MAX);
                // once per write: Study-style runs showed the same "wrote it just
                // now" on every read of the file, turn after turn
                // the write itself, exactly: its time and fingerprint when the
                // write was recorded, else its age to the tenth of a second
                let write_id = crate::freshness::latest_write(&app.store, &caller.scope, &path)
                    .map(|(ts, h)| format!("{ts:.6}:{h}"))
                    .unwrap_or_else(|| format!("{:.1}", crate::store::now() - age));
                let told_key = format!("toldwrite:{}:{}:{}", caller.scope, session, crate::codegraph::path_key(&path));
                let told = app.store.eph_get(&told_key).and_then(|v| v.as_str().map(str::to_string)).as_deref() == Some(write_id.as_str());
                if !mine && age <= 600.0 && !told {
                    let _ = app.store.eph_set(&told_key, &json!(write_id), Some(600.0));
                    // named the way the reader knows them: nickname, handle, or email
                    let mut entry = entry.clone();
                    let user = entry.get("user").and_then(Value::as_str).unwrap_or("").to_string();
                    let users: std::collections::BTreeSet<String> = [user.clone()].into_iter().filter(|u| !u.is_empty()).collect();
                    let labels = crate::activity::identity_labels(&app.store, &caller.user_id, &users);
                    if let Some(label) = labels.get(&user).and_then(|l| l.get("label")).and_then(Value::as_str) {
                        entry["who"] = json!(label);
                    }
                    let view = crate::freshness::read_view(&app.store, &caller.scope, &caller.user_id, &session, &path);
                    if let (Some(map), Some(v)) = (entry.as_object_mut(), view.as_object()) {
                        for (k, val) in v {
                            map.insert(k.clone(), val.clone());
                        }
                    }
                    if let Some(map) = response.as_object_mut() {
                        map.insert("recent_write".into(), entry);
                    }
                }
            }
        }
        let via = via_tag(&headers, "/observe");
        let presence = presence::report_presence(
            &app.store,
            &presence::PresenceInput {
                scope: &caller.scope,
                user_id: &caller.user_id,
                session: &session,
                path: &files[0].0,
                action: "reading",
                agent: &text(&body, "agent"),
                model: &text(&body, "model"),
                branch: &text(&body, "branch"),
                tokens: number(&body, "tokens"),
                turn_id: &text(&body, "turn_id"),
                context: number(&body, "input_tokens") + number(&body, "cache_read_tokens") + number(&body, "cache_creation_tokens"),
                via: &via,
            },
        );
        if let Some(map) = response.as_object_mut() {
            map.insert("presence".into(), presence);
        }
    }
    local::attach_inbox(&app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &mut response);
    Json(response)
}

/// PostToolUse write side: the file's new content.
///
/// The write path and the awareness envelope are both here. Still missing
/// before this can be routed: the lint findings and autofix applier, and the
/// setup, artifact and rename notices. Compare-and-swap is deliberately
/// absent — the HTTP endpoint never took `expected_hashes`; that belongs to
/// the MCP `report_edit` tool and moves with it.
async fn report_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // a refused call is still a call: counted before the viewer and
    // validation checks, where Python's /report counts it
    billing::count_call(&app.store, &caller.workspace);
    // reporting is a RECORD: a viewer (effective access "read") is refused
    // in the same shape as any other access denial, with one plain line
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    crate::sharing::note(&app.store, &caller.workspace, &caller.user_id, &text(&body, "machine"), crate::store::now());
    let (scope, user_id) = (caller.scope.clone(), caller.user_id.clone());
    let member = auth::member(&app.store, &caller.workspace, &caller.uid);
    let path = text(&body, "path");
    // a native hook that parsed on its machine sends the structure, not the file
    let local = body.get("structure").is_some().then_some(&body);
    let Some(content) = body.get("content").and_then(Value::as_str).or(local.map(|_| "")) else {
        return Json(json!({"ok": false, "reason": "missing repo_id, path, or content"}));
    };
    if path.is_empty() || text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id, path, or content"}));
    }
    if content.len() > 2_000_000 {
        return Json(json!({"ok": false, "reason": "file too large"}));
    }
    let agent = {
        let named = text(&body, "agent");
        if named.is_empty() { "collide-report-hook".to_string() } else { named }
    };
    // the git origin disagreeing with the configured id IS a GitHub rename:
    // adopt it server-side and hand the new id back for the client to take up
    let renamed_to = rename::adopt_origin(
        &app.store, &app.aliases, &caller.workspace, &text(&body, "repo_id"),
        &text(&body, "origin"), &user_id, member.as_ref(),
    );
    // the first call IS the watch — and the (N+1)th repo is not bound, not
    // recorded, the human told: the payload itself, as Python's
    // `report_edit` unwinds it through /report
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(refused);
    }
    presence::note_worker(&app.store, &scope, &user_id, &text(&body, "session"), &text(&body, "worker"), &text(&body, "worker_type"));
    crate::examples::note_write(&app.store, &scope, &text(&body, "session"), &path);
    let response = report::report_edit(
        &app.store,
        &report::ReportInput {
            scope: &scope,
            user_id: &user_id,
            path: &path,
            content,
            local,
            agent: &agent,
            model: &text(&body, "model"),
            branch: &text(&body, "branch"),
            session: &text(&body, "session"),
            tokens: number(&body, "tokens"),
            input_tokens: number(&body, "input_tokens"),
            output_tokens: number(&body, "output_tokens"),
            cache_read_tokens: number(&body, "cache_read_tokens"),
            cache_creation_tokens: number(&body, "cache_creation_tokens"),
            turn_id: &text(&body, "turn_id"),
            why: &text(&body, "why"),
            hot_ttl_s: app.hot_ttl_s,
            via: &via_tag(&headers, "/report"),
            workspace: &caller.workspace,
            draft: body.get("draft").and_then(Value::as_bool).unwrap_or(false),
            auto: true,
            verified: body.get("verified").filter(|v| v.is_object()).cloned(),
        },
    );
    mark_hooks_seen(&app.store, &scope, &user_id);
    // activation: the auto-report hook is live for this person
    analytics::capture_once(
        &app.store, &caller.uid, &caller.uid, "hooks_installed", &caller.workspace,
        json!({"repo_id": text(&body, "repo_id"), "agent": agent, "hook_version": body.get("hook_version")}),
    );
    analytics::capture_daily(&app.store, &caller.uid, "agent_active", &caller.workspace, json!({"via": "report"}));
    // a write joins the writer's working set: later changes to it by others are its deltas
    crate::deltas::touch_as(&app.store, &scope, &text(&body, "session"), &[path.clone()], true, crate::store::now());
    let mut answer = envelope::decorate(
        &app.store,
        &app.aliases,
        &envelope::EnvelopeInput {
            renamed_to: &renamed_to,
            scope: &scope,
            workspace: &caller.workspace,
            repo_id: &text(&body, "repo_id"),
            user_id: &user_id,
            hook_version: envelope::as_int(body.get("hook_version")),
            agents_version: envelope::as_int(body.get("agents_version")),
            settings_digest: body.get("settings_digest").and_then(Value::as_str).unwrap_or(""),
        },
        response,
    );
    local::attach_inbox(&app.store, &scope, &user_id, &text(&body, "session"), &mut answer);
    Json(answer)
}

/// What another workspace changed under the code you are about to touch.
///
/// NOT a mirror of a Python HTTP route — there isn't one. `check_collisions`
/// is an MCP tool, and this exposes the same engine ahead of the MCP port so
/// it can be built and proven now. The differential test drives Python
/// through its tool and this through the route, against one database.
///
/// Fails open: any trouble returns an empty result rather than an exception
/// into an agent's turn. A check that can break its caller is one agents learn
/// to skip, and the call is the whole product.
async fn check_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"collisions": [], "intents": []});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let paths = strings("paths");
    let symbols = strings("symbols_referenced");
    let agent = text(&body, "agent");
    let session = text(&body, "session");
    let via = via_tag(&headers, "/check");

    // presence from any protocol call, so an agent appears the moment it does
    // anything rather than only after its first reported edit
    presence::touch_presence(&app.store, &caller.scope, &caller.user_id, &session, &agent, &via);
    let _ = app.store.ledger_append(
        &caller.scope,
        "check_performed",
        &json!({"user": caller.user_id, "session": session, "agent": agent,
                "paths": paths, "symbols": symbols}),
        store::now(),
    );

    let response = collisions::check_collisions(&app.store, &collisions::CheckInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        paths,
        symbols,
        idle_after_s: app.idle_after_s,
        visible: visible_for(&app, &caller),
    });
    Json(events::with_deltas(&app.store, &caller.scope, &caller.user_id, response))
}

/// Declare an intent: the paths and symbols an agent is about to change.
///
/// Like `/check`, this is not a mirror of a Python route — `declare_intent` is
/// an MCP tool. The engine moves first and the transport follows.
async fn declare_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    let via = via_tag(&headers, "/intent/declare");
    // presence from any protocol call, before the lease is even validated —
    // Python's declare_intent seats the agent first
    presence::touch_presence(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "session"), &text(&body, "agent"), &via);
    Json(intents::declare(&app.store, &intents::DeclareInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        repo_id: &text(&body, "repo_id"),
        paths: strings("paths"),
        symbols: strings("symbols"),
        change_type: &text(&body, "change_type"),
        before: &text(&body, "before"),
        after: &text(&body, "after"),
        summary: &text(&body, "summary"),
        agent: &text(&body, "agent"),
        session: &text(&body, "session"),
        reference: &text(&body, "ref"),
        operations: body.get("operations").cloned(),
        idempotency_key: &text(&body, "idempotency_key"),
        ttl_s: app.intent_ttl_s,
        idle_after_s: app.idle_after_s,
        via: &via,
    }))
}

/// Renew an intent lease. The cheapest call in the protocol and the most
/// frequent, which is why it does nothing but read one key and write it back.
async fn heartbeat_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(intents::heartbeat(
        &app.store, &caller.scope, &caller.user_id,
        &text(&body, "intent_id"), app.intent_ttl_s,
    ))
}

/// Release an intent lease, optionally leaving the rationale behind as memory.
async fn complete_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    billing::count_call(&app.store, &caller.workspace);
    Json(intents::complete(&app.store, &intents::CompleteInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        intent_id: &text(&body, "intent_id"),
        session: &text(&body, "session"),
        rationale: &text(&body, "rationale"),
        via: &via_tag(&headers, "/intent/complete"),
    }))
}

/// Leave a tripwire on work that is not finished.
async fn defer_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let strings = |key: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    Json(intents::defer(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "agent"),
        &strings("paths"), &strings("symbols"), &text(&body, "note"),
        body.get("expires_days").and_then(Value::as_f64).unwrap_or(14.0),
    ))
}

/// Write one durable team fact, optionally anchored to the code it is about.
async fn remember_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let tags: Vec<String> = body
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    if caller.read_only {
        return viewer_refusal(&app, &caller, &json!({"ok": false}));
    }
    Json(memory::save(&app.store, &memory::SaveInput {
        scope: &caller.scope,
        user_id: &caller.user_id,
        fact: &text(&body, "fact"),
        tags: &tag_refs,
        agent: &text(&body, "agent"),
        anchor: &text(&body, "anchor"),
        auto: &text(&body, "auto"),
        supersedes: &text(&body, "supersedes"),
    }))
}

/// Search the workspace's saved facts, annotated with honest staleness.
async fn recall_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"memories": [], "total_matched": 0})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let tags: Vec<String> = body
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let plan = access::plan_for(&app.store, &caller.workspace);
    Json(memory::recall(
        &app.store, &caller.workspace, &caller.uid, &caller.user_id, &caller.scope,
        &text(&body, "query"), &tags,
        body.get("limit").and_then(Value::as_i64).unwrap_or(25),
        access::oldest_ts(&plan), plan.flag("knowledge_sharing", true),
    ))
}

/// The whole briefing: who did what, renames to adapt to, hotspots, mined
/// scars, protocol health, and the repo map an agent reads instead of
/// grepping.
async fn briefing_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"summary": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let since_s = body
        .get("since_days")
        .and_then(Value::as_f64)
        .map(|days| days * 86_400.0)
        .unwrap_or(7.0 * 86_400.0);
    let plan = access::plan_for(&app.store, &caller.workspace);
    Json(briefing::activity(
        &app.store, &caller.scope, &text(&body, "repo_id"),
        access::clamp_since(since_s, plan.history_window_s()), app.idle_after_s,
        &caller.user_id, plan.flag("knowledge_sharing", true)))
}

/// Counterfactual: if these workspaces landed together right now, which
/// symbols would disagree — and which notes and settled decisions would that
/// disturb?
async fn simulate_merge_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"conflicts": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let users: Vec<String> = body
        .get("users")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Json(merge::simulate_merge(&app.store, &caller.scope, &text(&body, "repo_id"), &users))
}

/// Where this map is unreliable, answered about itself.
async fn blind_spots_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"never_clean": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    Json(merge::blind_spots(
        &app.store, &caller.scope, &text(&body, "repo_id"),
        app.idle_after_s, app.hot_ttl_s,
    ))
}

/// Does my diff preserve the assumptions the other workspace's code relies
/// on? Interface oracle only, and it says so.
async fn differential_check_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    Json(merge::differential_check(
        &app.store, &caller.scope, &caller.user_id, &text(&body, "other_user"),
    ))
}

/// `COLLIDE_CORS_ORIGINS`: a comma-separated list, or `*` (the default).
fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{AllowOrigin, Any, CorsLayer};
    let configured = std::env::var("COLLIDE_CORS_ORIGINS").unwrap_or_else(|_| "*".into());
    let origins: Vec<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string)
        .collect();
    let allow = if origins.is_empty() || origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| o.parse().ok()))
    };
    CorsLayer::new().allow_origin(allow).allow_methods(Any).allow_headers(Any)
}

/// Discovery, dynamic registration, authorize, token: the OAuth flow an
/// MCP client walks before it ever calls a tool. The free version has none:
/// its local server is reached with the token kept beside it.
#[cfg(feature = "cloud")]
fn oauth_router(app: &Arc<App>) -> Router {
    oauth::build_oauth_router(
        Arc::clone(&app.store), app.public_url.clone(),
        app.dashboard_url.clone(), app.mcp_url.clone(), app.dashboard_auth.clone())
}

#[cfg(not(feature = "cloud"))]
fn oauth_router(_app: &Arc<App>) -> Router {
    Router::new()
}

/// MCP at `/mcp` on any host AND at the root — production's canonical
/// address is https://mcp.collidemcp.com with no path, and that is what every
/// client config points at. One service, mounted twice, so a session opened
/// at either address is found at either. Both mounts sit behind the gate.
///
/// The first Rust deploy mounted only `/mcp`, and every client configured
/// with the root address got "no MCP endpoint found" — a full outage for
/// exactly the clients that had been working. The Python gate had done this
/// from the start; it was the one piece of `app.py` that did not sit in a
/// route table, which is how it was missed.
fn mcp_mounts(app: Arc<App>) -> Router<Arc<App>> {
    let service = mcp_service(Arc::clone(&app));
    Router::new()
        .nest_service("/mcp", service.clone())
        .route_service("/", service)
        .route_layer(axum::middleware::from_fn_with_state(app, mcp_gate))
}

/// The address the caller reached us at, for the metadata pointer in a 401:
/// the configured public URL, else scheme and host off the request.
fn public_url_of(app: &App, headers: &HeaderMap) -> String {
    if !app.public_url.is_empty() {
        return app.public_url.clone();
    }
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// The MCP auth gate, ported whole from Python's `McpAuthGate`.
///
/// Three jobs, in this order. A human or a probe opening the root — anything
/// that is not a JSON-RPC POST, an event-stream GET, or a session DELETE —
/// is sent to the website, or handed a JSON card when no dashboard is
/// configured. An MCP request with no valid workspace-bound credential gets
/// the RFC 9728 401, whose WWW-Authenticate header is how an OAuth-capable
/// client discovers where to log in; without it a new user's client simply
/// fails, with nothing to follow. Everything else carries the address it was
/// reached on into the tool layer as a header, because the mount hides it.
async fn mcp_gate(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = request.uri().path().to_string();
    let at_root = path.trim_matches('/').is_empty();
    let headers = request.headers().clone();
    let accept = headers.get("accept").and_then(|v| v.to_str().ok()).unwrap_or("");
    let method = request.method().clone();

    if at_root
        && method != axum::http::Method::POST
        && method != axum::http::Method::DELETE
        && !accept.contains("text/event-stream")
    {
        if !app.dashboard_url.is_empty() {
            return axum::response::Redirect::temporary(&app.dashboard_url).into_response();
        }
        let card = json!({
            "service": "collide",
            "mcp_endpoint": if app.mcp_url.is_empty() { "/mcp".to_string() } else { app.mcp_url.clone() },
            "health": "/health",
        });
        return Json(card).into_response();
    }

    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok());
    let principal = auth::principal_for_bearer(&app.store, authorization, store::now());
    if principal.as_ref().map(|p| p.workspace.is_empty()).unwrap_or(true) {
        let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
        let root_host = at_root && oauth::mcp_at_root(&app.mcp_url, host);
        let (status, parts, body) = oauth::unauthorized_mcp_response(&public_url_of(&app, &headers), root_host);
        let mut response = axum::response::Response::builder().status(status);
        for (name, value) in parts {
            response = response.header(name, value);
        }
        return response
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| axum::http::StatusCode::UNAUTHORIZED.into_response());
    }

    let mut request = request;
    let via = format!(
        "{} {}",
        headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or(""),
        if path.is_empty() { "/" } else { path.as_str() }
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&via) {
        request.headers_mut().insert("x-collide-via", value);
    }
    next.run(request).await
}

/// The MCP endpoint, wired to the same `App` every HTTP route uses so a tool
/// call and its equivalent POST run identical code against identical state.
///
/// Sessions are kept in memory. That is correct for a single process and
/// wrong for several behind a load balancer, which is the same constraint the
/// Python half has and the same reason a given endpoint must be served by
/// exactly one half at a time.
fn mcp_service(
    app: Arc<App>,
) -> StreamableHttpService<mcp::CollideMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(mcp::CollideMcp { app: Arc::clone(&app) }),
        Arc::new(LocalSessionManager::default()),
        // The transport's DNS-rebinding guard admits only localhost by
        // default. That guard protects servers running on a developer's own
        // machine; this one sits behind a proxy under its public hostname,
        // where the default answered every real client with 403. The Python
        // half disables it for the same reason, and so does this.
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}

/// The dashboard's graph: structure plus every overlay, and the repo map.
async fn graph_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match authorize_bound(&app, &headers, &body, json!({"nodes": [], "edges": []})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    billing::count_call(&app.store, &caller.workspace);
    let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(600).clamp(50, 2000) as usize;
    Json(graphview::graph_view(
        &app.store, &caller.scope, &text(&body, "repo_id"), app.idle_after_s, limit,
    ))
}

/// The wake client's directed-message poll: pending messages for the
/// credential's user, acking the ids it has already handled. Same hook-token
/// auth as /report; fails open.
/// An agent tells the others something, from its shell (`collide-hook
/// message`): the free version has no MCP tools, and every agent can run a
/// command. See [`local::send`].
async fn message_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_bound(&app, &headers, &body, open.clone()) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    if caller.read_only {
        return Json(json!({"ok": false, "error": "a viewer's agents send nothing"}));
    }
    Json(local::send(&app.store, &caller.scope, &caller.repo_id, &caller.workspace, &caller.user_id, &body, app.idle_after_s))
}

/// What Collide did on this machine since `since` (see [`local::summary`]).
/// A local server's own credential only: the cloud has a dashboard instead.
async fn local_summary_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    if !local::active() {
        return Json(json!({"ok": false, "reason": "not a local server"}));
    }
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &json!({}), open) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let since = body.get("since").and_then(Value::as_f64).unwrap_or(crate::store::now() - 86_400.0);
    let mut answer = local::summary(&app.store, &caller.workspace, since);
    answer["ok"] = json!(true);
    Json(answer)
}

/// One batch of this machine's record, for `collide-hook sync` to send on
/// (see [`local::export`]).
async fn local_export_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    if !local::active() {
        return Json(json!({"ok": false, "reason": "not a local server"}));
    }
    let caller = match authorize_full(&app, &headers, &json!({}), json!({"ok": false})) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let cursor = body.get("cursor").cloned().unwrap_or_else(|| json!({}));
    Json(local::export(&app.store, &caller.workspace, &cursor, 2_000))
}

/// The person who may connect or sync a machine: the credential's owner,
/// signed in, a player in the workspace (a viewer's agents record nothing).
fn machine_caller(app: &App, headers: &HeaderMap) -> Result<Caller, Json<Value>> {
    let caller = authorize_full(app, headers, &json!({}), json!({"ok": false}))?;
    if caller.read_only {
        return Err(Json(json!({"ok": false, "error": "a viewer's machine stays local: ask the owner for a seat"})));
    }
    Ok(caller)
}

/// `collide-hook login` on a machine running the free version: the
/// machine is recorded under the workspace the sign-in chose, a hook
/// credential is minted for it, and the answer says whether the workspace
/// is paid (the machine then moves to Collide's servers and syncs) or Free
/// (it stays local). See [`machines`].
async fn machine_connect_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if text(&body, "machine").is_empty() {
        return Json(json!({"ok": false, "error": "missing machine"}));
    }
    machines::record(&app.store, &caller.workspace, &caller.uid, &caller.email, &body);
    let identity = crate::blocks::HookIdentity {
        auth_uid: &caller.uid, email: &caller.email, name: &caller.name, workspace: &caller.workspace,
    };
    let credentials = crate::blocks::mint_hook_credential(&app.store, &identity, "collide-machine", &app.public_url);
    let workspace_name = crate::workspaces::get(&app.store, &caller.workspace)
        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    let mut answer = machines::plan_answer(&app.store, &caller.workspace);
    answer["ok"] = json!(true);
    answer["credentials"] = credentials;
    answer["user"] = json!(caller.email);
    answer["workspace"] = json!(caller.workspace);
    answer["workspace_name"] = json!(workspace_name);
    answer["server_url"] = json!(app.public_url.trim_end_matches('/'));
    answer["dashboard_url"] = json!(app.dashboard_url);
    Json(answer)
}

/// A signed-in machine asks, now and then, whether its workspace is paid
/// yet; its local stats ride along for the dashboard.
async fn machine_status_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if !text(&body, "machine").is_empty() {
        machines::record(&app.store, &caller.workspace, &caller.uid, &caller.email, &body);
    }
    let mut answer = machines::plan_answer(&app.store, &caller.workspace);
    answer["ok"] = json!(true);
    Json(answer)
}

/// `collide add`: the person said yes, this repo joins the workspace. The
/// explicit way in from a terminal, as `setup` is from the MCP tools.
async fn repo_add_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open.clone()) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if caller.repo_id.trim().is_empty() {
        return Json(json!({"ok": false, "error": "missing repo_id"}));
    }
    if caller.read_only {
        return viewer_refusal(&app, &caller, &open);
    }
    if let Err(refused) = bind_scope_as(&app, &caller, true) {
        return Json(refused);
    }
    let workspace_name = crate::workspaces::get(&app.store, &caller.workspace)
        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    Json(json!({"ok": true, "repo_id": caller.repo_id, "workspace": caller.workspace, "workspace_name": workspace_name}))
}

/// `collide-hook install --claim CODE`: see [`machines::claim`].
async fn machine_claim_endpoint(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Json<Value> {
    Json(machines::claim(&app.store, &app.public_url, &body))
}

/// The free version's install line for the website: no account, no code.
async fn free_install_endpoint(State(app): State<Arc<App>>) -> Json<Value> {
    let server = if app.public_url.is_empty() { "https://mcp.collidemcp.com".to_string() } else { app.public_url.clone() };
    match machines::free_install(server.trim_end_matches('/')) {
        Some((version, command)) => Json(json!({"ok": true, "version": version, "command": command})),
        None => Json(json!({"ok": false, "error": "no builds of the free version on this server yet"})),
    }
}

/// The script behind `curl -fsSL https://collidemcp.com/install.sh | sh`.
async fn install_script_endpoint(State(app): State<Arc<App>>, headers: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    downloads::record(&app.store, "script", "", &downloads::client_ip(&headers));
    let server = if app.public_url.is_empty() { "https://mcp.collidemcp.com".to_string() } else { app.public_url.clone() };
    match machines::free_script_now(server.trim_end_matches('/')).await {
        Some(script) => ([(axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response(),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "#!/bin/sh\necho \"Collide: the builds are being published. Try again in a few minutes.\"\nexit 1\n",
        )
            .into_response(),
    }
}

/// One batch of a machine's local record, into its paid workspace.
async fn machine_import_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let caller = match machine_caller(&app, &headers) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    Json(machines::import(&app.store, &app.aliases, &caller.workspace, &caller.email, &body))
}

async fn inbox_endpoint(
    State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>,
) -> Json<Value> {
    let open = json!({"ok": false});
    let caller = match authorize_full(&app, &headers, &body, open) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    // a refused call is still a call: counted where Python's /inbox counts it
    billing::count_call(&app.store, &caller.workspace);
    if text(&body, "repo_id").is_empty() {
        return Json(json!({"ok": false, "reason": "missing repo_id"}));
    }
    // the first call IS the watch; the (N+1)th repo is not bound and its
    // box is not read — the flag and the line, as `authorize_bound` shapes it
    if let Err(refused) = bind_scope(&app, &caller) {
        return Json(json!({"ok": false, "workspace_full": true,
                           "reason": refused.get("error").cloned().unwrap_or(Value::Null)}));
    }
    let ack: Vec<String> = body
        .get("ack")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty())
            .map(str::to_string).collect())
        .unwrap_or_default();
    let inbox = agenttools::inbox_ack(
        &app.store, &caller.scope, &caller.user_id, &visible_for(&app, &caller), &ack);
    Json(access::own_messages_only(&app.store, &caller.workspace, &caller.user_id, inbox, &app.dashboard_url))
}

/// The dashboard's live feed: one scope's events as they happen. Refused
/// with the same close codes the Python half uses — 4401 for no credential,
/// 4403 for the wrong workspace or no access — before the socket is accepted.
async fn ws_feed(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(app): State<Arc<App>>,
    axum::extract::Path(repo_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let repo_id = repo_id.trim_start_matches('/').to_string();
    let token = text(&query, "token");
    let mut workspace_id = text(&query, "workspace");
    let principal = app.dashboard_auth.principal_for(&app.store, &token, store::now());
    let mut refusal: Option<u16> = None;
    let principal = match principal {
        None => { refusal = Some(4401); None }
        Some(principal) => {
            if !principal.workspace.is_empty() {
                if workspace_id.is_empty() {
                    workspace_id = principal.workspace.clone();
                }
                if workspace_id != principal.workspace {
                    refusal = Some(4403);
                }
            }
            Some(principal)
        }
    };
    if refusal.is_none() {
        let member = principal.as_ref().and_then(|p| {
            (!workspace_id.is_empty()).then(|| auth::member(&app.store, &workspace_id, &p.uid)).flatten()
        });
        if !access::has_repo_access(member.as_ref(), &repo_id) {
            refusal = Some(4403);
        }
    }
    let scope = app.aliases.scope_for(&workspace_id, &repo_id);
    // a viewer's live feed carries who and where, never the code (as the
    // dashboard routes do, routes_observe::redact_for_viewer)
    let viewer = refusal.is_none()
        && principal.as_ref().and_then(|p| auth::member(&app.store, &workspace_id, &p.uid)).is_some_and(|member| {
            billing::effective_access(&billing::plan_of(&app.store, &workspace_id), &member) == "read"
        });
    ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::{CloseFrame, Message};
        if let Some(code) = refusal {
            let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: "".into() }))).await;
            return;
        }
        let mut feed = app.store.subscribe(&scope);
        loop {
            match feed.recv().await {
                Ok(event) => {
                    let mut event = serde_json::to_value(&event).unwrap_or(Value::Null);
                    if viewer {
                        crate::access::redact_for_viewer(&mut event);
                    }
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
    .into_response()
}

/// Pure functions, exposed so the Python suite can feed both implementations
/// the same input and compare. These are the pieces where "reasonable but
/// different" is a bug: hunks land in the ledger and on change pages, and the
/// parse state machine decides what counts as a removal.
fn compute(input: &str) -> i32 {
    let Ok(request) = serde_json::from_str::<Value>(input) else {
        eprintln!("compute: stdin was not JSON");
        return 1;
    };
    let strings = |key: &str| -> Vec<String> {
        request
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect()
            })
            .unwrap_or_default()
    };
    let output = match request.get("op").and_then(Value::as_str).unwrap_or("") {
        "line_hashes" => {
            let lines = strings("lines");
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            json!(diff::line_hashes(&refs))
        }
        "hunks" => {
            let new_lines = strings("lines");
            let refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
            let hunks = diff::hunks(&strings("old"), &strings("new"), &refs);
            let (added, removed) = diff::hunk_totals(&hunks);
            json!({"hunks": hunks, "lines_added": added, "lines_removed": removed})
        }
        "apply_parse" => {
            let record = request.get("record").filter(|v| !v.is_null());
            let symbols = request
                .get("symbols")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let status = request.get("status").and_then(Value::as_str).unwrap_or("clean");
            let now = request.get("now").and_then(Value::as_f64).unwrap_or(0.0);
            let (record, events) = semantics::apply_parse(record, status, symbols, now);
            json!({"record": record, "events": events})
        }
        "serve" => {
            let record = request.get("record").filter(|v| !v.is_null());
            let now = request.get("now").and_then(Value::as_f64).unwrap_or(0.0);
            match semantics::serve(record, now) {
                Some(served) => json!({
                    "symbols": served.symbols,
                    "freshness": served.freshness,
                    "age_s": served.age_s,
                }),
                None => Value::Null,
            }
        }
        other => {
            eprintln!("compute: unknown op {other:?}");
            return 1;
        }
    };
    println!("{}", serde_json::to_string(&output).unwrap_or_default());
    0
}

/// Recompute the hashes the Python server would have written, over whatever
/// the database already holds. The Python test suite runs this against a
/// database Python itself wrote and asserts every value matches — which is
/// the whole safety argument for running both servers on one database.
fn parity(path: &PathBuf) -> i32 {
    let Ok(store) = Store::open(path) else {
        eprintln!("parity: cannot open {}", path.display());
        return 1;
    };
    let mut report = json!({
        "engine": "rust",
        "empty_root": hashing::empty_root(),
        "chain_hash_of_known_input": hashing::chain_hash(
            store::GENESIS,
            "ws:github.com/acme/api",
            1789646694.5665562,
            "edit_reported",
            &json!({"user": "alice", "path": "a.py", "lines_added": 3}),
        ),
    });

    // a merkle tree over a fixed file set, and the canonical JSON forms
    let mut files = BTreeMap::new();
    files.insert("pkg/a.py".to_string(), "aaa".to_string());
    files.insert("pkg/b.py".to_string(), "bbb".to_string());
    files.insert("top.py".to_string(), "ccc".to_string());
    let tree = hashing::Tree::build(files);

    let mut symbols = BTreeMap::new();
    symbols.insert("compute_tax".to_string(), "h1".to_string());
    symbols.insert("Ledger".to_string(), "h2".to_string());

    let sample = json!({"b": 2, "a": [1, {"d": 4, "c": 3}], "e": "x"});
    if let Some(map) = report.as_object_mut() {
        map.insert("merkle_root".into(), json!(tree.root));
        map.insert("merkle_dirs".into(), json!(tree.dirs));
        map.insert("file_hash".into(), json!(hashing::file_hash(&symbols)));
        map.insert("canonical_json".into(), json!(hashing::canonical_json(&sample)));
        // the encoding a ledger row is STORED in: Python's default separators
        // with sort_keys=True. The formatter alone does not sort, because
        // Python's default json.dumps does not either.
        map.insert("python_dumps".into(), json!(store::canonical_sorted(&sample)));
        // Non-ASCII is the case that actually broke a chain: Python's
        // json.dumps defaults to ensure_ascii=True and serde emits raw
        // UTF-8, and every fixture here used to be pure ASCII, so nothing
        // caught it. An em dash in an agent's summary was enough.
        let unicode_sample = json!({
            "summary": "renamed get_symbol \u{2014} see notes",
            "path": "src/caf\u{e9}.py",
            "emoji": "\u{1f680}",
            "control": "a\tb\nc",
        });
        map.insert("canonical_json_unicode".into(),
                   json!(hashing::canonical_json(&unicode_sample)));
        map.insert("python_dumps_unicode".into(),
                   json!(store::canonical_sorted(&unicode_sample)));
        // and re-verify the chain the Python server wrote, link by link —
        // the strongest statement of compatibility available
        let (chain_ok, bad_seq, rows) = store.ledger_verify();
        map.insert("ledger_rows_in_db".into(), json!(rows));
        map.insert("chain_ok".into(), json!(chain_ok));
        map.insert("chain_bad_seq".into(), json!(bad_seq));
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    0
}

/// Handlers call the store directly, and a store call can wait on the
/// writer: with one worker per core, a handful of waiting handlers stalled
/// every other request and kept write batches tiny. Eight per core went too
/// far the other way on staging (192 threads on 24 cores queued for CPU).
/// Two per core, 16 to 64, measured best; COLLIDE_WORKERS overrides.
fn main() {
    // the free version's `collide` program: the hooks, unless asked to be
    // the local server (or one of the server's own tools)
    #[cfg(feature = "cli")]
    {
        let first = std::env::args().nth(1);
        if !matches!(first.as_deref(), Some("local" | "compute" | "embedder" | "parity")) {
            collide_hooks::machine::set_serves_local();
            collide_hooks::run(std::env::args().skip(1).collect());
        }
    }
    // the free version, on the developer's machine: its environment is set
    // before the runtime starts and before anything reads it
    if std::env::args().nth(1).as_deref() == Some("local") {
        local::prepare_env();
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let workers = std::env::var("COLLIDE_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or((cores * 2).clamp(16, 64)).clamp(2, 512);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(serve_main());
}

async fn serve_main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let db_path = PathBuf::from(&data_dir).join("collide.db");

    if args.first().map(String::as_str) == Some("compute") {
        use std::io::Read;

        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        std::process::exit(compute(&input));
    }

    if args.first().map(String::as_str) == Some("embedder") {
        crate::embed::serve_embedder().await;
        return;
    }

    if args.first().map(String::as_str) == Some("parity") {
        let target = args.get(1).map(PathBuf::from).unwrap_or(db_path);
        std::process::exit(parity(&target));
    }

    // local: a port on 127.0.0.1 chosen now, so the server's own address is known
    let local_port = if local::active() {
        let asked = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok());
        match local::pick_port(asked) {
            Ok(port) => {
                std::env::set_var("COLLIDE_PUBLIC_URL", format!("http://127.0.0.1:{port}"));
                Some(port)
            }
            Err(said) => {
                // another local Collide is already serving: nothing to do
                eprintln!("collide local: {said}");
                std::process::exit(0);
            }
        }
    } else {
        None
    };
    // the same derivation the Python half does: an explicit MCP address wins,
    // else a host that is itself `mcp.` serves MCP at its root, else `/mcp`
    let public_url = std::env::var("COLLIDE_PUBLIC_URL").unwrap_or_default()
        .trim().trim_end_matches('/').to_string();
    let mcp_url = {
        let explicit = std::env::var("COLLIDE_MCP_URL").unwrap_or_default()
            .trim().trim_end_matches('/').to_string();
        if !explicit.is_empty() || public_url.is_empty() {
            explicit
        } else {
            let host = public_url.split("://").nth(1).unwrap_or("").split('/').next()
                .unwrap_or("").to_lowercase();
            if host.starts_with("mcp.") { public_url.clone() } else { format!("{public_url}/mcp") }
        }
    };
    restore_if_staged(&db_path);
    // with an engine the database lives on local disk and is made current
    // from the snapshot and the log before anything opens it
    let db_path = match std::env::var("COLLIDE_DB_PATH").ok().filter(|v| !v.trim().is_empty()) {
        Some(local) if crate::engine::configured().is_some() => std::path::PathBuf::from(local.trim()),
        _ => db_path,
    };
    if crate::engine::configured().is_some() {
        let seed = std::path::PathBuf::from(&data_dir).join("collide.db");
        let seed = (seed != db_path).then_some(seed);
        // the Postgres client blocks: off the async runtime
        let (target, seed_path) = (db_path.clone(), seed.clone());
        let prepared = std::thread::spawn(move || crate::engine::prepare(&target, seed_path.as_deref()))
            .join()
            .unwrap_or_else(|_| Err("the restore thread panicked".into()));
        match prepared {
            Ok(said) => tracing::info!("engine: {said}"),
            Err(error) => {
                eprintln!("engine: cannot make the database current: {error}");
                std::process::exit(1);
            }
        }
    }
    let store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open {}: {error}", db_path.display());
            std::process::exit(1);
        }
    };
    if local::active() {
        let workspace = local::bootstrap(&store);
        tracing::info!("collide local: workspace {workspace}, data in {}", local::dir().display());
    }
    // before the first request: existing accounts are not new signups
    analytics::backfill(&store);
    // seed the alias registry from scopes that already carry data, so a
    // friendly short name resolves onto the existing scope rather than
    // minting an empty one on the first call
    let aliases = Aliases::new();
    aliases.seed(&store.list_scopes(""));
    for (key, record) in store.kv_list("repo_alias", "") {
        let Some((workspace_id, from)) = key.split_once(':') else { continue };
        if let Some(canonical) = record.get("canonical").and_then(Value::as_str) {
            aliases.add_rename(workspace_id, from, canonical);
        }
    }

    // warm the Firebase key cache before the first dashboard request lands
    // (a local server has no dashboard and calls nothing out)
    if !local::active() {
        auth::spawn_firebase_key_refresh();
    }
    let app = Arc::new(App {
        store: Arc::new(store),
        aliases,
        started: std::time::Instant::now(),
        dashboard_url: std::env::var("COLLIDE_DASHBOARD_URL").unwrap_or_default(),
        public_url: public_url.clone(),
        mcp_url,
        dashboard_auth: auth::DashboardAuth::from_env(),
        hot_ttl_s: std::env::var("COLLIDE_HOT_TTL_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(900.0),
        idle_after_s: std::env::var("COLLIDE_IDLE_AFTER_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(600.0),
        intent_ttl_s: std::env::var("COLLIDE_INTENT_TTL_S")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(900.0),
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/gate", post(gate_endpoint))
        .route("/presence", post(presence_endpoint))
        .route("/report", post(report_endpoint))
        .route("/observe", post(observe_endpoint))
        .route("/brief", post(brief_endpoint))
        .route("/hook-alive", post(hook_alive_endpoint))
        .route("/brief/outcome", post(brief_outcome_endpoint))
        .route("/brief/stats", post(brief_stats_endpoint))
        .route("/verified", post(verified_endpoint))
        .route("/verify", post(verify_endpoint))
        .route("/attribute", post(attribute_endpoint))
        .route("/deltas", post(deltas_endpoint))
        .route("/land", post(land_endpoint))
        .route("/plugin/connect", post(plugin_connect_endpoint))
        .route("/apply/done", post(apply_done_endpoint))
        .route("/recipe", post(recipe_save_endpoint))
        .route("/recipe/get", post(recipe_get_endpoint))
        .route("/callers", post(callers_endpoint))
        .route("/check", post(check_endpoint))
        .route("/intent/declare", post(declare_endpoint))
        .route("/intent/heartbeat", post(heartbeat_endpoint))
        .route("/intent/complete", post(complete_endpoint))
        .route("/intent/defer", post(defer_endpoint))
        .route("/memory/save", post(remember_endpoint))
        .route("/memory/recall", post(recall_endpoint))
        .route("/briefing", post(briefing_endpoint))
        .route("/simulate-merge", post(simulate_merge_endpoint))
        .route("/blind-spots", post(blind_spots_endpoint))
        .route("/differential-check", post(differential_check_endpoint))
        .route("/graph", post(graph_endpoint))
        .route("/inbox", post(inbox_endpoint))
        .route("/message", post(message_endpoint))
        .route("/local/summary", post(local_summary_endpoint))
        .route("/local/export", post(local_export_endpoint))
        .route("/machine/connect", post(machine_connect_endpoint))
        .route("/machine/status", post(machine_status_endpoint))
        .route("/machine/import", post(machine_import_endpoint))
        .route("/machine/claim", post(machine_claim_endpoint))
        .route("/repo/add", post(repo_add_endpoint))
        .route("/artifacts/free", get(free_install_endpoint))
        .route("/install.sh", get(install_script_endpoint))
        .route("/ws/*repo_id", get(ws_feed))
        // Python's set_via, for every row these handlers write
        .route_layer(axum::middleware::from_fn(via_scope))
        // MCP over Streamable HTTP, at the same path the Python half serves.
        // This is the surface agents connect to; everything above it is the
        // engine those tools dispatch into, and the hooks' own fast paths.
        .merge(mcp_mounts(Arc::clone(&app)))
        // the dashboard's read side: activity, ledger, graph, journal, why,
        // costs, compliance, briefing, search
        .merge(routes_observe::router())
        .merge(billing::router())
        .merge(admin::router())
        .merge(routes_workspaces::router())
        .merge(routes_account::router())
        // axum caps request bodies at 2MB by default, which would reject an
        // oversized report with a bare 413 before the handler could answer
        // with the same JSON the Python server does. The real limit is the
        // handler's own check.
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(Arc::clone(&app))
        // discovery, dynamic registration, authorize, token: the OAuth flow
        // an MCP client walks before it ever calls a tool. Merged after the
        // state is applied because it carries its own.
        .merge(oauth_router(&app))
        // The dashboard calls /api/* and /ws/* from the browser on another
        // origin. Without this every one of those calls is refused by the
        // browser before it reaches a handler — the MCP and hook paths are
        // server to server and would carry on, and the dashboard would go
        // dark. Same policy as the Python half: origins from the env, any
        // method, any header, no credentials.
        .layer(cors_layer())
        // A panic inside a handler used to drop the connection with no
        // response at all — which the dashboard experienced as loading
        // forever. Now it is a 500 with a log line, which is at least a
        // fact someone can act on.
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        // every route: an answer leaves once the log holds what it wrote
        .layer(axum::middleware::from_fn_with_state(Arc::clone(&app), durable));

    // the compiled hooks this server hands out: pulled from the GitHub
    // release for this hook version at boot and hourly, so a fresh container
    // serves them within seconds and a version bump refreshes them by itself.
    // Off (and silent) unless a token or COLLIDE_HOOK_SYNC=1 says otherwise.
    //
    // A push deploys this server and starts the CI job that publishes the
    // release, and the deploy usually wins: the boot fetch finds no release
    // yet. So a miss retries every minute for the first half hour, then
    // hourly — the builds appear within a minute of CI publishing them.
    // meaning-based prompt matching: model and indexing thread, off the
    // request path, and only when COLLIDE_EMBEDDINGS=1
    crate::embed::start(app.store.clone());
    // the ephemeral tier and batched counters reach disk from here
    Store::start_flusher(app.store.clone());
    // the durable log, the lease and snapshots (COLLIDE_PG_URL)
    if crate::engine::Engine::start(app.store.clone()).is_some() {
        tracing::info!("engine: started; /health reports ready once caught up");
    }
    // repos idle for half an hour leave memory: their graph records, index,
    // snapshot and vectors are rebuilt on next use. COLLIDE_CACHE_IDLE_S tunes it.
    std::thread::Builder::new()
        .name("collide-evict".into())
        .spawn(|| {
            let idle = std::env::var("COLLIDE_CACHE_IDLE_S").ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(1_800.0);
            let every = (idle / 30.0).clamp(1.0, 60.0);
            loop {
                std::thread::sleep(std::time::Duration::from_secs_f64(every));
                let dropped = crate::codegraph::evict_idle(idle) + crate::graphview::evict_idle(idle) + crate::embed::evict_idle(idle);
                if dropped > 0 {
                    tracing::info!("memory: dropped {dropped} idle repo cache(s)");
                }
            }
        })
        .ok();
    if artifacts::sync_enabled() {
        tokio::spawn(async {
            let mut quick_retries = 30u32;
            loop {
                let wait = match artifacts::sync_from_github().await {
                    Ok(n) => {
                        if n == 0 {
                            tracing::info!("hook binaries: {} current", artifacts::release_tag());
                        } else {
                            tracing::info!("hook binaries: {} file(s) of {} fetched", n, artifacts::release_tag());
                        }
                        quick_retries = 0;
                        3600
                    }
                    Err(error) => {
                        tracing::warn!("hook binaries: {error}");
                        // until this release's builds exist, the version before
                        // is what the install script and setup hand out: have it
                        let previous = crate::envelope::HOOK_ARTIFACT_VERSION - 1;
                        if let Err(why) = artifacts::ensure_version(previous).await {
                            tracing::warn!("hook binaries: v{previous} too: {why}");
                        }
                        if quick_retries > 0 {
                            quick_retries -= 1;
                            60
                        } else {
                            3600
                        }
                    }
                };
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        });
    }

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8000);
    // [::] first: Railway's private network is IPv6, and on Linux a [::]
    // socket takes IPv4 too, so the public edge keeps working. A host with
    // IPv6 off falls back to plain IPv4, as the embedder does. A local
    // server listens on this machine's loopback and nowhere else.
    let (addr, bound) = if let Some(port) = local_port {
        let v4 = SocketAddr::from(([127, 0, 0, 1], port));
        (v4, tokio::net::TcpListener::bind(v4).await)
    } else {
        let addr = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => (addr, Ok(listener)),
            Err(_) => {
                let v4 = SocketAddr::from(([0, 0, 0, 0], port));
                (v4, tokio::net::TcpListener::bind(v4).await)
            }
        }
    };
    if let (Some(port), Ok(_)) = (local_port, &bound) {
        local::write_port(port);
        local::watch_for_update(app.store.clone());
    }
    tracing::info!("collide-server (rust) listening on {addr}");
    let listener = match bound {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot bind {addr}: {error}");
            std::process::exit(1);
        }
    };
    // a deploy sends SIGTERM: stop taking requests, then write what the
    // in-memory tiers still hold, so presence and counters survive it
    let flush_store = app.store.clone();
    let deadline_store = app.store.clone();
    let stopping = async move {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        // A deploy's standby is already serving, and writes reaching it
        // wait for this process's lease: hand it over fast. In-flight
        // requests get a short grace, then memory is written, shipped and
        // the lease released. Open websockets and MCP streams never finish
        // on their own, so this does not wait for the server to drain.
        std::thread::spawn(move || {
            let grace = std::env::var("COLLIDE_HANDOVER_GRACE_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(300);
            std::thread::sleep(std::time::Duration::from_millis(grace));
            crate::embed::persist_now();
            match deadline_store.engine() {
                Some(engine) => engine.release(&deadline_store),
                None => {
                    deadline_store.flush();
                    deadline_store.flush_trees(true);
                }
            }
            std::process::exit(0);
        });
    };
    if let Err(error) = axum::serve(listener, router).with_graceful_shutdown(stopping).await {
        eprintln!("server stopped: {error}");
        flush_store.flush();
        flush_store.flush_trees(true);
        std::process::exit(1);
    }
    flush_store.flush();
    flush_store.flush_trees(true);
    crate::embed::persist_now();
    if let Some(engine) = flush_store.engine() {
        let store = flush_store.clone();
        let _ = std::thread::spawn(move || engine.release(&store)).join();
    }
    // the runtime would otherwise wait on its blocking threads (the embedder,
    // the evictor) for ever: a deploy's SIGTERM has to end the process
    tracing::info!("collide-server stopped: in-memory state written");
    std::process::exit(0);
}

/// A database copied in whole (staging taking a copy of production, or a
/// restore from backup) is staged at `<data>/restore/collide.db` (with its
/// `-wal` when there is one) and swapped in here, before anything opens the
/// file: the current database is kept beside it as `collide.db.before-<ts>`,
/// never deleted.
fn restore_if_staged(db_path: &std::path::Path) {
    let Some(dir) = db_path.parent() else { return };
    let staged = dir.join("restore").join("collide.db");
    if !staged.exists() {
        return;
    }
    let stamp = crate::store::now() as u64;
    for suffix in ["", "-wal", "-shm"] {
        let current = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if current.exists() {
            let kept = dir.join(format!("collide.db.before-{stamp}{suffix}"));
            if let Err(error) = std::fs::rename(&current, &kept) {
                eprintln!("restore: cannot set {} aside: {error}; keeping the current database", current.display());
                return;
            }
        }
    }
    for suffix in ["", "-wal"] {
        let from = dir.join("restore").join(format!("collide.db{suffix}"));
        if from.exists() {
            let to = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
            if let Err(error) = std::fs::rename(&from, &to) {
                eprintln!("restore: cannot move {} in: {error}", from.display());
            }
        }
    }
    tracing::info!("restore: swapped in the staged database; the previous one is kept as collide.db.before-{stamp}");
}
