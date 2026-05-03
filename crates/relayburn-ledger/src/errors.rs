use thiserror::Error;

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid sessionId: {0:?}")]
    InvalidSessionId(String),

    #[error("could not acquire lock after {attempts} attempts (~{budget_ms}ms) - {detail}: {path}")]
    LockTimeout {
        attempts: u32,
        budget_ms: u64,
        detail: &'static str,
        path: String,
    },
}

pub type Result<T> = std::result::Result<T, LedgerError>;
