use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("network: {0}")]
    Network(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("auth failed")]
    AuthFailed,

    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DriverError>;

impl From<reqwest::Error> for DriverError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            DriverError::Network("timeout".into())
        } else if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
            DriverError::NotFound("resource".into())
        } else if e.status() == Some(reqwest::StatusCode::UNAUTHORIZED) {
            DriverError::AuthFailed
        } else {
            DriverError::Network(e.to_string())
        }
    }
}

/// A cloud file or directory.
#[derive(Debug, Clone)]
pub struct CloudFileItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    /// Direct download URL (with auth headers if needed).
    pub raw_url: Option<String>,
    /// Custom headers for raw_url.
    pub raw_headers: Vec<(String, String)>,
}

/// Cloud driver abstraction — aligns with Dart `CloudDriver`.
#[async_trait]
pub trait CloudDriver: Send + Sync {
    /// Initialise the driver (fetch tokens, validate credentials, etc.).
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /// List directory at remote path.
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>>;

    /// Get a single file item (must include `raw_url` for streaming/download).
    async fn get(&self, path: &str) -> Result<CloudFileItem>;

    /// Create a directory.
    async fn mkdir(&self, path: &str) -> Result<()>;

    /// Rename/move.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;

    /// Delete a file or directory.
    async fn remove(&self, path: &str) -> Result<()>;
}
