---
name: collide
description: How to work in a repository where Collide is on, alongside other agents and people. Use when the context holds a "Collide briefing", lines starting with Δ, a "Collide in flight" or "Collide: … just started" notice, a write "Collide blocked", or a "Collide landed" result, or when the user asks what teammates or other agents are doing in this repository.
---

# Working with Collide

Collide's hooks put what the rest of the team is doing into your context between your steps. You never need to call anything for that; read what arrives and act on it.

## What arrives, and what to do

- **Collide briefing** (at session start and after the user's prompt): exact signatures, callers, recent changes and notes for the code your task is about. Start there instead of searching. Its Coverage line says whether the callers listed are complete.
- **Lines starting with Δ**: a teammate changed something you are working on since your last step. Adapt to it; never undo it.
- **Collide in flight / "just started"**: a teammate's work you depend on exists but is not pushed yet. Build against the interface given, or keep to your own part until it lands. Do not write your own copy of it.
- **Collide blocked this write**: your copy of the file is missing a teammate's change. Pull, re-read the file, then retry; the write goes through once your copy has it.
- **Collide lint, "not on origin yet"**: a teammate renamed something your code uses, but the rename has not landed. Keep the old name until it has; landing applies it for you.
- **git push**: with Collide, a plain `git push` rebases onto teammates' new commits, runs the tests and pushes in one step. Read the one-line result; "It is pushed; nothing else to do" means exactly that. Only a conflict or a failing test needs you.

## Saying what changed your plan

When something Collide told you changes what you do, say so to the user in one plain sentence, naming whose work it was: "bob just renamed `settled_total` to `paid_total`, so I'm updating the callers to match." Say nothing about notices that changed nothing.

## When the repository is not connected

If the session starts with "this repository is not connected", tell the user once that `/collide:login` connects it (a browser sign-in), and carry on with the task.
