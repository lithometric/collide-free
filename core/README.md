# collide-core

Collide's parse hot path, in Rust. **Optional.** The server imports it when
it is installed and runs its own pure-Python engine when it is not, so a
deploy without this behaves identically — only slower.

## Why this and not more

`report_edit` is the highest-frequency call in the system: every Edit and
Write an agent makes fires the PostToolUse hook, which parses the file. With
several agents on one repo that is the busiest thing the server does. It is
also pure CPU with no I/O, which makes it the one place where Rust changes
the shape of the problem rather than shaving a constant.

The rest of the server — HTTP, OAuth, billing, storage drivers, the ledger —
is I/O-bound. Rewriting it would trade 349 passing tests for latency that is
dominated by the network anyway.

## What is in it

Two things, both chosen by measurement rather than by taste.

**The parse engine.** `report_edit` fires on every Edit and Write an agent
makes, and parsing is the only CPU-bound work on that path.

**Louvain community detection.** This is the most expensive thing Collide
computes. `repo_map` clusters the symbol graph into subsystems, and `repo_map`
rides in every briefing — the first call every agent makes. The partition is
cached by graph fingerprint, but the fingerprint moves on every reported edit,
so the cache is coldest exactly when the most agents are working.

## What it is

A line-for-line port of `collide/parser/__init__.py`: same declaration walk,
same typed edges (`inherits` / `calls` / `uses_type` / `references`), same
signatures and hashes, the same twelve tree-sitter grammars. `tests/
test_rust_core.py` feeds both engines Collide's own source plus a snippet per
language and asserts the outputs are identical, so "faster" can never quietly
mean "different".

The parse runs inside `Python::allow_threads`, so the GIL goes back to other
agents' requests while tree-sitter works.

## Measured, on this repo's own source

| | 1 thread | 4 threads |
|---|---|---|
| Python engine | 222 ms | 219 ms |
| Rust core | 129 ms | 53 ms |

Single-threaded it is about 1.7x. The bigger number is the scaling: the
Python engine holds the GIL through the whole tree walk, so four agents
reporting at once serialise (1.01x). The Rust core overlaps them (2.45x), so
four concurrent agents see roughly **4x** end to end.

Louvain is where the large multiple actually is, because NetworkX does that
work in interpreted Python rather than delegating to C:

| Graph | NetworkX | Rust core | |
|---|---|---|---|
| 2,800 nodes | 84 ms | 4.5 ms | 18.5x |
| 14,000 nodes | 466 ms | 26 ms | 18.1x |
| 70,000 nodes | 3,438 ms | 184 ms | 18.7x |

Modularity comes out equal or very slightly better (0.9872 against 0.9871 on
the largest graph), so the speed is not bought with a worse clustering.
Louvain is heuristic, so two correct implementations need not agree node for
node; what the test holds equal is the quality of the partition each reaches
and that every node lands in exactly one community.

## Build

```sh
cd server/collide-core
maturin develop --release        # installs into the active venv
cargo check                      # type-check without linking to libpython
```

`cargo build` alone will fail to link: the `pyo3/extension-module` feature
leaves Python symbols undefined on purpose, and maturin supplies them.

## Getting it into production

`server/railway.json` points at `Dockerfile`, which does NOT build this — the
deployed server runs the pure-Python engine and behaves identically. To ship
the core, change that one line to `Dockerfile.rust`. That image compiles the
crate in a builder stage, installs the wheel, and asserts the import at build
time so a broken core fails the deploy instead of silently serving the slow
path. It adds several minutes to every deploy.

## Forcing the pure-Python path

```sh
COLLIDE_PARSER=python
```

Useful for reproducing a parse bug against the reference engine.

## Everything else stays Python, deliberately

Profiling the write path after the parse moved over: per 101 files, parsing
takes 109 ms and *everything else combined* takes about 12 ms — per-line
hashing 9.5 ms, graph edge resolution 2.3 ms, merkle hashes and tree build
0.1 ms. Porting those would be motion without movement. The HTTP, OAuth,
billing, storage and ledger layers are I/O-bound, where the latency is a
network round trip and a database write that Rust does not make faster.

## Adding a language

One `LangSpec` entry in `src/langs.rs` and one in `collide/parser/`, then run
the differential test. The engine itself never changes.
