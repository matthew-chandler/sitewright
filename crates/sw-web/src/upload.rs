//! Receiving an uploaded file. Uploads have no size limit, so the request body is
//! streamed to a staging file on disk instead of being held in memory.

use std::path::PathBuf;

use axum::body::Body;
use futures_util::StreamExt;
use sw_sites::SiteStore;
use tokio::io::AsyncWriteExt;

/// Why a body could not be staged.
#[derive(Debug)]
pub enum StageError {
    /// Nothing was sent.
    Empty,
    /// The connection broke off mid-upload.
    Interrupted(String),
    /// The disk refused (full, or not writable).
    Disk(std::io::Error),
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("the file is empty"),
            Self::Interrupted(e) => write!(f, "the upload was interrupted: {e}"),
            Self::Disk(e) => write!(f, "the server could not store the file: {e}"),
        }
    }
}

/// Write `body` to a new staging file and return its path, for
/// [`SiteStore::add_upload_file`]. The file is removed again on any failure.
pub async fn stage(store: &SiteStore, body: Body) -> Result<PathBuf, StageError> {
    let path = store
        .upload_staging_path()
        .map_err(|e| StageError::Disk(std::io::Error::other(e.to_string())))?;
    let error = match write(&path, body).await {
        Ok(0) => StageError::Empty,
        Ok(_) => return Ok(path),
        Err(e) => e,
    };
    let _ = std::fs::remove_file(&path);
    Err(error)
}

/// Append `body` to the part file at `path`, which must currently be `offset` bytes
/// long (zero when it does not exist yet). Returns the new length, or the current length
/// as `Err(Ok(len))` when `offset` does not match, so the client can tell.
pub async fn append_part(
    path: &std::path::Path,
    offset: u64,
    body: Body,
) -> Result<u64, Result<u64, StageError>> {
    let current = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(Err(StageError::Disk(e))),
    };
    if current != offset {
        return Err(Ok(current));
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| Err(StageError::Disk(e)))?;
    match copy_body(file, body).await {
        Ok(added) => Ok(current + added),
        Err(e) => {
            // Drop a partly written part, so the client can send it again at `offset`.
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                let _ = f.set_len(current);
            }
            Err(Err(e))
        }
    }
}

async fn write(path: &PathBuf, body: Body) -> Result<u64, StageError> {
    let file = tokio::fs::File::create(path)
        .await
        .map_err(StageError::Disk)?;
    copy_body(file, body).await
}

async fn copy_body(file: tokio::fs::File, body: Body) -> Result<u64, StageError> {
    let mut file = tokio::io::BufWriter::new(file);
    let mut stream = body.into_data_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| StageError::Interrupted(e.to_string()))?;
        total += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(StageError::Disk)?;
    }
    file.flush().await.map_err(StageError::Disk)?;
    Ok(total)
}
