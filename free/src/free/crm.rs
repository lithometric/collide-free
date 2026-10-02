//! The free version has no CRM bridge; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use crate::store::Store;

#[allow(clippy::too_many_arguments)]
pub fn sync_deferred(
    _store: &std::sync::Arc<Store>, _workspace: &str, _scope: &str, _repo_id: &str, _user: &str, _agent: &str,
    _note: &str, _paths: &[String], _symbols: &[String], _via: &str,
) {
}
