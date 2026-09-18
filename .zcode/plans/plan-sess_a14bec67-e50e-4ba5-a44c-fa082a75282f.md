# Plan — `/mcp` web-search MCP server inside Spock

Per your selections: **web_search only**, **Streamable HTTP + legacy SSE**, **docs + version bump**. No Claude Code/VSCode extension sources touched; no new Rust dependencies; same listener and port — exactly "running Spock also serves MCP".

## Intent & success bar

External MCP clients (ZCode with `type:"http"` URL, or legacy `type:"sse"`) auto-connect to Spock and receive `mcp__<server>__web_search`; a model tool call executes through the live `[web_search]` engine (Brave/Searper/SearXNG/DDG) and returns readable hits. Claude Code `/v1/messages` emulation and Grok-Build `/v1/responses` shim stay orthogonal, untouched. Loopback-only, no auth; `[web_search]` config remains the single gate.

## Locked design decisions

- Endpoints, one guarded route arm (`p == "/mcp" || p.starts_with("/mcp/")`) slotted before the Anthropic default 404, whose dispatcher owns JSON-RPC-shaped responses end-to-end: `/mcp` = Streamable HTTP POST (JSON body when `Accept` includes `application/json`; `event: message` SSE frames when SSE-only; 406 Not Acceptable when neither) with `GET /mcp` → 405 Method Not Allowed + `Allow: POST` (spec-tolerated; upgrade to GET-stream parked). `/mcp/sse` = legacy 2024-11-05 GET stream opening with `event: endpoint` carrying `/mcp/sse/messages?sessionId=…`, keepalives every 12s; `/mcp/sse/messages` = legacy POST, routed onto the registered GET stream (HTTP 202 no-body), unknown sid → loud JSON-RPC 404. Legacy sid registry = module-static hub keyed to `Arc<Mutex<TcpStream>>` clones — no AppState churn.
- JSON-RPC 2.0: `initialize` (echo supported `2025-06-18`/`2025-03-26`, else latest; `capabilities.tools.listChanged=false`; `serverInfo` name "spock", version `crate::config::VERSION`); `ping`; `tools/list` one `web_search` entry (reuse wording "Search the public web…", `query` required, `readOnlyHint`); `tools/call` → live config `WebSearchConfig::from_section` → `run_web_search` → text content (title/url/snippet lines) + `isError:true` loud refusals for empty query, disabled (mirroring `/v1/responses` wording), provider failure; `structuredContent`/`outputSchema` parked. Notifications (no `id`) → 202/204 no-body. Errors: -32700 parse (HTTP 400), -32600/-32601/-32602/-32603 HTTP 200 envelopes. Streamable stateless — no `Mcp-Session-Id` assigned; legacy owns its own sid.
- Hygiene reuse, anchors: `run_web_search` (`server_tools.rs:543`) + section fields (`config.rs:114-128`) are 1:1; collapse the two duplicated inline builders (`server.rs:634-644`, `791-801`) with `WebSearchConfig::from_section` next to `from_toml_table` (`server_tools.rs:70`); extend `reason_phrase` (`server.rs:227-236`) with 202/204/405/406; add `write_status_only` (bodyless header, `Allow:` for 405) and mark `write_json`/`write_sse_headers`/`emit_sse` `pub(crate)` so `mcp.rs` reuses them, not duplicate writers. Advisor twin ~`server.rs:783-790` stays parked (unrelated churn).

## Files

| File | Change |
|---|---|
| `src/mcp.rs` | NEW protocol module: streamable + legacy dispatcher, tool sheets, call wrapper, unit tests (network-free) |
| `src/server.rs` | guarded arm, banner line 28 `| /mcp`, pub(crate) writer tags, status/helper hygiene |
| `src/server_tools.rs` | `from_section` shared conversion (~10 LOC) |
| `src/main.rs` | `mod mcp;` (line ~6, alphabetical) + `mcp:` status print (~341) |
| `README.md` | HTTP-API subsection: `/mcp` rows + ZCode JSON snippet (reuse published `127.0.0.1:8048` idiom) |
| `CHANGELOG.md` | Unreleased Added row |
| `config.example.toml` | `[web_search]` comment (~264) naming MCP endpoints |
| `Cargo.toml` | `0.3.0 → 0.4.0` (your lane; tag/push await your word) |
| tests | `mcp.rs` offline suite + one `server.rs` route e2e copying `vision_policy_e2e` template (`free_port`, `read_one_request`, spawn `serve`) |

## Steps

0. Hygiene first (tiny): `from_section` + builder collapse; reason_phrase statuses + `write_status_only` + pub(crate) tags.
1. `src/mcp.rs` pure protocol: initialize/list/call/ping/notification, Accept negotiation matrix, dispatcher subrouting, legacy hub, offline unit tests incl wrapper seam fed synthetic results Value.
2. Wiring in `server.rs`/`main.rs`: guarded arm before 154 (canonical 404 for `/mcp/foo` lives in dispatcher), banner + status line, `mod` decl.
3. Tests + route e2e (initialize+list through real `serve()` port, network-free).
4. Docs/version rows per your lane.
5. Validation: `cargo test` full suite → live JSON-RPC curl probes (initialize, list, call, notification, 406/400, legacy GET endpoint event + POST onto stream) → **ZCode gate**: Settings → MCP config row (`type:"http"`, URL `/mcp`; desktop-managed list is the right surface — your CLI `config.json` is sparse), connect status, live session prompt whose search needs hits → canary CC emulation + `/v1/responses` shim + chat completions unaffected.
6. Deploy: rebuild + `packaging/macos/build-app.sh`; swap `dist/Spock.app` and restart **on your word** — acknowledged doctrine: redeploy kills live sessions. Commit/push/tag await your word; worktree already carries recent edits M `src/server.rs`, `translate.rs`, `vision.rs`.
7. Parked (no premature complexity, only on evidence): advisor tool; GET `/mcp` stream upgrade; outputSchema/structuredContent; `max_results` param; resources/prompts + listChanged push; LAN bearer gate; Settings toggle + health `mcp` key; display-text/Advisor-from_section unification.

## Risks & gates

- ZCode's client-side transport quirks are unverified: a live connect is the acceptance gate; iterate `Accept`, GET-stream, `headers`, `timeoutMs` only on observed failure — no silent workarounds.
- Spec strictness is real: version echo, Accept, 406/405, 204/202, RPC codes — implement per 2025-06-18 and legacy 2024-11-05; unit matrix + live probes.
- Search is network-only: unit tests stay on the synthetic-results seam; real execution verified live.
- Restart kills sessions; CLI/banner signals help.
- Publishing: reuse README's already-published loopback idiom; never paste memory-file values into docs.
- MCP adds a third door; it does not replace emulation or the Responses shim.

Scope estimate: ~250–450 LOC module + ~40 wiring edits, offline-only test additions, no deps. Small.

## Checklist echo

Tool scope: web_search only. Transport: streamable + legacy SSE. Ship: docs + `0.4.0`.

---