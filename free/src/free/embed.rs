//! The free version has no embedding model: prompts are matched to code by the names they use; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::store::Store;

/// The id a file's own (module-level) entry goes by.
pub const MODULE: &str = "<module>";

pub fn health() -> Option<Value> {
    None
}

pub fn start(_store: Arc<Store>) {}

pub fn evict_idle(_idle_s: f64) -> usize {
    0
}

pub fn shed_texts() -> usize {
    0
}

pub fn persist_now() -> usize {
    0
}

pub fn enqueue(_scope: &str, _path: &str, _doc: &str, _symbols: &Map<String, Value>, _live: bool) {}

pub fn pending_for(_scope: &str) -> usize {
    0
}

pub fn prompt_identifiers(
    _store: &Store, _scope: &str, _prompt: &str, _k: usize, _task: Option<(&str, &str)>,
) -> (Vec<String>, Vec<Value>) {
    (Vec::new(), Vec::new())
}

pub fn reuse_candidates(_store: &Store, _scope: &str, _prompt: &str, _limit: usize) -> Vec<Value> {
    Vec::new()
}

pub(crate) fn query_vector(_text: &str) -> Option<Vec<f32>> {
    None
}

pub(crate) fn encode_vec(_v: &[f32]) -> String {
    String::new()
}

pub(crate) fn decode_vec(_s: &str) -> Vec<f32> {
    Vec::new()
}

pub(crate) fn cosine(_a: &[f32], _b: &[f32]) -> f32 {
    0.0
}

pub async fn serve_embedder() {}
