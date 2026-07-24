//! Error and result types for origo-core.

use thiserror::Error;

/// Errors surfaced by the metadata store, content store, and engine.
#[derive(Error, Debug)]
pub enum OrigoError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("is a directory: {0}")]
    IsADirectory(String),

    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("too large: {0}")]
    TooLarge(String),

    #[error("corrupt object: {0}")]
    Corrupt(String),

    #[error("content missing for hash {0}")]
    ContentMissing(String),

    #[error("metadata store error: {0}")]
    Metadata(String),

    #[error("content store error: {0}")]
    Content(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for OrigoError {
    fn from(e: rusqlite::Error) -> Self {
        OrigoError::Metadata(e.to_string())
    }
}

impl From<object_store::Error> for OrigoError {
    fn from(e: object_store::Error) -> Self {
        OrigoError::Content(e.to_string())
    }
}

impl From<tokio_postgres::Error> for OrigoError {
    fn from(e: tokio_postgres::Error) -> Self {
        OrigoError::Metadata(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, OrigoError>;
