//! What the server's per-repo caches may hold, and what happens when the
//! process grows anyway.
//!
//! A repo's map is held in memory while agents use it: its records
//! (codegraph.rs), its name index, and its assembled graph (graphview.rs).
//! Each part reports its size here. Past the budget, the repos used least
//! recently leave memory (their next use rebuilds them from disk), so the
//! caches have a ceiling however many big repos are active at once.
//!
//! The watchdog is the backstop: it reads the process's resident memory
//! every few seconds, and past the soft limit it sheds every cache, hands
//! freed memory back to the system, and tells the founders by email (once
//! every six hours at most). A bad day becomes slower briefings, not a
//! process killed at its memory limit.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use crate::store::{now, Store};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Part {
    Records,
    Index,
    Graph,
}

#[derive(Default)]
struct Entry {
    bytes: HashMap<Part, usize>,
    last_used: f64,
}

impl Entry {
    fn total(&self) -> usize {
        self.bytes.values().sum()
    }
}

fn entries() -> &'static Mutex<HashMap<String, Entry>> {
    static E: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

const MB: usize = 1024 * 1024;

/// The container's memory limit, where there is one to read (cgroup v2,
/// then v1). `None` on a machine with no limit set, or not on Linux.
fn memory_limit() -> Option<usize> {
    for path in ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(bytes) = raw.trim().parse::<usize>() {
                // v1 reports "no limit" as a huge number
                if bytes > 0 && bytes < (1usize << 50) {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

fn env_mb(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<usize>().ok()).map(|mb| mb * MB)
}

/// What the repo caches may hold in all: `COLLIDE_CACHE_BUDGET_MB`, else a
/// quarter of the container's limit up to 6 GB, else 3 GB.
pub fn budget() -> usize {
    static B: OnceLock<usize> = OnceLock::new();
    *B.get_or_init(|| {
        env_mb("COLLIDE_CACHE_BUDGET_MB")
            .or_else(|| memory_limit().map(|limit| (limit / 4).min(6 * 1024 * MB)))
            .unwrap_or(3 * 1024 * MB)
    })
}

/// Past this the watchdog sheds: `COLLIDE_MEMORY_SOFT_MB`, else half the
/// container's limit. `None`: no limit known, no watchdog.
pub fn soft_limit() -> Option<usize> {
    static S: OnceLock<Option<usize>> = OnceLock::new();
    *S.get_or_init(|| env_mb("COLLIDE_MEMORY_SOFT_MB").or_else(|| memory_limit().map(|limit| limit / 2)))
}

/// This repo's caches were just used.
pub fn touch(key: &str) {
    if let Ok(mut e) = entries().lock() {
        e.entry(key.to_string()).or_default().last_used = now();
    }
}

/// One part of a repo's caches now holds `bytes`. Past the budget, the
/// repos used least recently (never this one) leave memory.
pub fn report(key: &str, part: Part, bytes: usize) {
    let evict: Vec<String> = {
        let Ok(mut e) = entries().lock() else { return };
        let entry = e.entry(key.to_string()).or_default();
        entry.bytes.insert(part, bytes);
        entry.last_used = now();
        let mut total: usize = e.values().map(Entry::total).sum();
        if total <= budget() {
            return;
        }
        let mut by_age: Vec<(f64, String, usize)> = e
            .iter()
            .filter(|(k, _)| k.as_str() != key)
            .map(|(k, v)| (v.last_used, k.clone(), v.total()))
            .collect();
        by_age.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::new();
        for (_, k, size) in by_age {
            if total <= budget() {
                break;
            }
            total = total.saturating_sub(size);
            e.remove(&k);
            out.push(k);
        }
        out
    };
    for k in &evict {
        drop_key(k);
    }
    if !evict.is_empty() {
        EVICTED.fetch_add(evict.len() as u64, Ordering::Relaxed);
        tracing::info!("memory: cache budget reached; {} repo(s) left memory", evict.len());
    }
}

fn drop_key(key: &str) {
    crate::codegraph::evict_key(key);
    crate::graphview::evict_key(key);
}

/// Repos unused since `cutoff`, taken off the books (their caches are
/// dropped by the caller).
pub fn idle_keys(cutoff: f64) -> Vec<String> {
    let Ok(mut e) = entries().lock() else { return Vec::new() };
    let idle: Vec<String> = e.iter().filter(|(_, v)| v.last_used < cutoff).map(|(k, _)| k.clone()).collect();
    for k in &idle {
        e.remove(k);
    }
    idle
}

/// What the repo caches hold now, by their own reckoning.
pub fn held() -> usize {
    entries().lock().map(|e| e.values().map(Entry::total).sum()).unwrap_or(0)
}

static EVICTED: AtomicU64 = AtomicU64::new(0);
static SHEDS: AtomicU64 = AtomicU64::new(0);
static LAST_ALERT: AtomicU64 = AtomicU64::new(0);
static PEAK_RSS: AtomicU64 = AtomicU64::new(0);

/// The process's resident memory, in bytes. Linux only.
pub fn rss() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Every cache this process can rebuild from disk, dropped; freed memory
/// returned to the system. Returns how many repos left memory.
pub fn shed_all(store: &Store) -> usize {
    let repos = crate::codegraph::evict_all() + crate::graphview::evict_all();
    if let Ok(mut e) = entries().lock() {
        e.clear();
    }
    store.shed_caches();
    crate::embed::shed_texts();
    give_back();
    repos
}

/// Hand memory the allocator holds free back to the system.
fn give_back() {
    #[cfg(feature = "cloud")]
    // SAFETY: mi_collect only walks the allocator's own free lists
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

/// The watchdog: every few seconds, the process's memory against the soft
/// limit. Started once, on Linux, when a limit is known.
pub fn start_watchdog(store: Arc<Store>) {
    let Some(soft) = soft_limit() else { return };
    if rss().is_none() {
        return;
    }
    std::thread::Builder::new()
        .name("collide-memory".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let Some(resident) = rss() else { continue };
            PEAK_RSS.fetch_max(resident as u64, Ordering::Relaxed);
            if resident < soft {
                continue;
            }
            let repos = shed_all(&store);
            SHEDS.fetch_add(1, Ordering::Relaxed);
            let after = rss().unwrap_or(resident);
            tracing::warn!(
                "memory: {} MB resident past the {} MB soft limit; shed every cache ({repos} repo(s)), now {} MB",
                resident / MB, soft / MB, after / MB
            );
            alert(resident, soft, after, repos);
        })
        .ok();
}

/// Tell the founders, at most once every six hours.
fn alert(resident: usize, soft: usize, after: usize, repos: usize) {
    let stamp = now() as u64;
    let last = LAST_ALERT.load(Ordering::Relaxed);
    if stamp.saturating_sub(last) < 6 * 3600 || LAST_ALERT.compare_exchange(last, stamp, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    #[cfg(feature = "cloud")]
    {
        if !crate::email::configured() {
            return;
        }
        let to: Vec<String> = std::env::var("COLLIDE_ADMIN_EMAILS")
            .unwrap_or_default()
            .split(',')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        let host = std::env::var("COLLIDE_ENGINE_ID").unwrap_or_else(|_| "collide-server".into());
        let subject = format!("Collide server memory: {} MB, past its {} MB soft limit", resident / MB, soft / MB);
        let body = format!(
            "{host} reached {} MB of memory (soft limit {} MB). It dropped every cache it can rebuild ({repos} repo map(s)) \
and is at {} MB now. Agents keep working; briefings for the dropped repos are slower until their maps are rebuilt.\n\n\
The cache budget is {} MB. If this repeats, look at which repos are largest (/health, memory).",
            resident / MB, soft / MB, after / MB, budget() / MB
        );
        std::thread::spawn(move || {
            for address in to {
                let _ = crate::email::send(&address, &subject, &format!("<p>{}</p>", body.replace('\n', "<br>")), &body, "");
            }
        });
    }
    #[cfg(not(feature = "cloud"))]
    let _ = (resident, soft, after, repos);
}

/// For /health.
pub fn health() -> Value {
    let largest: Vec<Value> = entries()
        .lock()
        .map(|e| {
            let mut rows: Vec<(usize, String)> = e.iter().map(|(k, v)| (v.total(), k.clone())).collect();
            rows.sort_by(|a, b| b.0.cmp(&a.0));
            rows.into_iter()
                .take(5)
                .map(|(bytes, key)| json!({"repo": key.split_once('#').map(|(_, s)| s).unwrap_or(&key), "mb": bytes / MB}))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "rss_mb": rss().map(|b| b / MB),
        "peak_rss_mb": PEAK_RSS.load(Ordering::Relaxed) as usize / MB,
        "soft_limit_mb": soft_limit().map(|b| b / MB),
        "repo_caches_mb": held() / MB,
        "cache_budget_mb": budget() / MB,
        "evicted": EVICTED.load(Ordering::Relaxed),
        "sheds": SHEDS.load(Ordering::Relaxed),
        "largest": largest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn past_the_budget_the_least_recently_used_repos_leave_never_the_one_in_use() {
        let gb = 1024 * MB;
        let n = budget() / gb + 2;
        for i in 0..n {
            report(&format!("test#ws:repo{i}"), Part::Graph, gb);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let held_now = held();
        assert!(held_now <= budget(), "{held_now} > {}", budget());
        let keys: Vec<String> = entries().lock().unwrap().keys().cloned().collect();
        assert!(keys.contains(&format!("test#ws:repo{}", n - 1)), "the newest stays");
        assert!(!keys.contains(&"test#ws:repo0".to_string()), "the oldest left");
    }
}
