//! In-app Settings panel (macOS tray build) — WebView UI, no external editor.

use crate::config::{BackendConfig, Config, ProfileConfig, VERSION};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// JSON document the Settings UI loads / saves (friendlier than raw TOML for forms).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsDoc {
    pub version: String,
    pub config_path: String,
    pub server: ServerDoc,
    pub backends: Vec<BackendDoc>,
    pub profiles: Vec<ProfileDoc>,
    #[serde(default)]
    pub advisor: AdvisorDoc,
    #[serde(default)]
    pub web_search: WebSearchDoc,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AdvisorDoc {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_advisor_max_tokens")]
    pub max_tokens: u32,
}

fn default_advisor_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchDoc {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ws_provider_doc")]
    pub provider: String,
    /// SearXNG (or custom) base URL, e.g. http://127.0.0.1:8888
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default = "default_ws_max_doc")]
    pub max_results: u32,
}

fn default_ws_provider_doc() -> String {
    "duckduckgo".into()
}
fn default_ws_max_doc() -> u32 {
    5
}

impl Default for WebSearchDoc {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_ws_provider_doc(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            max_results: default_ws_max_doc(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerDoc {
    pub bind: String,
    pub port: u16,
    pub profile: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendDoc {
    pub name: String,
    /// "oauth" | "api_key" | "anthropic"
    #[serde(rename = "type")]
    pub kind: String,
    /// OAuth provider id when kind = oauth (xai, kimi, …)
    #[serde(default)]
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Optional env var name for API key when field empty (api_key).
    #[serde(default)]
    pub api_key_env: String,
    /// Optional extra headers as "Key: Value" lines (OpenRouter etc.).
    #[serde(default)]
    pub extra_headers_text: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProfileDoc {
    pub name: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub haiku: String,
    #[serde(default)]
    pub sonnet: String,
    #[serde(default)]
    pub opus: String,
    #[serde(default)]
    pub fable: String,
}

pub fn config_to_doc(cfg: &Config) -> SettingsDoc {
    let backends = cfg
        .backends
        .iter()
        .map(|(name, b)| match b {
            BackendConfig::Oauth {
                provider,
                base_url,
                api_key,
            } => BackendDoc {
                name: name.clone(),
                kind: "oauth".into(),
                provider: provider.clone(),
                base_url: base_url.clone(),
                api_key: api_key.clone().unwrap_or_default(),
                api_key_env: String::new(),
                extra_headers_text: String::new(),
            },
            BackendConfig::ApiKey {
                base_url,
                api_key,
                extra_headers,
                api_key_env,
                ..
            } => BackendDoc {
                name: name.clone(),
                kind: "api_key".into(),
                provider: String::new(),
                base_url: base_url.clone(),
                api_key: api_key.clone().unwrap_or_default(),
                api_key_env: api_key_env.clone().unwrap_or_default(),
                extra_headers_text: headers_to_text(extra_headers),
            },
            BackendConfig::Anthropic {
                base_url,
                api_key,
                api_key_env,
            } => BackendDoc {
                name: name.clone(),
                kind: "anthropic".into(),
                provider: String::new(),
                base_url: base_url.clone(),
                api_key: api_key.clone().unwrap_or_default(),
                api_key_env: api_key_env.clone().unwrap_or_default(),
                extra_headers_text: String::new(),
            },
        })
        .collect();

    let profiles = cfg
        .profiles
        .iter()
        .map(|(name, p)| ProfileDoc {
            name: name.clone(),
            default: p.default.clone().unwrap_or_default(),
            haiku: p.haiku.clone().unwrap_or_default(),
            sonnet: p.sonnet.clone().unwrap_or_default(),
            opus: p.opus.clone().unwrap_or_default(),
            fable: p.fable.clone().unwrap_or_default(),
        })
        .collect();

    SettingsDoc {
        version: VERSION.to_string(),
        config_path: crate::config::config_path().display().to_string(),
        server: ServerDoc {
            bind: cfg.server.bind.clone(),
            port: cfg.server.port,
            profile: cfg.server.profile.clone(),
        },
        backends,
        profiles,
        advisor: AdvisorDoc {
            enabled: cfg.advisor.enabled,
            model: cfg.advisor.model.clone().unwrap_or_default(),
            max_tokens: if cfg.advisor.max_tokens == 0 {
                4096
            } else {
                cfg.advisor.max_tokens
            },
        },
        web_search: WebSearchDoc {
            enabled: cfg.web_search.enabled,
            provider: if cfg.web_search.provider.trim().is_empty() {
                "duckduckgo".into()
            } else {
                cfg.web_search.provider.clone()
            },
            base_url: cfg.web_search.base_url.clone().unwrap_or_default(),
            api_key: cfg.web_search.api_key.clone().unwrap_or_default(),
            api_key_env: cfg.web_search.api_key_env.clone().unwrap_or_default(),
            max_results: if cfg.web_search.max_results == 0 {
                5
            } else {
                cfg.web_search.max_results
            },
        },
    }
}

pub fn doc_to_config(doc: &SettingsDoc) -> crate::error::Result<Config> {
    let mut backends = BTreeMap::new();
    for b in &doc.backends {
        let name = b.name.trim();
        if name.is_empty() {
            continue;
        }
        let kind = b.kind.trim().to_ascii_lowercase();
        let opt_key = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        let be = match kind.as_str() {
            "oauth" | "xai" => {
                let provider = if kind == "xai" {
                    "xai".to_string()
                } else {
                    let p = b.provider.trim();
                    if p.is_empty() {
                        return Err(crate::error::Error::Msg(format!(
                            "backend '{name}' (oauth) needs provider"
                        )));
                    }
                    p.to_string()
                };
                let def = crate::oauth::get_provider(&provider)
                    .map(|p| p.default_base_url.to_string())
                    .unwrap_or_default();
                let base_url = if b.base_url.trim().is_empty() {
                    def
                } else {
                    b.base_url.trim().to_string()
                };
                BackendConfig::Oauth {
                    provider,
                    base_url,
                    api_key: opt_key(&b.api_key),
                }
            }
            "api_key" | "openai" => {
                if b.base_url.trim().is_empty() {
                    return Err(crate::error::Error::Msg(format!(
                        "backend '{name}' (api_key) needs base_url"
                    )));
                }
                BackendConfig::ApiKey {
                    base_url: b.base_url.trim().to_string(),
                    api_key: opt_key(&b.api_key),
                    extra_headers: text_to_headers(&b.extra_headers_text),
                    api_key_env: opt_key(&b.api_key_env),
                    use_responses_api: false,
                    azure_deployment: None,
                    azure_api_version: None,
                }
            }
            "anthropic" => BackendConfig::Anthropic {
                base_url: if b.base_url.trim().is_empty() {
                    "https://api.anthropic.com".into()
                } else {
                    b.base_url.trim().to_string()
                },
                api_key: opt_key(&b.api_key),
                api_key_env: opt_key(&b.api_key_env),
            },
            other => {
                return Err(crate::error::Error::Msg(format!(
                    "unknown backend type '{other}' for '{name}'"
                )));
            }
        };
        backends.insert(name.to_string(), be);
    }
    if backends.is_empty() {
        return Err(crate::error::Error::Msg(
            "at least one backend is required".into(),
        ));
    }

    let mut profiles = BTreeMap::new();
    for p in &doc.profiles {
        let name = p.name.trim();
        if name.is_empty() {
            continue;
        }
        let opt = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        profiles.insert(
            name.to_string(),
            ProfileConfig {
                default: opt(&p.default),
                haiku: opt(&p.haiku),
                sonnet: opt(&p.sonnet),
                opus: opt(&p.opus),
                fable: opt(&p.fable),
                exact: BTreeMap::new(),
            },
        );
    }
    if profiles.is_empty() {
        return Err(crate::error::Error::Msg(
            "at least one profile is required".into(),
        ));
    }

    let profile = doc.server.profile.trim().to_string();
    if !profiles.contains_key(&profile) {
        return Err(crate::error::Error::Msg(format!(
            "active profile '{profile}' is not in the profiles list"
        )));
    }

    let bind = doc.server.bind.trim().to_string();
    let bind = if bind.is_empty() {
        "127.0.0.1".into()
    } else {
        bind
    };

    let opt_str = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };

    Ok(Config {
        server: crate::config::ServerConfig {
            bind,
            port: if doc.server.port == 0 {
                8048
            } else {
                doc.server.port
            },
            profile,
        },
        backends,
        profiles,
        advisor: crate::config::AdvisorSection {
            enabled: doc.advisor.enabled,
            model: opt_str(&doc.advisor.model),
            max_tokens: if doc.advisor.max_tokens == 0 {
                4096
            } else {
                doc.advisor.max_tokens
            },
        },
        web_search: crate::config::WebSearchSection {
            enabled: doc.web_search.enabled,
            provider: {
                let p = doc.web_search.provider.trim();
                if p.is_empty() {
                    "duckduckgo".into()
                } else {
                    p.to_string()
                }
            },
            base_url: opt_str(&doc.web_search.base_url),
            api_key: opt_str(&doc.web_search.api_key),
            api_key_env: opt_str(&doc.web_search.api_key_env),
            max_results: if doc.web_search.max_results == 0 {
                5
            } else {
                doc.web_search.max_results
            },
        },
    })
}

#[allow(dead_code)]
pub fn settings_html(initial: &SettingsDoc) -> String {
    let data = serde_json::to_string(initial).unwrap_or_else(|_| "{}".into());
    // Escape </script> in JSON for HTML embedding
    let data = data.replace("</", "<\\/");
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Spock Settings</title>
<style>
  :root {{
    color-scheme: light dark;
    --bg: #0f1115;
    --panel: #1a1d24;
    --border: #2a2f3a;
    --text: #e8eaed;
    --muted: #9aa0a6;
    --accent: #6ea8fe;
    --accent2: #3dd68c;
    --danger: #f07178;
    --input: #12151b;
    --radius: 10px;
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif;
  }}
  @media (prefers-color-scheme: light) {{
    :root {{
      --bg: #f4f5f7;
      --panel: #ffffff;
      --border: #d8dde6;
      --text: #1a1d24;
      --muted: #5f6368;
      --input: #f8f9fb;
    }}
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font-size: 13px;
    line-height: 1.4;
  }}
  header {{
    position: sticky; top: 0; z-index: 10;
    display: flex; align-items: center; gap: 12px;
    padding: 12px 16px;
    background: color-mix(in srgb, var(--panel) 92%, transparent);
    backdrop-filter: blur(10px);
    border-bottom: 1px solid var(--border);
  }}
  header h1 {{
    font-size: 15px; font-weight: 600; margin: 0;
    letter-spacing: -0.02em;
  }}
  header .path {{ color: var(--muted); font-size: 11px; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
  header .actions {{ display: flex; gap: 8px; }}
  main {{ padding: 16px; display: grid; gap: 14px; max-width: 920px; margin: 0 auto; }}
  section {{
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 14px;
  }}
  section h2 {{
    margin: 0 0 10px;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
    font-weight: 600;
  }}
  .row {{ display: flex; flex-wrap: wrap; gap: 10px; align-items: end; margin-bottom: 8px; }}
  label {{ display: flex; flex-direction: column; gap: 4px; min-width: 120px; flex: 1; }}
  label span {{ color: var(--muted); font-size: 11px; }}
  input, select, textarea {{
    background: var(--input);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 10px;
    font: inherit;
    outline: none;
  }}
  input:focus, select:focus, textarea:focus {{ border-color: var(--accent); }}
  button {{
    border: 1px solid var(--border);
    background: var(--input);
    color: var(--text);
    border-radius: 8px;
    padding: 8px 12px;
    font: inherit;
    cursor: pointer;
  }}
  button.primary {{
    background: var(--accent);
    border-color: transparent;
    color: #0b1020;
    font-weight: 600;
  }}
  button.ghost {{ background: transparent; }}
  button.danger {{ color: var(--danger); }}
  button:hover {{ filter: brightness(1.08); }}
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ text-align: left; padding: 6px 4px; border-bottom: 1px solid var(--border); vertical-align: top; }}
  th {{ color: var(--muted); font-size: 11px; font-weight: 600; }}
  td input, td select {{ width: 100%; min-width: 90px; }}
  .hint {{ color: var(--muted); font-size: 11px; margin-top: 6px; }}
  #status {{
    min-height: 18px;
    font-size: 12px;
    color: var(--muted);
  }}
  #status.ok {{ color: var(--accent2); }}
  #status.err {{ color: var(--danger); }}
  .chip {{
    display: inline-block;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--muted);
    font-size: 11px;
  }}
</style>
</head>
<body>
<header>
  <h1>Spock Settings</h1>
  <div class="path" id="cfgPath"></div>
  <div class="actions">
    <button class="ghost" id="btnReload" title="Reload from disk">Reload</button>
    <button class="primary" id="btnSave">Save &amp; Apply</button>
  </div>
</header>
<main>
  <div id="status"></div>

  <section>
    <h2>Server</h2>
    <div class="row">
      <label><span>Listen address</span><input id="bind" value="127.0.0.1"/></label>
      <label style="max-width:120px"><span>Port</span><input id="port" type="number" min="1" max="65535"/></label>
      <label><span>Active profile</span><select id="activeProfile"></select></label>
    </div>
    <p class="hint">Non-loopback binds are forced back to 127.0.0.1 for safety. Port changes need an app restart.</p>
  </section>

  <section>
    <h2>Backends</h2>
    <table>
      <thead>
        <tr>
          <th style="width:18%">Name</th>
          <th style="width:14%">Type</th>
          <th>Base URL</th>
          <th style="width:18%">API key</th>
          <th style="width:40px"></th>
        </tr>
      </thead>
      <tbody id="backendsBody"></tbody>
    </table>
    <div class="row" style="margin-top:10px">
      <button id="btnAddBackend">+ Backend</button>
      <span class="chip">OAuth = subscription login (provider xai/kimi) · API Key = OpenAI-compatible · Anthropic = passthrough</span>
    </div>
  </section>

  <section>
    <h2>Profiles &amp; routes</h2>
    <p class="hint">Route format: <code>backend:model</code> — e.g. <code>xai:grok-4.5</code> or <code>ollama:qwen2.5:14b</code>. Empty role inherits default / passthrough rules.</p>
    <table>
      <thead>
        <tr>
          <th style="width:14%">Name</th>
          <th>default</th>
          <th>haiku</th>
          <th>sonnet</th>
          <th>opus</th>
          <th>fable</th>
          <th style="width:40px"></th>
        </tr>
      </thead>
      <tbody id="profilesBody"></tbody>
    </table>
    <div class="row" style="margin-top:10px">
      <button id="btnAddProfile">+ Profile</button>
    </div>
  </section>
</main>
<script>
const INITIAL = {data};

function ipc(msg) {{
  const payload = typeof msg === 'string' ? msg : JSON.stringify(msg);
  try {{
    if (window.ipc && window.ipc.postMessage) {{
      window.ipc.postMessage(payload);
      return;
    }}
  }} catch (e) {{}}
  try {{
    if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ipc) {{
      window.webkit.messageHandlers.ipc.postMessage(payload);
      return;
    }}
  }} catch (e) {{}}
  setStatus('IPC unavailable — rebuild Spock.app', true);
}}

function setStatus(text, err) {{
  const el = document.getElementById('status');
  el.textContent = text || '';
  el.className = err ? 'err' : (text ? 'ok' : '');
}}

function backendRow(b) {{
  const tr = document.createElement('tr');
  tr.innerHTML = `
    <td><input class="b-name" value="${{esc(b.name || '')}}"/></td>
    <td>
      <select class="b-type">
        <option value="oauth" ${{b.type === 'oauth' || b.type === 'xai' ? 'selected' : ''}}>OAuth</option>
        <option value="api_key" ${{b.type === 'api_key' || b.type === 'openai' ? 'selected' : ''}}>API Key</option>
        <option value="anthropic" ${{b.type === 'anthropic' ? 'selected' : ''}}>Anthropic</option>
      </select>
    </td>
    <td><input class="b-url" value="${{esc(b.base_url || '')}}" placeholder="http://127.0.0.1:11434/v1"/></td>
    <td><input class="b-key" value="${{esc(b.api_key || '')}}" placeholder="xAI API key or Ollama key"/></td>
    <td><button class="danger b-del" title="Remove">×</button></td>`;
  tr.querySelector('.b-del').onclick = () => {{ tr.remove(); refreshProfileSelect(); }};
  tr.querySelector('.b-name').oninput = refreshProfileSelect;
  return tr;
}}

function profileRow(p) {{
  const tr = document.createElement('tr');
  tr.innerHTML = `
    <td><input class="p-name" value="${{esc(p.name || '')}}"/></td>
    <td><input class="p-default" value="${{esc(p.default || '')}}" placeholder="xai:grok-4.5"/></td>
    <td><input class="p-haiku" value="${{esc(p.haiku || '')}}"/></td>
    <td><input class="p-sonnet" value="${{esc(p.sonnet || '')}}"/></td>
    <td><input class="p-opus" value="${{esc(p.opus || '')}}"/></td>
    <td><input class="p-fable" value="${{esc(p.fable || '')}}"/></td>
    <td><button class="danger p-del" title="Remove">×</button></td>`;
  tr.querySelector('.p-del').onclick = () => {{ tr.remove(); refreshProfileSelect(); }};
  tr.querySelector('.p-name').oninput = refreshProfileSelect;
  return tr;
}}

function esc(s) {{
  return String(s).replace(/&/g,'&amp;').replace(/"/g,'&quot;').replace(/</g,'&lt;');
}}

function refreshProfileSelect() {{
  const sel = document.getElementById('activeProfile');
  const current = sel.value;
  const names = [...document.querySelectorAll('#profilesBody .p-name')].map(i => i.value.trim()).filter(Boolean);
  sel.innerHTML = names.map(n => `<option value="${{esc(n)}}">${{esc(n)}}</option>`).join('');
  if (names.includes(current)) sel.value = current;
  else if (names.length) sel.value = names[0];
}}

function loadDoc(doc) {{
  document.getElementById('cfgPath').textContent = doc.config_path || '';
  document.getElementById('bind').value = (doc.server && doc.server.bind) || '127.0.0.1';
  document.getElementById('port').value = (doc.server && doc.server.port) || 8048;
  const bb = document.getElementById('backendsBody');
  bb.innerHTML = '';
  (doc.backends || []).forEach(b => bb.appendChild(backendRow(b)));
  const pb = document.getElementById('profilesBody');
  pb.innerHTML = '';
  (doc.profiles || []).forEach(p => pb.appendChild(profileRow(p)));
  refreshProfileSelect();
  if (doc.server && doc.server.profile) {{
    document.getElementById('activeProfile').value = doc.server.profile;
  }}
}}

function collectDoc() {{
  const backends = [...document.querySelectorAll('#backendsBody tr')].map(tr => ({{
    name: tr.querySelector('.b-name').value.trim(),
    type: tr.querySelector('.b-type').value,
    base_url: tr.querySelector('.b-url').value.trim(),
    api_key: tr.querySelector('.b-key').value,
  }}));
  const profiles = [...document.querySelectorAll('#profilesBody tr')].map(tr => ({{
    name: tr.querySelector('.p-name').value.trim(),
    default: tr.querySelector('.p-default').value.trim(),
    haiku: tr.querySelector('.p-haiku').value.trim(),
    sonnet: tr.querySelector('.p-sonnet').value.trim(),
    opus: tr.querySelector('.p-opus').value.trim(),
    fable: tr.querySelector('.p-fable').value.trim(),
  }}));
  return {{
    version: INITIAL.version,
    config_path: INITIAL.config_path,
    server: {{
      bind: document.getElementById('bind').value.trim(),
      port: parseInt(document.getElementById('port').value, 10) || 8048,
      profile: document.getElementById('activeProfile').value,
    }},
    backends,
    profiles,
      advisor: Default::default(),
      web_search: Default::default(),
  }};
}}

document.getElementById('btnAddBackend').onclick = () => {{
  document.getElementById('backendsBody').appendChild(backendRow({{
    name: 'ollama', type: 'api_key', provider: '', base_url: 'http://127.0.0.1:11434/v1', api_key: '', api_key_env: '', extra_headers_text: ''
  }}));
}};
document.getElementById('btnAddProfile').onclick = () => {{
  document.getElementById('profilesBody').appendChild(profileRow({{
    name: 'custom', default: 'xai:grok-4.5', haiku: '', sonnet: '', opus: '', fable: ''
  }}));
  refreshProfileSelect();
}};
document.getElementById('btnSave').onclick = () => {{
  setStatus('Saving…');
  ipc({{ cmd: 'save', doc: collectDoc() }});
}};
document.getElementById('btnReload').onclick = () => {{
  setStatus('Reloading…');
  ipc({{ cmd: 'reload' }});
}};

// Host → page
window.__spockSetDoc = (doc) => {{ loadDoc(doc); setStatus('Loaded', false); }};
window.__spockStatus = (text, err) => setStatus(text, !!err);

loadDoc(INITIAL);
setStatus('Ready — edit routes, then Save & Apply');
</script>
</body>
</html>
"##
    )
}

