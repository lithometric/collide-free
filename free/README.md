# collide-server (Rust)

The server rewrite, in progress. This is a strangler migration, not a
big-bang replacement: the Rust server speaks to the **same SQLite database**
with the same schema, key shapes and JSON encodings, so endpoints move across
one at a time while both halves run.

## Why it is done this way

The Python server is 382 passing tests of protocol semantics — three-valued
parse state, when a symbol counts as removed, compare-and-swap rejection, the
commutativity table, scar mining, staleness confidence, seat enforcement,
OAuth. That behaviour *is* the product. Porting it wholesale means
re-deriving all of it at once with no reference implementation to check
against, and shipping nothing else while it happens.

Moving an endpoint at a time means every step is verifiable and the product
keeps working throughout.

## A caveat while both halves run

An endpoint must be served by exactly ONE of them at a time, routed at the
proxy. Durable state is shared, but the in-process event bus is not: an event
published by the Rust half would not reach a dashboard websocket held open by
the Python half. Cutting over is a routing change, not a load-balance.

## What is done

- **The three storage tiers** over the Python schema, connection-per-lock
  exactly like the Python driver (which is also what makes the ledger's
  read-head-then-insert race-free).
- **The ledger hash chain** and **the merkle tree**. These had to come first:
  the chain is what makes the record tamper-evident and the root is what two
  workspaces compare to decide whether they diverged. A one-bit disagreement
  reads as a broken chain and makes every collision check wrong.
- **Repo routing** — the alias registry that decides which canonical id an
  incoming name means. Get this wrong and an agent finds an empty scope
  instead of its teammates' work.
- **Credential resolution and per-repo access**, including the read-only and
  GitHub-collaborator refusals. Fails open on infrastructure trouble and
  closed only on an explicit denial, exactly as before.
- **`/gate`** — the write gate, with its blocked marker, scar-tissue score
  and both ledger rows.
- **`/presence`** — focus and last-action markers, and the per-turn token
  counter with its turn dedupe.
- **The write path** — the parse state machine, the line diff (a port of
  `difflib.SequenceMatcher`), the merkle advance, the graph update with its
  reverse index, the structured ledger row, and the hot markers the gate
  reads.
- **The near-realtime channel** — the durable event ring, and the
  `since_your_last_call` deltas that ride home on every response. MCP cannot
  push mid-turn, so the calls the protocol already mandates are the delivery
  mechanism.
- **The report envelope** — who the write was recorded as, whether the repo's
  committed hooks or its AGENTS block are stale, and whether it was renamed
  underneath this client.
- **The semantic linter** — the registry a teammate's certain rename, removal
  or signature change writes, the scan of the reporting file against it, the
  judgment-class reconciliation lifecycle, and override detection when a write
  lands on a path the gate just blocked.
- **Repo aliasing and renaming**, including adopting a rename detected from the
  client's git origin, with the refusal that stops a rename burying a live
  scope.
- **Usage metering**, on the same monthly row the Python half increments.
- **Drafts, tripwires, rewrite counters and the watchdog** — the advisory half
  of a report, none of which can block an edit.
- **`check_collisions`** — the merkle fast path, the symbol diff, live intents
  with their leases, and the salience scores. Exposed at `/check`, which is
  NOT a mirror of a Python route: the check is an MCP tool there, so the
  differential test drives Python through the tool and this through the route
  against one database.
- **The typed-operation algebra and `declare_intent`** — the closed operation
  set, the commutativity table, collision prediction against every other live
  lease, and re-litigation detection against settled decisions.
- **The rest of the intent lifecycle** — `heartbeat` renews a lease,
  `complete_intent` releases it and turns the rationale into a settled record
  plus an anchored note on each symbol it changed, `defer_intent` leaves a
  tripwire that outlives the agent that wrote it.
- **Anchored memory, both halves** — the anchor grammar, the hash and
  revision captured at write time, supersession, and the honesty layer on
  recall: stale when the anchor's hash moved, `possibly_stale` when something
  the anchor DEPENDS on changed after the note was written, conflict when a
  sibling was written against a different version of the same anchor, and a
  confidence earned from that event history rather than from age.
