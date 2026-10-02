//! The free version has no shared-login detection; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::store::Store;

pub fn logins(_store: &Store, _workspace: &str, _users: &BTreeSet<String>, _now: f64) -> Value {
    json!({})
}

pub fn note(_store: &Store, _workspace: &str, _user: &str, _machine: &str, _now: f64) {}

pub fn notice(_store: &Store, _scope: &str, _workspace: &str, _user: &str, _session: &str, _billing_url: &str, _now: f64) -> Option<String> {
    None
}
