# Spock — next plan (for review)

> Generated 2026-07-11 for Satinder. **No implementation until explicit go.**  
> Mirror of session plan; also stored in Claude memory `spock-next-plan`.  
> Hard rule: do **not** change Claude Code / VSCode sources — Spock only.

# Spock — next plan (review when back)

**Written:** 2026-07-11 (NZST)  
**Status:** Phases 0–2 + polish largely **SHIPPED** (2026-07-13). Deferred: Responses API dialect, server-tool zoo. Hygiene batch (betas strip, microcompact, mid-SSE, log-file, last-error toast) shipped.  
**Constraint:** Do **not** modify Claude Code / VSCode / VSCodium extension sources. Spock-only.  
**Related:** [[spock-claude-code-compat]] [[spock-advisor-research]] [[spock-rust-port]]

---

## Done this session (before you left)

1. Full advisor research saved → [[spock-advisor-research]]
2. Claude Code compat log created → [[spock-claude-code-compat]]  
   Baseline: **Spock 0.1.0 ↔ Claude Code CLI/ext 2.1.206** (VSCodium 1.128.0)
3. **README.md** updated with **Compatible Claude Code versions** table + troubleshooting row + link to this plan  
4. This plan written for your review  

---

## First principles (before adding backends)

Spock already has two backend families:

| `type` | What it is |
|---|---|
| `xai` | xAI chat + OAuth/API key special cases |
| `openai` | Generic OpenAI **Chat Completions** (`/v1/chat/completions`, `/v1/models`) |

**Most popular “new vendors” are not new families** — they are OpenAI-compatible gateways. Adding them as brand-new `BackendConfig` variants is often **premature complexity**. Prefer:

1. Documented **presets** in `config.example.toml` + Settings UX labels  
2. Small **header/auth quirks** on the existing openai path  
3. New code families only when the wire protocol is actually different (Responses API, Gemini native, Bedrock, etc.)

Elon algorithm: make requirements less dumb → delete → simplify → accelerate → automate last.

---

## Candidate backends (ranked)

### Tier A — “works today” as `type = "openai"` (preset + docs only)

Ship as commented examples + optional Settings “Add preset…” — **no new Rust family required** if Chat Completions + Bearer key is enough.

| Vendor | Typical `base_url` | Auth | Why popular with Claude Code users | Quirks to verify |
|---|---|---|---|---|
| **OpenRouter** | `https://openrouter.ai/api/v1` | Bearer API key | One key → many models (Claude, GPT, Grok, DeepSeek, …); huge ecosystem | Optional `HTTP-Referer` / `X-Title` headers; model ids like `anthropic/claude-…`, `openai/gpt-…` |
| **OpenAI (Chat Completions)** | `https://api.openai.com/v1` | Bearer API key | ChatGPT platform models for coding | Some models prefer **Responses API** (Tier B); tool calling shape usually OK |
| **DeepSeek** | `https://api.deepseek.com` | Bearer | Cheap strong code models; common Claude Code proxy target | Reasoning model fields may need stop/sanitization like Grok |
| **Groq** | `https://api.groq.com/openai/v1` | Bearer | Fast inference | Model list short; rate limits |
| **Together** | `https://api.together.xyz/v1` | Bearer | Open models hosted | — |
| **Fireworks** | `https://api.fireworks.ai/inference/v1` | Bearer | Open models hosted | — |
| **Mistral** | `https://api.mistral.ai/v1` | Bearer | EU / codestral | — |
| **Google Gemini (OpenAI compat)** | `https://generativelanguage.googleapis.com/v1beta/openai/` | Bearer / query key | Easy path without Gemini native schema | Confirm tool + stream parity |
| **Moonshot / Kimi** | `https://api.moonshot.ai/v1` (or CN endpoint) | Bearer | Long context; kimi-advisor ecosystem | Region endpoints |
| **NVIDIA NIM** | `https://integrate.api.nvidia.com/v1` | Bearer | Hosted open models | — |
| **LM Studio** | `http://127.0.0.1:1234/v1` | optional | Local GUI server | Loopback only |
| **vLLM / SGLang / llama-server** | user LAN/local `/v1` | optional | Self-host | Already covered by ollama-style openai type |
| **SiliconFlow / DashScope / Zhipu / MiniMax** | vendor `/v1` | Bearer | CN open-model ecosystems | Confirm HTTPS + model id format |

**Satinder-named priorities in chat:** OpenRouter, ChatGPT/OpenAI — both land here first (OpenAI Chat Completions). ChatGPT **Codex / Responses** path is Tier B.

### Tier B — needs real adapter work (new code)

