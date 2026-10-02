//! The free version has no release serving; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

use serde_json::Value;

pub fn available() -> Vec<Value> {
    Vec::new()
}

pub fn release_tag() -> String {
    format!("hooks-v{}", crate::envelope::HOOK_ARTIFACT_VERSION)
}

pub fn sync_enabled() -> bool {
    false
}

pub async fn sync_from_github() -> Result<usize, String> {
    Ok(0)
}
