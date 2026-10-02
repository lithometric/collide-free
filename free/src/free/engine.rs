//! The free version has no durable engine (Postgres log, object storage snapshots); the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

//! It is never started here, so it has no values: nothing can reach these.

use std::sync::Arc;

use serde_json::Value;

use crate::store::Store;

pub type Shipped = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

pub fn configured() -> Option<String> {
    None
}

pub fn prepare(_db_path: &std::path::Path, _seed: Option<&std::path::Path>) -> Result<String, String> {
    Err("the free version keeps its data on this machine".into())
}

pub enum Engine {}

impl Engine {
    pub fn start(_store: Arc<Store>) -> Option<Arc<Engine>> {
        None
    }
    pub fn ready(&self) -> bool {
        match *self {}
    }
    pub fn is_writer(&self) -> bool {
        match *self {}
    }
    pub fn acked(&self) -> i64 {
        match *self {}
    }
    pub fn wait_writable(&self) {
        match *self {}
    }
    pub fn enqueue(&self, _seq: i64, _changes: Vec<u8>) -> Shipped {
        match *self {}
    }
    pub fn wait_acked(&self, _seq: i64) {
        match *self {}
    }
    pub async fn wait_acked_async(&self, _seq: i64) {
        match *self {}
    }
    pub fn wait_shipped(&self, _done: &Shipped) {
        match *self {}
    }
    pub fn release(&self, _store: &Store) {
        match *self {}
    }
    pub fn view(&self, _store: &Store) -> Value {
        match *self {}
    }
}