| Vendor / API | Why not free | Scope |
|---|---|---|
| **OpenAI Responses API** (`/v1/responses`) | Different request/response + SSE than Chat Completions; some GPT/Codex paths want this | New family or openai dialect flag `api = "responses"` |
| **Azure OpenAI** | Resource URL + `api-key` header + deployment name in path | openai dialect + deployment routing |
| **Anthropic direct** | Already Messages-shaped; Spock currently *translates to* OpenAI — passthrough mode would skip translation | Optional `type = "anthropic"` passthrough for dual-stack |
| **Google Gemini native** | Non-OpenAI schema | Only if OpenAI-compat endpoint is insufficient |
| **Amazon Bedrock** | SigV4 + model ids | Large; low priority unless you need it |
| **ChatGPT subscription / Codex reverse-engineer proxies** | Fragile, ToS-grey, breaks often | **Skip** for Spock product — sovereignty + maintainability |

### Tier C — Spock features (not vendors)

| Feature | Why | Priority when you say go |
|---|---|---|
| **Advisor server-tool emulation** | Client already can send `advisor_20260301`; Spock strips it; no third-party proxy implements this cleanly for multi-backend | High product differentiation — see [[spock-advisor-research]] |
| **tool_choice hygiene** | Don’t send `tool_choice` when tools array empty after filter | Small, ship with any pass |
| **Compat matrix automation** | Document CLI/ext versions on release; optional smoke script | Medium |
| **Settings presets UI** | One-click OpenRouter / OpenAI / DeepSeek backend rows | UX after Tier A docs |

### Tier D — out of scope / reject

- Patching Claude Code or VSCode extension  
- Spoofing Anthropic login  
- Mass multi-tenant public gateway (Spock stays loopback)  
- Competitor drama / “better than X proxy” marketing  

---

## Recommended implementation order (when you say go)

### Phase 0 — Hygiene (small, ship first)

1. Drop `tool_choice` when no tools remain after filtering (fixes WebSearch-style 400s)  
2. Keep stripping non-`input_schema` tools **unless** advisor emulation handles them  
3. README already has compat table — update matrix on each release  

### Phase 1 — OpenAI-compat presets (OpenRouter + OpenAI + friends)

1. Extend `config.example.toml` with commented backends:

```toml
# [backends.openrouter]
# type = "openai"
# base_url = "https://openrouter.ai/api/v1"
# api_key = "sk-or-..."

# [backends.openai]
# type = "openai"
# base_url = "https://api.openai.com/v1"
# api_key = "sk-..."

# [backends.deepseek]
# type = "openai"
# base_url = "https://api.deepseek.com"
# api_key = "sk-..."
```

2. Optional openai extras (minimal):
   - `extra_headers` map (OpenRouter Referer/Title)  
   - env key fallbacks: `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`  
3. Settings: preset buttons or template dropdown — still `type = openai` under the hood  
4. Smoke tests: non-stream + stream + one tool round-trip per preset  
5. README: “Using OpenRouter / OpenAI / DeepSeek” short section  

**Delete temptation:** do **not** add `type = "openrouter"` unless headers/env need a real branch.

### Phase 2 — Advisor emulation (Spock-native)

Per [[spock-advisor-research]]:

1. Detect `tools[]` entry `type == advisor_20260301`  
2. Convert to function tool for upstream executor  
3. Nested completion to advisor model via existing router  
4. Stream `server_tool_use` + `advisor_tool_result` for Claude Code UI  
5. Config `[advisor] enabled / model / max_tokens`  
6. **Zero** Claude Code file changes  

### Phase 3 — Harder dialects (only if still needed)

1. OpenAI Responses API dialect  
2. Azure deployment routing  
3. Anthropic passthrough (optional)  

### Phase 4 — Polish

1. Compat smoke script + release checklist  
2. CHANGELOG rows for each verified Claude Code version  
3. Model-fetch quirks per vendor  

---

## Decision checklist for you (when back)

Answer these and implementation can start without more research:

1. **Phase 1 go?** Ship OpenRouter + OpenAI (+ DeepSeek?) as openai presets first?  
2. **Phase 2 go?** Advisor emulation in same milestone or later?  
3. **OpenAI shape:** Chat Completions only for v1, or Responses API required for your ChatGPT use case?  
4. **Any must-have beyond OpenRouter/OpenAI?** (Gemini, Groq, Azure, …)  
5. **Settings UI presets** in same PR as config examples, or config-only first?  

---

## Files that would change (estimate, Phase 1+0)

| File | Change |
|---|---|
| `config.example.toml` | Preset backends |
| `README.md` | OpenRouter/OpenAI usage section; keep compat table |
| `src/backends/openai_compat.rs` | optional extra_headers / env keys |
| `src/config.rs` | optional fields on Openai variant |
| `src/settings.rs` | presets in UI if wanted |
| `src/translate.rs` | tool_choice hygiene; later advisor |
| `src/server.rs` | advisor loop later |
| `CHANGELOG.md` | entry on ship |
| tests | preset routing + tool_choice |

**Not touched:** Claude Code, VSCodium extension, VS Code extension.

---

