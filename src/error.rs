//! Error types used across leancd.

use thiserror::Error;

/// Top-level error type for leancd.
#[derive(Debug, Error)]
pub enum Error {
    #[error("git operation failed: {0}")]
    Git(String),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("kubernetes error: {0}")]
    Kube(#[from] kube::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("hook error: {0}")]
    Hook(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Manifest(format!("json: {err}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
