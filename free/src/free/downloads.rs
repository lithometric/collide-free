//! The free version has no download counting; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use crate::store::Store;

pub fn client_ip(_headers: &axum::http::HeaderMap) -> String {
    String::new()
}

pub fn record(_store: &Store, _kind: &str, _target: &str, _ip: &str) {}
