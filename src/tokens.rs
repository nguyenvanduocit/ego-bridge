//! Per-client bearer tokens. Only SHA-256 digests are stored, so the token
//! file cannot be replayed if it leaks. The file is re-read on every request,
//! which makes `token revoke` take effect without a restart.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::write_private;

const TOKEN_PREFIX: &str = "egb_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEntry {
    pub name: String,
    pub token_sha256: String,
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    clients: Vec<ClientEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("client name must be 1-64 ASCII letters, digits, '-', '_' or '.': {0:?}")]
    InvalidName(String),
    #[error("a client named {0:?} already exists; revoke it first")]
    Exists(String),
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

#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<TokenFile, TokenError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| TokenError::Parse {
                path: self.path.clone(),
                source,
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(TokenFile::default()),
            Err(source) => Err(TokenError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn save(&self, file: &TokenFile) -> Result<(), TokenError> {
        let bytes = serde_json::to_vec_pretty(file).map_err(|source| TokenError::Parse {
            path: self.path.clone(),
            source,
        })?;
        write_private(&self.path, &bytes).map_err(|source| TokenError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Create a client and return its token. The token is not stored and
    /// cannot be shown again.
    pub fn add(&self, name: &str) -> Result<String, TokenError> {
        if !valid_name(name) {
            return Err(TokenError::InvalidName(name.to_owned()));
        }
        let mut file = self.load()?;
        if file.clients.iter().any(|client| client.name == name) {
            return Err(TokenError::Exists(name.to_owned()));
        }
        let token = format!(
            "{TOKEN_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
        );
        file.clients.push(ClientEntry {
            name: name.to_owned(),
            token_sha256: hash_token(&token),
            created_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
        });
        self.save(&file)?;
        Ok(token)
    }

    /// Remove a client. Returns whether it existed.
    pub fn revoke(&self, name: &str) -> Result<bool, TokenError> {
        let mut file = self.load()?;
        let before = file.clients.len();
        file.clients.retain(|client| client.name != name);
        let removed = file.clients.len() != before;
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Result<Vec<ClientEntry>, TokenError> {
        Ok(self.load()?.clients)
    }

    /// The client name that owns `token`, if any.
    pub fn authenticate(&self, token: &str) -> Result<Option<String>, TokenError> {
        let digest = hash_token(token);
        Ok(self
            .load()?
            .clients
            .into_iter()
            .find(|client| client.token_sha256 == digest)
            .map(|client| client.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_token_authenticates_until_revoked() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = TokenStore::new(temp.path().join("tokens.json"));

        let token = store.add("pod-a")?;
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(store.authenticate(&token)?.as_deref(), Some("pod-a"));
        assert_eq!(store.authenticate("egb_wrong")?, None);

        let saved = fs::read_to_string(store.path())?;
        assert!(
            !saved.contains(&token),
            "plaintext token must not be stored"
        );

        assert!(store.revoke("pod-a")?);
        assert_eq!(store.authenticate(&token)?, None);
        assert!(!store.revoke("pod-a")?);
        Ok(())
    }

    #[test]
    fn rejects_duplicate_and_invalid_names() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = TokenStore::new(temp.path().join("tokens.json"));
        store.add("pod-a")?;
        assert!(matches!(store.add("pod-a"), Err(TokenError::Exists(_))));
        assert!(matches!(
            store.add("bad name"),
            Err(TokenError::InvalidName(_))
        ));
        assert!(matches!(store.add(""), Err(TokenError::InvalidName(_))));
        Ok(())
    }

    #[test]
    fn tokens_are_unique() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = TokenStore::new(temp.path().join("tokens.json"));
        assert_ne!(store.add("a")?, store.add("b")?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let store = TokenStore::new(temp.path().join("tokens.json"));
        store.add("a")?;
        let mode = fs::metadata(store.path())?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        Ok(())
    }
}
