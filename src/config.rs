use crate::error::{Error, Result};
use crate::oauth::registry::{get_provider, CompletionsQuirk};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_PORT: u16 = 8048;
pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_XAI_BASE: &str = "https://api.x.ai/v1";
pub const DEFAULT_GROK_MODEL: &str = "grok-4.5";
pub const CHAT_DEFAULT_MODEL: &str = "grok-4.3";
pub const UA: &str = concat!("spock/", env!("CARGO_PKG_VERSION"));

pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h);
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        return PathBuf::from(h);
    }
    PathBuf::from(".")
}

pub fn config_path() -> PathBuf {
    home_dir().join(".config/spock/config.toml")
}

pub fn config_dir() -> PathBuf {
    home_dir().join(".config/spock")
}

/// Legacy xAI token path (Python-era); still used as import source.
#[allow(dead_code)]
pub fn legacy_xai_auth_path() -> PathBuf {
    home_dir().join(".config/grok-test/auth.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    /// Curated shortlist for external pickers (Grok Build, etc.).
    /// Orthogonal to `profiles` (Claude Code 5-slot role map).
    #[serde(default)]
    pub catalog: CatalogSection,
    #[serde(default)]
    pub advisor: AdvisorSection,
    #[serde(default)]
    pub web_search: WebSearchSection,
}

/// Hand-picked `backend:model` entries served on `GET /v1/models` for external
/// agents. When non-empty, the list is those entries only (local, no backend
/// probes, no Claude aliases). Empty catalog keeps the legacy merge.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogSection {
    #[serde(default)]
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Client-facing id, usually `backend:model` (e.g. `xai:grok-4.5`).
    pub id: String,
    /// Optional picker label.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Context window in tokens. When set, emitted so Grok Build applies
    /// per-model compaction correctly. When omitted, the list card leaves it
    /// unset (Grok defaults ~200k). List path never probes backends.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// When true, `/v1/models` advertises `supportsReasoningEffort` so Grok Build
    /// enables `/effort` and the effort picker. When omitted, Spock uses a
    /// conservative id heuristic (xai/kimi/deepseek → true).
    #[serde(default)]
    pub supports_reasoning_effort: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_advisor_max")]
    pub max_tokens: u32,
}

fn default_advisor_max() -> u32 {
    4096
}

impl Default for AdvisorSection {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_tokens: default_advisor_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ws_provider")]
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_ws_max")]
    pub max_results: u32,
}

fn default_ws_provider() -> String {
    "duckduckgo".into()
}
fn default_ws_max() -> u32 {
    5
}

impl Default for WebSearchSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_ws_provider(),
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_results: default_ws_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_profile")]
    pub profile: String,
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_profile() -> String {
    "xai-only".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            profile: default_profile(),
        }
    }
}

/// Backend kinds. Legacy TOML `type = "xai"` / `"openai"` accepted on read via custom deserialize.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    /// Device OAuth (xAI, Kimi Code, …) via registry `provider`.
    Oauth {
        provider: String,
        #[serde(default)]
        base_url: String,
        /// Optional console/API key escape hatch (beats OAuth).
        #[serde(default)]
        api_key: Option<String>,
    },
    /// OpenAI-compatible Chat Completions with bearer / Azure key.
    ApiKey {
        base_url: String,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        use_responses_api: bool,
        #[serde(default)]
        azure_deployment: Option<String>,
        #[serde(default)]
        azure_api_version: Option<String>,
        /// llama-server ds4-ports named KV sessions. When true, CC traffic
        /// uses native `/completion` + `/fork` + `/close_session`. Missing
        /// routes or unknown session_id fail the request — never fall through
        /// to `/v1/chat/completions`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        kv_sessions: bool,
    },
    /// Forward Anthropic Messages JSON as-is.
    Anthropic {
        base_url: String,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
    },
}

