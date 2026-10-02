//! The free version has no rate limits; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use crate::store::Store;

pub fn allow_call(_store: &Store, _workspace: &str) -> bool {
    true
}

pub fn allow_embed(_store: &Store, _scope: &str, _texts: usize) -> bool {
    true
}
