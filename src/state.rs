use crate::backends::{build_backends, BackendHandle};
use crate::config::{config_path, Config};
use crate::error::{Error, Result};
use crate::oauth::OauthStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Last upstream failure for Settings toast / status.
#[derive(Debug, Clone, Default)]
pub struct LastUpstreamError {
    pub message: String,
    pub status: u16,
    pub at_unix: f64,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub backends: Arc<RwLock<HashMap<String, BackendHandle>>>,
    /// Internal mutexes only — do not wrap in another Mutex (refresh must not block all backends).
    pub oauth: Arc<OauthStore>,
    pub last_upstream_error: Arc<Mutex<Option<LastUpstreamError>>>,
    /// Per-backend last `/fork` probe. `Ok` = implemented. Sticky 404 stays an error string.
    pub kv_fork_probe: Arc<Mutex<HashMap<String, std::result::Result<(), String>>>>,
    /// Vision sidecar caption cache (in-memory only).
    pub vision_cache: Arc<crate::vision::VisionCache>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let backends = build_backends(&config);
        Self {
            config: Arc::new(RwLock::new(config)),
            backends: Arc::new(RwLock::new(backends)),
            oauth: Arc::new(OauthStore::default()),
            last_upstream_error: Arc::new(Mutex::new(None)),
            kv_fork_probe: Arc::new(Mutex::new(HashMap::new())),
            vision_cache: Arc::new(crate::vision::VisionCache::default()),
        }
    }

    pub fn record_upstream_error(&self, status: u16, message: &str) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if let Ok(mut g) = self.last_upstream_error.lock() {
            *g = Some(LastUpstreamError {
                message: message.chars().take(800).collect(),
                status,
                at_unix: at,
            });
        }
    }

    pub fn last_error_snapshot(&self) -> Option<LastUpstreamError> {
        self.last_upstream_error.lock().ok().and_then(|g| g.clone())
    }

    pub fn reload_from_disk(&self) -> Result<()> {
        let path = config_path();
        let cfg = Config::load(&path)?;
        self.apply_config(cfg)
    }

    /// Replace live config + rebuild backend handles (oauth store preserved).
    pub fn apply_config(&self, cfg: Config) -> Result<()> {
        let backends = build_backends(&cfg);
        *self
            .config
            .write()
            .map_err(|_| Error::Msg("config lock".into()))? = cfg;
        *self
            .backends
            .write()
            .map_err(|_| Error::Msg("backends lock".into()))? = backends;
        if let Ok(mut g) = self.kv_fork_probe.lock() {
            g.clear();
        }
        Ok(())
    }

    pub fn apply_and_save(&self, cfg: Config) -> Result<()> {
        let path = config_path();
        cfg.save(&path)?;
        self.apply_config(cfg)
    }

    pub fn snapshot_config(&self) -> Result<Config> {
        self.with_config(|c| c.clone())
    }

    pub fn with_config<R>(&self, f: impl FnOnce(&Config) -> R) -> Result<R> {
        let g = self
            .config
            .read()
            .map_err(|_| Error::Msg("config lock".into()))?;
        Ok(f(&g))
    }
}
