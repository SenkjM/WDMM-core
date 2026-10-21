use thiserror::Error;

#[derive(Debug, Error)]
pub enum WdmmError {
    #[error("driver error: {0}")]
    Driver(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("unsupported operation: {0}")]
    Unsupported(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("other: {0}")]
    Other(String),
}

impl From<std::io::Error> for WdmmError {
    fn from(e: std::io::Error) -> Self {
        WdmmError::Io(e.to_string())
    }
}
