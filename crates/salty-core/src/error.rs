/// Errors from the service core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Database failure.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A record failed verification.
    #[error("{0}")]
    Update(#[from] weirdnet_update::Error),
    /// Nothing at that name.
    #[error("not found: {0}")]
    NotFound(String),
    /// The record conflicts with what is stored (seq, update id, root version).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Bad input.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;
