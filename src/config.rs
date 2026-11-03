//! Where ego-bridge keeps its files, and the client's connection settings.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const HOME_ENV: &str = "EGO_BRIDGE_HOME";
pub const URL_ENV: &str = "EGO_BRIDGE_URL";
pub const TOKEN_ENV: &str = "EGO_BRIDGE_TOKEN";

/// `$EGO_BRIDGE_HOME`, or `~/.config/ego-bridge`.
pub fn home_dir() -> io::Result<PathBuf> {
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let user_home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(user_home).join(".config").join("ego-bridge"))
}

pub fn tokens_path(home: &Path) -> PathBuf {
    home.join("tokens.json")
}

pub fn audit_path(home: &Path) -> PathBuf {
    home.join("audit.jsonl")
}

pub fn runs_dir(home: &Path) -> PathBuf {
    home.join("runs")
}

pub fn client_config_path(home: &Path) -> PathBuf {
    home.join("client.json")
}

/// How a client reaches its bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientConfig {
    /// MCP endpoint, for example `https://mac.tailnet.ts.net/mcp`.
    pub url: String,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "ego-bridge client is not configured: run `ego-bridge connect --url <url> --token <token>` or set {URL_ENV} and {TOKEN_ENV}"
    )]
    Missing,
    #[error("read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

impl ClientConfig {
    /// Environment variables win over the saved file.
    pub fn load(home: &Path) -> Result<Self, ConfigError> {
        let env_url = std::env::var(URL_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        let env_token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        if let (Some(url), Some(token)) = (env_url.clone(), env_token.clone()) {
            return Ok(Self { url, token });
        }
        let path = client_config_path(home);
        let saved = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                .map_err(|source| ConfigError::Parse { path, source })?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(ConfigError::Missing),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        Ok(Self {
            url: env_url.unwrap_or(saved.url),
            token: env_token.unwrap_or(saved.token),
        })
    }

    pub fn save(&self, home: &Path) -> Result<(), ConfigError> {
        let path = client_config_path(home);
        let bytes = serde_json::to_vec_pretty(self).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        write_private(&path, &bytes).map_err(|source| ConfigError::Write { path, source })
    }
}

/// Write a file readable only by the owner, replacing it atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    restrict_file(&tmp)?;
    fs::rename(&tmp, path)
}

#[cfg(unix)]
pub fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
pub fn restrict_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub fn restrict_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}
