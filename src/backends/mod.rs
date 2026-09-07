pub mod openai_compat;
pub mod responses;

use crate::config::{BackendConfig, Config};
use crate::error::{Error, Result};
use crate::oauth::registry::{get_provider, request_headers, CompletionsQuirk, DeviceCtx};
use crate::oauth::{access_token, AccessMode, OauthStore};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

pub enum UpstreamBody {
    Json(Value),
    Stream(Box<dyn Read + Send>),
}

/// Cheap to clone — holds config only, no live sockets. Callers should clone out of
/// `AppState.backends` and **drop the RwLock** before long upstream I/O so Save/reload
/// cannot be blocked by hour-long LAN generations.
#[derive(Clone)]
pub struct BackendHandle {
    /// Backend map key (profile route target). Kept for diagnostics / future admin.
    #[allow(dead_code)]
    pub name: String,
    pub quirk: CompletionsQuirk,
    pub config: BackendConfig,
}

impl BackendHandle {
    pub fn from_config(name: &str, cfg: &BackendConfig) -> Self {
        Self {
            name: name.to_string(),
            quirk: cfg.quirk(),
            config: cfg.clone(),
        }
    }

    pub fn family_name(&self) -> &'static str {
        match &self.config {
            BackendConfig::Oauth { .. } => "oauth",
            BackendConfig::ApiKey { .. } => "api_key",
            BackendConfig::Anthropic { .. } => "anthropic",
            BackendConfig::Responses { .. } => "responses",
        }
    }

    pub fn is_anthropic(&self) -> bool {
        matches!(self.config, BackendConfig::Anthropic { .. })
    }

    /// The Codex/OpenAI wire: the `responses` backend kind, or an `oauth`
    /// backend whose provider is `openai` (its endpoint speaks Responses API
    /// only — never chat completions).
    pub fn is_codex(&self) -> bool {
        matches!(&self.config, BackendConfig::Responses { .. })
            || matches!(&self.config, BackendConfig::Oauth { provider, .. } if provider == "openai")
    }

    /// Bearer for the Codex wire: configured api_key, or the OAuth provider token.
    fn codex_bearer(&self, oauth: &OauthStore) -> Result<String> {
        if let Some(k) = self.config.api_key() {
            return Ok(k);
        }
        let provider = self.codex_provider();
        crate::oauth::access_token(oauth, provider, None, crate::oauth::AccessMode::Proxy)
    }

    fn codex_provider(&self) -> &str {
        match &self.config {
            BackendConfig::Responses { provider, .. } => provider.as_str(),
            BackendConfig::Oauth { provider, .. } => provider.as_str(),
            _ => "openai",
        }
    }

    /// Probe the Codex `/models` route (requires `client_version`). Falls back
    /// to a static shortlist so `/v1/models` never silently 400s.
    fn codex_list_models(&self, oauth: &OauthStore) -> Result<Vec<String>> {
        let base_url = self.config.base_url();
        let key = self.codex_bearer(oauth)?;
        let path = self.config.responses_path();
        let dir = match path.rsplit_once('/') {
            Some((d, _)) if !d.is_empty() => d,
            _ => "",
        };
        let models_path = if dir.is_empty() {
            "/models?client_version=0.131.0".to_string()
        } else {
            format!("/{dir}/models?client_version=0.131.0")
        };
        let v = openai_compat::get_json(
            base_url,
            Some(&key),
            &models_path,
            self.config.extra_headers(),
            None,
            None,
            None,
        )?;
        let mut out = Vec::new();
        if let Some(models) = v.get("models").and_then(|m| m.as_array()) {
            for m in models {
                if let Some(slug) = m.get("slug").and_then(|s| s.as_str()) {
                    out.push(slug.to_string());
                }
            }
        }
        if out.is_empty() {
            out = vec!["gpt-5.5".into(), "gpt-5.4".into(), "gpt-5.2".into()];
        }
        Ok(out)
    }

    fn oauth_bearer(
        &self,
        store: &OauthStore,
    ) -> Result<(String, BTreeMap<String, String>, String)> {
        let (provider_id, api_key) = match &self.config {
            BackendConfig::Oauth {
                provider, api_key, ..
            } => (provider.as_str(), api_key.as_deref()),
            _ => return Err(Error::Msg("not an oauth backend".into())),
        };
        let p = get_provider(provider_id)
            .ok_or_else(|| Error::Auth(format!("unknown provider '{provider_id}'")))?;
        let token = access_token(store, provider_id, api_key, AccessMode::Proxy)?;
        let ctx = DeviceCtx::current();
        let headers = request_headers(p, &ctx);
        Ok((token, headers, p.user_agent.to_string()))
    }

    /// Prefer token `resource_url` (Qwen regional endpoint) over static config base.
    fn oauth_base_url(&self) -> String {
        let config_base = self.config.base_url().to_string();
        let provider_id = match &self.config {
            BackendConfig::Oauth { provider, .. } => provider.as_str(),
            _ => return config_base,
        };
        if let Some(t) = crate::oauth::load_tokens(provider_id) {
            if let Some(u) = crate::oauth::resource_base_url(&t) {
                return u;
            }
        }
        config_base
    }

    pub fn chat(&self, body: &Value, stream: bool, oauth: &OauthStore) -> Result<UpstreamBody> {
        if self.config.kv_sessions() {
            return Err(Error::Msg(
                "kv_sessions: BackendHandle::chat is disabled. Use native /completion \
                 + /fork + /close_session. Falling through to /chat/completions is the bug."
                    .into(),
            ));
        }
        // The OpenAI/Codex provider speaks ONLY the Responses API (stream-only),
        // never chat completions. Route it to the responses wire regardless of
        // whether it was configured as an `oauth` backend or the `responses` kind.
        if self.is_codex() {
            let provider = match &self.config {
                BackendConfig::Responses { provider, .. } => provider.as_str(),
                BackendConfig::Oauth { provider, .. } => provider.as_str(),
                _ => "openai",
            };
            let api_key = self.config.api_key();
            let (token, acct) = match api_key {
                Some(k) => (k, None),
                None => {
                    let t = crate::oauth::access_token(
                        oauth,
                        provider,
                        None,
                        crate::oauth::AccessMode::Proxy,
                    )?;
                    let acct = crate::oauth::load_tokens(provider).and_then(|toks| {
                        toks.extra
                            .get("account_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    });
                    (t, acct)
                }
            };
            let path = if self.config.responses_path().is_empty() {
                "codex/responses"
            } else {
                self.config.responses_path()
            };
            return responses::chat(
                self.config.base_url(),
                path,
                Some(token.as_str()),
                acct.as_deref(),
                body,
                stream,
                self.config.extra_headers(),
            );
        }
        match &self.config {
            BackendConfig::Oauth { .. } => {
                let (token, headers, ua) = self.oauth_bearer(oauth)?;
                let base_url = self.oauth_base_url();
                openai_compat::chat(
                    &base_url,
                    Some(&token),
                    body,
                    stream,
                    &headers,
                    None,
                    None,
                    false,
                    Some(&ua),
                )
            }
            BackendConfig::ApiKey { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::chat(
                    base_url,
                    key.as_deref(),
                    body,
                    stream,
                    self.config.extra_headers(),
                    self.config.azure_deployment(),
                    self.config.azure_api_version(),
                    self.config.use_responses_api(),
                    None,
                )
            }
            BackendConfig::Anthropic { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::anthropic_messages(base_url, key.as_deref(), body, stream)
            }
            BackendConfig::Responses { .. } => {
                // Codex / ChatGPT subscription Responses wire. An explicit
                // api_key means an API-key gateway (no account header);
                // otherwise the `provider` OAuth token is the bearer.
                let api_key = self.config.api_key();
                let provider = match &self.config {
                    BackendConfig::Responses { provider, .. } => provider.as_str(),
                    _ => "openai",
                };
                let (token, acct) = match api_key {
                    Some(k) => (k, None),
                    None => {
                        let t = crate::oauth::access_token(
                            oauth,
                            provider,
                            None,
                            crate::oauth::AccessMode::Proxy,
                        )?;
                        let acct = crate::oauth::load_tokens(provider)
                            .and_then(|toks| {
                                toks.extra
                                    .get("account_id")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            });
                        (t, acct)
                    }
                };
                responses::chat(
                    self.config.base_url(),
                    self.config.responses_path(),
                    Some(token.as_str()),
                    acct.as_deref(),
                    body,
                    stream,
                    self.config.extra_headers(),
                )
            }
        }
    }

    /// POST to a native llama-server path (`/fork`, `/completion`, `/close_session`,
    /// `/apply-template`). Origin is `base_url` with a trailing `/v1` stripped.
    /// Never used as a chat/completions fallback.
    pub fn native_post(
        &self,
        path: &str,
        body: &Value,
        stream: bool,
        oauth: &OauthStore,
    ) -> Result<UpstreamBody> {
        match &self.config {
            BackendConfig::ApiKey { base_url, .. } => {
                let key = self.config.api_key();
                let origin = crate::kv_sessions::llama_origin(base_url);
                openai_compat::post_json(
                    &origin,
                    key.as_deref(),
                    path,
                    body,
                    stream,
                    self.config.extra_headers(),
                    None,
                )
            }
            BackendConfig::Oauth { .. } => {
                let (token, headers, ua) = self.oauth_bearer(oauth)?;
                let origin = crate::kv_sessions::llama_origin(&self.oauth_base_url());
                openai_compat::post_json(
                    &origin,
                    Some(&token),
                    path,
                    body,
                    stream,
                    &headers,
                    Some(&ua),
                )
            }
            BackendConfig::Anthropic { .. } => Err(Error::Msg(
                "kv_sessions: Anthropic backends have no llama-server native routes".into(),
            )),
            BackendConfig::Responses { .. } => Err(Error::Msg(
                "kv_sessions: Responses backends have no llama-server native routes".into(),
            )),
        }
    }

    pub fn get_json(&self, path: &str, oauth: &OauthStore) -> Result<Value> {
        match &self.config {
            BackendConfig::Oauth { .. } => {
                let (token, headers, ua) = self.oauth_bearer(oauth)?;
                let base_url = self.oauth_base_url();
                openai_compat::get_json(
                    &base_url,
                    Some(&token),
                    path,
                    &headers,
                    None,
                    None,
                    Some(&ua),
                )
            }
            BackendConfig::ApiKey { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::get_json(
                    base_url,
                    key.as_deref(),
                    path,
                    self.config.extra_headers(),
                    self.config.azure_deployment(),
                    self.config.azure_api_version(),
                    None,
                )
            }
            BackendConfig::Anthropic { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::get_json(
                    base_url,
                    key.as_deref(),
                    path,
                    self.config.extra_headers(),
                    None,
                    None,
                    None,
                )
            }
            BackendConfig::Responses { base_url, .. } => {
                let key = self.responses_bearer(oauth)?;
                openai_compat::get_json(
                    base_url,
                    Some(&key),
                    path,
                    self.config.extra_headers(),
                    None,
                    None,
                    None,
                )
            }
        }
    }

    /// Bearer for the Responses backend: the configured api_key, or the OAuth
    /// provider token. Used for status/model probes, not the chat path.
    fn responses_bearer(&self, oauth: &OauthStore) -> Result<String> {
        if let Some(k) = self.config.api_key() {
            return Ok(k);
        }
        let provider = match &self.config {
            BackendConfig::Responses { provider, .. } => provider.as_str(),
            _ => "openai",
        };
        crate::oauth::access_token(oauth, provider, None, crate::oauth::AccessMode::Proxy)
    }

    pub fn list_models(&self, oauth: &OauthStore) -> Result<Vec<String>> {
        if self.is_codex() {
            return self.codex_list_models(oauth);
        }
        match &self.config {
            BackendConfig::Oauth { .. } => {
                let (token, headers, ua) = self.oauth_bearer(oauth)?;
                let base_url = self.oauth_base_url();
                openai_compat::list_models(&base_url, Some(&token), &headers, Some(&ua))
            }
            BackendConfig::ApiKey { base_url, .. } | BackendConfig::Anthropic { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::list_models(
                    base_url,
                    key.as_deref(),
                    self.config.extra_headers(),
                    None,
                )
            }
            BackendConfig::Responses { .. } => self.codex_list_models(oauth),
        }
    }
}

pub fn build_backends(cfg: &Config) -> HashMap<String, BackendHandle> {
    cfg.backends
        .iter()
        .map(|(name, bc)| (name.clone(), BackendHandle::from_config(name, bc)))
        .collect()
}

pub fn get_backend<'a>(
    map: &'a HashMap<String, BackendHandle>,
    name: &str,
) -> Result<&'a BackendHandle> {
    map.get(name)
        .ok_or_else(|| Error::Msg(format!("unknown backend '{name}'")))
}