// Custom deserialize to accept legacy type = "xai" | "openai".
impl<'de> Deserialize<'de> for BackendConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            provider: Option<String>,
            #[serde(default)]
            base_url: Option<String>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            extra_headers: BTreeMap<String, String>,
            #[serde(default)]
            api_key_env: Option<String>,
            #[serde(default)]
            use_responses_api: bool,
            #[serde(default)]
            azure_deployment: Option<String>,
            #[serde(default)]
            azure_api_version: Option<String>,
            #[serde(default)]
            kv_sessions: bool,
        }
        let r = Raw::deserialize(deserializer)?;
        let kind = r.kind.trim().to_ascii_lowercase();
        match kind.as_str() {
            "oauth" => {
                let provider = r
                    .provider
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| serde::de::Error::custom("oauth backend needs provider"))?;
                if get_provider(&provider).is_none() {
                    return Err(serde::de::Error::custom(format!(
                        "unknown oauth provider '{provider}' (known: {})",
                        crate::oauth::provider_ids_csv()
                    )));
                }
                let base_url = r
                    .base_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        get_provider(&provider)
                            .map(|p| p.default_base_url.to_string())
                            .unwrap_or_default()
                    });
                Ok(BackendConfig::Oauth {
                    provider,
                    base_url,
                    api_key: r.api_key,
                })
            }
            // Legacy alias
            "xai" => Ok(BackendConfig::Oauth {
                provider: "xai".into(),
                base_url: r
                    .base_url
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_XAI_BASE.to_string()),
                api_key: r.api_key,
            }),
            "api_key" | "openai" => {
                let base_url = r
                    .base_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        serde::de::Error::custom("api_key/openai backend needs base_url")
                    })?;
                Ok(BackendConfig::ApiKey {
                    base_url,
                    api_key: r.api_key,
                    extra_headers: r.extra_headers,
                    api_key_env: r.api_key_env,
                    use_responses_api: r.use_responses_api,
                    azure_deployment: r.azure_deployment,
                    azure_api_version: r.azure_api_version,
                    kv_sessions: r.kv_sessions,
                })
            }
            "anthropic" => {
                let base_url = r
                    .base_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "https://api.anthropic.com".into());
                Ok(BackendConfig::Anthropic {
                    base_url,
                    api_key: r.api_key,
                    api_key_env: r.api_key_env,
                })
            }
            other => Err(serde::de::Error::custom(format!(
                "unknown backend type '{other}' (use oauth, api_key, anthropic)"
            ))),
        }
    }
}

