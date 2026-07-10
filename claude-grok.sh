#!/usr/bin/env bash
# Launch Claude Code backed by Spock (Grok / multi-backend proxy).
# Starts `spock serve` if nothing is listening on PORT.
set -euo pipefail

PORT="${PORT:-8048}"
DIR="$(cd "$(dirname "$0")" && pwd)"
GROK_MODEL_ID="${GROK_MODEL_ID:-grok-4.5[1m]}"

find_spock() {
  if [[ -n "${SPOCK_BIN:-}" && -x "${SPOCK_BIN}" ]]; then
    echo "$SPOCK_BIN"
    return
  fi
  if command -v spock >/dev/null 2>&1; then
    command -v spock
    return
  fi
  for c in \
    "$DIR/spock" \
    "$DIR/target/release/spock" \
    "$DIR/target/debug/spock" \
    "/Applications/Spock.app/Contents/MacOS/spock"; do
    if [[ -x "$c" ]]; then
      echo "$c"
      return
    fi
  done
  return 1
}

if ! curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
  SPOCK_BIN="$(find_spock)" || {
    echo "spock binary not found — build with: cargo build --release --features tray" >&2
    echo "or install from GitHub Releases / open Spock.app" >&2
    exit 1
  }
  echo "starting spock serve on port $PORT (log: /tmp/spock.log)"
  (nohup "$SPOCK_BIN" serve --port "$PORT" >/tmp/spock.log 2>&1 &)
  for _ in $(seq 1 30); do
    curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
    sleep 0.3
  done
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 || {
    echo "proxy failed to start — check /tmp/spock.log" >&2
    exit 1
  }
fi

export ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT"
export ANTHROPIC_AUTH_TOKEN="${ANTHROPIC_AUTH_TOKEN:-dummy}"
export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-dummy}"

export ANTHROPIC_MODEL="${ANTHROPIC_MODEL:-$GROK_MODEL_ID}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${ANTHROPIC_DEFAULT_OPUS_MODEL:-$GROK_MODEL_ID}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${ANTHROPIC_DEFAULT_SONNET_MODEL:-$GROK_MODEL_ID}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${ANTHROPIC_DEFAULT_HAIKU_MODEL:-${GROK_SMALL_MODEL:-$GROK_MODEL_ID}}"
export ANTHROPIC_DEFAULT_FABLE_MODEL="${ANTHROPIC_DEFAULT_FABLE_MODEL:-$GROK_MODEL_ID}"

export CLAUDE_CODE_AUTO_COMPACT_WINDOW="${CLAUDE_CODE_AUTO_COMPACT_WINDOW:-500000}"
export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE="${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:-90}"

echo "Spock → $ANTHROPIC_BASE_URL  model=$ANTHROPIC_MODEL  compact=${CLAUDE_CODE_AUTO_COMPACT_WINDOW}@${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE}%"
exec "${CLAUDE_BIN:-claude}" --model "$ANTHROPIC_MODEL" "$@"
