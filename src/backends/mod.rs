pub mod openai_compat;

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

    pub fn base_url(&self) -> &str {
        self.config.base_url()
    }

    pub fn family_name(&self) -> &'static str {
        match &self.config {
            BackendConfig::Oauth { .. } => "oauth",
            BackendConfig::ApiKey { .. } => "api_key",
            BackendConfig::Anthropic { .. } => "anthropic",
        }
    }

    pub fn is_anthropic(&self) -> bool {
        matches!(self.config, BackendConfig::Anthropic { .. })
    }

    fn oauth_bearer(&self, store: &OauthStore) -> Result<(String, BTreeMap<String, String>, String)> {
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
        }
    }

    pub fn list_models(&self, oauth: &OauthStore) -> Result<Vec<String>> {
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
