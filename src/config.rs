use crate::error::{Error, Result};
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

/// OAuth token file path — keep Python-era location so existing logins work.
/// Intentionally NOT dirs::config_dir() (that is ~/Library on macOS).
pub fn auth_path() -> PathBuf {
    home_dir().join(".config/grok-test/auth.json")
}

pub fn config_path() -> PathBuf {
    home_dir().join(".config/spock/config.toml")
}

#[allow(dead_code)]
pub fn config_dir() -> PathBuf {
    home_dir().join(".config/spock")
}

pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h);
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        return PathBuf::from(h);
    }
    PathBuf::from(".")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub advisor: AdvisorSection,
    #[serde(default)]
    pub web_search: WebSearchSection,
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
    /// SearXNG (or custom) base, e.g. http://127.0.0.1:8888 — no path.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendConfig {
    Xai {
        #[serde(default = "default_xai_base")]
        base_url: String,
        /// Optional xAI API key (console.x.ai). When set, skips OAuth for this backend.
        /// Priority: this field → env `XAI_TOKEN` → OAuth device flow.
        #[serde(default)]
        api_key: Option<String>,
    },
    Openai {
        base_url: String,
        #[serde(default)]
        api_key: Option<String>,
        /// Optional extra HTTP headers (e.g. OpenRouter HTTP-Referer / X-Title).
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        /// If `api_key` is empty, read bearer from this env var (e.g. OPENROUTER_API_KEY).
        #[serde(default)]
        api_key_env: Option<String>,
        /// When true, call OpenAI Responses API (`/v1/responses`) instead of chat completions.
        /// Not fully implemented for all shapes — prefer chat completions.
        #[serde(default)]
        use_responses_api: bool,
        /// Azure OpenAI: if set, requests use `api-key` header and
        /// `{base}/openai/deployments/{deployment}/chat/completions?api-version=...`.
        #[serde(default)]
        azure_deployment: Option<String>,
        #[serde(default)]
        azure_api_version: Option<String>,
    },
    /// Forward Anthropic Messages JSON as-is (no OpenAI translation).
    Anthropic {
        base_url: String,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
    },
}

fn default_xai_base() -> String {
    DEFAULT_XAI_BASE.to_string()
}

