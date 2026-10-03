# Collide for Claude Code

Run several Claude Code agents on one repository, alone or with a team, without them tripping over each other.

- **Each agent starts in the right place.** When you type a prompt, the agent is handed the exact functions it is about: their signatures, who calls them, and what changed recently. Less searching.
- **Agents hear about each other's work.** When a teammate's agent renames a function yours uses, or starts writing the module your task needs, your agent is told on its next step.
- **Writes that would clash are stopped.** An edit against a function a teammate just changed is held until your agent has their version.
- **Pushing takes one step.** `git push` is rebased onto your teammates' new commits, tested and pushed for the agent, instead of the reject-pull-retest loop.

In our team benchmark (12 agents, 4 people, one repository), agents with Collide used 47% fewer tokens than the same agents without it, with every task correct. [The study](https://collidemcp.com/benchmarks/study-3-teams-easy-to-hard).

## Install

In Claude Code, send these as two separate messages:

```
/plugin marketplace add lithometric/collide-plugin
```
```
/plugin install collide@collide
```

That is all. The next session sets Collide up for every repository on this machine: free, no account, and nothing leaves the machine. Run as many agents as you like in as many repositories as you like; agents in the same repository see each other's work and can message each other.

`/collide:status` shows what Collide did on this machine. `/collide:login` links the machine to a Collide account: once that account's workspace is on Team, the machine joins it by itself and brings its history along, so teammates' agents on other machines join in.

## What is in this repository

- `hooks/hooks.json`: the Claude Code hooks (session start, prompt, every read, edit and command, failed calls, stop), all run through `bin/collide`.
- `bin/collide`: the launcher. It fetches the pinned `collide-hook` binary once and runs it; it fails open.
- `skills/collide`: teaches your agent how to read what Collide tells it (briefings, Δ lines, blocked writes, landings) and to say when a teammate's work changed its plan.
- `commands/`: `/collide:login` and `/collide:status`.

The MCP server is optional. To give your agent Collide's tools as well (look up any symbol, blast radius, team memory), add it once: `claude mcp add --transport http collide https://mcp.collidemcp.com`.

## What it sends, and what it keeps

On the free version, nothing: everything stays in `~/.collide/local` on your machine. A signed-in machine tells your account only its name and its totals (tokens saved, agents, repositories) until its workspace is on Team. On Team, this is what reaches Collide:

- Your code's **structure**: function and class names, signatures, docstrings, and which function calls which. Parsed on your machine; file contents are not sent.
- What your agents are **doing**: which files they read, edit and run, and when.
- Your **prompts** are used once to find the code they are about, then dropped. Only a numeric summary of the prompt's meaning is kept, for two hours.

Your sign-in credential stays in `~/.collide/credentials.json` on your machine and is never written into the repository.

## How it runs

Every hook goes through `bin/collide`, which runs Collide's native `collide` program. On first use it downloads Collide (one program: the hooks and the local server) pinned in `hooks.lock` for your platform, and refuses any file whose SHA-256 does not match; later updates must carry Collide's signature. Nothing runs through Python or Node. If a binary cannot run, the hooks stay silent and never block your agent.

The local server listens on 127.0.0.1 only and keeps its data in `~/.collide/local`. A repository already set up with Collide's `setup` command keeps its own hooks and connection; the plugin stands down there so nothing is reported twice.

## Plans

Free is everything on one machine: unlimited agents and repositories, no account. Team connects agents across machines, adds the dashboard and matches prompts to code by meaning ([pricing](https://collidemcp.com/pricing)).

## License

This plugin (the hooks, the launcher and the commands in this repository) is MIT licensed. The `collide-hook` binary it downloads and the Collide service are not open source; they are free to use under Collide's terms.
