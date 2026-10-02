//! Things that exist only to agree with Python.
//!
//! Both servers write the same rows and answer the same agents, so anywhere
//! Python's formatting is observable — a number in a stored record, a list
//! inside an error sentence an agent reads — this side has to produce the
//! same characters. These are not helpers for convenience; each one is here
//! because the obvious Rust spelling quietly disagrees.

use serde_json::Value;

/// Python's `round(value, places)`.
///
/// Rust's `f64::round` breaks ties away from zero and Python's breaks them to
/// even, so the two disagree on exactly the values integer counts produce:
/// sixteen edits with three unchecked is a rate of 0.8125, which Python
/// reports as 0.812 and naive Rust as 0.813. Formatting is the fix rather
/// than a workaround — Rust's float formatter rounds to nearest with ties to
/// even over the true decimal value, which is what Python's `round` does,
/// including the cases where scaling by a power of ten introduces its own
/// error (`round(2.675, 2)` is 2.67, not 2.68, because 2.675 is really
/// 2.67499999...).
pub fn python_round(value: f64, places: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.places$}").parse().unwrap_or(value)
}

/// A fresh 12-hex-character id, the shape Python's `uuid4().hex[:12]` has.
///
/// The obvious spelling — hash the current nanosecond — is wrong here, and
/// quietly so. The clock does not advance once per call: five ids generated
/// in a tight loop came back as three distinct values, and ten thousand came
/// back as eight hundred. `complete_intent` writes up to five anchored
/// rationale notes in a loop and `mine_scars` mints in a loop too, so two of
/// them would land on the same key and one would silently overwrite the
/// other. Losing a note is worse than any cost of getting this right.
///
/// So: a strictly increasing counter guarantees no two ids from THIS process
/// ever collide, and a per-process random seed — `RandomState` is seeded by
/// the OS — keeps two processes sharing one database from lining up.
pub fn new_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static SEED: OnceLock<RandomState> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let tick = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = SEED.get_or_init(RandomState::new).build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(tick);
    let hash = hasher.finish();
    // 64 bits of hash, 48 of which survive the truncation to 12 hex
    format!("{hash:016x}")[..12].to_string()
}

/// Python's list repr, which these error strings are compared against —
/// single quotes, `, ` between items. Rust's `{:?}` uses double quotes.
pub fn python_list(items: &[&str]) -> String {
    format!("[{}]", items.iter().map(|item| format!("'{item}'")).collect::<Vec<_>>().join(", "))
}

/// Python's truth value for a JSON field.
///
/// Python spells "is this set" as `if payload.get(k):`, which is false for a
/// missing key, `None`, `false`, zero, and — the part that catches people —
/// an empty string, list or object. Rust's obvious spelling is a null check,
/// which disagrees on every one of those last three. Anywhere the Python side
/// tests a field for truthiness, this side has to test it the same way, or
/// the two disagree about whether a note was superseded or a tripwire fired.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// Python's `repr` of a string, which an agent reads inside an error
/// sentence. Single quotes normally; double quotes when the value itself
/// contains a single quote and no double, which is the rule CPython uses.
pub fn python_repr(value: &str) -> String {
    if value.contains('\'') && !value.contains('"') {
        format!("\"{value}\"")
    } else {
        format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rounds_ties_to_even_like_python() {
        // the rates integer counts actually produce
        assert_eq!(python_round(0.8125, 3), 0.812);
        assert_eq!(python_round(0.9375, 3), 0.938);
        assert_eq!(python_round(0.1875, 3), 0.188);
        // the percentages the summary sentence prints
        assert_eq!(python_round(82.5, 0), 82.0);
        assert_eq!(python_round(12.5, 0), 12.0);
        assert_eq!(python_round(87.5, 0), 88.0);
        // scaling by a power of ten would get these wrong in the other
        // direction, because the literal is not the decimal it looks like
        assert_eq!(python_round(2.675, 2), 2.67);
        assert_eq!(python_round(0.6000000000000001, 2), 0.6);
    }

    #[test]
    fn ids_are_distinct_in_a_tight_loop() {
        // the shape complete_intent and mine_scars both use. The clock alone
        // could not tell these apart.
        let ids: std::collections::HashSet<String> = (0..10_000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 10_000, "an id that repeats silently overwrites a note");
        assert!(ids.iter().all(|id| id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit())));
    }

    #[test]
    fn writes_pythons_list_repr() {
        assert_eq!(python_list(&["add", "delete"]), "['add', 'delete']");
        assert_eq!(python_list(&[]), "[]");
    }

    #[test]
    fn matches_pythons_truth_value() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!(0))));
        // the three a null check gets wrong
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&json!([]))));
        assert!(!truthy(Some(&json!({}))));
        assert!(truthy(Some(&json!("set"))));
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!({"ts": 1})))); 
    }

    #[test]
    fn writes_pythons_string_repr() {
        assert_eq!(python_repr("bob@example.com"), "'bob@example.com'");
        assert_eq!(python_repr("o'brien"), "\"o'brien\"");
        assert_eq!(python_repr("say \"hi\""), "'say \"hi\"'");
    }
}
