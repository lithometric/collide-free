#!/bin/sh
# Pin the collide-hook builds the server publishes now: writes hooks.lock with
# the version and each platform's SHA-256. The launcher downloads only these.
#   scripts/pin.sh [server]
server="${1:-https://mcp.collidemcp.com}"
cd "$(dirname "$0")/.." || exit 1
curl -fsSL -A curl "$server/artifacts/hooks" > hooks.index.json || exit 1
python3 - <<'PY' > hooks.lock.new || exit 1
import json
d = json.load(open("hooks.index.json"))
print("# collide-hook builds this plugin runs, pinned by scripts/pin.sh. Any download")
print("# whose SHA-256 differs is refused. Updates after install are signed by Collide.")
print("version", d["version"])
# the free version's one program, collide
for b in sorted(d.get("free_builds", []), key=lambda b: b["target"]):
    print(b["target"], b["sha256"])
PY
rm -f hooks.index.json
mv hooks.lock.new hooks.lock && cat hooks.lock
