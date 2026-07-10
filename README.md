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
./claude-grok.sh          # starts the proxy if needed, then launches claude on Grok
```

`claude-grok.sh` starts the proxy if needed, forces the model to
`grok-4.5[1m]` (override with `GROK_MODEL_ID` or `ANTHROPIC_MODEL`), and sets
the compact window (see [Context window](#context-window-500k) below). Extra
args are passed through to `claude`. Manual equivalent:

```bash
export ANTHROPIC_BASE_URL=http://localhost:8048
export ANTHROPIC_AUTH_TOKEN=dummy   # required by clients, ignored by the proxy
export ANTHROPIC_MODEL=grok-4.5[1m]
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=500000
export CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=90
claude --model "$ANTHROPIC_MODEL"
```

On launch it prints a one-liner so you can confirm routing:

```text
Spock → http://localhost:8048  model=grok-4.5[1m]  compact=500000@90%
```

If the UI still shows `fable`/`opus`/`sonnet`, the model was not overridden —
use `./claude-grok.sh` (or pass `--model grok-4.5[1m]`) rather than bare
`claude` after exporting only the base URL.

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

The proxy **strips** Claude's `[1m]` / `[1]` / `[500k]` suffix before calling
xAI (those tags are client-side context hints, not real model ids). Without
that strip you get `400 Model not found: grok-4.5[1m]`.

Keep auto-compact enabled (default). Do not set `DISABLE_AUTO_COMPACT`.
Status-line `% used` still reflects the full assumed window; only compaction
math uses `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.

### Auto Mode (permission classifier)

Claude Code Auto Mode classifies tool calls (often with `stop_sequences` for
XML tags like `</block>`). Through Spock:

1. `GET /v1/models` / `GET /v1/models/{id}` advertise Claude aliases so the
   client does not treat the classifier model as missing.
2. `POST /v1/messages` maps any non-`grok*` / non-`*haiku*` id to `GROK_MODEL`.
3. **Critical:** xAI reasoning models (`grok-4.5`, `grok-4.3`, …) **reject**
   OpenAI `stop` / `presence_penalty` / `frequency_penalty`. Spock drops those
   before upstream. Without that drop you get:
   `400 Model grok-4.5 does not support parameter stop` → Auto Mode fails
   closed with "`… is temporarily unavailable, so auto mode cannot determine
   the safety of Bash`".

Ollama / llama.cpp accept `stop`, which is why the same Auto Mode path worked
there and not here until this strip was added.

If Auto Mode still fails after upgrading, restart the proxy and start a
**new** Claude Code session.

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

- Claude Code context suffixes (`[1m]`, `[1]`, `[500k]`, …) are stripped before
  the upstream call. Client-facing `model` in responses keeps the original id.
- Bare names starting with `grok` pass through to xAI.
- Names containing `haiku` (Claude Code's cheap background / Auto Mode helper
  calls) map to `GROK_SMALL_MODEL`.
- Everything else (`claude-opus-4-8`, `claude-sonnet-5`, `fable`, …) maps to
  `GROK_MODEL`.
- `GET /v1/models` merges the live xAI list with synthetic Claude/Grok aliases
  so Auto Mode and the model picker do not 404.

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

### Effort (`xhigh` / ultracode)

Claude Code session effort (`--effort`, `effortLevel`, ultracode) is sent as
Anthropic `output_config.effort`. Spock maps that to xAI `reasoning_effort`:

| Claude `output_config.effort` | xAI `reasoning_effort` |
|---|---|
| `low` / `medium` / `high` / `xhigh` | same |
| `max` | `xhigh` (xAI has no `max`) |

Legacy `thinking.budget_tokens` still maps when `output_config.effort` is
absent: &lt;5k → low, &lt;15k → medium, &lt;40k → high, else xhigh. Adaptive
thinking with no budget defaults to high. `thinking.type=disabled` forces
`none` (keeps Auto Mode classifiers clean).

**Local vs cloud multi-agent:** Claude Code **Workflow / Agent / ultracode**
orchestration runs in the client and only needs `/v1/messages` — it works
through Spock. Cloud-only Anthropic products (**Managed Agents** sandbox API,
**ultrareview** cloud fleet, Task Budgets on Anthropic infra) do **not** — they
hit Anthropic-hosted control planes Spock does not implement. Local Workflow
is the sovereign equivalent for almost all coding work.

## Limitations

- Grok `reasoning_content` maps to Anthropic `thinking` blocks only when the
  client enables `thinking` (stream + non-stream). Auto Mode classifier calls
  do not enable it, so they get plain text. Inbound thinking is re-sent as
  `reasoning_content`; `output_config.effort` and `thinking.budget_tokens` map
  to xAI `reasoning_effort` (see [Effort](#effort-xhigh--ultracode)).
  `stop_sequences` are dropped for reasoning models (xAI rejects `stop`).
  `cache_control` is dropped; `count_tokens` is an estimate.
- The Batch, Files, Admin, and Managed Agents APIs are not implemented
  (Claude Code local Workflow does not need them).
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
