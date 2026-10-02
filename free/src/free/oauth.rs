//! The free version has no OAuth sign-in; the cloud's lives in Collide's private
//! repo. These are its free answers, under the same names, so the engine
//! builds and runs on one machine without it.

//! The local server is reached with the token beside it, never by sign-in.

pub fn mcp_at_root(_mcp_url: &str, _host: &str) -> bool {
    false
}

pub fn unauthorized_mcp_response(_base: &str, _at_root: bool) -> (u16, Vec<(&'static str, String)>, &'static [u8]) {
    (401, vec![("content-type", "application/json".to_string())], br#"{"error":"unauthorized"}"#)
}
