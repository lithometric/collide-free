---
name: login
description: Sign this machine in to Collide, linking it to the user's Collide account. Use when the user asks to sign in, connect or link Collide, or when a session says the repository is not connected. Optional: Collide's free version needs no account.
---

# Sign this machine in to Collide

The free version already works without an account: agents on this machine see each other's work and can message each other. Signing in links the machine to the user's Collide account, so that when their workspace is on Team the machine switches to it by itself and brings its history along.

Run this command in a shell and wait for it to finish. It opens a browser for the user to sign in and waits up to five minutes:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/collide" login
```

The program is `bin/collide` in this plugin's folder, two levels above this skill's folder; if the path above comes out empty, run it from there.

When it finishes, tell the user in one or two sentences what it printed: which account and workspace the machine is linked to, and whether it stays local (Free) or now works with their team. Never print or repeat a token.
