//! How long the hot routes take inside this server — handler time, network
//! excluded — per route, over the current minute and the last full one.
//! Load tests run from far away, where the round trip hides the server's own
//! share; this is the number that says which side a slow call came from.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

/// Bucket upper bounds, in milliseconds; the last is everything above.
const BOUNDS_MS: [f64; 14] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, f64::INFINITY];

#[derive(Clone, Default)]
struct Histogram {
    counts: [u64; 14],
    max_ms: f64,
}

impl Histogram {
    fn add(&mut self, ms: f64) {
        let at = BOUNDS_MS.iter().position(|b| ms <= *b).unwrap_or(BOUNDS_MS.len() - 1);
        self.counts[at] += 1;
        self.max_ms = self.max_ms.max(ms);
    }

    /// The bucket bound under which `q` of the calls fell.
    fn quantile(&self, q: f64) -> f64 {
        let total: u64 = self.counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let want = (total as f64 * q).ceil() as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return if BOUNDS_MS[i].is_finite() { BOUNDS_MS[i] } else { self.max_ms };
            }
        }
        self.max_ms
    }

    fn view(&self) -> Value {
        let total: u64 = self.counts.iter().sum();
        json!({"n": total, "p50_ms": self.quantile(0.5), "p95_ms": self.quantile(0.95), "p99_ms": self.quantile(0.99), "max_ms": (self.max_ms * 10.0).round() / 10.0})
    }
}

struct Window {
    minute: u64,
    current: HashMap<String, Histogram>,
    last: HashMap<String, Histogram>,
}

fn window() -> &'static Mutex<Window> {
    static W: std::sync::OnceLock<Mutex<Window>> = std::sync::OnceLock::new();
    W.get_or_init(|| Mutex::new(Window { minute: 0, current: HashMap::new(), last: HashMap::new() }))
}

fn roll(w: &mut Window, minute: u64) {
    if minute != w.minute {
        w.last = if minute == w.minute + 1 { std::mem::take(&mut w.current) } else { HashMap::new() };
        w.current.clear();
        w.minute = minute;
    }
}

pub fn record(route: &str, ms: f64) {
    let minute = (crate::store::now() / 60.0) as u64;
    let mut w = window().lock().unwrap_or_else(|e| e.into_inner());
    roll(&mut w, minute);
    w.current.entry(route.to_string()).or_default().add(ms);
}

pub fn view() -> Value {
    let minute = (crate::store::now() / 60.0) as u64;
    let mut w = window().lock().unwrap_or_else(|e| e.into_inner());
    roll(&mut w, minute);
    let pack = |m: &HashMap<String, Histogram>| -> Value {
        let mut routes: Vec<(&String, &Histogram)> = m.iter().collect();
        routes.sort_by(|a, b| a.0.cmp(b.0));
        Value::Object(routes.into_iter().map(|(k, h)| (k.clone(), h.view())).collect())
    };
    json!({"this_minute": pack(&w.current), "last_minute": pack(&w.last)})
}

/// Time the steps of one handler: each `mark` records the time since the
/// previous one as `<handler>:<step>`, beside the routes.
pub struct Phases {
    handler: &'static str,
    at: std::time::Instant,
}

impl Phases {
    pub fn new(handler: &'static str) -> Self {
        Phases { handler, at: std::time::Instant::now() }
    }

    pub fn mark(&mut self, step: &str) {
        let now = std::time::Instant::now();
        record(&format!("{}:{step}", self.handler), (now - self.at).as_secs_f64() * 1000.0);
        self.at = now;
    }
}
