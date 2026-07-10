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
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            BackendConfig::Xai { base_url, .. } => base_url,
            BackendConfig::Openai { base_url, .. } => base_url,
        }
    }

    /// Optional static bearer (API key) for this backend.
    #[allow(dead_code)]
    pub fn api_key(&self) -> Option<&str> {
        match self {
            BackendConfig::Xai { api_key, .. } | BackendConfig::Openai { api_key, .. } => {
                api_key.as_deref().map(str::trim).filter(|s| !s.is_empty())
            }
        }
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
        },
    );

    Config {
        server: ServerConfig::default(),
        backends,
        profiles,
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
