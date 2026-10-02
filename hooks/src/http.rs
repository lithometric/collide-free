//! The one network call each hook makes, with the same hard budget the
//! Python hooks enforce: any error, timeout or non-200 is swallowed and the
//! agent proceeds. Reporting must never disturb the work it reports on.

use std::time::Duration;

use serde_json::Value;

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

/// POST JSON, returning the decoded body. `Err(())` means "could not talk to
/// the server" — every caller treats that as allow / skip.
pub fn post(
    server: &str,
    endpoint: &str,
    token: &str,
    user_agent: &str,
    payload: &Value,
    remaining: Duration,
) -> Result<Value, ()> {
    if remaining.is_zero() {
        return Err(());
    }
    let response = agent(remaining)
        .post(&format!("{server}{endpoint}"))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", user_agent)
        .send_string(&payload.to_string())
        .map_err(|_| ())?;
    if response.status() != 200 {
        return Err(());
    }
    let body = response.into_string().map_err(|_| ())?;
    Ok(serde_json::from_str(&body).unwrap_or(Value::Object(Default::default())))
}
