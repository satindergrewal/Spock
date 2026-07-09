#!/usr/bin/env bash
# Launch Claude Code backed by the Grok proxy.
# Starts grok_proxy.py first if it isn't already running.
set -euo pipefail

PORT="${PORT:-8048}"
DIR="$(cd "$(dirname "$0")" && pwd)"

if ! curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1; then
  echo "starting grok_proxy.py on port $PORT (log: /tmp/grok_proxy.log)"
  (cd "$DIR" && nohup python3 grok_proxy.py >/tmp/grok_proxy.log 2>&1 &)
  for _ in $(seq 1 20); do
    curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && break
    sleep 0.3
  done
  curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 || {
    echo "proxy failed to start — check /tmp/grok_proxy.log" >&2
    exit 1
  }
fi

export ANTHROPIC_BASE_URL="http://localhost:$PORT"
export ANTHROPIC_AUTH_TOKEN=dummy
# grok-4.5 ≈ 500k context. Pair with model id grok-4.5[1m] so Claude Code
# doesn't clamp to ~200k; this env makes compact math use 500k instead of 1M.
export CLAUDE_CODE_AUTO_COMPACT_WINDOW="${CLAUDE_CODE_AUTO_COMPACT_WINDOW:-500000}"
export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE="${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:-90}"
exec "${CLAUDE_BIN:-claude}" "$@"
