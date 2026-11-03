//! Append-only JSON Lines record of every tool call: who, when, what ran and
//! how it ended. The file is owner-only because scripts can contain secrets.

use std::path::PathBuf;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Serialize)]
pub struct AuditRecord<'a> {
    pub time: String,
    pub client: &'a str,
    pub tool: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<&'a str>,
    pub files_in: Vec<&'a str>,
    pub files_out: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub async fn append(&self, record: &AuditRecord<'_>) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let _guard = self.lock.lock().await;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&self.path).await?;
        file.write_all(&line).await?;
        file.flush().await
    }
}

pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string()
}