impl BackendConfig {
    pub fn kind_name(&self) -> &'static str {
        match self {
            BackendConfig::Oauth { .. } => "oauth",
            BackendConfig::ApiKey { .. } => "api_key",
            BackendConfig::Anthropic { .. } => "anthropic",
        }
    }

    #[allow(dead_code)] // used by tests + status CLI paths
    pub fn oauth_provider(&self) -> Option<&str> {
        match self {
            BackendConfig::Oauth { provider, .. } => Some(provider.as_str()),
            _ => None,
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            BackendConfig::Oauth { base_url, .. } => base_url,
            BackendConfig::ApiKey { base_url, .. } => base_url,
            BackendConfig::Anthropic { base_url, .. } => base_url,
        }
    }

    pub fn quirk(&self) -> CompletionsQuirk {
        match self {
            BackendConfig::Oauth { provider, .. } => get_provider(provider)
                .map(|p| p.quirk)
                .unwrap_or(CompletionsQuirk::Generic),
            BackendConfig::ApiKey { .. } => CompletionsQuirk::Generic,
            BackendConfig::Anthropic { .. } => CompletionsQuirk::Generic,
        }
    }

    pub fn extra_headers(&self) -> &BTreeMap<String, String> {
        match self {
            BackendConfig::ApiKey { extra_headers, .. } => extra_headers,
            BackendConfig::Oauth { .. } | BackendConfig::Anthropic { .. } => {
                static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> =
                    std::sync::OnceLock::new();
                EMPTY.get_or_init(BTreeMap::new)
            }
        }
    }

    /// Static API key only (no OAuth). For Oauth backends this is the escape-hatch key.
    pub fn api_key(&self) -> Option<String> {
        match self {
            BackendConfig::Oauth { api_key, .. } => api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            BackendConfig::ApiKey {
                api_key,
                api_key_env,
                ..
            } => {
                if let Some(k) = api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    return Some(k.to_string());
                }
                if let Some(env_name) = api_key_env
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Ok(v) = std::env::var(env_name) {
                        let v = v.trim().to_string();
                        if !v.is_empty() {
                            return Some(v);
                        }
                    }
                }
                for name in [
                    "OPENROUTER_API_KEY",
                    "OPENAI_API_KEY",
                    "DEEPSEEK_API_KEY",
                    "GROQ_API_KEY",
                    "TOGETHER_API_KEY",
                    "FIREWORKS_API_KEY",
                    "MISTRAL_API_KEY",
                    "MOONSHOT_API_KEY",
                ] {
                    if let Ok(v) = std::env::var(name) {
                        let v = v.trim().to_string();
                        if !v.is_empty() {
                            return Some(v);
                        }
                    }
                }
                None
            }
            BackendConfig::Anthropic {
                api_key,
                api_key_env,
                ..
            } => {
                if let Some(k) = api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    return Some(k.to_string());
                }
                if let Some(env_name) = api_key_env
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Ok(v) = std::env::var(env_name) {
                        let v = v.trim().to_string();
                        if !v.is_empty() {
                            return Some(v);
                        }
                    }
                }
                std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            }
        }
    }

    pub fn azure_deployment(&self) -> Option<&str> {
        match self {
            BackendConfig::ApiKey {
                azure_deployment, ..
            } => azure_deployment
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    pub fn azure_api_version(&self) -> Option<&str> {
        match self {
            BackendConfig::ApiKey {
                azure_api_version, ..
            } => azure_api_version
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    pub fn use_responses_api(&self) -> bool {
        matches!(
            self,
            BackendConfig::ApiKey {
                use_responses_api: true,
                ..
            }
        )
    }

    /// llama-server native session routes (ds4-ports). Off by default.
    pub fn kv_sessions(&self) -> bool {
        matches!(
            self,
            BackendConfig::ApiKey {
                kv_sessions: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    pub default: Option<String>,
    pub haiku: Option<String>,
    pub sonnet: Option<String>,
    pub opus: Option<String>,
    pub fable: Option<String>,
    #[serde(default)]
    pub exact: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        default_config()
    }
}

pub fn default_config() -> Config {
    let mut backends = BTreeMap::new();
    backends.insert(
        "xai".to_string(),
        BackendConfig::Oauth {
            provider: "xai".into(),
            base_url: DEFAULT_XAI_BASE.to_string(),
            api_key: None,
        },
    );
    backends.insert(
        "ollama".to_string(),
        BackendConfig::ApiKey {
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            api_key: None,
            extra_headers: BTreeMap::new(),
            api_key_env: None,
            use_responses_api: false,
            azure_deployment: None,
            azure_api_version: None,
            kv_sessions: false,
        },
    );

    let mut profiles = BTreeMap::new();
    profiles.insert(
        "xai-only".to_string(),
        ProfileConfig {
            default: Some(format!("xai:{DEFAULT_GROK_MODEL}")),
            haiku: None,
            sonnet: None,
            opus: None,
            fable: None,
            exact: BTreeMap::new(),
        },
    );
    profiles.insert(
        "hybrid".to_string(),
        ProfileConfig {
            default: Some(format!("xai:{DEFAULT_GROK_MODEL}")),
            haiku: Some("ollama:qwen2.5:14b".to_string()),
            sonnet: Some(format!("xai:{DEFAULT_GROK_MODEL}")),
            opus: Some(format!("xai:{DEFAULT_GROK_MODEL}")),
            fable: Some(format!("xai:{DEFAULT_GROK_MODEL}")),
            exact: BTreeMap::new(),
        },
    );
    profiles.insert(
        "local-only".to_string(),
        ProfileConfig {
            default: Some("ollama:qwen2.5:14b".to_string()),
            haiku: Some("ollama:qwen2.5:7b".to_string()),
            sonnet: Some("ollama:qwen2.5:14b".to_string()),
            opus: Some("ollama:qwen2.5:14b".to_string()),
            fable: Some("ollama:qwen2.5:14b".to_string()),
            exact: BTreeMap::new(),
        },
    );

    Config {
        server: ServerConfig::default(),
        backends,
        profiles,
        catalog: CatalogSection::default(),
        advisor: Default::default(),
        web_search: Default::default(),
    }
}

impl Config {
    pub fn load_or_init() -> Result<Self> {
        let path = config_path();
        if path.exists() {
            Self::load(&path)
        } else {
            let cfg = default_config();
            cfg.save(&path)?;
            eprintln!("  wrote default config → {}", path.display());
            Ok(cfg)
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text).map_err(|e| Error::Toml(e.to_string()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::Toml(e.to_string()))?;
        fs::write(path, text)?;
        Ok(())
    }

    pub fn active_profile(&self) -> Result<&ProfileConfig> {
        self.profiles
            .get(&self.server.profile)
            .ok_or_else(|| Error::Msg(format!("unknown profile '{}'", self.server.profile)))
    }

    #[allow(dead_code)]
    pub fn set_profile(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            return Err(Error::Msg(format!("unknown profile '{name}'")));
        }
        self.server.profile = name.to_string();
        Ok(())
    }

    pub fn port_from_env_or_self(&self) -> u16 {
        std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(self.server.port)
    }

    pub fn bind_addr(&self) -> String {
        let host = if self.server.bind == "127.0.0.1" || self.server.bind == "localhost" {
            self.server.bind.clone()
        } else {
            eprintln!(
                "  warning: refusing non-loopback bind '{}'; using 127.0.0.1",
                self.server.bind
            );
            DEFAULT_BIND.to_string()
        };
        format!("{host}:{}", self.port_from_env_or_self())
    }

    /// Any backend using this oauth provider has a config api_key set?
    pub fn oauth_config_key_set(&self, provider_id: &str) -> bool {
        self.backends.values().any(|b| match b {
            BackendConfig::Oauth {
                provider, api_key, ..
            } if provider.eq_ignore_ascii_case(provider_id) => api_key
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty()),
            _ => false,
        })
    }
}

/// Env overrides used by single-backend fallback / chat.
#[allow(dead_code)]
pub struct EnvOverrides {
    pub xai_token: Option<String>,
    pub xai_api_base: String,
    pub grok_model: String,
    pub grok_small_model: String,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        let grok_model =
            std::env::var("GROK_MODEL").unwrap_or_else(|_| DEFAULT_GROK_MODEL.to_string());
        let grok_small_model =
            std::env::var("GROK_SMALL_MODEL").unwrap_or_else(|_| grok_model.clone());
        Self {
            xai_token: std::env::var("XAI_TOKEN").ok(),
            xai_api_base: std::env::var("XAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_XAI_BASE.to_string()),
            grok_model,
            grok_small_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_oauth_and_legacy_xai() {
        let toml = r#"
[server]
profile = "xai-only"
[backends.xai]
type = "xai"
[backends.kimi]
type = "oauth"
provider = "kimi"
[backends.ollama]
type = "openai"
base_url = "http://127.0.0.1:11434/v1"
[profiles.xai-only]
default = "xai:grok-4.5"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.backends["xai"].kind_name(), "oauth");
        assert_eq!(cfg.backends["xai"].oauth_provider(), Some("xai"));
        assert_eq!(cfg.backends["kimi"].oauth_provider(), Some("kimi"));
        assert_eq!(cfg.backends["ollama"].kind_name(), "api_key");
        assert_eq!(
            cfg.backends["kimi"].base_url(),
            "https://api.kimi.com/coding/v1"
        );
    }

    #[test]
    fn parse_api_key_extra_headers() {
        let toml = r#"
[server]
profile = "p"
[backends.openrouter]
type = "api_key"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
[backends.openrouter.extra_headers]
HTTP-Referer = "https://example.com"
[profiles.p]
default = "openrouter:x"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let be = &cfg.backends["openrouter"];
        assert_eq!(
            be.extra_headers().get("HTTP-Referer").map(String::as_str),
            Some("https://example.com")
        );
    }

    #[test]
    fn serialize_writes_oauth_not_xai() {
        let cfg = default_config();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("type = \"oauth\"") || text.contains("type = 'oauth'"));
        assert!(!text.contains("type = \"xai\""));
        assert!(text.contains("type = \"api_key\"") || text.contains("type = 'api_key'"));
    }

    #[test]
    fn default_config_has_xai_and_ollama() {
        let cfg = default_config();
        assert!(cfg.backends.contains_key("xai"));
        assert!(cfg.backends.contains_key("ollama"));
        assert!(!cfg.advisor.enabled);
    }

    #[test]
    fn parse_kv_sessions_opt_in() {
        let toml = r#"
[server]
profile = "p"
[backends.ds4]
type = "api_key"
base_url = "http://10.0.0.5:8080/v1"
kv_sessions = true
[backends.ollama]
type = "api_key"
base_url = "http://127.0.0.1:11434/v1"
[profiles.p]
default = "ds4:qwen"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(cfg.backends["ds4"].kv_sessions());
        assert!(!cfg.backends["ollama"].kv_sessions());
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("kv_sessions"));
    }
}
