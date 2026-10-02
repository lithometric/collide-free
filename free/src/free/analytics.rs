//! The free version has no product analytics; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use serde_json::Value;

use crate::store::Store;

pub fn backfill(_store: &Store) {}
pub fn capture(_distinct_id: &str, _event: &str, _workspace: &str, _properties: Value) {}
pub fn capture_once(_store: &Store, _key: &str, _distinct_id: &str, _event: &str, _workspace: &str, _properties: Value) {}
pub fn capture_daily(_store: &Store, _distinct_id: &str, _event: &str, _workspace: &str, _properties: Value) {}
pub fn signed_up(_store: &Store, _uid: &str, _email: &str, _name: &str, _via: &str) {}
