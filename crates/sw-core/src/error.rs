//! The error type shared by the core crate.

use std::path::PathBuf;

/// Errors from configuration, the data directory and the database.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("{0}")]
    InvalidName(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