/// Handle IPC payload from the Settings WebView. Returns optional JSON to push back via eval.
#[allow(dead_code)]
pub fn handle_ipc(state: &AppState, body: &str) -> (String, bool) {
    // returns (status message, is_error)
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (format!("bad IPC json: {e}"), true),
    };
    let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    match cmd {
        "save" => {
            let doc_val = v.get("doc").cloned().unwrap_or(Value::Null);
            let doc: SettingsDoc = match serde_json::from_value(doc_val) {
                Ok(d) => d,
                Err(e) => return (format!("invalid settings: {e}"), true),
            };
            match doc_to_config(&doc) {
                Ok(mut cfg) => {
                    // Preserve per-profile exact maps from current config (not in form UI yet)
                    if let Ok(old) = state.snapshot_config() {
                        for (name, prof) in cfg.profiles.iter_mut() {
                            if let Some(old_p) = old.profiles.get(name) {
                                if !old_p.exact.is_empty() {
                                    prof.exact = old_p.exact.clone();
                                }
                            }
                        }
                    }
                    match state.apply_and_save(cfg) {
                        Ok(()) => (
                            format!("Saved & applied · profile {}", doc.server.profile),
                            false,
                        ),
                        Err(e) => (format!("save failed: {e}"), true),
                    }
                }
                Err(e) => (format!("{e}"), true),
            }
        }
        "reload" => match state.reload_from_disk() {
            Ok(()) => ("Reloaded from disk".into(), false),
            Err(e) => (format!("reload failed: {e}"), true),
        },
        "get" => ("ok".into(), false),
        other => (format!("unknown cmd '{other}'"), true),
    }
}

