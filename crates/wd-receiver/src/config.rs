use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "io.github.gabrialdeora.WirelessDisplay";
pub const DEFAULT_PORT: u16 = 48321;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub log_level: String,
    pub window_width: i32,
    pub window_height: i32,
    pub window_maximized: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub listen_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig {
                log_level: "info".into(),
                window_width: 900,
                window_height: 640,
                window_maximized: false,
            },
            network: NetworkConfig {
                listen_port: DEFAULT_PORT,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config directory unavailable")]
    NoConfigDir,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    let dir = dirs::config_dir()
        .ok_or(ConfigError::NoConfigDir)?
        .join("wireless-display");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("config.toml"))
}

/// Directory holding the receiver identity (self-signed cert + key).
pub fn identity_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wireless-display")
        .join("identity")
}

/// JSON file listing paired phones.
pub fn paired_devices_file() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wireless-display")
        .join("paired-devices.json")
}

impl Config {
    pub fn load_or_create() -> Result<(Self, PathBuf), ConfigError> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Result<(Self, PathBuf), ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(raw) => match toml::from_str::<Config>(&raw) {
                Ok(cfg) => Ok((cfg.fill_defaults_from_env(), path.to_path_buf())),
                Err(source) => {
                    let backup = path.with_extension("toml.bak");
                    let _ = std::fs::rename(path, &backup);
                    tracing::warn!(path = %path.display(), backup = %backup.display(), %source,
                        "invalid config file; moved aside and regenerated defaults");
                    let cfg = Config::default();
                    cfg.save_to(&path.to_path_buf())?;
                    Ok((cfg, path.to_path_buf()))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.save_to(&path.to_path_buf())?;
                tracing::info!(path = %path.display(), "created default config");
                Ok((cfg, path.to_path_buf()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&config_path()?)
    }

    fn save_to(&self, path: &PathBuf) -> Result<(), ConfigError> {
        let raw = toml::to_string_pretty(self).expect("config serializes");
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn fill_defaults_from_env(mut self) -> Self {
        if let Ok(port) = std::env::var("WD_LISTEN_PORT")
            .map_err(|_| ())
            .and_then(|p| p.parse().map_err(|_| ()))
        {
            self.network.listen_port = port;
        }
        self
    }
}

pub fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.general.log_level.is_empty() {
        return Err("log_level must not be empty".into());
    }
    if cfg.network.listen_port < 1024 {
        return Err(format!(
            "listen_port {} is privileged (<1024)",
            cfg.network.listen_port
        ));
    }
    if cfg.general.window_width < 320 || cfg.general.window_height < 240 {
        return Err("window size below minimum 320x240".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_roundtrip() {
        let cfg = Config::default();
        validate(&cfg).unwrap();
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn partial_files_get_defaulted_fields() {
        let parsed: Config = toml::from_str("[network]\nlisten_port = 50000").unwrap();
        assert_eq!(parsed.general.log_level, "info");
        assert_eq!(parsed.network.listen_port, 50000);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not [valid toml ===").unwrap();
        let (loaded, loaded_path) = Config::load_from(&path).unwrap();
        assert_eq!(loaded_path, path);
        assert_eq!(loaded, Config::default());
        assert!(
            path.with_extension("toml.bak").exists(),
            "corrupt file backed up"
        );
        let regenerated = std::fs::read_to_string(&path).unwrap();
        assert!(!regenerated.contains("not [valid"));
    }

    #[test]
    fn validation_rejects_privileged_ports() {
        let mut cfg = Config::default();
        cfg.network.listen_port = 80;
        assert!(validate(&cfg).is_err());
    }
}
