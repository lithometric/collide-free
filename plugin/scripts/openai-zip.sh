#!/bin/sh
# The plugin as OpenAI's plugin portal takes it: no hooks (the portal refuses
# plugins that carry them), the Codex manifest with the listing fields, the
# skills, the launcher the login and status skills run, and the icon.
# Collide's MCP server is added in the portal's MCPs tab, and its setup tool
# installs Collide's hooks on the user's machine from there.
#   scripts/openai-zip.sh [out.zip]
set -e
cd "$(dirname "$0")/.."
version=$(python3 -c "import json;print(json.load(open('.claude-plugin/plugin.json'))['version'])")
out="${1:-$HOME/Desktop/collide-plugin-openai-$version.zip}"
work=$(mktemp -d)
dest="$work/collide"
mkdir -p "$dest"
git archive HEAD skills bin assets .codex-plugin LICENSE README.md hooks.lock | tar -x -C "$dest"
mkdir -p "$dest/.claude-plugin"
python3 - "$dest" <<'PY'
import json, sys
dest = sys.argv[1]
manifest = json.load(open(".claude-plugin/plugin.json"))
manifest.pop("hooks", None)
json.dump(manifest, open(f"{dest}/.claude-plugin/plugin.json", "w"), indent=2)
PY
rm -f "$out"
(cd "$work" && zip -qr "$out" collide)
rm -rf "$work"
echo "$out"
