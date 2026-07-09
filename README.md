# Grok Subscription Proxy

Use your Grok subscription as an **Anthropic-API-compatible** endpoint — no xAI API key.

Two files, zero dependencies (Python 3.9+ stdlib only):

| File | What it does |
|---|---|
| `grok_test.py` | One-time OAuth login (device-code flow) + quick prompt tester |
| `grok_proxy.py` | Local server on `http://localhost:8048` that speaks the Anthropic Messages API (and OpenAI chat completions) backed by xAI |

## Prerequisites

- **Python 3.9+** (`python3 --version`) — stdlib only; no `pip install`, no venv
- **Grok / X subscription** — browser must be logged into that account for the one-time OAuth approve
- Optional: `curl` (health checks + `claude-grok.sh`), Claude Code CLI or the VSCodium/VS Code extension

## How it works

xAI runs a standard OAuth 2.0 device-code flow (RFC 8628) on `auth.x.ai` with a
shared public client — the same one grok-cli and OpenClaw use (the consent
screen may say "Grok Build"). The granted token carries the `api:access` scope,
so it works as a plain Bearer token against `https://api.x.ai/v1`, billed to
your subscription. The proxy translates Anthropic Messages API requests to xAI
chat completions and back, including streaming and tool calls.

Both files must stay in the same directory — the proxy imports the OAuth logic
from `grok_test.py`.

## Quick start

```bash
# 1. Log in (once) — prints a URL + code, approve in your browser
python3 grok_test.py

# 2. Start the proxy
python3 grok_proxy.py
```

Tokens are cached in `~/.config/grok-test/auth.json` (chmod 600) and
auto-refreshed. `python3 grok_test.py --logout` forgets them. If that file
already exists from a previous login, skip step 1.

## Test it

Anthropic format:

```bash
curl http://localhost:8048/v1/messages \
  -H "content-type: application/json" \
  -d '{"model":"grok-4.5","max_tokens":256,"messages":[{"role":"user","content":"hello, what can you do?"}]}' | jq
```

OpenAI format (same port):

```bash
curl http://localhost:8048/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"grok-4.5","messages":[{"role":"user","content":"hello"}],"max_tokens":256}' | jq
```

List available models:

```bash
curl -s http://localhost:8048/v1/models | jq            # standard list
curl -s http://localhost:8048/v1/language-models | jq   # pricing + capabilities
```

## Use with Claude Code

### CLI

```bash
./claude-grok.sh          # starts the proxy if needed, then launches claude
```

`claude-grok.sh` passes any arguments straight to `claude` and sets the compact
window (see [Context window](#context-window-500k) below). Manual equivalent:

```bash
export ANTHROPIC_BASE_URL=http://localhost:8048
export ANTHROPIC_AUTH_TOKEN=dummy   # required by clients, ignored by the proxy
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000
claude
```

### VSCodium / VS Code extension

The IDE path does **not** auto-start the proxy — run it yourself first:

```bash
python3 grok_proxy.py
```

Then set Claude Code env vars in your editor settings
(`claudeCode.environmentVariables` in VSCodium/VS Code user settings):

```json
"claudeCode.environmentVariables": [
  { "name": "ANTHROPIC_BASE_URL", "value": "http://localhost:8048" },
  { "name": "ANTHROPIC_API_KEY", "value": "xai" },
  { "name": "ANTHROPIC_AUTH_TOKEN", "value": "xai" },
  { "name": "ANTHROPIC_MODEL", "value": "grok-4.5[1m]" },
  { "name": "ANTHROPIC_DEFAULT_FABLE_MODEL", "value": "grok-4.5[1m]" },
  { "name": "CLAUDE_CODE_AUTO_COMPACT_WINDOW", "value": "500000" },
  { "name": "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "value": "90" },
  { "name": "API_TIMEOUT_MS", "value": "3000000" }
]
```

Start a **new** Claude Code session after changing those values (env is read at
session start). Every request then runs on your Grok subscription.

### Context window (500k)

Grok-4.5's real context limit is about **500k tokens**. Claude Code decides when
to auto-compact from its **assumed** model window, not from xAI:

| Client assumption | What happens |
|---|---|
| Default / unknown model (~200k) | Compacts far too early |
| `…[1m]` only (1M window) | Compacts too late; can overflow Grok |
| `…[1m]` + `CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000` | Compacts near 500k (correct) |

Recommended:

- Model id: `grok-4.5[1m]` so Claude Code does not clamp to ~200k
- `CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000` so compact math uses 500k
- Optional: `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90` → fire at ~450k (headroom)

Keep auto-compact enabled (default). Do not set `DISABLE_AUTO_COMPACT`.
Status-line `% used` still reflects the full assumed window; only compaction
math uses `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.

## Endpoints

| Endpoint | Notes |
|---|---|
| `POST /v1/messages` | Anthropic Messages API — streaming + non-streaming, system prompts, images, tool use |
| `POST /v1/messages/count_tokens` | Rough estimate (chars / 4) |
| `POST /v1/chat/completions` | OpenAI-style passthrough |
| `GET /v1/models` | Model list (xAI passthrough) |
| `GET /v1/models/{id}` | Single model details |
| `GET /v1/language-models` | xAI extended list: pricing, modalities |
| `GET /health` | Proxy status |

## Model mapping

- Model names starting with `grok` pass through unchanged.
- Names containing `haiku` (Claude Code's cheap background calls) map to
  `GROK_SMALL_MODEL`.
- Everything else (`claude-sonnet-5`, etc.) maps to `GROK_MODEL`.

## Configuration (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8048` | Proxy listen port |
| `GROK_MODEL` | `grok-4.5` | Default / main model |
| `GROK_SMALL_MODEL` | = `GROK_MODEL` | Target for `*haiku*` requests |
| `XAI_TOKEN` | — | Skip OAuth entirely (e.g. use a real API key) |
| `XAI_API_BASE` | `https://api.x.ai/v1` | Upstream override (testing) |

Example — big model for main turns, cheap one for background chatter:

```bash
GROK_MODEL=grok-4.5 GROK_SMALL_MODEL=grok-4.3-mini python3 grok_proxy.py
```

(Pick real IDs from `curl -s localhost:8048/v1/models | jq -r '.data[].id'`.)

## Limitations

- Anthropic-only features are ignored gracefully: `thinking` blocks and
  `cache_control` are dropped, `count_tokens` is an estimate.
- The Batch, Files, and Admin APIs are not implemented (Claude Code doesn't
  use them).
- The proxy binds to `127.0.0.1` only — it is not meant to be exposed to a
  network; anyone who can reach it can spend your subscription.
- The login rides xAI's shared OAuth client; if xAI ever restricts that client
  to their own tools, the login step stops working.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `401 … token rejected` | `python3 grok_test.py --logout && python3 grok_test.py` |
| `Address already in use` | Another instance is running: `lsof -nP -iTCP:8048` — kill it or set `PORT` |
| Connection refused / Claude Code can't reach API | Start the proxy: `python3 grok_proxy.py` (IDE path does not auto-start it) |
| Compacts too early (~200k) | Set `CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000` and use `grok-4.5[1m]` |
| Hits Grok context limit / overflows | Ensure compact window is 500k, not bare `[1m]` alone; optional `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90` |
| Login URL never approves | Code expires in ~30 min; re-run, make sure the browser is logged in to your Grok/X account |
| Env changes ignored in IDE | Start a **new** Claude Code session after editing settings |
