---
description: Sign this machine in to Collide (optional; the free version needs no account)
allowed-tools: Bash
---

Sign this machine in to Collide. The free version already works without it: agents on this machine see each other and message each other with no account. Signing in links the machine to the user's Collide account, so that when their workspace is on Team the machine switches to it by itself and brings its history along.

Run this command with the Bash tool and wait for it to finish (it waits up to five minutes while the user signs in in the browser it opens):

```
"${CLAUDE_PLUGIN_ROOT}/bin/collide" login
```

When it finishes, tell the user in one or two sentences what it printed: which account and workspace the machine is linked to, and whether it stays local (Free) or now works with their team. Do not print or repeat any token.