- **The briefing's ledger half** — the consolidation step that turns an
  append-only ledger into the gist a session starts from: who changed what
  through which agent, renames to adapt to, collision hotspots, abandoned
  intents, fired and live tripwires, the check-before-write rate, and the
  convention-drift report that diffs the protocol block's own rules against
  what the ledger shows agents doing. The graph-derived half — the repo map —
  is still Python's.
- **The scar miner** — failed attempts recovered from the ledger with zero
  voluntary writes: an intent declared, edits landing on its symbols, then a
  later edit restoring the prior hash. Nobody writes "I tried this and it did
  not work", so the shape it left behind is the only record there is. Revert
  detection reads the scope-level hash trajectory, so a restore from any
  workspace counts, and the notes are deduped by intent and symbol.
- **`simulate_merge`** — the counterfactual: if these workspaces landed
  together right now, which symbols would disagree, which anchored notes go
  stale, and which settled rationales would be re-litigated. Precision over
  recall: only the same symbol, held by two workspaces, at two different
  hashes. A one-sided add is in-flight work.
- **`blind_spots`** — where the map is unreliable, answered about itself:
  files that never parsed cleanly, files serving state old enough to be
  wrong, workspaces gone quiet.
- **`differential_check`** — the narrow interface oracle: does my diff
  preserve the assumptions the other workspace's code relies on? FAIL is
  earned (a signature diverging on a symbol their parsed code references);
  implementation-only divergence is reported as nothing rather than as a pass
  it did not earn; everything else is UNKNOWN, because reliance outside the
  parsed fragment is unknowable here.
- **The graph read side** — assembling the whole graph from the per-file
  records, resolving cross-file edges against the current file set, Louvain
  clustering with the co-change edges mined from the ledger, and the overlays
  that make it Collide's graph rather than a static map: who owns each region,
  what is leased right now, what was tried and reverted, and which symbols
  would collide if touched. Both the `repo_map` a briefing carries and the
  dashboard's full graph payload come out of it.
- **The blast radius** — transitive dependents by distance, and the breaking
  overlay that rides on it: a rename anchors on the old name, the new name
  and the file, so the signal always lands on a node that exists, and the
  ripple follows the stale reference only, so a caller that adopts the new
  name drops out of the radius.
- **MCP over Streamable HTTP**, at the same `/mcp` path the Python half
  serves. This was the structural blocker: however much of the engine was
  ported, an agent could not connect to it, because an agent speaks MCP and
  every route here speaks plain JSON. The protocol itself is the official Rust
  SDK's job; what is Collide's is the tool table, the per-caller filtering
  that hides write tools from read-only members, and the dispatch into the
  engine. The tool descriptions and schemas are GENERATED from the Python
  docstrings rather than retyped, because a description is not documentation
  here — it is what an agent reads to decide which tool to call, so a reworded
  one is a different tool.
- **The graph queries** — `blast_radius`, `graph_neighbors`, `graph_path`,
  `repo_map`, `get_symbol`. These are the reads an agent does INSTEAD of
  opening files and following imports, so two halves disagreeing here would
  mean two agents on one repo working from different maps of it. The miss case
  is deliberate: a symbol the graph has not seen returns a named gap, never an
  empty result, because an empty blast radius reads as "nothing depends on
  this" and that is the most dangerous thing this could say wrongly.
- **`list_activity`** and the identity labels that go with it — who is here,
  what they are touching, what they have claimed. Nicknames are resolved per
  CALLER, because they are that person's private names for other people.
- **The account layer** — OAuth discovery, dynamic registration, authorize
  and token (mounted beside the MCP endpoint), workspaces with members,
  invites, seats and repo aliases, and the protocol block with the four setup
  tools that write it into agents' config files. The block text and every
  hook command were extracted from the *running* Python module into files
  pulled in with `include_str!`, so byte-identity is by construction rather
  than by review, and a golden test holds each JSON artifact to Python's
  output. Token secrets and invite codes come from `/dev/urandom`, never from
  the id generator, which is not a CSPRNG and does not claim to be.
- **The native-hook registry**, so `setup` offers the compiled hooks exactly
  when this server has a build to serve and stays silent otherwise.
- **The Guava CRM integration**, with both tools hidden from the menu until a
  CRM is connected.
- **Agent messaging, reconciliations, explain_code, graph interop** — a
  message sent through one half is read through the other, because one
  database means one inbox. The GraphML and Cypher exporters were held to
  NetworkX's actual output by running it, not by reading its docs: its writer
  splices each new key at index zero, which is why the `<key>` block comes out
  reversed, and nobody would guess that from a comment.
