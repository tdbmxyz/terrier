//! Configuration: `terrier.toml` (or `$TERRIER_CONFIG`) merged over
//! defaults with figment, `TERRIER_` env vars on top. Same conventions
//! as ferret.

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: SocketAddr,
    pub db_path: PathBuf,
    /// Directory of the built web frontend to serve (None = API only).
    pub static_dir: Option<PathBuf>,
    pub scrape: ScrapeConfig,
    pub notifications: NotificationsConfig,
    /// Leboncoin ventes_immobilières plugin.
    pub leboncoin: LeboncoinConfig,
    /// Ouest France Immo plugin (needs a stealth fetch_command).
    pub ouestfrance: OuestFranceConfig,
    pub llm: LlmConfig,
    pub enrichment: EnrichmentConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ([0, 0, 0, 0], 4810).into(),
            db_path: "terrier.db".into(),
            static_dir: None,
            scrape: ScrapeConfig::default(),
            notifications: NotificationsConfig::default(),
            leboncoin: LeboncoinConfig::default(),
            ouestfrance: OuestFranceConfig::default(),
            llm: LlmConfig::default(),
            enrichment: EnrichmentConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrapeConfig {
    /// Consecutive failures of one source before an ntfy alert fires.
    pub failure_alert_after: u32,
    /// Re-notify a known match when its price drops by at least this %.
    pub renotify_drop_pct: f64,
    /// Search locations merged into the rotation are capped here.
    pub max_search_locations: usize,
}

impl Default for ScrapeConfig {
    fn default() -> Self {
        Self {
            failure_alert_after: 5,
            renotify_drop_pct: 1.0,
            max_search_locations: 20,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// ntfy server base URL; unset = notifications disabled.
    pub ntfy_url: Option<url::Url>,
    pub topic: String,
    /// Bearer token file for protected topics.
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeboncoinConfig {
    pub enabled: bool,
    /// Baseline locations ("Rennes_35000" URL form or "Rennes 35000" —
    /// normalized by the plugin). Search locations pile on top.
    pub locations: Vec<String>,
    pub pages_per_location: u32,
    pub delay_ms: u64,
    pub interval_minutes: u64,
}

impl Default for LeboncoinConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            locations: Vec::new(),
            pages_per_location: 1,
            delay_ms: 3000,
            interval_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OuestFranceConfig {
    pub enabled: bool,
    pub locations: Vec<String>,
    pub delay_ms: u64,
    pub interval_minutes: u64,
    /// External fetch argv with `{url}` substitution — the site is
    /// bot-walled, a stealth browser wrapper is required.
    pub fetch_command: Vec<String>,
}

impl Default for OuestFranceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            locations: Vec::new(),
            delay_ms: 5000,
            interval_minutes: 120,
            fetch_command: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    /// OpenAI-compatible base url (llama.cpp: http://host:8080/v1).
    pub base_url: String,
    pub model: String,
    pub api_key_file: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://127.0.0.1:8080/v1".into(),
            model: String::new(),
            api_key_file: None,
            timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrichmentConfig {
    /// Queue poll cadence per source.
    pub poll_seconds: u64,
    pub max_attempts: u32,
    /// Images downloaded per listing at most.
    pub max_images: usize,
    /// Where images land; relative paths resolve against the CWD.
    pub images_dir: PathBuf,
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            poll_seconds: 60,
            max_attempts: 8,
            max_images: 10,
            images_dir: "images".into(),
        }
    }
}

pub fn load() -> anyhow::Result<Config> {
    let path = std::env::var("TERRIER_CONFIG").unwrap_or_else(|_| "terrier.toml".into());
    let config: Config = Figment::new()
        .merge(figment::providers::Serialized::defaults(Config::default()))
        .merge(Toml::file(path))
        .merge(Env::prefixed("TERRIER_").split("__"))
        .extract()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert_eq!(config.listen.port(), 4810);
        assert!(!config.leboncoin.enabled, "sources are opt-in");
        assert!(!config.ouestfrance.enabled);
        assert!(
            config.notifications.ntfy_url.is_none(),
            "notifications opt-in"
        );
        assert!(!config.llm.enabled);
    }
}
