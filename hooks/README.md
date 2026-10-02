# collide-hooks

Collide's agent-side components as one native binary, `collide-hook`. Together
they replace every Python client the protocol needs, so a developer's machine
runs Collide without a Python interpreter at all.

| Subcommand | Replaces | Fires |
|---|---|---|
| `gate` | `gate_hook.py` | PreToolUse, before every Edit/Write/shell write |
| `report` | `report_hook.py` | PostToolUse on every tool call, and on Stop |
| `pre-commit` | `pre_commit.py` | the git pre-commit hook |
| `watch` | `collide-watch` | the disk watcher, for humans editing without an agent |
| `check-credentials` | `report_hook.py --check-credentials` | SessionStart |
| `add-credentials` | `report_hook.py --add-credentials` | one-time install |

## Why this is where Rust pays

These are not hot loops, they are hot *processes*. The gate runs before every
write with a hard 200 ms fail-open budget, and a Python interpreter spends
about 33 ms of that budget merely booting. The reporter fires on every single
tool call an agent makes.

Measured on this machine, same input, same work:

| | Startup to decision |
|---|---|
| `python gate_hook.py` | 34.4 ms |
| `collide-hook gate` | 2.0 ms |

About 17x, on the component that runs most often. An agent making 200 edits in
a session gets roughly six seconds back, and that time is in-band: the
developer waits on it.

## Behaviour is held identical, not assumed

`server/tests/test_rust_hooks.py` drives the Python hook and this binary with
the same stdin, the same filesystem and the same fake server, then asserts
they agree on the exit code, stdout, stderr and the exact request body they
sent. It covers allow and block, fail-open when the server is down, repo-root
discovery from a subdirectory, shell-write detection, `apply_patch` across
several files, out-of-repo files, usage-limit reporting, turn cost from the
transcript, and all three credential resolution orders.

A hook decides whether a write is blocked and what teammates see, so a
behavioural difference here is a correctness bug, not a performance footnote.

## Install

```sh
cd server/collide-hooks
cargo build --release
mkdir -p <your repo>/.collide/bin
cp target/release/collide-hook <your repo>/.collide/bin/
```

The generated hook commands look for `.collide/bin/collide-hook` first, then
`collide-hook` on PATH, and fall through to the committed Python artifact when
neither exists. Nothing breaks if the binary is absent; it is strictly a
speedup.

## Fail open, in Rust

The Python hooks guarantee fail-open with a bare `except`. The Rust equivalent
is a `catch_unwind` around the whole dispatch that exits 0, plus `Result` at
every I/O edge. The one deliberate non-zero exit is the gate returning 2 for a
real block, and the pre-commit gate returning 1.

## Watching

```sh
collide-hook watch --once                 # one pass, for CI or cron
collide-hook watch --interval 5           # poll forever
```

Server, token and repo id come from `.collide/config.json` and
`~/.collide/credentials.json` when not passed as flags, exactly as the hooks
resolve them. Reports carry the `collide-watch` identity, which is what marks
an edit disk-confirmed rather than agent-claimed.
