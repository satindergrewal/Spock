# Changelog

All notable changes to Spock are documented here.

## [Unreleased]

### Added
- **Vision policy for text-only backends** (`text_only = true` on oauth/api_key backends + `[vision]` section) — text-only upstreams (vLLM DSV4-Flash etc.) hard-400 on image content, and clients re-send the full transcript, so one screenshot poisons every later request in the session. Spock now rewrites image content before the request leaves: `mode = "strip"` (default) replaces images with an omission note; `mode = "describe"` captions each screenshot via an OpenAI-compatible VL sidecar (llama-server + mmproj) and inlines the caption as text. Any sidecar failure degrades to strip — a request never dies here. Covers `/v1/messages` (incl. anthropic passthrough and KV sessions) and the verbatim `/v1/chat/completions` ingress. Captions cached in memory only (sha256 of image + prompt); no per-request image limit — one failed sidecar call strips the rest of that request (bounded stall), a healthy sidecar captions every image. Retroactively un-sticks poisoned sessions: the next request goes out clean. glm-5.3 name-matcher flatten unchanged as a safety net.
- **`POST /v1/responses` search shim** — grok-build `web_search` posts OpenAI Responses (`{base}/responses` + hosted `web_search` tool). Search-only: run `[web_search]` (Brave/Serper/SearXNG/DDG) and return a completed Responses object with `output_text` + `url_citation`. No tool / search disabled / empty query → 400. Not a general Responses proxy (no chat/completions fallback).
- **macOS Settings Catalog effort column** — auto / 1 / 0 next to context; Save & Apply writes `supports_reasoning_effort` so Grok Build `/model` Tab shows effort on non-heuristic rows (Qwen, local GGUF)
- **llama-server KV sessions** (`kv_sessions = true` on an api_key backend) — Claude Code traffic parks a named master via native `/completion`, children `POST /fork` with `parent_session_id`, leave via `POST /close_session`. `/v1/chat/completions` is never the session path. Missing routes or unknown session **error the request** (no silent cold prefill). Inherit proof: HTTP `cache_n` / `prompt_n`. Headers: `x-spock-session`, `x-spock-parent-session`, `x-spock-close-session`. `cache_control` on the shared prefix names the master.
- **Generalized OAuth providers** — registry-driven `spock login|logout <provider>` / `spock providers` (xai, kimi, qwen); status + menus follow the table
- Backend types: `oauth` / `api_key` / `anthropic` (Settings labels: **OAuth** / **API Key** / **Anthropic**)
- **Kimi Code** OAuth (`provider = "kimi"`, `https://api.kimi.com/coding/v1`, KimiCLI UA + X-Msh headers)
- **Qwen OAuth (optional)** — `provider = "qwen"` chat.qwen.ai device flow + PKCE S256; uses token `resource_url` when present (qwen-code path)
- **Qwen Cloud (qwencloud.com)** documented as `type = "api_key"` on `dashscope-intl…/compatible-mode/v1` + `DASHSCOPE_API_KEY` (Token Plan / Max Preview)
- Token files: `~/.config/spock/oauth-<provider>.json` (imports legacy grok-test / kimi-cli / qwen-code paths once; logout clears them)

### Changed
- Breaking: bare `spock login` removed — use `spock login xai` (or `kimi` / `qwen`)
- Proxy never opens a browser mid-request; refresh is single-flight per provider
- Long streaming generations: idle read/write timeouts (1h) instead of total 600s request caps; stream clients skip server-tools buffer path; backend map lock dropped before upstream I/O
- macOS app: Local Network + Bonjour keys so LAN backends work from Spock.app
- Product framing: multi-backend Anthropic Messages proxy (not Grok-only)

