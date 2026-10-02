//! The free version has no one-command cloud setup; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use serde_json::Value;

use crate::store::Store;

pub fn offer(_store: &Store, _mode: &str, _can_write: bool, _req: &crate::blocks::ArtifactRequest) -> Option<Value> {
    None
}
