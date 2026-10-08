---
name: status
description: Show what Collide is doing on this machine and whether this repository is connected. Use when the user asks whether Collide is on, working, connected, or what it saved.
---

# Collide status

Run this in a shell and summarize the result for the user in one or two sentences:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/collide" status
```

The program is `bin/collide` in this plugin's folder, two levels above this skill's folder; if the path above comes out empty, run it from there.