impl BackendConfig {
    pub fn kind_name(&self) -> &'static str {
        match self {
            BackendConfig::Xai { .. } => "xai",
            BackendConfig::Openai { .. } => "openai",
            BackendConfig::Anthropic { .. } => "anthropic",
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            BackendConfig::Xai { base_url, .. } => base_url,
            BackendConfig::Openai { base_url, .. } => base_url,
            BackendConfig::Anthropic { base_url, .. } => base_url,
        }
    }

    /// Extra request headers (openai backends only; xAI returns empty).
    pub fn extra_headers(&self) -> &BTreeMap<String, String> {
        match self {
            BackendConfig::Openai { extra_headers, .. } => extra_headers,
            BackendConfig::Xai { .. } | BackendConfig::Anthropic { .. } => {
                static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> =
                    std::sync::OnceLock::new();
                EMPTY.get_or_init(BTreeMap::new)
            }
        }
    }

    /// Optional static bearer (API key) for this backend.
    /// Priority: config `api_key` → `api_key_env` → well-known env names by backend purpose.
    pub fn api_key(&self) -> Option<String> {
        match self {
            BackendConfig::Xai { api_key, .. } => api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| {
                    std::env::var("XAI_TOKEN")
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                }),
            BackendConfig::Openai {
                api_key,
                api_key_env,
                ..
            } => {
                if let Some(k) = api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
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
                // Common fallbacks when api_key_env not set (order matters little).
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
                if let Some(k) = api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
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
            BackendConfig::Openai {
                azure_deployment, ..
            } => azure_deployment.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    pub fn azure_api_version(&self) -> Option<&str> {
        match self {
            BackendConfig::Openai {
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
            BackendConfig::Openai {
                use_responses_api: true,
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
    /// Exact model-id overrides (not role keys).
    /// TOML: under [profiles.name.exact] or we also accept unknown string keys via deny_unknown_fields off.
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
        BackendConfig::Xai {
            base_url: DEFAULT_XAI_BASE.to_string(),
            api_key: None,
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

    // hybrid example ollama backend (inactive until user has Ollama)
    backends.insert(
        "ollama".to_string(),
        BackendConfig::Openai {
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            api_key: None,
            extra_headers: BTreeMap::new(),
            api_key_env: None,
            use_responses_api: false,
            azure_deployment: None,
            azure_api_version: None,
        },
    );

    Config {
        server: ServerConfig::default(),
        backends,
        profiles,
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

    #[allow(dead_code)] // used by Settings UI + tray
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
        // Force loopback for safety even if misconfigured.
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
    use std::collections::BTreeMap;

    #[test]
    fn parse_openai_extra_headers_and_env() {
        let toml = r#"
[server]
profile = "xai-only"
[backends.openrouter]
type = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
[backends.openrouter.extra_headers]
HTTP-Referer = "https://example.com"
X-Title = "Spock"
[profiles.xai-only]
default = "xai:grok-4.5"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let be = cfg.backends.get("openrouter").expect("backend");
        assert_eq!(be.kind_name(), "openai");
        assert_eq!(be.base_url(), "https://openrouter.ai/api/v1");
        assert_eq!(
            be.extra_headers().get("HTTP-Referer").map(String::as_str),
            Some("https://example.com")
        );
        assert_eq!(
            be.extra_headers().get("X-Title").map(String::as_str),
            Some("Spock")
        );
    }

    #[test]
    fn parse_advisor_web_search_sections() {
        let toml = r#"
[server]
profile = "xai-only"
[backends.xai]
type = "xai"
[profiles.xai-only]
default = "xai:grok-4.5"
[advisor]
enabled = true
model = "xai:grok-4.5"
max_tokens = 2048
[web_search]
enabled = true
provider = "brave"
api_key_env = "BRAVE_API_KEY"
max_results = 7
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(cfg.advisor.enabled);
        assert_eq!(cfg.advisor.model.as_deref(), Some("xai:grok-4.5"));
        assert_eq!(cfg.advisor.max_tokens, 2048);
        assert!(cfg.web_search.enabled);
        assert_eq!(cfg.web_search.provider, "brave");
        assert_eq!(cfg.web_search.max_results, 7);
    }

    #[test]
    fn parse_anthropic_and_azure_fields() {
        let toml = r#"
[server]
profile = "p"
[backends.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
[backends.azure]
type = "openai"
base_url = "https://res.openai.azure.com"
azure_deployment = "gpt-4o"
azure_api_version = "2024-06-01"
api_key_env = "AZURE_OPENAI_API_KEY"
[profiles.p]
default = "anthropic:claude-sonnet-5"
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.backends["anthropic"].kind_name(), "anthropic");
        assert_eq!(cfg.backends["azure"].azure_deployment(), Some("gpt-4o"));
        assert_eq!(cfg.backends["azure"].azure_api_version(), Some("2024-06-01"));
        assert!(!cfg.backends["azure"].use_responses_api());
    }

    #[test]
    fn openai_api_key_prefers_config_over_env_list() {
        let be = BackendConfig::Openai {
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("sk-config".into()),
            extra_headers: BTreeMap::new(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            use_responses_api: false,
            azure_deployment: None,
            azure_api_version: None,
        };
        assert_eq!(be.api_key().as_deref(), Some("sk-config"));
    }

    #[test]
    fn default_config_has_xai_and_ollama() {
        let cfg = default_config();
        assert!(cfg.backends.contains_key("xai"));
        assert!(cfg.backends.contains_key("ollama"));
        assert!(!cfg.advisor.enabled);
        assert!(!cfg.web_search.enabled);
    }
}
