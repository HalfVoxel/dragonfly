#!/usr/bin/env bash
# Thin launcher: the actual MCP channel server lives in the dragonfly binary
# (`dragonfly watch-mcp`). A wrapper is needed because plugin .mcp.json
# commands resolve against ${CLAUDE_PLUGIN_ROOT}, not the user's PATH setup.
set -euo pipefail
if command -v dragonfly >/dev/null 2>&1; then
  exec dragonfly watch-mcp "$@"
fi
if [ -x "$HOME/.cargo/bin/dragonfly" ]; then
  exec "$HOME/.cargo/bin/dragonfly" watch-mcp "$@"
fi
echo "dragonfly-watcher: dragonfly binary not found on PATH or in ~/.cargo/bin" >&2
exit 1
