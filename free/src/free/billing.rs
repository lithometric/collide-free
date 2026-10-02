//! The free version has no billing; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::store::Store;

pub const BILLING_BUCKET: &str = "billing";
pub const TRIAL_DAYS: i64 = 14;

pub enum Limit {
    Count(Option<i64>),
    Text(&'static str),
    Flag(bool),
}

/// A plan, as access reads it. On one machine there is one, and nothing in
/// it is limited: an empty list means every count is unmetered.
pub struct PlanDef {
    pub name: &'static str,
    pub label: &'static str,
    pub limits: &'static [(&'static str, Limit)],
}

pub const PLANS: &[PlanDef] = &[
    PlanDef { name: "free", label: "Collide Free", limits: &[] },
    PlanDef { name: "business", label: "Collide Free", limits: &[] },
];

pub fn count_call(_store: &Store, _workspace_id: &str) {}

/// The local workspace's plan record, as `local::bootstrap` wrote it.
pub fn plan_of(store: &Store, workspace_id: &str) -> Value {
    store.kv_get(BILLING_BUCKET, workspace_id).unwrap_or_else(|| json!({"plan": "business", "status": "active", "source": "local"}))
}

pub fn effective_access(_plan_record: &Value, _member: &Value) -> &'static str {
    "write"
}

pub fn is_active_status(status: Option<&Value>) -> bool {
    matches!(status.and_then(Value::as_str), Some("active") | Some("trialing") | Some("past_due"))
}

pub fn bound_repos(store: &Store, workspace_id: &str) -> BTreeSet<String> {
    let prefix = format!("{workspace_id}:");
    let mut keys: BTreeSet<String> = store.list_scopes(&prefix).iter().map(|s| crate::repo::repo_key(&s[prefix.len()..])).collect();
    keys.remove("");
    keys
}

pub fn repo_cap_error(_store: &Store, _workspace_id: &str, _repo_id: &str) -> Option<Value> {
    None
}

pub fn seat_limit(_store: &Store, _workspace_id: &str) -> Option<i64> {
    None
}

pub fn start_trial(store: &Store, workspace_id: &str, _days: i64, _source: &str) -> Value {
    plan_of(store, workspace_id)
}

pub fn usage_notice(_store: &Store, _workspace_id: &str) -> Option<String> {
    None
}

pub fn workspace_limit_for(_store: &Store, _owned: &[Value]) -> Option<i64> {
    None
}

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}
