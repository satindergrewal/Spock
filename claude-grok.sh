#!/usr/bin/env bash
# Launch Claude Code CLI backed by Spock (multi-backend proxy).
# Starts `spock serve` if nothing is listening on PORT.
#
# Defaults target the **fable** role → LAN backend (see ~/.config/spock/config.toml
# profiles.hybrid.fable). Override any model via env.
#
# Examples:
#   ./claude-grok.sh                          # fable → LAN GLM
#   ANTHROPIC_MODEL=claude-opus-4-8 ./claude-grok.sh   # opus role (xAI on hybrid)
#   GROK_MODEL_ID=grok-4.5[1m] ANTHROPIC_DEFAULT_OPUS_MODEL=$GROK_MODEL_ID ./claude-grok.sh
set -euo pipefail

PORT="${PORT:-8048}"
DIR="$(cd "$(dirname "$0")" && pwd)"

# Client model ids — Spock routes by role word in the id (fable/opus/sonnet/haiku).
FABLE_MODEL_ID="${FABLE_MODEL_ID:-claude-fable-5}"
GROK_MODEL_ID="${GROK_MODEL_ID:-grok-4.5[1m]}"
# Primary session model (what `claude --model` gets). Default: fable → LAN.
SESSION_MODEL="${ANTHROPIC_MODEL:-$FABLE_MODEL_ID}"

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
    "$DIR/dist/Spock.app/Contents/MacOS/spock-proxy" \
    "/Applications/Spock.app/Contents/MacOS/spock-proxy" \
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
    echo "spock binary not found — build with: cargo build --release" >&2
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

export ANTHROPIC_MODEL="$SESSION_MODEL"
# All roles default to fable so Auto/subagents stay on LAN (avoids xAI quota when
# testing local models). Override per-role with ANTHROPIC_DEFAULT_*_MODEL.
export ANTHROPIC_DEFAULT_FABLE_MODEL="${ANTHROPIC_DEFAULT_FABLE_MODEL:-$FABLE_MODEL_ID}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${ANTHROPIC_DEFAULT_OPUS_MODEL:-$FABLE_MODEL_ID}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${ANTHROPIC_DEFAULT_SONNET_MODEL:-$FABLE_MODEL_ID}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${ANTHROPIC_DEFAULT_HAIKU_MODEL:-$FABLE_MODEL_ID}"

export CLAUDE_CODE_AUTO_COMPACT_WINDOW="${CLAUDE_CODE_AUTO_COMPACT_WINDOW:-500000}"
export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE="${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE:-90}"

echo "Spock → $ANTHROPIC_BASE_URL  model=$ANTHROPIC_MODEL  fable=$ANTHROPIC_DEFAULT_FABLE_MODEL  compact=${CLAUDE_CODE_AUTO_COMPACT_WINDOW}@${CLAUDE_AUTOCOMPACT_PCT_OVERRIDE}%"
exec "${CLAUDE_BIN:-claude}" --model "$ANTHROPIC_MODEL" "$@"
