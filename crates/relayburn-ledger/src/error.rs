use thiserror::Error;

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json encode/decode: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid session id: {0}")]
    InvalidSessionId(String),

    #[error("schema downgrade: db at version {found}, this build supports up to {supported}")]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LedgerError>;