## Compat policy (ongoing)

- On every Spock release or confirmed session after Claude Code upgrade → append row to [[spock-claude-code-compat]] **and** README table  
- On break → **Broken** row with versions + symptom before fix  

Current baseline: Spock **0.2.0** + Claude Code **2.1.207** (also verified 2.1.206).

---

## Addendum 2026-07-12 — other cloud features (advisor-class)

Full map in Claude memory `spock-cloud-features` (also project memory).

| Feature | Cloud? | Spock like advisor? |
|---|---|---|
| Advisor | Messages server tool | **Yes (planned)** |
| WebSearch (`web_search_20250305`) | Nested Messages server tool | **Yes — high value** |
| CronCreate/List/Delete | Client local | No need (already works) |
| WebFetch | Client local HTTP | No need |
| ToolSearch | Client local catalog | No need |
| RemoteTrigger / bridge / org settings | claude.ai OAuth APIs | Out of scope |
| code_execution / hosted MCP connectors | API server tools (rare on CC coding path) | Skip for now |

**Implication:** Phase 2 should be framed as **server-tool emulation** (advisor + web_search), not advisor alone. Same intercept/emit machinery; two handlers.

### Deferred: other server result types Claude Code already parses

Client can render these if Spock (or Anthropic) returns them. **Do not implement until Satinder asks** — backlog only:

| Result block | Server tool / context | Priority |
|---|---|---|
| `server_tool_use` | Shared wrapper | Always (shared infra) |
| `advisor_tool_result` | advisor | P0 |
| `web_search_tool_result` | web_search | P0 |
| `web_fetch_tool_result` | API web_fetch (not client WebFetch) | P3 optional |
| `code_execution_tool_result` | hosted code_execution | P3 skip unless asked |
| `bash_code_execution_tool_result` | server bash sandbox | P3 skip (local Bash exists) |
| `text_editor_code_execution_tool_result` | server text editor | P3 skip (local Edit/Write exist) |
| `tool_search_tool_result` | API tool-search dialect | P3 low |
| `mcp_tool_use` / `mcp_tool_result` | Anthropic-hosted MCP connectors | skip (user MCP is local) |

When Satinder says “add code_execution / server web_fetch / …”, check `spock-cloud-features` and implement a handler that emits the matching block pair.

---

## Addendum 2026-07-12 — xAI quota / IDE error surfacing (authorized)

**Problem:** xAI subscription/credits exhausted. CLI Claude Code showed the error; VSCodium Claude Code did not (or looked like a silent/auth failure). Spock must map vendor quota/auth/rate-limit into Anthropic-shaped errors the IDE will display.

**Work (started same day):**

1. `classify_upstream_http` in `src/server.rs` — 401/402/403/429 + quota-ish body text → **502** with loud `Spock upstream …` message and `rate_limit_error` / `authentication_error` types (not bare Anthropic auth).
2. Unit tests for SuperGrok-style 403, 401, 429.
3. Still todo when authorized for a full ship: mid-SSE error path if stream starts then fails; optional Settings status toast for last upstream error; README troubleshooting row.

**Not done:** rebuild/restart Spock.app until Satinder restarts the menu-bar app (proxy is `dist/Spock.app/.../spock-proxy`).

---

## Addendum 2026-07-12 — llama-server Settings (lan-step)

**Correction:** Base URL was already `http://10.0.0.50:8081/v1` (documentation example; not `/models`). Earlier misread of curl example.

**Real issues:**

1. **Save & Apply required** before Fetch — proxy only knew `ollama`/`xai` until save. Admin `PUT /spock/v1/config` with `lan-step` works.
2. **Fetch models fallback bug** — on `/models` connect failure, code fell through to Ollama `/api/tags` and reported that secondary error (“No route to host” on `/api/tags`). Fixed: report `/models` failure first; only fall back to tags on empty body or 404.
3. Transient LAN “No route to host” can still happen; retry Fetch when host is up.
4. UI: Add backend no longer defaults every row to name `ollama`; help text notes root URL + Save before Fetch.

**Model id** from llama-server may be a full GGUF path — use as route target after Fetch.


---

## Addendum 2026-07-13 — hygiene batch (authorized + shipped)

Satinder approved “worth doing soon” + microcompact + Settings toast; deferred zoo stays parked.

| Item | Status |
|---|---|
| Strip Anthropic `betas` / only-keys for non-Anthropic | ✅ `prepare_for_openai_compat` |
| Client microcompact (`clear_tool_uses` / `clear_thinking`) | ✅ |
| Mid-SSE upstream error → SSE `error` event | ✅ |
| `--log-file` + App `~/Library/Logs/Spock/spock.log` | ✅ |
| Settings last-upstream-error banner | ✅ `/spock/v1/status` + toast |
| Responses API | Honest stub only (not implemented) |
| Server-tool zoo (code_execution, …) | **Parked** until asked |

