//! The three storage tiers over the same SQLite file the Python server uses.
//!
//! Schema, key shapes and JSON encodings are deliberately identical: the two
//! servers can run against one database during the migration, and a row
//! written by either is readable by the other. That is what makes moving an
//! endpoint at a time safe.
//!
//! Concurrency: one WRITER connection behind a lock (which is what makes the
//! ledger's read-head-then-insert race-free) and a pool of READER
//! connections, which WAL lets run beside the writer and each other. Every
//! hook call made about fifteen small reads, and with one connection for all
//! of them the server topped out near 600 requests a second while its cores
//! sat idle waiting on the lock.
//!
//! When this process is the database's only writer (production; see
//! [`Store::open`]), three things also live in memory, exactly:
//! - the ephemeral tier (presence, markers, notices): read from memory,
//!   written through to disk in coalesced batches, reloaded at startup;
//! - small kv rows, cached write-through, so a sign-in check or a plan
//!   lookup is a map read;
//! - prefix scans and stats (`kv_list`, `kv_stat`, `list_scopes`), cached
//!   against a version that every write under that prefix moves.
//! The parity tests run the Python server on the same file, so a second
//! writer can exist there: `COLLIDE_DB_SHARED=1` turns every cache off.

use std::path::Path;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::hashing::chain_hash;

tokio::task_local! {
    /// The highest log position the current request wrote: the request
    /// answers once the log has it (the `durable` middleware), so a request
    /// that writes twenty-five times waits for the log once, not per write.
    pub static DURABLE: std::cell::Cell<i64>;
}

tokio::task_local! {
    /// Which address the current request came in on — "host path" — set
    /// around every hot-route handler (the `via_scope` middleware) and every
    /// MCP tool dispatch, the way Python's `_VIA` contextvar is set at the
    /// top of every hook request and tool call. `ledger_append` stamps it
    /// onto every row as `via`, so both halves' rows answer "who is still on
    /// api.collidemcp.com/mcp vs the mcp. root". Dashboard routes set none,
    /// and Python's `record()` stamps none there either.
    pub static VIA: String;
}

/// The current request's via tag, or "" outside any request scope (a
/// background task, the CLI, a test).
pub fn current_via() -> String {
    VIA.try_with(|via| via.clone()).unwrap_or_default()
}

/// Python's `record()`: `{**payload, "via": tag}` when a tag is known and
/// the row does not carry one already — appended last, as Python's dict
/// merge leaves it.
fn stamp_via(payload: &Value) -> Value {
    let via = current_via();
    if via.is_empty() {
        return payload.clone();
    }
    match payload {
        Value::Object(map) if !map.contains_key("via") => {
            let mut stamped = map.clone();
            stamped.insert("via".into(), crate::presence::via_view(&via));
            Value::Object(stamped)
        }
        other => other.clone(),
    }
}

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub const SCHEMA_SQL: &str = SCHEMA;

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
CREATE TABLE IF NOT EXISTS ephem (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    expires REAL
);
CREATE TABLE IF NOT EXISTS shared_files (
    scope TEXT NOT NULL,
    user TEXT NOT NULL,
    path TEXT NOT NULL,
    record TEXT NOT NULL,
    updated REAL NOT NULL,
    PRIMARY KEY (scope, user, path)
);
CREATE INDEX IF NOT EXISTS idx_shared_files_path ON shared_files (scope, path);
CREATE TABLE IF NOT EXISTS shared_trees (
    scope TEXT NOT NULL,
    user TEXT NOT NULL,
    tree TEXT NOT NULL,
    root TEXT NOT NULL,
    updated REAL NOT NULL,
    PRIMARY KEY (scope, user)
);
CREATE TABLE IF NOT EXISTS app_kv (
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated REAL NOT NULL,
    PRIMARY KEY (bucket, key)
);
CREATE TABLE IF NOT EXISTS ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    ts REAL NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ledger_scope_ts ON ledger (scope, ts);
CREATE TABLE IF NOT EXISTS engine_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS engine_outbox (
    seq INTEGER PRIMARY KEY,
    changes BLOB NOT NULL
);
"#;

/// The tables whose changes make up the engine's log (engine.rs): all of
/// the data, none of the engine's own bookkeeping.
pub const LOGGED_TABLES: [&str; 5] = ["ephem", "shared_files", "shared_trees", "app_kv", "ledger"];

// Phase 1 uses part of this surface; the rest is what the endpoints
// still to move across will call. Kept whole so the tier is reviewable
// against the Python driver side by side rather than in fragments.
#[allow(dead_code)]
pub struct Store {
    /// Live subscribers per scope: the dashboard's WebSocket feed. This is
    /// the half of the event path that is NOT shared between the two
    /// servers — a subscriber on one never hears an event published on the
    /// other — which is why an endpoint must be served by exactly one half.
    bus: std::sync::Mutex<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Value>>>,
    conn: Mutex<Connection>,
    /// Read-only connections beside the writer. Empty for an in-memory
    /// database, whose connections cannot see each other.
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    /// The in-memory tiers; `None` when another process may write the file.
    mem: Option<Mem>,
    /// Writes waiting for the writer; see [`Store::write`].
    queue: Mutex<Vec<WriteOp>>,
    /// The engine this store logs to, once it is running: every committed
    /// batch's changes are shipped through it before the batch's callers
    /// hear back. Unset, the store is a plain database.
    engine: std::sync::OnceLock<Arc<crate::engine::Engine>>,
    /// The last log sequence number this database holds.
    log_seq: std::sync::atomic::AtomicI64,
    /// Writes run, batches committed, and time spent holding the writer
    /// for them (ns): what /health reports as the writer's load.
    write_ops: std::sync::atomic::AtomicU64,
    write_batches: std::sync::atomic::AtomicU64,
    write_busy_ns: std::sync::atomic::AtomicU64,
    /// Per kind of write: how many, and the time they held the writer (ns).
    write_by_label: Mutex<HashMap<&'static str, (u64, u64)>>,
    /// Bytes written per kv bucket (rows, bytes): what the log carries.
    kv_bytes: Mutex<HashMap<String, (u64, u64)>>,
    /// History reads answered from memory, and from disk.
    tail_reads: std::sync::atomic::AtomicU64,
    disk_reads: std::sync::atomic::AtomicU64,
    /// The write-ahead log beside the database, when there is a file.
    wal: Option<std::path::PathBuf>,
    /// The database file, when there is one.
    path: Option<std::path::PathBuf>,
    /// A connection of its own for checkpoints, which then run beside the
    /// writer instead of inside its commits.
    checkpointer: Option<Mutex<Connection>>,
    /// The last verified link and verdict — see [`Store::ledger_verify_at`].
    chain_check: Mutex<Option<ChainCheck>>,
    /// This store's identity: in-memory caches key by it, so a cache can
    /// never answer for another database (tests open many at once). It moves
    /// when the rows are replaced underneath (a standby applying the log).
    id: std::sync::atomic::AtomicU64,
}

/// Where chain verification left off: the last sound row, when the whole
/// chain was last walked, and what that walk concluded.
struct ChainCheck {
    seq: i64,
    hash: String,
    full_at: f64,
    verdict: (bool, Option<i64>, i64),
}

/// What a queued write hands back once its batch is committed.
type Deliver = Box<dyn FnOnce() + Send>;
type WriteOp = (&'static str, Box<dyn FnOnce(&Store, &Connection) -> Deliver + Send>);
/// A committed batch: the log acknowledgement to await, then its results.
type Settle = (Option<crate::engine::Shipped>, Vec<Deliver>);

/// How often the ephemeral tier's changes reach disk. A crash loses at most
/// this much presence — markers that expire within minutes anyway.
pub const EPH_FLUSH_MS: u64 = 250;
/// Ephemeral entries living less than this are kept in memory only.
const EPH_PERSIST_MIN_TTL_S: f64 = 600.0;
/// How often the ephemeral tier's lasting entries are written.
const EPH_WRITE_EVERY_S: f64 = 1.0;
/// Past this the write-ahead log is checkpointed and truncated.
const WAL_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// A single write holding the writer this long is logged.
const SLOW_WRITE_MS: u64 = 50;
const SHARDS: usize = 32;
/// Rows larger than this are read from disk every time rather than cached:
/// the cache is for the many small lookups every call makes (a credential,
/// a membership, a plan), not for graph records.
const KV_CACHE_MAX_BYTES: usize = 8 * 1024;
const KV_CACHE_PER_SHARD: usize = 4096;
/// What the row cache may hold, in bytes of stored JSON, per shard: a cap
/// by count alone let 131k rows of up to 8 KB each (every map row of a big
/// repo, a second copy of it) stay resident.
const KV_CACHE_SHARD_BYTES: usize = 8 * 1024 * 1024;
const SCAN_CACHE_MAX_ROWS: usize = 256;
const SCAN_CACHE_ENTRIES: usize = 16_384;
/// What cached prefix scans may hold in all, roughly.
const SCAN_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Buckets read in bulk and kept by the graph's own cache (codegraph.rs):
/// caching their rows here too was a second copy of every map.
const UNCACHED_BUCKETS: [&str; 2] = ["graph", "graphrev"];

type EphShard = RwLock<BTreeMap<String, (Value, Option<f64>)>>;
type KvShard = RwLock<HashMap<(String, String), Option<Value>>>;

/// The in-memory tiers of an exclusive store.
struct Mem {
    eph: Vec<EphShard>,
    /// Ephemeral keys changed since the last flush: the new row, or `None`
    /// for a delete. Coalesced, so a marker set fifty times is written once.
    dirty: Mutex<HashMap<String, Option<(String, Option<f64>)>>>,
    kv: Vec<KvShard>,
    /// Bytes each row-cache shard holds (an over-count after removals,
    /// which only clears a shard sooner).
    kv_shard_bytes: Vec<AtomicU64>,
    scan_bytes: AtomicU64,
    /// Write versions: per bucket, and per (bucket, key head), where the
    /// head is the key through its second ':' — a scope. A cached scan or
    /// stat is current while the version it was filled under still stands.
    versions: RwLock<HashMap<String, u64>>,
    scans: Mutex<HashMap<(String, String), (u64, std::sync::Arc<Vec<(String, Value)>>)>>,
    stats: Mutex<HashMap<(String, String), (u64, (i64, f64))>>,
    /// Every scope with a tree row: `list_scopes` answers from here.
    scopes: RwLock<BTreeSet<String>>,
    /// Counter increments not yet on disk: (bucket, key, field) -> by.
    counts: Mutex<HashMap<(String, String, String), i64>>,
    /// Each busy repo's recent history, so the reads every hook call makes
    /// ("what changed since my last step") never touch disk.
    tails: RwLock<HashMap<String, std::sync::Arc<RwLock<Tail>>>>,
    /// The chain's head hash, so an append does not read it back first.
    head: Mutex<Option<String>>,
    /// Trees written since the last tree flush, newest per (scope, user):
    /// (tree, root, updated). A tree is every file's hash for one person in
    /// one repo, rewritten whole on every edit — 53 KB for a 654-file repo,
    /// about 85% of all bytes written — so it reaches disk every
    /// [`TREE_FLUSH_S`] instead, and reads see these first.
    trees: RwLock<HashMap<(String, String), (Value, String, f64)>>,
    trees_dirty: Mutex<std::collections::HashSet<(String, String)>>,
    trees_flushed: Mutex<f64>,
}

/// How often written trees reach disk.
pub const TREE_FLUSH_S: f64 = 2.0;

/// How far back a repo's in-memory history reaches. A read further back
/// than this goes to disk.
pub const TAIL_WINDOW_S: f64 = 3600.0;
/// The most rows one repo keeps in memory; past it the oldest go and the
/// window starts later.
const TAIL_MAX_ROWS: usize = 50_000;

/// One repo's recent rows. Every row with `ts >= from_ts` is here: the
/// rows loaded when the tail was made, plus every row appended since (the
/// append runs under the writer, and so did the load).
///
/// Rows are in seq order, and their timestamps nearly so. Each carries the
/// running maximum of the timestamps up to it, which never decreases: a read
/// binary-searches that for its start, so it costs the rows it returns, not
/// the hour it could have scanned.
struct Tail {
    from_ts: f64,
    rows: std::collections::VecDeque<(f64, LedgerRow)>,
    /// When a read last came (f64 bits): atomic, so a read never needs the
    /// write lock — every agent's step reads the tail.
    used: std::sync::atomic::AtomicU64,
}

impl Tail {
    fn push(&mut self, row: LedgerRow) {
        let most = self.rows.back().map(|(most, _)| *most).unwrap_or(f64::MIN).max(row.ts);
        self.rows.push_back((most, row));
    }

    fn since(&self, ts: f64, kinds: Option<&[&str]>) -> Vec<LedgerRow> {
        let start = self.rows.partition_point(|(most, _)| *most < ts);
        self.rows
            .range(start..)
            .filter(|(_, row)| row.ts >= ts && kinds.map(|k| k.contains(&row.kind.as_str())).unwrap_or(true))
            .map(|(_, row)| row.clone())
            .collect()
    }
}

fn shard_of(key: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    (h.finish() as usize) % SHARDS
}

/// A key's head: through its second ':' when it has two, else the whole
/// key. Every key under a prefix with two ':' shares that prefix's head.
fn head(key: &str) -> Option<&str> {
    let first = key.find(':')?;
    let second = key[first + 1..].find(':')? + first + 1;
    Some(&key[..=second])
}

impl Mem {
    fn load(conn: &Connection) -> Mem {
        let cutoff = now();
        let eph: Vec<EphShard> = (0..SHARDS).map(|_| RwLock::new(BTreeMap::new())).collect();
        if let Ok(mut statement) = conn.prepare("SELECT key, value, expires FROM ephem") {
            if let Ok(rows) = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<f64>>(2)?))
            }) {
                for (key, value, expires) in rows.flatten() {
                    if expires.map(|at| at <= cutoff).unwrap_or(false) {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&value) {
                        eph[shard_of(&key)].write().unwrap().insert(key, (value, expires));
                    }
                }
            }
        }
        let mut scopes = BTreeSet::new();
        if let Ok(mut statement) = conn.prepare("SELECT DISTINCT scope FROM shared_trees") {
            if let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) {
                scopes.extend(rows.flatten());
            }
        }
        Mem {
            eph,
            dirty: Mutex::new(HashMap::new()),
            kv: (0..SHARDS).map(|_| RwLock::new(HashMap::new())).collect(),
            kv_shard_bytes: (0..SHARDS).map(|_| AtomicU64::new(0)).collect(),
            scan_bytes: AtomicU64::new(0),
            versions: RwLock::new(HashMap::new()),
            scans: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            scopes: RwLock::new(scopes),
            counts: Mutex::new(HashMap::new()),
            tails: RwLock::new(HashMap::new()),
            head: Mutex::new(None),
            trees: RwLock::new(HashMap::new()),
            trees_dirty: Mutex::new(std::collections::HashSet::new()),
            trees_flushed: Mutex::new(now()),
        }
    }

    fn eph_put(&self, key: &str, value: Value, expires: Option<f64>) {
        // a marker that lives minutes (presence, focus, editing) is made
        // again by the agent's next step: writing it to disk bought nothing
        // and, scattered over the table, cost a 4 KB page per row
        let lasting = expires.map(|at| at - now() >= EPH_PERSIST_MIN_TTL_S).unwrap_or(true);
        let text = lasting.then(|| value.to_string());
        self.eph[shard_of(key)].write().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), (value, expires));
        if let Some(text) = text {
            self.dirty.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), Some((text, expires)));
        }
    }

    fn eph_remove(&self, key: &str) {
        self.eph[shard_of(key)].write().unwrap_or_else(|e| e.into_inner()).remove(key);
        self.dirty.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), None);
    }

    /// The version a scan or stat under `prefix` is current against.
    fn version_for(&self, bucket: &str, prefix: &str) -> u64 {
        let key = match head(prefix) {
            Some(h) => format!("{bucket}\u{0}{h}"),
            None => bucket.to_string(),
        };
        self.versions.read().unwrap_or_else(|e| e.into_inner()).get(&key).copied().unwrap_or(0)
    }

    /// A write to `key` in `bucket`: move its versions, forget its row.
    fn touched(&self, bucket: &str, key: &str) {
        {
            let mut versions = self.versions.write().unwrap_or_else(|e| e.into_inner());
            *versions.entry(bucket.to_string()).or_insert(0) += 1;
            if let Some(h) = head(key) {
                *versions.entry(format!("{bucket}\u{0}{h}")).or_insert(0) += 1;
            }
        }
        self.kv[shard_of(key)].write().unwrap_or_else(|e| e.into_inner()).remove(&(bucket.to_string(), key.to_string()));
    }

    /// Cache a row read from disk — unless a write to it landed since
    /// `seen` was taken. The check runs under the shard lock that
    /// `touched` removes under, and `touched` moves the version first, so a
    /// row read before a write can never be cached after it.
    fn kv_remember(&self, bucket: &str, key: &str, value: Option<Value>, seen: u64, bytes: usize) {
        if UNCACHED_BUCKETS.contains(&bucket) {
            return;
        }
        let at = shard_of(key);
        let mut shard = self.kv[at].write().unwrap_or_else(|e| e.into_inner());
        if self.version_for(bucket, key) != seen {
            return;
        }
        let held = &self.kv_shard_bytes[at];
        if shard.len() >= KV_CACHE_PER_SHARD || held.load(Ordering::Relaxed) as usize + bytes > KV_CACHE_SHARD_BYTES {
            shard.clear();
            held.store(0, Ordering::Relaxed);
        }
        held.fetch_add((bytes + key.len() + 96) as u64, Ordering::Relaxed);
        shard.insert((bucket.to_string(), key.to_string()), value);
    }

    /// Everything a cache holds that the disk also has: the memory
    /// watchdog's first resort.
    fn shed(&self) {
        for (shard, held) in self.kv.iter().zip(&self.kv_shard_bytes) {
            shard.write().unwrap_or_else(|e| e.into_inner()).clear();
            held.store(0, Ordering::Relaxed);
        }
        self.scans.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.scan_bytes.store(0, Ordering::Relaxed);
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn cache_bytes(&self) -> u64 {
        self.kv_shard_bytes.iter().map(|b| b.load(Ordering::Relaxed)).sum::<u64>() + self.scan_bytes.load(Ordering::Relaxed)
    }
}

