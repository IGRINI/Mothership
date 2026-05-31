#[derive(Debug, thiserror::Error)]
pub enum MothershipError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("credential vault error: {0}")]
    CredentialVault(String),

    #[error("runtime error: {0}")]
    Runtime(String),
}

pub type Result<T> = std::result::Result<T, MothershipError>;