### Fixed
- **Mid-conversation `role:"system"` reminders 400 on template-strict upstreams**: Claude Code delivers some system-reminders (e.g. the Agent-tool types list) as `role:"system"` messages *inside* `messages`. Qwen3.5+ jinja chat templates (SGLang / vLLM / llama.cpp) hard-400 with `System message must be at the beginning.` — LAN Qwen looked dead (GPU idle) while every request died at template validation. Generic (OpenAI-compat) backends and the llama-server KV path now fold non-leading system messages into user turns wrapped in `<system-reminder>` tags (the client's own idiom); xAI / Kimi keep passthrough.
- **z.ai GLM 5.3 text-only content**: Claude Code screenshots become OpenAI `image_url` parts; GLM-5.3 400s `messages.content.type is invalid, allowed values: ['text']`. Flatten images to a text note for `glm-5.3*` only — vision backends unchanged.
- **Accepted sockets inherit `O_NONBLOCK` on Darwin**: listen socket is nonblocking so the accept loop can poll shutdown. macOS/`accept()` copies that flag onto the client fd; `read_exact` then returns `WouldBlock` (os error 35) as soon as the kernel buffer is empty. Claude Code reports `ECONNRESET`; Grok Build reports `reqwest error stream: error sending request`. Force blocking + idle timeouts on every accepted socket. `/v1/chat/completions` now logs `route` and emits a stream error event instead of dying silent.
- **Catalog `/v1/models`**: non-empty catalog is served from local entries only. Live backend `/models` probes no longer block Grok's ~5s catalog fetch (empty `/model` picker, `unknown` footer).
- **Advisor protocol**: run the nested review; never leak `advisor` as client `tool_use` (`No such tool available: advisor`). Flatten leftover `server_tool_use` / `advisor_tool_result` to text on the OpenAI-compat path (xAI 400). VSCodium 2.1.226 webview cannot render those block types (`Unsupported content type: server_tool_use` as a chat line) — emit labeled **text** instead. Strip `<|eos|>`. Log advisor start/end/duration.
- **WebSearch on stream**: Claude Code nested `web_search_20250305` calls now run Spock server-tool emulation (was skipped when `stream:true`); emit real `server_tool_use` + `web_search_tool_result` blocks; SSE keepalives during long rounds
- **Login…** from the menu bar runs as a direct child process (no Terminal/AppleScript stall)
- Menu + CLI surface already-logged-in OAuth state clearly
- Slow LAN (llama-server) streams no longer die at ~10 minutes while the backend still generates
- Mid-SSE llama.cpp tool-call diff aborts (`Invalid diff: now finding less tool calls`) labeled as **upstream llama-server**, not Spock
- Streamed OpenAI tool_calls merge by `index` (Qwen/Token Plan): empty `id` arg chunks no longer open extra tool_use blocks — fixes Claude Code Bash getting empty/broken commands on Fable→Qwen
- Route `backend:model` client ids to that backend when it exists (stop falling through to profile default / xAI)
- Docs: Qwen Cloud **Coding Plan** (`coding-intl…`, `sk-sp-…`) vs **Token Plan** (`token-plan.ap-southeast-1…`, separate key) — `qwen3.8-max-preview` is Token Plan only

## [0.2.0] - 2026-07-13

### Added
- OpenAI-compat presets in `config.example.toml` (OpenRouter, OpenAI, DeepSeek, Groq, …)
- `extra_headers` + `api_key_env` on openai backends
- Server-tool emulation: `[advisor]` + `[web_search]` (DuckDuckGo / Brave / Serper / SearXNG)
- Anthropic passthrough backend type (`type = "anthropic"`)
- Azure OpenAI deployment fields (`azure_deployment`, `azure_api_version`)
- Responses API flag stub (`use_responses_api` — Chat Completions remain default; honest error if enabled)
- Client-side microcompact: applies `context_management` (`clear_tool_uses_20250919`, `clear_thinking_20251015`) before OpenAI-compat / xAI
- Strip Anthropic-only fields (`betas`, `context_management`, …) for non-Anthropic backends
- Mid-SSE upstream error → Anthropic `error` event + Settings toast
- `spock serve --log-file PATH` (Unix); Spock.app logs to `~/Library/Logs/Spock/spock.log`
- Last upstream error on `/spock/v1/status` + Settings dismissible banner
- `scripts/smoke-compat.sh` for post-upgrade checks
- Cargo `--locked` in CI, release, and macOS app build

### Fixed
- Auto Mode classifier: do not send `reasoning_effort: "none"` (Grok 400)
- Drop `tool_choice` when all tools stripped (server tools / WebSearch nested call)
- Synthetic SSE: emit `input_json_delta` for tool_use (client tools with advisor on)
- Webview-friendly advisor/web_search results as plain text (no unsupported block types)
- CLI `spock status` reports `config_api_key` + advisor/web_search + last_err
- Settings auth source (API key vs OAuth); clearer llama-server model-list errors
- Loud 502 mapping for upstream 401/402/403/429 (IDE-friendly)

### Notes
- Claude Code verified: **2.1.206** and **2.1.207** (see README compat table)
- Enable advisor/web_search explicitly in config; defaults off
- Deferred server-tool zoo (code_execution, hosted bash, …) stays parked

## [0.1.0] - 2026-07-11

### Added
- Rust multi-backend Anthropic-compatible proxy (`spock serve`)
- Route Claude Code roles (haiku / sonnet / opus / fable / default) to different vendors
- xAI auth: OAuth device flow **or** console API key (`api_key` / `XAI_TOKEN`)
- OpenAI-compatible backends (Ollama, llama-server, LAN)
- Native **Spock.app** (SwiftUI menu bar): Settings, Chat, profile switch, status colors
- Model discovery (Fetch models) for Ollama/xAI in Settings
- CLI: `login`, `logout`, `chat`, `status`, `reload`, `serve`, `app`
- Config: `~/.config/spock/config.toml` (hot reload)
- Preserves Python-era OAuth path `~/.config/grok-test/auth.json`
- GitHub Actions CI + tag-driven multi-platform releases

### Notes
- First official binary release (Python implementation removed)
- macOS App asset is Apple Silicon (`darwin-arm64`); CLI covers all listed platforms
