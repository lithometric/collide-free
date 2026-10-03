# Collide: run multiple AI coding agents on one repo without conflicts

[![License: MIT](https://img.shields.io/badge/hooks-MIT-34d399.svg)](#what-is-in-this-repository)
[![npm](https://img.shields.io/npm/v/collidemcp.svg?label=npx%20collidemcp)](https://www.npmjs.com/package/collidemcp)
[![MCP](https://img.shields.io/badge/MCP-server-34d399.svg)](https://collidemcp.com/docs)
[![Works with](https://img.shields.io/badge/works%20with-Claude%20Code%20%C2%B7%20Codex%20%C2%B7%20Cursor-111.svg)](https://collidemcp.com)

**Collide is the coordination layer for multiplayer coding agents: it keeps Claude Code, Codex and Cursor agents, on one machine or across a team's machines, from overwriting each other on one codebase, and hands each agent what the others already learned.**

This repository is **Collide Free**: run as many coding agents as you like on one machine, in as many repositories as you like, without them tripping over each other. Free, no account, and nothing leaves your machine.

```sh
curl -fsSL https://collidemcp.com/install.sh | sh
```

or, with Node (any terminal, including Windows PowerShell):

```sh
npx collidemcp
```

One install covers every repository on the machine. Agents already open pick it up at their next prompt.

## Why

Run two or three Claude Code agents (or Codex, or Cursor) on the same codebase and they start colliding: one renames a function another is still calling, two write the same module, a push fails and the agent burns turns pulling and re-testing. Each agent also re-reads the same files the others just read. Separate git worktrees keep their files apart, but the work still meets at merge time.

Collide sits under the agents you already run. It does not run agents or host machines. Its hooks tell each agent what the others are changing while they work, hold a write that would clash, and hand each prompt the exact code it is about, so agents search less.

In Collide's benchmark (12 Claude Code agents at once on one repository, hard tasks), agents with Collide used **53% fewer tokens** than the same agents without it. [The study](https://collidemcp.com/benchmarks/study-5-twelve-agents-53-percent) · [all benchmarks](https://collidemcp.com/benchmarks)

## What your agents get

- **A briefing before they start.** The functions a prompt is about, with their signatures, who calls them and what changed recently, handed to the agent before it searches.
- **Each other's work as it happens.** When another agent renames a function yours uses, or writes the module your task needs, your agent hears about it on its next step.
- **Writes that would clash are held.** An edit against code another agent just changed waits until your agent has their version.
- **Lint from teammates' changes.** A rename or a new signature becomes a rule your agents are checked against.
- **One-step pushes.** `git push` brings in the other agents' commits, runs your tests on the result and pushes, instead of the reject, pull, retry loop.
- **Messages between agents.** `collide message "..."` reaches the other agents in the repository, or one of them (`--to`), or whoever is on a piece of code (`--about path.py::symbol`).
- **Notes that stay with the code.** When an agent finishes a change, what it said about the change is kept on the functions it touched, and the next agent there is handed it.
- **MCP tools.** `get_symbol`, `blast_radius`, `recap`, `remember` and more, from the same code graph the hooks keep.
- **What it saved.** Once a day your agent's terminal shows the tokens Collide saved on this machine.

## How it compares

| | Collide | Git worktrees | Agent orchestrators and cloud agents |
|---|---|---|---|
| Where agents run | Where they already run | Where they already run | On their servers or VMs |
| Agents see each other's changes while working | Yes | No, only at merge | Varies |
| Holds a write that would clash | Yes | No | No |
| Briefs each agent on the code its prompt is about | Yes | No | Varies |
| Works with Claude Code, Codex and Cursor together | Yes | Yes | Varies |

They combine: agents in separate worktrees still benefit from Collide telling them what the others are changing. Detailed comparisons: [collidemcp.com/blog/compare](https://collidemcp.com/blog/compare).

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

Collide Free connects the agents on one machine. [Collide Team](https://collidemcp.com/pricing) ($10 per seat per month, 14 days free) connects machines: your teammates' agents see your agents' work, and prompts are matched to code by meaning, not only by the names they use. `collide upgrade` moves a machine over with nothing else to download, and brings its history along.

## Frequently asked questions

**How do I run multiple Claude Code agents on the same repo without conflicts?**
Install Collide (one command above), then start your agents as usual. Each agent hears what the others are changing, a write against code another agent is changing is held until it has their version, and `git push` rebases, tests and pushes in one step. Guide: [run multiple Claude Code agents on one repo](https://collidemcp.com/blog).

**Does it work with Codex and Cursor?**
Yes. The install registers Collide's hooks for Claude Code, Codex and Cursor, and its MCP server for any MCP client.

**Is Collide an MCP server?**
It is both hooks and an MCP server. The hooks do the work with no extra model calls; the MCP server adds tools an agent can call on purpose. The hosted server is `https://mcp.collidemcp.com`.

**Does my code leave my machine?**
Not on Collide Free. On Team, the structure of your code (names, signatures, who calls what) reaches Collide's servers; it is parsed on your machine and file contents are not sent.

**Is it really free?**
Collide Free is free with no account and no time limit, for one machine.

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

---

If Collide saved your agents a merge conflict, a **star** helps other developers running parallel agents find it. Questions and ideas: [open an issue](https://github.com/lithometric/collide-free/issues). Website: [collidemcp.com](https://collidemcp.com) · Claude Code plugin: [lithometric/collide-plugin](https://github.com/lithometric/collide-plugin)
