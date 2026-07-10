# Changelog

All notable changes to Spock are documented here.

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
