pub mod openai_compat;
pub mod xai;

use crate::config::{BackendConfig, Config};
use crate::error::{Error, Result};
use crate::translate::BackendFamily;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use crate::auth::TokenCache;

pub enum UpstreamBody {
    Json(Value),
    /// Streaming response: status already 200 assumed by caller after construction.
    Stream(Box<dyn Read + Send>),
}

#[allow(dead_code)]
pub struct BackendHandle {
    pub name: String,
    pub family: BackendFamily,
    pub config: BackendConfig,
}

impl BackendHandle {
    pub fn from_config(name: &str, cfg: &BackendConfig) -> Self {
        let family = match cfg {
            BackendConfig::Xai { .. } => BackendFamily::Xai,
            BackendConfig::Openai { .. } => BackendFamily::Openai,
            BackendConfig::Anthropic { .. } => BackendFamily::Anthropic,
        };
        Self {
            name: name.to_string(),
            family,
            config: cfg.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        self.config.base_url()
    }

    pub fn family_name(&self) -> &'static str {
        match self.family {
            BackendFamily::Xai => "xai",
            BackendFamily::Openai => "openai",
            BackendFamily::Anthropic => "anthropic",
        }
    }

    pub fn chat(
        &self,
        body: &Value,
        stream: bool,
        tokens: &Arc<Mutex<TokenCache>>,
    ) -> Result<UpstreamBody> {
        match &self.config {
            BackendConfig::Xai { base_url, api_key } => {
                let key = self.config.api_key();
                xai::chat(
                    base_url,
                    key.as_deref().or(api_key.as_deref()),
                    body,
                    stream,
                    tokens,
                )
            }
            BackendConfig::Openai { base_url, .. } => {
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
                )
            }
            BackendConfig::Anthropic { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::anthropic_messages(
                    base_url,
                    key.as_deref(),
                    body,
                    stream,
                )
            }
        }
    }

    pub fn get_json(&self, path: &str, tokens: &Arc<Mutex<TokenCache>>) -> Result<Value> {
        match &self.config {
            BackendConfig::Xai { base_url, api_key } => {
                let key = self.config.api_key();
                xai::get_json(
                    base_url,
                    path,
                    key.as_deref().or(api_key.as_deref()),
                    tokens,
                )
            }
            BackendConfig::Openai { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::get_json(
                    base_url,
                    key.as_deref(),
                    path,
                    self.config.extra_headers(),
                    self.config.azure_deployment(),
                    self.config.azure_api_version(),
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
                )
            }
        }
    }

    /// Discover model ids for Settings (OpenAI /models or Ollama /api/tags).
    pub fn list_models(&self, tokens: &Arc<Mutex<TokenCache>>) -> Result<Vec<String>> {
        match &self.config {
            BackendConfig::Xai { base_url, api_key } => {
                let key = self.config.api_key();
                let v = xai::get_json(
                    base_url,
                    "/models",
                    key.as_deref().or(api_key.as_deref()),
                    tokens,
                )?;
                let mut ids = Vec::new();
                if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
                    for m in data {
                        if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                            ids.push(id.to_string());
                        }
                    }
                }
                ids.sort();
                ids.dedup();
                Ok(ids)
            }
            BackendConfig::Openai { base_url, .. } | BackendConfig::Anthropic { base_url, .. } => {
                let key = self.config.api_key();
                openai_compat::list_models(
                    base_url,
                    key.as_deref(),
                    self.config.extra_headers(),
                )
            }
        }
    }

    #[allow(dead_code)]
    pub fn health(&self, tokens: &Arc<Mutex<TokenCache>>) -> (bool, String) {
        match self.get_json("/models", tokens) {
            Ok(_) => (true, "ok".into()),
            Err(e) => (false, e.to_string()),
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
