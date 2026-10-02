# Collide Free

Run as many coding agents as you like on one machine, in as many repositories as you like, without them tripping over each other. Claude Code, Codex and Cursor, side by side. Free, no account, and nothing leaves your machine.

```sh
curl -fsSL https://collidemcp.com/install.sh | sh
```

One install covers every repository on the machine. Agents already open pick it up at their next prompt.

## What your agents get

- **A briefing before they start.** The functions a prompt is about, with their signatures, who calls them and what changed recently, handed to the agent before it searches.
- **Each other's work as it happens.** When another agent renames a function yours uses, or writes the module your task needs, your agent hears about it on its next step.
- **Writes that would clash are held.** An edit against code another agent just changed waits until your agent has their version.
- **Lint from teammates' changes.** A rename or a new signature becomes a rule your agents are checked against.
- **One-step pushes.** `git push` brings in the other agents' commits, runs your tests on the result and pushes, instead of the reject, pull, retry loop.
- **Messages.** `collide message "..."` reaches the other agents in the repository, or one of them (`--to`), or whoever is on a piece of code (`--about path.py::symbol`).
- **MCP tools.** `get_symbol`, `blast_radius`, `recap`, `remember` and more, from the same map the hooks keep.
- **What it saved.** Once a day your agent's terminal shows the tokens Collide saved on this machine.

## Commands

| Command | What it does |
|---|---|
| `collide status` | What Collide did on this machine, and where its data is |
| `collide message "..."` | Tell the other agents in this repository something |
| `collide agents-md` | Add Collide's section to this repository's AGENTS.md, after what is already there |
| `collide login` | Link this machine to a Collide account |
| `collide upgrade` | Bring in your team: start Team for your workspace |
| `collide uninstall` | Take Collide's hooks out (your local data stays) |

## Where your data is

Everything stays in `~/.collide/local` on your machine: a small server on `127.0.0.1` that only this machine can reach. Collide Free makes no calls out, except to check for a signed update of itself.

## Team

Collide Free connects the agents on one machine. [Collide Team](https://collidemcp.com/pricing) connects machines: your teammates' agents see your agents' work, and prompts are matched to code by meaning, not only by the names they use. `collide upgrade` moves a machine over with nothing else to download, and brings its history along.

## Build from source

```sh
cd free && cargo build --release
./target/release/collide install
```

`free/` builds the one program, `collide`: the hooks your agents run (`hooks/`, with the parser in `core/`) and the local server (`collide local`).

## What is in this repository

| Folder | What it is | License |
|---|---|---|
| `hooks/` | What runs in your agent sessions: briefings, the write gate, landing, messages, the MCP bridge, install | MIT |
| `core/` | The code parser and graph the hooks and the engine share | MIT |
| `free/` | The engine: the local server that keeps the map, the briefings, lint, landing and memory | [FSL-1.1-MIT](free/LICENSE.md) |
| `plugin/` | The Claude Code plugin | MIT |

The engine's license lets you use, change and share it for anything except offering a competing product or service; each version becomes MIT two years after its release.
