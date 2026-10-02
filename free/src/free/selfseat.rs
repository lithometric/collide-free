//! The free version has no self-bought seats; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use serde_json::Value;

pub fn is_self_paid(_member: &Value) -> bool {
    false
}