- `/health`.

One structural change rode in with this batch and is worth knowing about.
Every JSON artifact the setup tools write into a user's repo has to match
Python byte for byte, and Python's `json.dumps` keeps insertion order where
serde_json alphabetised — so `.collide/config.json` came out with its keys
reordered, which would have shown as a diff in every repo on the next sync.
The map now preserves insertion order crate-wide. That made the two places
where *sorted* is a correctness property rather than a formatting choice —
the chain hash and the stored ledger payload — sort explicitly, where before
they leaned on a crate default that had quietly stopped being true. The
encoding-parity test caught the one path that had not been made explicit.
- `parity`, a self-check the Python suite drives.

## What the port actually buys, measured

Both servers on one SQLite file, release build, 40 files and 120 symbols.
`gate`, `presence` and `report` are HTTP against HTTP. `briefing` is Python
called in-process with no transport at all against Rust over real HTTP, which
handicaps Rust by the full round trip — so that number is a floor.

| operation | python p50 | rust p50 | python p95 | rust p95 |
|---|---|---|---|---|
| gate | 0.48ms | 0.31ms | 0.74ms | 0.49ms |
| presence | 0.49ms | 0.28ms | 1.02ms | 0.41ms |
| report | 1.89ms | 1.26ms | 2.38ms | 1.69ms |
| graph (dashboard) | 5.51ms | 2.46ms | 6.09ms | 3.08ms |
| briefing | 16.59ms | 6.81ms | 38.52ms | 7.31ms |

So the server is 1.5x to 2.4x, and the tail is the better part of it: the
briefing's 95th percentile goes from 38.5ms to 7.3ms, because there is no
garbage collector deciding when to pause.

The hook is where it stops being incremental. It runs as a fresh process on
every Edit and Write inside a 200ms budget, so startup is most of its cost:

| gate hook, cold start to exit | p50 | p95 | share of the 200ms budget |
|---|---|---|---|
| python | 41.5ms | 45.3ms | 21% |
| rust | 2.8ms | 3.1ms | 1% |

Fifteen times, and it is the number that matters most, because it is paid on
every single edit an agent makes rather than once a session.

Two caveats worth stating. The graph assembly and Louvain are cached on the
snapshot fingerprint here exactly as they are in Python — without that the
briefing is about 1.8x slower on a repeat, and the first cut of this port
simply did not have them. And a benchmark that pointed the Python side at
`/briefing` or `/check` is measuring a 404: those are MCP tools there, not
HTTP routes, and only `/health`, `/gate`, `/inbox`, `/presence` and `/report`
are served over HTTP by the Python half.

## The proof, not the promise

`server/tests/test_rust_server.py` holds the two implementations together:

- the chain hash over a fixed input is byte-identical
- the merkle root, directory hashes and file hash are identical
- both JSON encodings match — the compact one the chain hash uses, and
  Python's default-separator form the stored payload uses
- Python writes a real hash-chained ledger and **Rust re-verifies every
  link**, which only passes if schema, encoding, float formatting and hashing
  all agree
- corrupt one payload and Rust stops on exactly that row

`test_rust_endpoints.py` runs both servers against one database and compares
the answer AND the state left behind: the block message an agent reads word
for word, the collisions list, the blocked marker, the salience score, the
ledger rows, the presence markers, token dedupe across turns, and fail-open
on a missing or expired credential.

Ordering is the quiet half of this. Python's `Counter.most_common` breaks
ties by insertion order, and the rows feeding these counters arrive in ledger
sequence — so "first seen" means "happened earliest", which is a real
ordering. Sorting ties by name here would be just as deterministic and
quietly disagree on every tie; the Rust counter remembers arrival order
instead.

Two of the sharpest bugs so far came out of auditing this side rather than
out of a failing test, and neither would have raised anything.

