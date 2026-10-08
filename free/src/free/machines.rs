//! The free version has no machine accounts (multi-machine sync); the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use serde_json::{json, Value};

use crate::store::Store;

/// What a machine's local record carries to a paid workspace when it
/// upgrades: the same list the cloud imports.
pub const SYNCED_KINDS: &[&str] = &[
    "edit_reported", "agent_message", "memory_saved", "intent_declared", "intent_completed",
    "brief_outcome", "map_answered", "deltas_delivered", "batch_applied", "collision_prevented",
    "claim_prevented", "setup_one_command", "land_saved",
];

fn not_here() -> Value {
    json!({"ok": false, "error": "machine accounts are part of Collide's cloud"})
}

pub fn plan_answer(_store: &Store, _workspace: &str) -> Value {
    not_here()
}

pub fn record(_store: &Store, _workspace: &str, _uid: &str, _email: &str, _body: &Value) -> Value {
    not_here()
}

pub fn import(_store: &Store, _aliases: &crate::repo::Aliases, _workspace: &str, _uid: &str, _allowed: &[String], _email: &str, _body: &Value) -> Value {
    not_here()
}

pub fn claim(_store: &Store, _public_url: &str, _body: &Value) -> Value {
    not_here()
}

pub fn free_install(_server: &str) -> Option<(i64, String)> {
    None
}

pub async fn free_script_now(_server: &str) -> Option<String> {
    None
}

pub fn offer_free(_store: &Store, _public_url: &str, _workspace: &str, _uid: &str, _email: &str, _name: &str) -> Option<Value> {
    None
}
