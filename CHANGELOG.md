# Changelog

All notable changes to Spock are documented here.

## [Unreleased]

### Added
- **Generalized OAuth providers** — registry-driven `spock login|logout <provider>` / `spock providers` (xai, kimi); status + menus follow the table
- Backend types: `oauth` / `api_key` / `anthropic` (Settings labels: **OAuth** / **API Key** / **Anthropic**)
- **Kimi Code** OAuth (`provider = "kimi"`, `https://api.kimi.com/coding/v1`, KimiCLI UA + X-Msh headers)
- Token files: `~/.config/spock/oauth-<provider>.json` (imports legacy grok-test / kimi-cli paths once; logout clears them)

### Changed
- Breaking: bare `spock login` removed — use `spock login xai` (or `kimi`)
- Proxy never opens a browser mid-request; refresh is single-flight per provider
- Long streaming generations: idle read/write timeouts (1h) instead of total 600s request caps; stream clients skip server-tools buffer path; backend map lock dropped before upstream I/O
- macOS app: Local Network + Bonjour keys so LAN backends work from Spock.app
- Product framing: multi-backend Anthropic Messages proxy (not Grok-only)

### Fixed
- **Login…** from the menu bar runs as a direct child process (no Terminal/AppleScript stall)
- Menu + CLI surface already-logged-in OAuth state clearly
- Slow LAN (llama-server) streams no longer die at ~10 minutes while the backend still generates

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