**Rounding.** Rust's `f64::round` breaks ties away from zero; Python's
`round` breaks them to even. Integer edit counts land on exact halves
constantly — sixteen writes with three unchecked is a rate of 0.8125, which
Python stores as 0.812 and this side stored as 0.813, and the summary
sentence an agent reads diverged at every `.5` percent. Every rounding site
now goes through `compat::python_round`, which formats rather than scales:
Rust's float formatter rounds to nearest with ties to even over the true
decimal value, which is exactly what Python's `round` does, including the
cases where multiplying by a power of ten introduces its own error
(`round(2.675, 2)` is 2.67, because 2.675 is really 2.67499999...).

**Id generation.** Both id generators hashed the current nanosecond, and the
clock does not advance once per call: five ids generated in a tight loop came
back as three distinct values, and ten thousand came back as eight hundred.
`complete_intent` writes up to five anchored rationale notes in a loop and
`mine_scars` mints in a loop, so notes were overwriting each other under a
shared key — no error, just a shorter list. `compat::new_id` now mixes a
strictly increasing counter with a per-process random seed, and a test walks
ten thousand.

Four real bugs surfaced while writing the differential tests themselves. `path_key` was sha256 here and sha1
in Python, which would have silently split every path into two key spaces —
the Rust gate finding no hot markers and the dashboard showing half the scar
tissue, with nothing erroring. The forward-slice symbol index kept whichever
definition of a duplicated name arrived first, over two SQL listings that
order nothing, so the same query could blame a different file on each run;
both halves now take the smallest path, which is arbitrary but stable.
`blind_spots` built two lists over those same unordered listings and then
truncated them at fifty, so the same repo reported not merely a different
order but a different SET of blind spots on each call; sorted, the cap always
keeps the same fifty. And the test itself opened a second storage handle,
whose ephemeral startup wipe deleted the markers under test.

## `/report` is complete

Audited line by line against the Python endpoint and its service call: the
write path, the awareness envelope, the semantic lint and its reconciliation
lifecycle, git-origin rename adoption, usage metering, drafts, tripwires,
rewrite counters and the unattended-run watchdog. Fifty-three differential
tests cover it.

Compare-and-swap is deliberately absent rather than missing: the HTTP endpoint
never took `expected_hashes`. That belongs to the MCP `report_edit` tool and
moves with it.

It is still unrouted only because nothing routes yet — cutting over is a proxy
change, and that is the user's call, not a code change here.

## One difference kept on purpose

The Python half caches the preferred repo id in memory at startup; this one
reads it through to storage on every report. Read-through is the behaviour
that survives two processes sharing a database, which is exactly the situation
during this migration. It is covered by its own test rather than left to be
discovered.

## What is next

The port is complete as a server: every MCP tool, every hook endpoint, every
dashboard route, OAuth, billing, the WebSocket feed, and the advisory layer.
`Dockerfile.collide-rs` builds it alone; `Dockerfile.rust` beside it is the
older shape — Python with the Rust core compiled in — and is untouched, so the
cutover is a change of which Dockerfile the Railway service builds and nothing
else. The image builds to 193 MB, and the check that matters passed against
the running container from outside: `/health` reports the Rust engine, and a
real MCP client completes the handshake and lists all thirty-six tools.

The first build failed on something worth knowing about the design: the
server embeds the three Python hook scripts at compile time, because `setup`
hands them to repos as committed artifacts, so the binary has to carry their
source and the image has to copy `client/` in before it builds.

Deliberately not ported: the Postgres and Redis storage drivers. The deploy
runs SQLite on a volume, both halves share that file, and porting two drivers
nothing points at would be work in search of a user. Say so and they get done.

Two known gaps, both stated rather than papered over. The Python half pushes
`tools/list_changed` to live MCP sessions when a member's access changes or a
CRM connects; this half does not yet, so a live session sees its new menu on
reconnect rather than mid-conversation. And two HTTP clients are in the tree —
`ureq` for the synchronous outbound calls and `reqwest` for OAuth's metadata
fetch — which is one more than it needs and a consolidation for later.

Two agents on one login could not be told apart in the ledger until this
batch: the hook never forwarded the session id Claude Code hands it. It does
now, and recap groups spend per conversation — which is the measurement the
swarm claim needs.

For scale: the Python server is about 14,200 lines. The parser (~1,000) was
already Rust; this is roughly another 6,300 across the core and the server.
`service.py` alone is 4,346 lines of protocol semantics, and that is still the
bulk of what remains.

## Build

```sh
cd server/collide-rs
cargo build --release
./target/release/collide-server parity /path/to/collide.db
```
