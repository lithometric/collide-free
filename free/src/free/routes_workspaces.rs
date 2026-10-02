//! The free version has no dashboard or admin routes; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::sync::Arc;

pub fn router() -> axum::Router<Arc<crate::App>> {
    axum::Router::new()
}
