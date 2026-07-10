use crate::auth::TokenCache;
use crate::backends::{build_backends, BackendHandle};
use crate::config::{config_path, Config};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub backends: Arc<RwLock<HashMap<String, BackendHandle>>>,
    pub tokens: Arc<Mutex<TokenCache>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let backends = build_backends(&config);
        Self {
            config: Arc::new(RwLock::new(config)),
            backends: Arc::new(RwLock::new(backends)),
            tokens: Arc::new(Mutex::new(TokenCache::default())),
        }
    }

    pub fn reload_from_disk(&self) -> Result<()> {
        let path = config_path();
        let cfg = Config::load(&path)?;
        self.apply_config(cfg)
    }

    /// Replace live config + rebuild backend handles.
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
