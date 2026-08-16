#!/usr/bin/env bash
# Usage: ./install.sh <seat-slug> <bridge-abs-path>
# Example: ./install.sh agencyos-cc /Users/jasonburks/.../tools/buzz-bridge
set -euo pipefail
SLUG="${1:?seat slug required}"
BRIDGE_DIR="${2:-$(cd "$(dirname "$0")" && pwd)}"
CONFIG_DIR="$HOME/.claude/mcp"
mkdir -p "$CONFIG_DIR"
cat > "$CONFIG_DIR/buzz-bridge-${SLUG}.json" <<EOF
{
  "mcpServers": {
    "buzz-bridge": {
      "command": "bun",
      "args": ["${BRIDGE_DIR}/buzz-bridge.ts"],
      "env": {
        "BUZZ_CLERK_DIR": "$HOME/.buzz-clerk"
      }
    }
  }
}
EOF
echo "Installed MCP config: $CONFIG_DIR/buzz-bridge-${SLUG}.json"
echo "Launch Claude Code with: claude --mcp-config $CONFIG_DIR/buzz-bridge-${SLUG}.json"