#[allow(dead_code)]
pub fn current_doc_json(state: &AppState) -> String {
    match state.snapshot_config() {
        Ok(cfg) => serde_json::to_string(&config_to_doc(&cfg)).unwrap_or_else(|_| "{}".into()),
        Err(_) => "{}".into(),
    }
}

// keep json! available for future API responses
#[allow(dead_code)]
fn _unused() {
    let _ = json!({});
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    #[test]
    fn headers_roundtrip() {
        let mut m = BTreeMap::new();
        m.insert("HTTP-Referer".into(), "https://x.test".into());
        m.insert("X-Title".into(), "Spock".into());
        let text = headers_to_text(&m);
        let back = text_to_headers(&text);
        assert_eq!(back.get("HTTP-Referer").map(String::as_str), Some("https://x.test"));
        assert_eq!(back.get("X-Title").map(String::as_str), Some("Spock"));
    }

    #[test]
    fn text_to_headers_skips_comments() {
        let h = text_to_headers("# c\nFoo: bar\n\n");
        assert_eq!(h.get("Foo").map(String::as_str), Some("bar"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn roundtrip_default_config() {
        let cfg = default_config();
        let doc = config_to_doc(&cfg);
        let back = doc_to_config(&doc).expect("doc_to_config");
        assert_eq!(back.server.profile, cfg.server.profile);
        assert_eq!(back.server.port, cfg.server.port);
        assert!(back.backends.contains_key("xai"));
        assert!(back.profiles.contains_key("hybrid"));
        assert_eq!(
            back.profiles["hybrid"].haiku.as_deref(),
            Some("ollama:qwen2.5:14b")
        );
    }
}


fn headers_to_text(h: &BTreeMap<String, String>) -> String {
    h.iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_to_headers(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if !k.is_empty() {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}