/// How long a verified prefix is trusted before the whole chain is walked
/// again. Python's `FULL_WALK_EVERY_S`.
pub const FULL_WALK_EVERY_S: f64 = 3600.0;

#[allow(dead_code)]
impl Store {
    /// Open (creating) the database. The in-memory tiers are on unless
    /// `COLLIDE_DB_SHARED=1` says another process writes the same file.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let shared = std::env::var("COLLIDE_DB_SHARED").map(|v| v.trim() == "1").unwrap_or(false);
        Self::open_with(path, !shared)
    }

    pub fn open_with(path: &Path, exclusive: bool) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // WAL with NORMAL sync: a commit is durable against a process crash
        // and fsyncs at checkpoints rather than on every write — the setting
        // SQLite recommends for WAL. The wait covers a checkpoint colliding
        // with the Python half in the parity tests.
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA cache_size=-65536; PRAGMA mmap_size=1073741824;")?;
        let in_memory = path.as_os_str() == ":memory:" || path.as_os_str().is_empty();
        let count = if in_memory {
            0
        } else {
            std::env::var("COLLIDE_DB_READERS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or_else(|| {
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 16)
            })
        };
        let mut readers = Vec::with_capacity(count);
        for _ in 0..count {
            let reader = Connection::open(path)?;
            reader.execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=1; PRAGMA cache_size=-32768; PRAGMA mmap_size=1073741824;")?;
            readers.push(Mutex::new(reader));
        }
        let mem = if exclusive { Some(Mem::load(&conn)) } else { None };
        let log_seq = read_log_seq(&conn);
        // SQLite checkpoints from inside a commit once the log passes 1,000
        // pages; with readers always active that checkpoint cannot finish and
        // is retried on every commit, which made each write cost ~2 ms under
        // load. The flusher checkpoints from its own connection instead.
        let checkpointer = if in_memory || !exclusive {
            None
        } else {
            conn.execute_batch("PRAGMA wal_autocheckpoint=0;")?;
            let c = Connection::open(path)?;
            c.execute_batch("PRAGMA busy_timeout=5000;")?;
            Some(Mutex::new(c))
        };
        let wal = (!in_memory).then(|| {
            let mut wal = path.as_os_str().to_owned();
            wal.push("-wal");
            std::path::PathBuf::from(wal)
        });
        Ok(Store {
            conn: Mutex::new(conn),
            readers,
            next_reader: AtomicUsize::new(0),
            mem,
            queue: Mutex::new(Vec::new()),
            engine: std::sync::OnceLock::new(),
            log_seq: std::sync::atomic::AtomicI64::new(log_seq),
            write_ops: std::sync::atomic::AtomicU64::new(0),
            write_batches: std::sync::atomic::AtomicU64::new(0),
            write_busy_ns: std::sync::atomic::AtomicU64::new(0),
            write_by_label: Mutex::new(HashMap::new()),
            kv_bytes: Mutex::new(HashMap::new()),
            tail_reads: std::sync::atomic::AtomicU64::new(0),
            disk_reads: std::sync::atomic::AtomicU64::new(0),
            wal,
            path: (!in_memory).then(|| path.to_path_buf()),
            checkpointer,
            bus: std::sync::Mutex::new(std::collections::HashMap::new()),
            chain_check: Mutex::new(None),
            id: std::sync::atomic::AtomicU64::new(NEXT_STORE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
        })
    }

    /// The writer. A panic while holding it poisons nothing that matters:
    /// every statement is its own transaction.
    fn writer(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run a write, group-committed: writes arriving together share one
    /// transaction. The caller queues its write and takes the writer; if an
    /// earlier holder already ran it, it returns at once, else it runs the
    /// whole queue — its own write and everyone's behind it — as one
    /// transaction. A result is handed back only after its batch commits,
    /// so a write that returned is on disk exactly as before; what changes
    /// is that a thousand agents' rows cost a few commits, not a thousand.
    fn write<R: Send + 'static>(&self, label: &'static str, op: impl FnOnce(&Store, &Connection) -> R + Send + 'static) -> R {
        // a standby's writes wait for the lease — before any lock is taken,
        // since the standby needs the writer to catch up to take it
        if let Some(engine) = self.engine.get() {
            engine.wait_writable();
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<R>(1);
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push((label, Box::new(move |store, conn| {
            let result = op(store, conn);
            Box::new(move || {
                let _ = tx.send(result);
            })
        })));
        loop {
            if let Ok(result) = rx.try_recv() {
                self.made_durable(self.log_seq());
                return result;
            }
            match self.conn.try_lock() {
                Ok(conn) => {
                    let pending = self.drain(&conn);
                    drop(conn);
                    // the log's acknowledgement is awaited with the writer
                    // free, so the next batch commits while this one ships
                    self.settle(pending);
                    // a write queued while we let go is taken by us too
                    self.drain_if_waiting();
                }
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    let conn = poisoned.into_inner();
                    let pending = self.drain(&conn);
                    drop(conn);
                    self.settle(pending);
                }
                // someone holds the writer and will take the queue: wait for
                // the result rather than queueing on the lock (hundreds of
                // threads each taking the lock only to find their write done
                // was the convoy that stalled every repo's edits)
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            if let Ok(result) = rx.recv_timeout(std::time::Duration::from_millis(1)) {
                self.made_durable(self.log_seq());
                return result;
            }
        }
    }

    /// Writes queued while the last holder was letting go of the writer.
    pub fn drain_if_waiting(&self) {
        while !self.queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            let Ok(conn) = self.conn.try_lock() else { return };
            let pending = self.drain(&conn);
            drop(conn);
            self.settle(pending);
        }
    }

    /// Hand each drained batch's results back once the log has the batch.
    /// Hand each drained batch's results back as soon as it is committed
    /// here; each caller then waits for the log itself (see `write`).
    fn settle(&self, pending: Vec<Settle>) {
        for (_shipped, delivers) in pending {
            for deliver in delivers {
                deliver();
            }
        }
    }

    /// A write this caller made is committed here at or below `seq`: inside
    /// a request, the request answers once the log has it; anywhere else
    /// (a background thread) the caller waits for the log now.
    fn made_durable(&self, seq: i64) {
        let Some(engine) = self.engine.get() else { return };
        if seq <= engine.acked() {
            return;
        }
        let noted = DURABLE.try_with(|mark| {
            if seq > mark.get() {
                mark.set(seq);
            }
        });
        if noted.is_err() {
            let at = std::time::Instant::now();
            engine.wait_acked(seq);
            let mut kinds = self.write_by_label.lock().unwrap_or_else(|e| e.into_inner());
            let entry = kinds.entry("(ship wait)").or_insert((0, 0));
            entry.0 += 1;
            entry.1 += at.elapsed().as_nanos() as u64;
        }
    }

    fn drain(&self, conn: &Connection) -> Vec<Settle> {
        let mut settled: Vec<Settle> = Vec::new();
        loop {
            let engine = self.engine.get().cloned();
            // a standby runs no writes: they stay queued for when it holds
            // the lease (never wait here, holding the writer)
            if engine.as_ref().is_some_and(|e| !e.is_writer()) {
                return settled;
            }
            let ops: Vec<WriteOp> = std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()));
            if ops.is_empty() {
                return settled;
            }
            let started = std::time::Instant::now();
            self.write_ops.fetch_add(ops.len() as u64, Ordering::Relaxed);
            self.write_batches.fetch_add(1, Ordering::Relaxed);
            let logged = engine.is_some();
            let batch = (logged || ops.len() > 1) && conn.is_autocommit() && conn.execute_batch("BEGIN IMMEDIATE").is_ok();
            // the batch's exact changes, recorded as they happen: what the
            // engine ships, and what a restore replays
            let mut session = if logged && batch { open_session(conn) } else { None };
            let delivers: Vec<Deliver> = ops
                .into_iter()
                .map(|(label, op)| {
                    let at = std::time::Instant::now();
                    let deliver = op(self, conn);
                    let took = at.elapsed();
                    if took > std::time::Duration::from_millis(SLOW_WRITE_MS) {
                        tracing::warn!("store: slow write {label}: {} ms", took.as_millis());
                    }
                    let mut slow = self.write_by_label.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = slow.entry(label).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 += took.as_nanos() as u64;
                    deliver
                })
                .collect();
            let mut shipped: Option<(i64, Vec<u8>)> = None;
            if let Some(mut open) = session.take() {
                let mut changes = Vec::new();
                let empty = open.is_empty();
                // a patchset: each row's new values only (a changeset also
                // carries every updated row's old values, twice the bytes)
                if !empty && open.patchset_strm(&mut changes).is_ok() && !changes.is_empty() {
                    drop(open);
                    let seq = self.log_seq.load(Ordering::SeqCst) + 1;
                    // durable here with the data itself: the outbox is what
                    // is shipped again if the process dies before Postgres has it
                    let saved = conn
                        .prepare_cached("INSERT INTO engine_outbox (seq, changes) VALUES (?, ?)")
                        .and_then(|mut st| st.execute(params![seq, changes]))
                        .and_then(|_| conn.prepare_cached(
                            "INSERT INTO engine_meta (key, value) VALUES ('seq', ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        ).and_then(|mut st| st.execute(params![seq.to_string()])));
                    if saved.is_ok() {
                        shipped = Some((seq, changes));
                    }
                }
                if let Some(engine) = &engine {
                    let acked = engine.acked();
                    if acked > 0 {
                        let _ = conn.prepare_cached("DELETE FROM engine_outbox WHERE seq <= ?").and_then(|mut st| st.execute(params![acked]));
                    }
                }
            }
            let commit_at = std::time::Instant::now();
            let committed = if batch {
                match conn.execute_batch("COMMIT") {
                    Ok(()) => true,
                    Err(_) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        self.forget_after_failed_commit();
                        false
                    }
                }
            } else {
                true
            };
            if batch {
                let mut kinds = self.write_by_label.lock().unwrap_or_else(|e| e.into_inner());
                let entry = kinds.entry("(commit)").or_insert((0, 0));
                entry.0 += 1;
                entry.1 += commit_at.elapsed().as_nanos() as u64;
            }
            let mut waiting = None;
            if let (true, Some((seq, changes)), Some(engine)) = (committed, shipped, &engine) {
                self.log_seq.store(seq, Ordering::SeqCst);
                // the callers hear back once the log has the batch (or the
                // engine's wait runs out, which it reports): see `settle`
                waiting = Some(engine.enqueue(seq, changes));
            }
            self.write_busy_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            settled.push((waiting, delivers));
        }
    }

    /// Committed batches the log may not have yet, oldest first.
    pub fn outbox(&self) -> Vec<(i64, Vec<u8>)> {
        let conn = self.reader();
        let Ok(mut statement) = conn.prepare("SELECT seq, changes FROM engine_outbox ORDER BY seq") else { return Vec::new() };
        statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// A consistent copy of the whole database at `to`, read beside the
    /// writer (a read transaction; writes carry on).
    pub fn copy_to(&self, to: &std::path::Path) -> rusqlite::Result<()> {
        let Some(path) = &self.path else { return Err(rusqlite::Error::InvalidPath(to.to_path_buf())) };
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        conn.execute("VACUUM INTO ?", [to.to_string_lossy().to_string()])?;
        Ok(())
    }

    /// Attach the engine: from now on every committed batch is logged.
    pub fn attach_engine(&self, engine: Arc<crate::engine::Engine>) {
        let _ = self.engine.set(engine);
    }

    /// Whether this process may write now (always, without an engine).
    pub fn writable(&self) -> bool {
        self.engine.get().map(|e| e.is_writer()).unwrap_or(true)
    }

    pub fn engine(&self) -> Option<Arc<crate::engine::Engine>> {
        self.engine.get().cloned()
    }

    pub fn log_seq(&self) -> i64 {
        self.log_seq.load(Ordering::SeqCst)
    }

    /// Changesets from the log that this database does not hold yet, applied
    /// in order under the writer (a standby catching up, a restore). Nothing
    /// applied here is logged again. Every in-memory tier is rebuilt after,
    /// since the rows moved underneath it.
    pub fn apply_log(&self, entries: &[(i64, Vec<u8>)]) -> rusqlite::Result<i64> {
        let applied = self.apply_log_quietly(entries)?;
        self.reload_memory();
        Ok(applied)
    }

    /// `apply_log` without rebuilding memory: a standby following a busy
    /// writer applies chunk after chunk and rebuilds once in a while.
    pub fn apply_log_quietly(&self, entries: &[(i64, Vec<u8>)]) -> rusqlite::Result<i64> {
        let conn = self.writer();
        let mut applied = self.log_seq();
        for (seq, changes) in entries {
            if *seq <= applied {
                continue;
            }
            apply_changeset(&conn, changes)?;
            conn.execute(
                "INSERT INTO engine_meta (key, value) VALUES ('seq', ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![seq.to_string()],
            )?;
            applied = *seq;
            self.log_seq.store(applied, Ordering::SeqCst);
        }
        Ok(applied)
    }

    /// Rebuild every in-memory tier from disk, and move the store's cache
    /// identity so process-wide caches keyed by it (graph records, snapshots,
    /// indexes) start over.
    pub fn reload_memory(&self) {
        if let Some(mem) = &self.mem {
            let fresh = Mem::load(&self.writer());
            for (i, shard) in fresh.eph.into_iter().enumerate() {
                *mem.eph[i].write().unwrap_or_else(|e| e.into_inner()) = shard.into_inner().unwrap_or_default();
            }
            for shard in &mem.kv {
                shard.write().unwrap_or_else(|e| e.into_inner()).clear();
            }
            mem.versions.write().unwrap_or_else(|e| e.into_inner()).clear();
            mem.scans.lock().unwrap_or_else(|e| e.into_inner()).clear();
            mem.stats.lock().unwrap_or_else(|e| e.into_inner()).clear();
            *mem.scopes.write().unwrap_or_else(|e| e.into_inner()) = fresh.scopes.into_inner().unwrap_or_default();
            mem.tails.write().unwrap_or_else(|e| e.into_inner()).clear();
            *mem.head.lock().unwrap_or_else(|e| e.into_inner()) = None;
            mem.trees.write().unwrap_or_else(|e| e.into_inner()).clear();
            mem.trees_dirty.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.id.store(NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// A batch that did not commit leaves memory ahead of disk: the chain
    /// head and every tail are re-read from disk on next use.
    fn forget_after_failed_commit(&self) {
        tracing::error!("store: a write batch failed to commit and was rolled back");
        if let Some(mem) = &self.mem {
            *mem.head.lock().unwrap_or_else(|e| e.into_inner()) = None;
            mem.tails.write().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// A reader: the first idle one, else wait on the next in turn. Falls
    /// back to the writer when there are none (an in-memory database).
    fn reader(&self) -> MutexGuard<'_, Connection> {
        if self.readers.is_empty() {
            return self.writer();
        }
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed);
        let n = self.readers.len();
        for i in 0..n {
            if let Ok(guard) = self.readers[(start + i) % n].try_lock() {
                return guard;
            }
        }
        self.readers[start % n].lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start the thread that writes the in-memory tiers to disk every
    /// [`EPH_FLUSH_MS`] and sweeps expired entries every minute.
    pub fn start_flusher(store: std::sync::Arc<Store>) {
        if store.mem.is_none() {
            return;
        }
        let _ = std::thread::Builder::new().name("collide-flush".into()).spawn(move || {
            let mut last_sweep = now();
            let mut last_eph_write = now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(EPH_FLUSH_MS));
                if !store.writable() {
                    continue;
                }
                if now() - last_eph_write >= EPH_WRITE_EVERY_S {
                    store.flush();
                    last_eph_write = now();
                } else {
                    store.flush_counts();
                }
                store.flush_trees(false);
                store.drain_if_waiting();
                store.checkpoint_if_long();
                if now() - last_sweep > 60.0 {
                    store.sweep_ephemeral();
                    store.trim_tails();
                    last_sweep = now();
                }
            }
        });
    }

    /// The writer's load since start: writes, batches (commits), and the
    /// seconds spent holding the writer for them.
    pub fn write_stats(&self) -> serde_json::Value {
        let ops = self.write_ops.load(Ordering::Relaxed);
        let batches = self.write_batches.load(Ordering::Relaxed);
        serde_json::json!({
            "writes": ops,
            "commits": batches,
            "busy_s": (self.write_busy_ns.load(Ordering::Relaxed) as f64 / 1e9 * 1000.0).round() / 1000.0,
            "readers": self.readers.len(),
            "kv_written": self.kv_bytes.lock().map(|m| m.iter().map(|(k, (n, b))| (k.clone(), serde_json::json!({"rows": n, "kb": b / 1024}))).collect::<serde_json::Map<_, _>>()).unwrap_or_default(),
            "history_reads": {"memory": self.tail_reads.load(Ordering::Relaxed), "disk": self.disk_reads.load(Ordering::Relaxed)},
            "by_kind": self.write_by_label.lock().map(|m| m.iter().map(|(k, (n, ns))| (k.to_string(), serde_json::json!({"n": n, "ms": ns / 1_000_000}))).collect::<serde_json::Map<_, _>>()).unwrap_or_default(),
            "memory_tiers": self.mem.is_some(),
        })
    }

    /// Whether the in-memory tiers are on.
    pub fn exclusive(&self) -> bool {
        self.mem.is_some()
    }

    /// Write the ephemeral tier's pending changes to disk now. The flusher
    /// does this every [`EPH_FLUSH_MS`]; shutdown calls it once more.
    /// Fold the held counter increments in.
    pub fn flush_counts(&self) {
        let Some(mem) = &self.mem else { return };
        if !self.writable() {
            return;
        }
        let counts: HashMap<(String, String, String), i64> =
            std::mem::take(&mut *mem.counts.lock().unwrap_or_else(|e| e.into_inner()));
        for ((bucket, key, field), by) in counts {
            self.apply_bump(&bucket, &key, &field, by);
        }
    }

    pub fn flush(&self) {
        let Some(mem) = &self.mem else { return };
        // a standby holds nothing of its own to write
        if !self.writable() {
            return;
        }
        self.flush_counts();
        // taken and written as one queued write, so two flushes land in order
        let dirty: HashMap<String, Option<(String, Option<f64>)>> =
            std::mem::take(&mut *mem.dirty.lock().unwrap_or_else(|e| e.into_inner()));
        if dirty.is_empty() {
            return;
        }
        let _ = self.write("flush_ephemeral", move |_, conn| -> rusqlite::Result<()> {
            for (key, entry) in &dirty {
                match entry {
                    Some((value, expires)) => {
                        conn.prepare_cached(
                            "INSERT INTO ephem (key, value, expires) VALUES (?, ?, ?)
                             ON CONFLICT(key) DO UPDATE SET value=excluded.value, expires=excluded.expires",
                        )?
                        .execute(params![key, value, expires])?;
                    }
                    None => {
                        conn.prepare_cached("DELETE FROM ephem WHERE key = ?")?.execute(params![key])?;
                    }
                }
            }
            Ok(())
        });
    }

    /// Add `by` to a numeric field of a kv row (created as `{field: by}`).
    /// An exclusive store holds the increment and folds it in at the next
    /// flush — a call counter touched by every request is then one write a
    /// quarter-second instead of one per call. [`Store::kv_pending`] is what
    /// is held; readers that need the exact count add it.
    pub fn kv_bump(&self, bucket: &str, key: &str, field: &str, by: i64) {
        if let Some(mem) = &self.mem {
            *mem.counts.lock().unwrap_or_else(|e| e.into_inner())
                .entry((bucket.to_string(), key.to_string(), field.to_string())).or_insert(0) += by;
            return;
        }
        self.apply_bump(bucket, key, field, by);
    }

    pub fn kv_pending(&self, bucket: &str, key: &str, field: &str) -> i64 {
        self.mem.as_ref().and_then(|mem| {
            mem.counts.lock().unwrap_or_else(|e| e.into_inner())
                .get(&(bucket.to_string(), key.to_string(), field.to_string())).copied()
        }).unwrap_or(0)
    }

    fn apply_bump(&self, bucket: &str, key: &str, field: &str, by: i64) {
        let mut record = self.kv_get(bucket, key).unwrap_or_else(|| serde_json::json!({}));
        let Some(map) = record.as_object_mut() else { return };
        let current = map.get(field).and_then(Value::as_i64).unwrap_or(0);
        map.insert(field.to_string(), serde_json::json!(current + by));
        let _ = self.kv_put(bucket, key, &record, now());
    }

    /// Keep the write-ahead log short. SQLite's own checkpoints copy pages
    /// back but can only restart the log when no reader is inside it, and
    /// with a pool of readers busy on every request that moment never
    /// comes: the log grew to 194 MB in 90 seconds of load and every read
    /// slowed with it (2,200 requests a second fell to 400). A TRUNCATE
    /// checkpoint waits the few milliseconds for the readers in flight to
    /// finish, then starts the log over.
    pub fn checkpoint_if_long(&self) -> bool {
        let Some(wal) = &self.wal else { return false };
        // copy committed pages back beside the writer; never blocks it
        if let Some(checkpointer) = &self.checkpointer {
            let c = checkpointer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get::<_, i64>(0));
        }
        let size = std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0);
        let limit = std::env::var("COLLIDE_WAL_LIMIT_MB").ok().and_then(|v| v.parse::<u64>().ok()).map(|mb| mb * 1024 * 1024).unwrap_or(WAL_LIMIT_BYTES);
        if size < limit {
            return false;
        }
        // the log only starts over once no reader is inside it: TRUNCATE
        // waits the milliseconds for those in flight, then resets it
        let conn = self.writer();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get::<_, i64>(0)).map(|busy| busy == 0).unwrap_or(false)
    }

    /// Drop expired ephemeral entries, in memory and on disk.
    pub fn sweep_ephemeral(&self) -> usize {
        let cutoff = now();
        let mut dropped = 0;
        if let Some(mem) = &self.mem {
            for shard in &mem.eph {
                let mut shard = shard.write().unwrap_or_else(|e| e.into_inner());
                let before = shard.len();
                shard.retain(|_, (_, expires)| expires.map(|at| at > cutoff).unwrap_or(true));
                dropped += before - shard.len();
            }
        }
        let _ = self.write("sweep_ephemeral", move |_, conn| conn.execute("DELETE FROM ephem WHERE expires IS NOT NULL AND expires <= ?", params![cutoff]));
        dropped
    }

    pub fn ping(&self) -> bool {
        self.reader().query_row("SELECT 1", [], |row| row.get::<_, i64>(0)).is_ok()
    }

    // ------------------------------------------------------------ ephemeral

    pub fn eph_set(&self, key: &str, value: &Value, ttl_s: Option<f64>) -> rusqlite::Result<()> {
        let expires = ttl_s.map(|ttl| now() + ttl);
        if let Some(mem) = &self.mem {
            mem.eph_put(key, value.clone(), expires);
            return Ok(());
        }
        let conn = self.writer();
        conn.execute(
            "INSERT INTO ephem (key, value, expires) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, expires=excluded.expires",
            params![key, value.to_string(), expires],
        )?;
        Ok(())
    }

    pub fn eph_get(&self, key: &str) -> Option<Value> {
        if let Some(mem) = &self.mem {
            let found = mem.eph[shard_of(key)].read().unwrap_or_else(|e| e.into_inner()).get(key).cloned();
            let (value, expires) = found?;
            if expires.map(|at| at <= now()).unwrap_or(false) {
                mem.eph_remove(key);
                return None;
            }
            return Some(value);
        }
        let conn = self.writer();
        let row: Option<(String, Option<f64>)> = conn
            .query_row("SELECT value, expires FROM ephem WHERE key = ?", params![key], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .ok()
            .flatten();
        let (value, expires) = row?;
        if expires.map(|at| at <= now()).unwrap_or(false) {
            let _ = conn.execute("DELETE FROM ephem WHERE key = ?", params![key]);
            return None;
        }
        serde_json::from_str(&value).ok()
    }

    pub fn eph_delete(&self, key: &str) {
        if let Some(mem) = &self.mem {
            mem.eph_remove(key);
            return;
        }
        let key = key.to_string();
        let _ = self.write("eph_delete", move |_, conn| conn.execute("DELETE FROM ephem WHERE key = ?", params![key]));
    }

    /// Prefix scan over the ephemeral table. GLOB with `[` escaped, byte for
    /// byte the Python driver's pattern, so a `_` or `%` in a scope (a repo
    /// id like `acme/api_v2`) is matched literally and never widens the read
    /// onto a sibling scope.
    pub fn eph_scan(&self, prefix: &str) -> Vec<(String, Value)> {
        if let Some(mem) = &self.mem {
            let cutoff = now();
            let mut out: Vec<(String, Value)> = Vec::new();
            for shard in &mem.eph {
                let shard = shard.read().unwrap_or_else(|e| e.into_inner());
                for (key, (value, expires)) in shard.range(prefix.to_string()..) {
                    if !key.starts_with(prefix) {
                        break;
                    }
                    if expires.map(|at| at > cutoff).unwrap_or(true) {
                        out.push((key.clone(), value.clone()));
                    }
                }
            }
            // the disk scan has no ORDER BY; sorted is a stable superset
            out.sort_by(|a, b| a.0.cmp(&b.0));
            return out;
        }
        let conn = self.reader();
        let mut statement = match conn
            .prepare("SELECT key, value, expires FROM ephem WHERE key GLOB ?1")
        {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let pattern = glob_prefix(prefix);
        let rows = statement.query_map(params![pattern], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<f64>>(2)?,
            ))
        });
        let Ok(rows) = rows else { return Vec::new() };
        let cutoff = now();
        rows.flatten()
            .filter(|(_, _, expires)| expires.map(|at| at > cutoff).unwrap_or(true))
            .filter_map(|(key, value, _)| serde_json::from_str(&value).ok().map(|v| (key, v)))
            .collect()
    }

    // ------------------------------------------------------------- shared kv

    pub fn kv_get(&self, bucket: &str, key: &str) -> Option<Value> {
        let seen = match &self.mem {
            Some(mem) => {
                let cached = mem.kv[shard_of(key)].read().unwrap_or_else(|e| e.into_inner())
                    .get(&(bucket.to_string(), key.to_string())).cloned();
                if let Some(value) = cached {
                    return value;
                }
                Some(mem.version_for(bucket, key))
            }
            None => None,
        };
        let conn = self.reader();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM app_kv WHERE bucket = ? AND key = ?",
                params![bucket, key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        drop(conn);
        let value: Option<Value> = raw.as_deref().and_then(|raw| serde_json::from_str(raw).ok());
        if let (Some(mem), Some(seen)) = (&self.mem, seen) {
            let bytes = raw.as_ref().map(String::len).unwrap_or(0);
            if bytes <= KV_CACHE_MAX_BYTES {
                mem.kv_remember(bucket, key, value.clone(), seen, bytes);
            }
        }
        value
    }

    /// A write under `bucket`/`key` landed: caches that could hold it go.
    fn kv_touched(&self, bucket: &str, key: &str) {
        if let Some(mem) = &self.mem {
            mem.touched(bucket, key);
        }
    }

    pub fn kv_put(&self, bucket: &str, key: &str, value: &Value, now_ts: f64) -> rusqlite::Result<()> {
        let result = self.kv_put_disk(bucket, key, value, now_ts);
        self.kv_touched(bucket, key);
        result
    }

    fn count_bytes(&self, bucket: &str, bytes: usize) {
        let mut m = self.kv_bytes.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(bucket.to_string()).or_insert((0, 0));
        e.0 += 1;
        e.1 += bytes as u64;
    }

    fn kv_put_disk(&self, bucket: &str, key: &str, value: &Value, now_ts: f64) -> rusqlite::Result<()> {
        let (bucket, key, value) = (bucket.to_string(), key.to_string(), value.to_string());
        self.count_bytes(&bucket, value.len());
        self.write("kv_put", move |_, conn| {
            conn.prepare_cached(
                "INSERT INTO app_kv (bucket, key, value, updated) VALUES (?, ?, ?, ?)
                 ON CONFLICT(bucket, key) DO UPDATE SET value=excluded.value, updated=excluded.updated",
            )?
            .execute(params![bucket, key, value, now_ts])?;
            Ok(())
        })
    }

    /// Several rows in one write (one queue entry, one commit): a report's
    /// graph record and every reverse-index row it moves.
    pub fn kv_put_many(&self, rows: Vec<(String, String, Value)>, now_ts: f64) -> rusqlite::Result<()> {
        let keys: Vec<(String, String)> = rows.iter().map(|(b, k, _)| (b.clone(), k.clone())).collect();
        let owned: Vec<(String, String, String)> = rows.into_iter().map(|(b, k, v)| (b, k, v.to_string())).collect();
        for (b, _, v) in &owned {
            self.count_bytes(b, v.len());
        }
        let result = self.write("kv_put_many", move |_, conn| -> rusqlite::Result<()> {
            let mut statement = conn.prepare_cached(
                "INSERT INTO app_kv (bucket, key, value, updated) VALUES (?, ?, ?, ?)
                 ON CONFLICT(bucket, key) DO UPDATE SET value=excluded.value, updated=excluded.updated",
            )?;
            for (bucket, key, value) in &owned {
                statement.execute(params![bucket, key, value, now_ts])?;
            }
            Ok(())
        });
        for (bucket, key) in keys {
            self.kv_touched(&bucket, &key);
        }
        result
    }

    /// Idempotent: deleting an absent key is not an error, matching the
    /// Python driver's plain `DELETE ... WHERE` with no row-count check.
    /// The next number of a counter, atomically: one statement reads,
    /// raises to at least `floor`, increments and returns — two agents
    /// asking at once get two different numbers and no gap. The counter
    /// lives in app_kv as `{"n": <last issued>}`. Python's `kv_next`.
    pub fn kv_next(&self, bucket: &str, key: &str, floor: i64, now_ts: f64) -> rusqlite::Result<i64> {
        let result = self.kv_next_disk(bucket, key, floor, now_ts);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_next_disk(&self, bucket: &str, key: &str, floor: i64, now_ts: f64) -> rusqlite::Result<i64> {
        let (bucket, key) = (bucket.to_string(), key.to_string());
        self.write("kv_next", move |_, conn| conn.query_row(
            "INSERT INTO app_kv (bucket, key, value, updated) VALUES (?1, ?2, json_object('n', ?3 + 1), ?4)
             ON CONFLICT(bucket, key) DO UPDATE SET
               value = json_object('n', max(CAST(json_extract(app_kv.value, '$.n') AS INTEGER), ?3) + 1),
               updated = excluded.updated
             RETURNING CAST(json_extract(value, '$.n') AS INTEGER)",
            params![bucket, key, floor, now_ts],
            |row| row.get(0),
        ))
    }

    /// Insert only when the key is new; `true` when this call wrote it. Two
    /// agents racing for the same claim get one winner.
    pub fn kv_put_if_absent(&self, bucket: &str, key: &str, value: &Value, now_ts: f64) -> rusqlite::Result<bool> {
        let result = self.kv_put_if_absent_disk(bucket, key, value, now_ts);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_put_if_absent_disk(&self, bucket: &str, key: &str, value: &Value, now_ts: f64) -> rusqlite::Result<bool> {
        let (bucket, key, value) = (bucket.to_string(), key.to_string(), value.to_string());
        self.write("kv_put_if_absent", move |_, conn| {
            let changed = conn.execute(
                "INSERT OR IGNORE INTO app_kv (bucket, key, value, updated) VALUES (?, ?, ?, ?)",
                params![bucket, key, value, now_ts],
            )?;
            Ok(changed == 1)
        })
    }

    /// Take a lease, atomically: granted when the key is free, expired, or
    /// already this holder's (a renewal). One statement, so two agents asking
    /// at once get one winner. Returns the holder and expiry now on record.
    pub fn kv_lease(&self, bucket: &str, key: &str, holder: &str, ttl_s: f64, now_ts: f64) -> rusqlite::Result<(bool, String, f64)> {
        let result = self.kv_lease_disk(bucket, key, holder, ttl_s, now_ts);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_lease_disk(&self, bucket: &str, key: &str, holder: &str, ttl_s: f64, now_ts: f64) -> rusqlite::Result<(bool, String, f64)> {
        let (bucket, key, holder) = (bucket.to_string(), key.to_string(), holder.to_string());
        self.write("kv_lease", move |_, conn| {
        let changed = conn.execute(
            "INSERT INTO app_kv (bucket, key, value, updated) VALUES (?1, ?2, json_object('holder', ?3, 'until', ?4), ?5)
             ON CONFLICT(bucket, key) DO UPDATE SET value = excluded.value, updated = excluded.updated
             WHERE CAST(json_extract(app_kv.value, '$.until') AS REAL) < ?5
                OR json_extract(app_kv.value, '$.holder') = ?3",
            params![bucket, key, holder, now_ts + ttl_s, now_ts],
        )?;
        let (current, until): (String, f64) = conn.query_row(
            "SELECT json_extract(value, '$.holder'), CAST(json_extract(value, '$.until') AS REAL) FROM app_kv WHERE bucket = ? AND key = ?",
            params![bucket, key],
            |row| Ok((row.get::<_, Option<String>>(0)?.unwrap_or_default(), row.get::<_, Option<f64>>(1)?.unwrap_or(0.0))),
        )?;
        Ok((changed == 1, current, until))
        })
    }

    /// Give a lease back, only if this holder still has it.
    pub fn kv_release_lease(&self, bucket: &str, key: &str, holder: &str) -> rusqlite::Result<bool> {
        let result = self.kv_release_lease_disk(bucket, key, holder);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_release_lease_disk(&self, bucket: &str, key: &str, holder: &str) -> rusqlite::Result<bool> {
        let (bucket, key, holder) = (bucket.to_string(), key.to_string(), holder.to_string());
        self.write("kv_release_lease", move |_, conn| {
            let changed = conn.execute(
                "DELETE FROM app_kv WHERE bucket = ? AND key = ? AND json_extract(value, '$.holder') = ?",
                params![bucket, key, holder],
            )?;
            Ok(changed == 1)
        })
    }

    /// Raise a `kv_next` counter to at least `floor` without issuing a number.
    pub fn kv_raise(&self, bucket: &str, key: &str, floor: i64, now_ts: f64) -> rusqlite::Result<()> {
        let result = self.kv_raise_disk(bucket, key, floor, now_ts);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_raise_disk(&self, bucket: &str, key: &str, floor: i64, now_ts: f64) -> rusqlite::Result<()> {
        let (bucket, key) = (bucket.to_string(), key.to_string());
        self.write("kv_raise", move |_, conn| { conn.execute(
            "INSERT INTO app_kv (bucket, key, value, updated) VALUES (?1, ?2, json_object('n', ?3), ?4)
             ON CONFLICT(bucket, key) DO UPDATE SET
               value = json_object('n', max(CAST(json_extract(app_kv.value, '$.n') AS INTEGER), ?3)),
               updated = excluded.updated",
            params![bucket, key, floor, now_ts],
        )?;
        Ok(()) })
    }

    pub fn kv_delete(&self, bucket: &str, key: &str) -> rusqlite::Result<()> {
        let result = self.kv_delete_disk(bucket, key);
        self.kv_touched(bucket, key);
        result
    }

    fn kv_delete_disk(&self, bucket: &str, key: &str) -> rusqlite::Result<()> {
        let (bucket, key) = (bucket.to_string(), key.to_string());
        self.write("kv_delete", move |_, conn| {
            conn.execute("DELETE FROM app_kv WHERE bucket = ? AND key = ?", params![bucket, key])?;
            Ok(())
        })
    }

    /// Prefix scan over one kv bucket. GLOB with `[` escaped like the Python
    /// driver (see `eph_scan`), so per-scope reads that embed a repo id stay
    /// on that scope.
    pub fn kv_list(&self, bucket: &str, prefix: &str) -> Vec<(String, Value)> {
        let Some(mem) = &self.mem else { return self.kv_list_disk(bucket, prefix) };
        let id = (bucket.to_string(), prefix.to_string());
        let seen = mem.version_for(bucket, prefix);
        if let Some((version, rows)) = mem.scans.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            if *version == seen {
                return (**rows).clone();
            }
        }
        let rows = self.kv_list_disk(bucket, prefix);
        // small scans only: a membership list, an inbox; never a whole graph
        if rows.len() <= SCAN_CACHE_MAX_ROWS && !UNCACHED_BUCKETS.contains(&bucket) {
            let bytes: usize = rows.iter().map(|(k, v)| 48 + k.len() + crate::codegraph::value_bytes(v)).sum();
            let mut scans = mem.scans.lock().unwrap_or_else(|e| e.into_inner());
            if scans.len() >= SCAN_CACHE_ENTRIES || mem.scan_bytes.load(Ordering::Relaxed) as usize + bytes > SCAN_CACHE_BYTES {
                scans.clear();
                mem.scan_bytes.store(0, Ordering::Relaxed);
            }
            mem.scan_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
            scans.insert(id, (seen, std::sync::Arc::new(rows.clone())));
        }
        rows
    }

    /// Every row under a prefix, handed to `f` one at a time as it is read:
    /// a whole bucket of a big repo is never held at once.
    pub fn kv_for_each(&self, bucket: &str, prefix: &str, mut f: impl FnMut(String, Value)) {
        let conn = self.reader();
        let Ok(mut statement) = conn.prepare(
            "SELECT key, value FROM app_kv WHERE bucket = ? AND key GLOB ?2 ORDER BY key",
        ) else {
            return;
        };
        let Ok(mut rows) = statement.query(params![bucket, glob_prefix(prefix)]) else { return };
        while let Ok(Some(row)) = rows.next() {
            let (Ok(key), Ok(raw)) = (row.get::<_, String>(0), row.get::<_, String>(1)) else { continue };
            if let Ok(value) = serde_json::from_str(&raw) {
                f(key, value);
            }
        }
    }

    /// The `limit` rows under a prefix written most recently, newest first.
    pub fn kv_list_recent(&self, bucket: &str, prefix: &str, limit: usize) -> Vec<(String, Value)> {
        let conn = self.reader();
        let Ok(mut statement) = conn.prepare(
            "SELECT key, value FROM app_kv WHERE bucket = ? AND key GLOB ?2 ORDER BY updated DESC LIMIT ?3",
        ) else {
            return Vec::new();
        };
        let rows = statement.query_map(params![bucket, glob_prefix(prefix), limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten().filter_map(|(key, value)| serde_json::from_str(&value).ok().map(|v| (key, v))).collect()
    }

    /// Drop the row and scan caches (the memory watchdog); the disk has them.
    pub fn shed_caches(&self) {
        if let Some(mem) = &self.mem {
            mem.shed();
            mem.tails.write().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// Roughly what the row and scan caches hold, in bytes.
    pub fn cache_bytes(&self) -> u64 {
        self.mem.as_ref().map(|m| m.cache_bytes()).unwrap_or(0)
    }

    fn kv_list_disk(&self, bucket: &str, prefix: &str) -> Vec<(String, Value)> {
        let conn = self.reader();
        let mut statement = match conn.prepare(
            "SELECT key, value FROM app_kv WHERE bucket = ? AND key GLOB ?2 ORDER BY key",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let pattern = glob_prefix(prefix);
        let rows = statement
            .query_map(params![bucket, pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(key, value)| serde_json::from_str(&value).ok().map(|v| (key, v)))
            .collect()
    }

    /// Rows under a prefix written at or after `since`, with their write time:
    /// how an in-memory copy of a bucket catches up without re-reading it all.
    pub fn kv_list_since(&self, bucket: &str, prefix: &str, since: f64) -> Vec<(String, Value, f64)> {
        let conn = self.reader();
        let mut statement = match conn.prepare(
            "SELECT key, value, updated FROM app_kv WHERE bucket = ? AND key GLOB ?2 AND updated >= ?3",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![bucket, glob_prefix(prefix), since], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(key, value, updated)| serde_json::from_str(&value).ok().map(|v| (key, v, updated)))
            .collect()
    }

    /// How many rows a prefix holds and when the newest was written: a cheap
    /// test (no values read or parsed) of whether a cache of them is current.
    pub fn kv_stat(&self, bucket: &str, prefix: &str) -> (i64, f64) {
        let Some(mem) = &self.mem else { return self.kv_stat_disk(bucket, prefix) };
        let id = (bucket.to_string(), prefix.to_string());
        let seen = mem.version_for(bucket, prefix);
        if let Some((version, stat)) = mem.stats.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            if *version == seen {
                return *stat;
            }
        }
        let stat = self.kv_stat_disk(bucket, prefix);
        let mut stats = mem.stats.lock().unwrap_or_else(|e| e.into_inner());
        if stats.len() >= SCAN_CACHE_ENTRIES {
            stats.clear();
        }
        stats.insert(id, (seen, stat));
        stat
    }

    fn kv_stat_disk(&self, bucket: &str, prefix: &str) -> (i64, f64) {
        let conn = self.reader();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(updated), 0) FROM app_kv WHERE bucket = ? AND key GLOB ?2",
            params![bucket, glob_prefix(prefix)],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
        )
        .unwrap_or((0, 0.0))
    }

    // ---------------------------------------------------------- shared files

    pub fn get_file(&self, scope: &str, user: &str, path: &str) -> Option<Value> {
        let conn = self.reader();
        let raw: Option<String> = conn
            .query_row(
                "SELECT record FROM shared_files WHERE scope = ? AND user = ? AND path = ?",
                params![scope, user, path],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        serde_json::from_str(&raw?).ok()
    }

    pub fn put_file(
        &self, scope: &str, user: &str, path: &str, record: &Value, now_ts: f64,
    ) -> rusqlite::Result<()> {
        let (scope, user, path, record) = (scope.to_string(), user.to_string(), path.to_string(), record.to_string());
        self.write("put_file", move |_, conn| {
            conn.prepare_cached(
                "INSERT INTO shared_files (scope, user, path, record, updated) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(scope, user, path) DO UPDATE SET record=excluded.record, updated=excluded.updated",
            )?
            .execute(params![scope, user, path, record, now_ts])?;
            Ok(())
        })
    }

    pub fn get_file_all_users(&self, scope: &str, path: &str) -> Vec<(String, Value)> {
        let conn = self.reader();
        let mut statement = match conn
            .prepare("SELECT user, record FROM shared_files WHERE scope = ? AND path = ?")
        {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![scope, path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(user, record)| serde_json::from_str(&record).ok().map(|v| (user, v)))
            .collect()
    }

    pub fn list_files(&self, scope: &str, user: &str) -> Vec<(String, Value)> {
        let conn = self.reader();
        let mut statement =
            match conn.prepare("SELECT path, record FROM shared_files WHERE scope = ? AND user = ?") {
                Ok(statement) => statement,
                Err(_) => return Vec::new(),
            };
        let rows = statement.query_map(params![scope, user], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(path, record)| serde_json::from_str(&record).ok().map(|v| (path, v)))
            .collect()
    }

    // ---------------------------------------------------------- shared trees

    pub fn get_tree(&self, scope: &str, user: &str) -> Option<Value> {
        if let Some(mem) = &self.mem {
            if let Some((tree, _, _)) = mem.trees.read().unwrap_or_else(|e| e.into_inner()).get(&(scope.to_string(), user.to_string())) {
                return Some(tree.clone());
            }
        }
        let conn = self.reader();
        let raw: Option<String> = conn
            .query_row(
                "SELECT tree FROM shared_trees WHERE scope = ? AND user = ?",
                params![scope, user],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        serde_json::from_str(&raw?).ok()
    }

    /// Every workspace that has reported into this scope, with its tree root —
    /// the fast path for a collision check, which compares roots before it
    /// compares anything else.
    pub fn list_workspaces(&self, scope: &str) -> Vec<Value> {
        let conn = self.reader();
        let Ok(mut statement) =
            conn.prepare("SELECT user, root, updated FROM shared_trees WHERE scope = ?")
        else {
            return Vec::new();
        };
        let rows = statement.query_map(params![scope], |row| {
            Ok(serde_json::json!({
                "user": row.get::<_, String>(0)?,
                "root": row.get::<_, String>(1)?,
                "updated": row.get::<_, f64>(2)?,
            }))
        });
        let mut rows: Vec<Value> = rows.map(|rows| rows.flatten().collect()).unwrap_or_default();
        drop(statement);
        drop(conn);
        // trees written since the last tree flush answer for their user
        if let Some(mem) = &self.mem {
            let trees = mem.trees.read().unwrap_or_else(|e| e.into_inner());
            for ((s, user), (_, root, updated)) in trees.iter() {
                if s != scope {
                    continue;
                }
                let fresh = serde_json::json!({"user": user, "root": root, "updated": updated});
                match rows.iter_mut().find(|row| row.get("user").and_then(Value::as_str) == Some(user.as_str())) {
                    Some(row) => *row = fresh,
                    None => rows.push(fresh),
                }
            }
        }
        rows
    }

    pub fn put_tree(&self, scope: &str, user: &str, tree: &Value, now_ts: f64) -> rusqlite::Result<()> {
        let root = tree.get("root").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(mem) = &self.mem {
            let id = (scope.to_string(), user.to_string());
            mem.trees.write().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), (tree.clone(), root, now_ts));
            mem.trees_dirty.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
            if !mem.scopes.read().unwrap_or_else(|e| e.into_inner()).contains(scope) {
                mem.scopes.write().unwrap_or_else(|e| e.into_inner()).insert(scope.to_string());
            }
            return Ok(());
        }
        let (owned_scope, user, tree) = (scope.to_string(), user.to_string(), tree.to_string());
        self.write("put_tree", move |_, conn| {
            conn.prepare_cached(
                "INSERT INTO shared_trees (scope, user, tree, root, updated) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(scope, user) DO UPDATE SET tree=excluded.tree, root=excluded.root, updated=excluded.updated",
            )?
            .execute(params![owned_scope, user, tree, root, now_ts])
        })?;
        Ok(())
    }

    /// Write the trees changed since the last tree flush: `force` at
    /// shutdown, otherwise once every [`TREE_FLUSH_S`]. Only the newest
    /// tree per person per repo is written, however many edits it saw.
    pub fn flush_trees(&self, force: bool) {
        let Some(mem) = &self.mem else { return };
        if !self.writable() {
            return;
        }
        {
            let mut last = mem.trees_flushed.lock().unwrap_or_else(|e| e.into_inner());
            if !force && now() - *last < TREE_FLUSH_S {
                return;
            }
            *last = now();
        }
        let ids: Vec<(String, String)> = mem.trees_dirty.lock().unwrap_or_else(|e| e.into_inner()).drain().collect();
        if ids.is_empty() {
            return;
        }
        let rows: Vec<(String, String, String, String, f64)> = {
            let trees = mem.trees.read().unwrap_or_else(|e| e.into_inner());
            ids.iter()
                .filter_map(|(scope, user)| {
                    let (tree, root, updated) = trees.get(&(scope.clone(), user.clone()))?;
                    Some((scope.clone(), user.clone(), tree.to_string(), root.clone(), *updated))
                })
                .collect()
        };
        let _ = self.write("flush_trees", move |_, conn| -> rusqlite::Result<()> {
            let mut statement = conn.prepare_cached(
                "INSERT INTO shared_trees (scope, user, tree, root, updated) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(scope, user) DO UPDATE SET tree=excluded.tree, root=excluded.root, updated=excluded.updated",
            )?;
            for (scope, user, tree, root, updated) in &rows {
                statement.execute(params![scope, user, tree, root, updated])?;
            }
            Ok(())
        });
        // trees nobody wrote for a while leave memory; disk has them now
        let cutoff = now() - 1_800.0;
        let dirty = mem.trees_dirty.lock().unwrap_or_else(|e| e.into_inner()).clone();
        mem.trees.write().unwrap_or_else(|e| e.into_inner()).retain(|id, (_, _, updated)| *updated >= cutoff || dirty.contains(id));
    }

    /// Drop a scope's live state — every file and tree row — leaving its
    /// ledger history untouched. The chain is append-only and global, so
    /// `remove_repo` uses this to make a repo vanish from the workspace's
    /// repo list without erasing what it recorded.
    pub fn delete_scope(&self, scope: &str) -> rusqlite::Result<()> {
        let owned = scope.to_string();
        self.write("delete_scope", move |_, conn| -> rusqlite::Result<()> {
            conn.execute("DELETE FROM shared_files WHERE scope = ?", params![owned])?;
            conn.execute("DELETE FROM shared_trees WHERE scope = ?", params![owned])?;
            Ok(())
        })?;
        if let Some(mem) = &self.mem {
            mem.scopes.write().unwrap_or_else(|e| e.into_inner()).remove(scope);
            mem.trees.write().unwrap_or_else(|e| e.into_inner()).retain(|(s, _), _| s != scope);
            mem.trees_dirty.lock().unwrap_or_else(|e| e.into_inner()).retain(|(s, _)| s != scope);
        }
        Ok(())
    }

    // --------------------------------------------------------------- ledger

    /// Append one row. Reading the head and inserting under the same lock is
    /// what keeps the chain contiguous under concurrency.
    /// Append one row, stamped with the request's `via` when one is in
    /// scope — Python's `service.record()`. Rows copied verbatim from
    /// another scope (a repo move) go through `ledger_append_raw`, as
    /// Python's move writes them through the driver, unstamped.
    pub fn ledger_append(
        &self, scope: &str, kind: &str, payload: &Value, ts: f64,
    ) -> rusqlite::Result<Value> {
        self.ledger_append_raw(scope, kind, &stamp_via(payload), ts)
    }

    pub fn ledger_append_raw(
        &self, scope: &str, kind: &str, payload: &Value, ts: f64,
    ) -> rusqlite::Result<Value> {
        let (scope, kind, payload) = (scope.to_string(), kind.to_string(), payload.clone());
        self.write("ledger_append", move |store, conn| store.ledger_append_locked(conn, &scope, &kind, &payload, ts))
    }

    /// Append a row nobody waits on — an audit row for a gate that cleared,
    /// a saving — without holding the caller until it commits. It joins the
    /// next batch (the flusher runs one at least every [`EPH_FLUSH_MS`]), so a
    /// busy gate answers at memory speed while its row still lands. Where
    /// another process shares the file it is written at once, as before.
    pub fn ledger_append_later(&self, scope: &str, kind: &str, payload: &Value, ts: f64) {
        if self.mem.is_none() {
            let _ = self.ledger_append(scope, kind, payload, ts);
            return;
        }
        let (scope, kind, payload) = (scope.to_string(), kind.to_string(), stamp_via(payload));
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push(("ledger_append_later", Box::new(move |store: &Store, conn: &Connection| {
            let _ = store.ledger_append_locked(conn, &scope, &kind, &payload, ts);
            Box::new(|| {}) as Deliver
        })));
        // with an engine the next writer, or the flusher within a
        // quarter-second, takes it; a plain database writes it now
        if self.engine.get().is_none() {
            self.drain_if_waiting();
        }
    }

    /// The append itself, run by whoever holds the writer. Reading the head
    /// and inserting under the same lock is what keeps the chain contiguous.
    fn ledger_append_locked(
        &self, conn: &Connection, scope: &str, kind: &str, payload: &Value, ts: f64,
    ) -> rusqlite::Result<Value> {
        let cached = self.mem.as_ref().and_then(|mem| mem.head.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let prev_hash: String = match cached {
            Some(hash) => hash,
            None => conn
                .query_row("SELECT hash FROM ledger ORDER BY seq DESC LIMIT 1", [], |row| row.get(0))
                .optional()?
                .unwrap_or_else(|| GENESIS.to_string()),
        };
        let row_hash = chain_hash(&prev_hash, scope, ts, kind, payload);
        conn.prepare_cached(
            "INSERT INTO ledger (scope, ts, kind, payload, prev_hash, hash) VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![scope, ts, kind, canonical_sorted(payload), prev_hash, row_hash])?;
        let seq = conn.last_insert_rowid();
        if let Some(mem) = &self.mem {
            *mem.head.lock().unwrap_or_else(|e| e.into_inner()) = Some(row_hash.clone());
            let tail = mem.tails.read().unwrap_or_else(|e| e.into_inner()).get(scope).cloned();
            if let Some(tail) = tail {
                // exactly what a read from disk would parse: keys sorted
                let row = LedgerRow { seq, kind: kind.to_string(), payload: crate::hashing::sorted(payload), ts };
                tail.write().unwrap_or_else(|e| e.into_inner()).push(row);
            }
        }
        Ok(serde_json::json!({
            "seq": seq, "scope": scope, "ts": ts, "kind": kind,
            "payload": payload, "hash": row_hash,
        }))
    }

    /// Every scope that already carries data. Matches the Python driver:
    /// distinct scopes in shared_trees, which is what the alias registry
    /// seeds from.
    pub fn list_scopes(&self, prefix: &str) -> Vec<String> {
        if let Some(mem) = &self.mem {
            let scopes = mem.scopes.read().unwrap_or_else(|e| e.into_inner());
            return scopes.range(prefix.to_string()..).take_while(|s| s.starts_with(prefix)).cloned().collect();
        }
        let conn = self.reader();
        let Ok(mut statement) = conn.prepare(
            "SELECT DISTINCT scope FROM shared_trees WHERE scope GLOB ? ORDER BY scope",
        ) else {
            return Vec::new();
        };
        // GLOB, like the Python side, so a '[' in a scope cannot become a
        // character class
        let pattern = glob_prefix(prefix);
        let rows = statement.query_map(params![pattern], |row| row.get::<_, String>(0));
        rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Distinct scopes with at least one ledger row at or after `since_ts`,
    /// matching the Python driver's `count_active_scopes` byte for byte —
    /// including its quirk of binding `prefix` straight into a `LIKE ... ||
    /// '%'` with no escaping, so a literal `%` or `_` in a workspace id acts
    /// as a wildcard on both sides. Billing's monthly active-repo count reads
    /// this: repos with real activity this month, not every scope that has
    /// ever accreted from auto-registration. -1 signals "couldn't answer" —
    /// a poisoned lock or a query error — so the caller can fall back to the
    /// all-time `list_scopes` count instead of reporting zero.
    pub fn count_active_scopes(&self, prefix: &str, since_ts: f64) -> i64 {
        let conn = self.reader();
        conn.query_row(
            "SELECT COUNT(DISTINCT scope) FROM ledger WHERE scope LIKE ? || '%' AND ts >= ?",
            params![prefix, since_ts],
            |row| row.get(0),
        )
        .unwrap_or(-1)
    }

    /// Every scope with a ledger row at or after `since_ts` — the admin
    /// dashboard's walk across all workspaces. Rust only.
    pub fn ledger_scopes_since(&self, since_ts: f64) -> Vec<String> {
        let conn = self.reader();
        let Ok(mut statement) =
            conn.prepare("SELECT DISTINCT scope FROM ledger WHERE ts >= ? ORDER BY scope")
        else {
            return Vec::new();
        };
        let rows = statement.query_map(params![since_ts], |row| row.get::<_, String>(0));
        rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Rows for one scope at or after a timestamp, in sequence order —
    /// which is what makes "already seen" mean "happened earlier" for every
    /// caller that walks them.
    pub fn ledger_since(&self, scope: &str, ts: f64) -> Vec<LedgerRow> {
        self.ledger_since_of(scope, ts, None)
    }

    /// `ledger_since`, only rows of these kinds. In a busy repo most rows
    /// are gate checks; a reader that wants edits filters before copying,
    /// which keeps the tail's lock short for the appends waiting on it.
    pub fn ledger_since_kinds(&self, scope: &str, ts: f64, kinds: &[&str]) -> Vec<LedgerRow> {
        self.ledger_since_of(scope, ts, Some(kinds))
    }

    fn ledger_since_of(&self, scope: &str, ts: f64, kinds: Option<&[&str]>) -> Vec<LedgerRow> {
        if let Some(mem) = &self.mem {
            let stamp = now();
            if ts >= stamp - TAIL_WINDOW_S + 1.0 {
                let tail = mem.tails.read().unwrap_or_else(|e| e.into_inner()).get(scope).cloned();
                let tail = match tail {
                    Some(tail) => tail,
                    None => self.load_tail(mem, scope, stamp - TAIL_WINDOW_S),
                };
                {
                    let held = tail.read().unwrap_or_else(|e| e.into_inner());
                    if ts >= held.from_ts {
                        held.used.store(stamp.to_bits(), Ordering::Relaxed);
                        self.tail_reads.fetch_add(1, Ordering::Relaxed);
                        return held.since(ts, kinds);
                    }
                }
            }
        }
        self.disk_reads.fetch_add(1, Ordering::Relaxed);
        let rows = self.ledger_since_disk(scope, ts);
        match kinds {
            Some(kinds) => rows.into_iter().filter(|row| kinds.contains(&row.kind.as_str())).collect(),
            None => rows,
        }
    }

    /// Make a repo's tail: read its rows since `from_ts` under the writer, so
    /// no append lands between the read and the tail taking appends.
    fn load_tail(&self, mem: &Mem, scope: &str, from_ts: f64) -> std::sync::Arc<RwLock<Tail>> {
        // the hour's rows through a reader, beside the writer: a busy repo's
        // hour is tens of thousands of rows, and parsing them under the
        // writer stalled every write (and a standby's catch-up) for seconds
        let bulk = ledger_rows_since(&self.reader(), scope, from_ts);
        let top = bulk.iter().map(|row| row.seq).max().unwrap_or(0);
        // then, under the writer (no append can land in between), the few
        // rows committed while that read ran, and the tail starts taking appends
        let conn = self.writer();
        if let Some(tail) = mem.tails.read().unwrap_or_else(|e| e.into_inner()).get(scope) {
            return tail.clone();
        }
        let mut tail = Tail { from_ts, rows: std::collections::VecDeque::new(), used: std::sync::atomic::AtomicU64::new(now().to_bits()) };
        for row in bulk {
            tail.push(row);
        }
        for row in ledger_rows_after_seq(&conn, scope, top) {
            if row.ts >= from_ts {
                tail.push(row);
            }
        }
        let tail = std::sync::Arc::new(RwLock::new(tail));
        mem.tails.write().unwrap_or_else(|e| e.into_inner()).insert(scope.to_string(), tail.clone());
        drop(conn);
        tail
    }

    /// Age out tails: rows past the window, rows past the cap, and repos
    /// nobody has read for the window.
    pub fn trim_tails(&self) {
        let Some(mem) = &self.mem else { return };
        let stamp = now();
        let mut tails = mem.tails.write().unwrap_or_else(|e| e.into_inner());
        tails.retain(|_, tail| {
            let mut tail = tail.write().unwrap_or_else(|e| e.into_inner());
            if stamp - f64::from_bits(tail.used.load(Ordering::Relaxed)) > TAIL_WINDOW_S {
                return false;
            }
            let floor = stamp - TAIL_WINDOW_S;
            let mut raised = tail.from_ts;
            while tail.rows.len() > TAIL_MAX_ROWS || tail.rows.front().is_some_and(|(_, row)| row.ts < floor) {
                let Some((_, row)) = tail.rows.pop_front() else { break };
                // rows are in seq order, not strictly ts order: the window
                // starts just past the newest row let go
                raised = raised.max(row.ts + 1e-6);
            }
            tail.from_ts = raised.max(tail.from_ts);
            true
        });
    }

    /// Rows since `ts` whose stored payload text contains any of `needles`,
    /// from disk: a first pass over a long window that needs a few kinds of
    /// row only filters in SQL instead of parsing every row.
    pub fn ledger_since_containing(&self, scope: &str, ts: f64, needles: &[&str]) -> Vec<LedgerRow> {
        if needles.is_empty() {
            return Vec::new();
        }
        let conn = self.reader();
        let clause = needles.iter().map(|_| "instr(payload, ?) > 0").collect::<Vec<_>>().join(" OR ");
        let sql = format!("SELECT seq, kind, payload, ts FROM ledger WHERE scope = ? AND ts >= ? AND ({clause}) ORDER BY seq ASC");
        let Ok(mut statement) = conn.prepare(&sql) else { return Vec::new() };
        let mut values: Vec<rusqlite::types::Value> = vec![scope.to_string().into(), ts.into()];
        values.extend(needles.iter().map(|n| rusqlite::types::Value::from(n.to_string())));
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(seq, kind, payload, ts)| serde_json::from_str(&payload).ok().map(|payload| LedgerRow { seq, kind, payload, ts }))
            .collect()
    }

    fn ledger_since_disk(&self, scope: &str, ts: f64) -> Vec<LedgerRow> {
        let conn = self.reader();
        ledger_rows_since(&conn, scope, ts)
    }
}

/// One scope's rows with a sequence above `seq`, in order.
fn ledger_rows_after_seq(conn: &Connection, scope: &str, seq: i64) -> Vec<LedgerRow> {
    let Ok(mut statement) = conn.prepare("SELECT seq, kind, payload, ts FROM ledger WHERE scope = ? AND seq > ? ORDER BY seq ASC") else {
        return Vec::new();
    };
    let rows = statement.query_map(params![scope, seq], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter_map(|(seq, kind, payload, ts)| serde_json::from_str(&payload).ok().map(|payload| LedgerRow { seq, kind, payload, ts }))
        .collect()
}

fn ledger_rows_since(conn: &Connection, scope: &str, ts: f64) -> Vec<LedgerRow> {
    {
        let mut statement = match conn.prepare(
            "SELECT seq, kind, payload, ts FROM ledger WHERE scope = ? AND ts >= ? ORDER BY seq ASC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![scope, ts], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
        });
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .filter_map(|(seq, kind, payload, ts)| {
                serde_json::from_str(&payload)
                    .ok()
                    .map(|payload| LedgerRow { seq, kind, payload, ts })
            })
            .collect()
    }
}

impl Store {
    /// A page of one scope's history, newest first internally but returned
    /// oldest-first within the page — matching the Python driver's `tail`,
    /// which the dashboard and the public worklog page both read directly.
    /// `before_seq` is an exclusive cursor: pass the oldest `seq` already
    /// shown to page further back.
    pub fn ledger_tail(&self, scope: &str, limit: i64, before_seq: Option<i64>) -> Vec<LedgerRow> {
        let conn = self.reader();
        let rows = if let Some(before_seq) = before_seq {
            let mut statement = match conn.prepare(
                "SELECT seq, kind, payload, ts FROM ledger WHERE scope = ? AND seq < ? ORDER BY seq DESC LIMIT ?",
            ) {
                Ok(statement) => statement,
                Err(_) => return Vec::new(),
            };
            let rows = statement.query_map(params![scope, before_seq, limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
            });
            rows.map(|rows| rows.flatten().collect::<Vec<_>>()).unwrap_or_default()
        } else {
            let mut statement = match conn.prepare(
                "SELECT seq, kind, payload, ts FROM ledger WHERE scope = ? ORDER BY seq DESC LIMIT ?",
            ) {
                Ok(statement) => statement,
                Err(_) => return Vec::new(),
            };
            let rows = statement.query_map(params![scope, limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
            });
            rows.map(|rows| rows.flatten().collect::<Vec<_>>()).unwrap_or_default()
        };
        rows.into_iter()
            .rev()
            .filter_map(|(seq, kind, payload, ts)| {
                serde_json::from_str(&payload).ok().map(|payload| LedgerRow { seq, kind, payload, ts })
            })
            .collect()
    }

    /// One page of the FULL ledger row (seq, prev_hash and hash included —
    /// what `ledger_since` deliberately drops) for one scope, newest-first
    /// then restored to ascending seq order. Mirrors `SqliteLedger.tail` in
    /// `storage/sqlite_driver.py`: `before_seq` is an exclusive cursor that
    /// pages older history, which is what the dashboard's ledger view walks
    /// on "load more". `ledger_since` cannot serve that view — it answers a
    /// different question (rows after a timestamp) and its `LedgerRow`
    /// shape has no seq/hash for a caller to page or verify a chain link by.
    pub fn ledger_page(&self, scope: &str, limit: i64, before_seq: Option<i64>) -> Vec<Value> {
        let conn = self.reader();
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
            let seq: i64 = row.get(0)?;
            let scope: String = row.get(1)?;
            let ts: f64 = row.get(2)?;
            let kind: String = row.get(3)?;
            let payload: String = row.get(4)?;
            let prev_hash: String = row.get(5)?;
            let hash: String = row.get(6)?;
            let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            Ok(serde_json::json!({
                "seq": seq, "scope": scope, "ts": ts, "kind": kind,
                "payload": payload, "prev_hash": prev_hash, "hash": hash,
            }))
        };
        let rows: Vec<Value> = if let Some(before_seq) = before_seq {
            let Ok(mut statement) = conn.prepare(
                "SELECT seq, scope, ts, kind, payload, prev_hash, hash FROM ledger \
                 WHERE scope = ? AND seq < ? ORDER BY seq DESC LIMIT ?",
            ) else {
                return Vec::new();
            };
            statement
                .query_map(params![scope, before_seq, limit], map_row)
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        } else {
            let Ok(mut statement) = conn.prepare(
                "SELECT seq, scope, ts, kind, payload, prev_hash, hash FROM ledger \
                 WHERE scope = ? ORDER BY seq DESC LIMIT ?",
            ) else {
                return Vec::new();
            };
            statement
                .query_map(params![scope, limit], map_row)
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        rows.into_iter().rev().collect()
    }

    pub fn ledger_count(&self, scope: &str) -> i64 {
        Some(self.reader())
            .and_then(|conn| {
                if scope.is_empty() {
                    conn.query_row("SELECT COUNT(*) FROM ledger", [], |row| row.get(0)).ok()
                } else {
                    conn.query_row(
                        "SELECT COUNT(*) FROM ledger WHERE scope = ?",
                        params![scope],
                        |row| row.get(0),
                    )
                    .ok()
                }
            })
            .unwrap_or(0)
    }

    /// Re-verify the chain the way the Python server does: walk every row in
    /// sequence and recompute each link. Returns (ok, first bad seq, rows).
    /// The chain's verdict: `(ok, bad_seq, rows)` — `rows` is the table's
    /// total, the global chain length.
    pub fn ledger_verify(&self) -> (bool, Option<i64>, i64) {
        self.ledger_verify_at(now(), false).0
    }

    /// `(verdict, rows walked)`.
    ///
    /// Incremental, because the dashboard asks on every load and every 30 s
    /// after: a full walk recomputes every link from row 1, grows with
    /// history for ever, and holds the store's lock while it runs — which is
    /// the lock every gate check queues behind. The last verified `(seq,
    /// hash)` is kept as an anchor and only the rows past it are walked (the
    /// chain guarantees a verified prefix stays verified unless an old row
    /// is altered); everything is re-walked once per [`FULL_WALK_EVERY_S`],
    /// when the anchor row itself changed, or on `force_full` (the route's
    /// `?verify=full`). So a tampered OLD row shows within the hour or on
    /// demand; a bad NEW row shows at once. Python's `_verify_at`.
    pub fn ledger_verify_at(&self, at: f64, force_full: bool) -> ((bool, Option<i64>, i64), i64) {
        let Ok(mut check) = self.chain_check.lock() else { return ((false, None, 0), 0) };
        let conn = self.reader();
        // the anchor must still be there, unchanged: a rewritten or
        // truncated tail sends the walk back to the start
        let anchored = match check.as_ref() {
            Some(c) if !force_full && at - c.full_at < FULL_WALK_EVERY_S => conn
                .query_row("SELECT hash FROM ledger WHERE seq = ?", [c.seq], |row| row.get::<_, String>(0))
                .optional()
                .ok()
                .flatten()
                .is_some_and(|hash| hash == c.hash),
            _ => false,
        };
        if anchored {
            let c = check.as_mut().expect("anchored implies a check");
            if !c.verdict.0 {
                // a broken chain stays broken: new rows cannot mend it, and
                // the hourly walk is when a repaired history gets re-read
                return (c.verdict, 0);
            }
            let (verdict, walked, tail) = walk_chain(&conn, c.seq, &c.hash);
            if verdict.0 {
                if let Some((seq, hash)) = tail {
                    c.seq = seq;
                    c.hash = hash;
                }
            } else {
                c.verdict = verdict;
            }
            return (verdict, walked);
        }
        let (verdict, walked, tail) = walk_chain(&conn, 0, GENESIS);
        let (seq, hash) = tail.unwrap_or((0, GENESIS.to_string()));
        *check = Some(ChainCheck { seq, hash, full_at: at, verdict });
        (verdict, walked)
    }
}

/// Verify every row past `after_seq`, linking from `start_hash`: the
/// verdict (with `rows` = the table's total), how many rows were walked, and
/// the last sound `(seq, hash)` — the next anchor. Python's `_walk`.
fn walk_chain(conn: &Connection, after_seq: i64, start_hash: &str) -> ((bool, Option<i64>, i64), i64, Option<(i64, String)>) {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM ledger", [], |row| row.get(0)).unwrap_or(0);
    let Ok(mut statement) = conn.prepare(
        "SELECT seq, scope, ts, kind, payload, prev_hash, hash FROM ledger WHERE seq > ? ORDER BY seq",
    ) else {
        return ((false, None, total), 0, None);
    };
    let rows = statement.query_map([after_seq], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    });
    let Ok(rows) = rows else { return ((false, None, total), 0, None) };
    let mut previous = start_hash.to_string();
    let mut walked = 0;
    let mut tail = None;
    for row in rows.flatten() {
        let (seq, scope, ts, kind, payload, prev_hash, hash) = row;
        walked += 1;
        let parsed: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        // the row's own hash may be over either encoding this half has ever
        // used (see `hashing::legacy_chain_hash`); the link to the previous
        // row is exact
        let sound = hash == chain_hash(&previous, &scope, ts, &kind, &parsed)
            || hash == crate::hashing::legacy_chain_hash(&previous, &scope, ts, &kind, &parsed);
        if prev_hash != previous || !sound {
            return ((false, Some(seq), total), walked, tail);
        }
        previous = hash.clone();
        tail = Some((seq, hash));
    }
    ((true, None, total), walked, tail)
}

/// `json.dumps(payload, sort_keys=True)` — note the Python side stores the
/// ledger payload with DEFAULT separators, not the compact ones used for the
/// chain hash. Reproduced exactly so a row written here is byte-identical.
#[allow(dead_code)]
/// The stored payload text: `json.dumps(payload, sort_keys=True)`. Sorted
/// explicitly — the map now preserves insertion order for the artifacts
/// written to users' repos, and this is a place where sorted is correctness.
pub fn canonical_sorted(value: &Value) -> String {
    python_dumps(&crate::hashing::sorted(value))
}

/// Python's `json.dumps` default formatting: ", " between items and ": "
/// after keys, keys sorted.
pub fn python_dumps(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let inner = map
                .iter()
                .map(|(key, item)| {
                    format!("{}: {}", crate::hashing::ascii_string(key), python_dumps(item))
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        Value::Array(items) => {
            let inner = items.iter().map(python_dumps).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        // same reason as the chain hash: Python stores this column with
        // ensure_ascii, and a row written here has to be byte-identical
        Value::String(text) => crate::hashing::ascii_string(text),
        other => other.to_string(),
    }
}

#[allow(dead_code)]
/// One ledger row as its readers need it: what happened, the payload, and
/// when. The sequence is not carried because every caller walks the rows in
/// order and asks "did this precede that", which order already answers.
#[derive(Clone)]
pub struct LedgerRow {
    /// The row's position in the chain. Two readers turned out to need it —
    /// search hits and the explain chain both link into the change view by
    /// it — so "every caller only needs ordering" stopped being true.
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
    pub ts: f64,
}

impl Store {
    /// A live feed of one scope's events. The channel is created on first
    /// subscribe and bounded: a subscriber that stops reading is lagged,
    /// never allowed to hold the publisher hostage.
    pub fn subscribe(&self, scope: &str) -> tokio::sync::broadcast::Receiver<Value> {
        let mut bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
        bus.entry(scope.to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel(256).0)
            .subscribe()
    }

    /// Fan an event out to live subscribers. No subscribers is the common
    /// case and costs a map lookup.
    pub fn broadcast(&self, scope: &str, event: &Value) {
        let bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = bus.get(scope) {
            let _ = sender.send(event.clone());
        }
    }
}

/// The GLOB pattern that matches every key starting with `prefix`, with the
/// one GLOB metacharacter a key can carry (`[`) escaped. Byte for byte the
/// Python driver's `prefix.replace("[", "[[]") + "*"`: GLOB reads `_` and
/// `%` literally, which is the whole reason the scans use it over LIKE.
fn glob_prefix(prefix: &str) -> String {
    format!("{}*", prefix.replace('[', "[[]"))
}

static NEXT_STORE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Store {
    /// The key an in-memory cache of this store's `scope` goes under.
    pub fn cache_key(&self, scope: &str) -> String {
        format!("{}#{scope}", self.id.load(Ordering::Relaxed))
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A session recording every change to the logged tables.
fn open_session(conn: &Connection) -> Option<rusqlite::session::Session<'_>> {
    let mut session = rusqlite::session::Session::new(conn).ok()?;
    for table in LOGGED_TABLES {
        session.attach(Some(table)).ok()?;
    }
    Some(session)
}

/// Apply one changeset: a row that differs or already exists is replaced
/// by the logged one; a delete of a row that is not there is skipped.
pub fn apply_changeset(conn: &Connection, changes: &[u8]) -> rusqlite::Result<()> {
    use rusqlite::session::{ConflictAction, ConflictType};
    let mut input: &[u8] = changes;
    conn.apply_strm(&mut input, None::<fn(&str) -> bool>, |kind, _item| match kind {
        ConflictType::SQLITE_CHANGESET_DATA | ConflictType::SQLITE_CHANGESET_CONFLICT => ConflictAction::SQLITE_CHANGESET_REPLACE,
        _ => ConflictAction::SQLITE_CHANGESET_OMIT,
    })
}

/// The log sequence number a database file holds (0 when it has none).
pub fn read_log_seq(conn: &Connection) -> i64 {
    conn.query_row("SELECT value FROM engine_meta WHERE key = 'seq'", [], |row| row.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory() -> Store {
        // SQLite treats the literal path ":memory:" specially regardless of
        // it being passed through as a real filesystem `Path`.
        Store::open(Path::new(":memory:")).expect("in-memory store")
    }

    /// Python's `test_verify_walks_only_what_is_new_and_re_anchors_on_demand`,
    /// step for step.
    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("collide-store-{name}-{}-{}", std::process::id(), now()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("collide.db")
    }

    #[test]
    fn ephemeral_tier_lives_in_memory_and_survives_a_restart_after_flush() {
        let path = temp_db("eph");
        {
            let store = Store::open_with(&path, true).unwrap();
            store.eph_set("focus:ws:r:alice", &serde_json::json!({"p": "a.py"}), Some(3_600.0)).unwrap();
            store.eph_set("focus:ws:r:bob", &serde_json::json!({"p": "b.py"}), Some(3_600.0)).unwrap();
            // a minutes-long marker lives in memory only
            store.eph_set("focus:ws:r:brief", &serde_json::json!({"p": "c.py"}), Some(60.0)).unwrap();
            assert!(store.eph_get("focus:ws:r:brief").is_some());
            store.eph_delete("focus:ws:r:brief");
            store.eph_set("focus:ws:r:gone", &serde_json::json!({}), Some(-1.0)).unwrap();
            store.eph_set("focus:ws:r2:carol", &serde_json::json!({}), None).unwrap();
            assert_eq!(store.eph_get("focus:ws:r:alice").unwrap()["p"], "a.py");
            assert!(store.eph_get("focus:ws:r:gone").is_none());
            let keys: Vec<String> = store.eph_scan("focus:ws:r:").into_iter().map(|(k, _)| k).collect();
            assert_eq!(keys, vec!["focus:ws:r:alice", "focus:ws:r:bob"]);
            store.eph_delete("focus:ws:r:bob");
            // nothing on disk until the flush
            let disk: i64 = store.writer().query_row("SELECT COUNT(*) FROM ephem", [], |r| r.get(0)).unwrap();
            assert_eq!(disk, 0);
            store.flush();
        }
        let store = Store::open_with(&path, true).unwrap();
        let keys: Vec<String> = store.eph_scan("focus:ws:").into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["focus:ws:r2:carol", "focus:ws:r:alice"]);
    }

    #[test]
    fn cached_kv_reads_follow_every_kind_of_write() {
        let path = temp_db("kv");
        let store = Store::open_with(&path, true).unwrap();
        assert!(store.kv_get("tokens", "t1").is_none());
        store.kv_put("tokens", "t1", &serde_json::json!({"uid": "a"}), 1.0).unwrap();
        assert_eq!(store.kv_get("tokens", "t1").unwrap()["uid"], "a");
        store.kv_put("tokens", "t1", &serde_json::json!({"uid": "b"}), 2.0).unwrap();
        assert_eq!(store.kv_get("tokens", "t1").unwrap()["uid"], "b");
        store.kv_delete("tokens", "t1").unwrap();
        assert!(store.kv_get("tokens", "t1").is_none());
        assert_eq!(store.kv_next("seq", "ws:r:n", 0, 1.0).unwrap(), 1);
        assert_eq!(store.kv_get("seq", "ws:r:n").unwrap()["n"], 1);
        assert_eq!(store.kv_next("seq", "ws:r:n", 0, 1.0).unwrap(), 2);
        assert_eq!(store.kv_get("seq", "ws:r:n").unwrap()["n"], 2);
        assert!(store.kv_lease("land", "ws:r:l", "a", 60.0, 1.0).unwrap().0);
        assert_eq!(store.kv_get("land", "ws:r:l").unwrap()["holder"], "a");
        assert!(store.kv_release_lease("land", "ws:r:l", "a").unwrap());
        assert!(store.kv_get("land", "ws:r:l").is_none());
        // scans and stats move with writes under their scope, and only there
        store.kv_put("inbox", "ws:r:bob:1", &serde_json::json!(1), 5.0).unwrap();
        assert_eq!(store.kv_list("inbox", "ws:r:").len(), 1);
        assert_eq!(store.kv_stat("inbox", "ws:r:"), (1, 5.0));
        store.kv_put("inbox", "ws:r:bob:2", &serde_json::json!(2), 6.0).unwrap();
        assert_eq!(store.kv_list("inbox", "ws:r:").len(), 2);
        assert_eq!(store.kv_list("inbox", "ws:").len(), 2);
        assert_eq!(store.kv_stat("inbox", "ws:r:"), (2, 6.0));
        store.kv_delete("inbox", "ws:r:bob:1").unwrap();
        assert_eq!(store.kv_list("inbox", "ws:r:bob:").len(), 1);
        assert_eq!(store.kv_stat("inbox", "ws:r:"), (1, 6.0));
        // a second connection to the file sees every write: nothing is held back
        let other = Connection::open(&path).unwrap();
        let n: i64 = other.query_row("SELECT COUNT(*) FROM app_kv WHERE bucket = 'inbox'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn counters_batch_until_the_flush_and_readers_can_add_what_is_held() {
        let store = Store::open_with(&temp_db("count"), true).unwrap();
        for _ in 0..5 {
            store.kv_bump("usage", "ws:2026-09", "calls", 1);
        }
        assert!(store.kv_get("usage", "ws:2026-09").is_none());
        assert_eq!(store.kv_pending("usage", "ws:2026-09", "calls"), 5);
        store.flush();
        assert_eq!(store.kv_get("usage", "ws:2026-09").unwrap()["calls"], 5);
        assert_eq!(store.kv_pending("usage", "ws:2026-09", "calls"), 0);
        store.kv_bump("usage", "ws:2026-09", "calls", 2);
        store.flush();
        assert_eq!(store.kv_get("usage", "ws:2026-09").unwrap()["calls"], 7);
        // a shared store writes at once
        let shared = Store::open_with(&temp_db("count-shared"), false).unwrap();
        shared.kv_bump("usage", "k", "calls", 1);
        assert_eq!(shared.kv_get("usage", "k").unwrap()["calls"], 1);
    }

    #[test]
    fn list_scopes_answers_from_memory_and_follows_trees() {
        let store = Store::open_with(&temp_db("scopes"), true).unwrap();
        store.put_tree("ws1:a", "u", &serde_json::json!({"root": "x"}), 1.0).unwrap();
        store.put_tree("ws1:b", "u", &serde_json::json!({"root": "x"}), 1.0).unwrap();
        store.put_tree("ws2:c", "u", &serde_json::json!({"root": "x"}), 1.0).unwrap();
        assert_eq!(store.list_scopes("ws1:"), vec!["ws1:a", "ws1:b"]);
        store.delete_scope("ws1:a").unwrap();
        assert_eq!(store.list_scopes("ws1:"), vec!["ws1:b"]);
        assert_eq!(store.list_scopes(""), vec!["ws1:b", "ws2:c"]);
    }

    #[test]
    fn a_read_racing_a_write_never_caches_the_old_row() {
        // many readers and one writer hammer one key; after the writer's last
        // write every read must return that last value
        let store = std::sync::Arc::new(Store::open_with(&temp_db("race"), true).unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..4).map(|_| {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || while !stop.load(Ordering::Relaxed) { let _ = store.kv_get("b", "ws:r:k"); })
        }).collect();
        for i in 0..500 {
            store.kv_put("b", "ws:r:k", &serde_json::json!(i), i as f64).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers { r.join().unwrap(); }
        assert_eq!(store.kv_get("b", "ws:r:k").unwrap(), serde_json::json!(499));
    }

    #[test]
    fn recent_history_from_memory_matches_disk_row_for_row() {
        let store = Store::open_with(&temp_db("tail"), true).unwrap();
        let t0 = now();
        store.ledger_append("ws:r", "edit_reported", &serde_json::json!({"z": 1, "a": {"y": 2.5, "b": "é"}}), t0 - 10.0).unwrap();
        // first read makes the tail
        let _ = store.ledger_since("ws:r", t0 - 60.0);
        store.ledger_append("ws:r", "check_performed", &serde_json::json!({"user": "bob", "paths": ["a.py"]}), t0 - 5.0).unwrap();
        store.ledger_append("ws:other", "check_performed", &serde_json::json!({}), t0).unwrap();
        // a row written with an old timestamp (a repo move) still shows
        store.ledger_append("ws:r", "moved", &serde_json::json!({"n": 1}), t0 - 30.0).unwrap();
        for since in [t0 - 60.0, t0 - 20.0, t0 - 6.0, t0 - 1.0, t0 + 1.0] {
            let memory: Vec<(i64, String, String, f64)> = store.ledger_since("ws:r", since).into_iter()
                .map(|r| (r.seq, r.kind, r.payload.to_string(), r.ts)).collect();
            let disk: Vec<(i64, String, String, f64)> = store.ledger_since_disk("ws:r", since).into_iter()
                .map(|r| (r.seq, r.kind, r.payload.to_string(), r.ts)).collect();
            assert_eq!(memory, disk, "since {since}");
        }
        // older than the window: disk answers
        assert_eq!(store.ledger_since("ws:r", 0.0).len(), 3);
        // and the chain an append builds from the cached head verifies
        assert_eq!(store.ledger_verify().0, true);
    }

    #[test]
    fn group_committed_writes_all_land_and_the_chain_holds() {
        let store = std::sync::Arc::new(Store::open_with(&temp_db("group"), true).unwrap());
        let threads: Vec<_> = (0..16).map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    let row = store.ledger_append("ws:r", "check_performed", &serde_json::json!({"t": t, "i": i}), now()).unwrap();
                    assert!(row["seq"].as_i64().unwrap() > 0);
                    store.kv_put("b", &format!("ws:r:{t}"), &serde_json::json!(i), now()).unwrap();
                    assert_eq!(store.kv_get("b", &format!("ws:r:{t}")).unwrap(), serde_json::json!(i));
                }
            })
        }).collect();
        for t in threads { t.join().unwrap(); }
        assert_eq!(store.ledger_count("ws:r"), 3200);
        assert_eq!(store.ledger_verify(), (true, None, 3200));
        let seqs: std::collections::BTreeSet<i64> = store.ledger_since("ws:r", 0.0).iter().map(|r| r.seq).collect();
        assert_eq!(seqs.len(), 3200);
    }

    #[test]
    fn a_row_appended_later_lands_in_order_with_the_chain_whole() {
        let store = Store::open_with(&temp_db("later"), true).unwrap();
        store.ledger_append("ws:r", "edit_reported", &serde_json::json!({"n": 1}), now()).unwrap();
        store.ledger_append_later("ws:r", "check_performed", &serde_json::json!({"n": 2}), now());
        store.ledger_append("ws:r", "edit_reported", &serde_json::json!({"n": 3}), now()).unwrap();
        store.drain_if_waiting();
        let kinds: Vec<String> = store.ledger_since("ws:r", 0.0).into_iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec!["edit_reported", "check_performed", "edit_reported"]);
        assert_eq!(store.ledger_verify(), (true, None, 3));
    }

    #[test]
    fn trees_are_read_from_memory_and_written_at_the_tree_flush() {
        let path = temp_db("trees");
        {
            let store = Store::open_with(&path, true).unwrap();
            store.put_tree("ws:r", "alice", &serde_json::json!({"root": "a1", "files": {"x.py": "h1"}}), 1.0).unwrap();
            store.put_tree("ws:r", "alice", &serde_json::json!({"root": "a2", "files": {"x.py": "h2"}}), 2.0).unwrap();
            assert_eq!(store.get_tree("ws:r", "alice").unwrap()["root"], "a2");
            assert_eq!(store.list_workspaces("ws:r")[0]["root"], "a2");
            assert_eq!(store.list_scopes("ws:"), vec!["ws:r"]);
            let disk: i64 = store.writer().query_row("SELECT COUNT(*) FROM shared_trees", [], |r| r.get(0)).unwrap();
            assert_eq!(disk, 0, "nothing on disk before the tree flush");
            store.flush_trees(true);
        }
        let store = Store::open_with(&path, true).unwrap();
        assert_eq!(store.get_tree("ws:r", "alice").unwrap()["root"], "a2");
        let listed = store.list_workspaces("ws:r");
        assert_eq!((listed.len(), listed[0]["root"].as_str()), (1, Some("a2")));
    }

    #[test]
    fn chain_verification_walks_only_what_is_new() {
        let store = open_memory();
        for i in 0..5 {
            store.ledger_append("t:r", "edit_reported", &serde_json::json!({"user": "alice", "i": i}), 1.0 + i as f64).unwrap();
        }
        assert_eq!(store.ledger_verify_at(100.0, false), ((true, None, 5), 5));
        // the second ask walks nothing
        assert_eq!(store.ledger_verify_at(101.0, false), ((true, None, 5), 0));
        store.ledger_append("t:r", "edit_reported", &serde_json::json!({"user": "bob"}), 7.0).unwrap();
        assert_eq!(store.ledger_verify_at(102.0, false), ((true, None, 6), 1));

        // a tampered OLD row: invisible to the incremental pass by design…
        store.conn.lock().unwrap()
            .execute("UPDATE ledger SET payload = ? WHERE seq = 3", ["{\"user\": \"mallory\"}"]).unwrap();
        assert_eq!(store.ledger_verify_at(103.0, false), ((true, None, 6), 0));
        // …caught on demand, and the verdict then sticks without re-walking
        assert_eq!(store.ledger_verify_at(104.0, true), ((false, Some(3), 6), 3));
        assert_eq!(store.ledger_verify_at(105.0, false), ((false, Some(3), 6), 0));
        // repaired history is re-read at the hourly walk
        let good = canonical_sorted(&serde_json::json!({"user": "alice", "i": 2}));
        store.conn.lock().unwrap().execute("UPDATE ledger SET payload = ? WHERE seq = 3", [good]).unwrap();
        assert_eq!(store.ledger_verify_at(106.0, false).0, (false, Some(3), 6));
        assert_eq!(store.ledger_verify_at(106.0 + FULL_WALK_EVERY_S, false), ((true, None, 6), 6));

        // a bad NEW row — wrong link to the previous row — is caught at once
        let bogus = chain_hash(&"f".repeat(64), "t:r", 8.0, "edit_reported", &serde_json::json!({"user": "eve"}));
        store.conn.lock().unwrap().execute(
            "INSERT INTO ledger (scope, ts, kind, payload, prev_hash, hash) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["t:r", 8.0, "edit_reported", "{\"user\": \"eve\"}", "f".repeat(64), bogus],
        ).unwrap();
        assert_eq!(store.ledger_verify_at(107.0 + FULL_WALK_EVERY_S, false), ((false, Some(7), 7), 1));
        // a truncated tail takes the anchor with it: the walk starts over at
        // once instead of trusting a link that is no longer there
        store.conn.lock().unwrap().execute("DELETE FROM ledger WHERE seq >= 6", []).unwrap();
        assert_eq!(store.ledger_verify_at(108.0 + FULL_WALK_EVERY_S, false), ((true, None, 5), 5));
    }

    #[test]
    fn prefix_scans_read_underscore_and_percent_literally() {
        // `_` and `%` are LIKE wildcards; a repo id can carry both. The scans
        // use GLOB with `[` escaped, as Python's driver does, so a per-scope
        // read of api_v2 never returns api-v2's rows (or api[v2]'s).
        let store = open_memory();
        for scope in ["ws:acme/api_v2", "ws:acme/api-v2", "ws:acme/api%v2", "ws:acme/api[v2]"] {
            store.eph_set(&format!("intent:{scope}:x"), &serde_json::json!({"scope": scope}), Some(60.0)).unwrap();
            store.kv_put("inbox", &format!("{scope}:bob:x"), &serde_json::json!({"scope": scope}), 1.0).unwrap();
        }
        for scope in ["ws:acme/api_v2", "ws:acme/api-v2", "ws:acme/api%v2", "ws:acme/api[v2]"] {
            let eph: Vec<String> = store.eph_scan(&format!("intent:{scope}:"))
                .into_iter().map(|(_, v)| v["scope"].as_str().unwrap().to_string()).collect();
            assert_eq!(eph, vec![scope.to_string()], "eph_scan widened on {scope}");
            let kv: Vec<String> = store.kv_list("inbox", &format!("{scope}:"))
                .into_iter().map(|(_, v)| v["scope"].as_str().unwrap().to_string()).collect();
            assert_eq!(kv, vec![scope.to_string()], "kv_list widened on {scope}");
        }
        // the plain prefix still fans out across the workspace
        assert_eq!(store.eph_scan("intent:ws:").len(), 4);
        assert_eq!(store.kv_list("inbox", "ws:").len(), 4);
    }

    #[test]
    fn count_active_scopes_counts_distinct_scopes_since_a_timestamp() {
        let store = open_memory();
        store.ledger_append("ws1:repo-a", "report", &serde_json::json!({}), 100.0).unwrap();
        store.ledger_append("ws1:repo-a", "report", &serde_json::json!({}), 200.0).unwrap();
        store.ledger_append("ws1:repo-b", "report", &serde_json::json!({}), 50.0).unwrap();
        store.ledger_append("ws2:repo-c", "report", &serde_json::json!({}), 200.0).unwrap();

        // repo-a and repo-b both wrote under ws1, but repo-b's only row is
        // before the cutoff — only repo-a counts from that point on.
        assert_eq!(store.count_active_scopes("ws1:", 100.0), 1);
        // widen the window to include repo-b's earlier row too.
        assert_eq!(store.count_active_scopes("ws1:", 0.0), 2);
        // a prefix that matches nothing answers zero, not -1 (the "query
        // failed" sentinel is reserved for an actual error).
        assert_eq!(store.count_active_scopes("ws-none:", 0.0), 0);
    }
}
