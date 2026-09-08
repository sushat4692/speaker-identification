//! Shared error type for `asr-core`.
//!
//! Kept UI-agnostic on purpose: this crate must not know about GPUI, HTTP
//! frameworks, or any other presentation layer. See the crate-level docs.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("failed to open media file: {0}")]
    Io(#[from] std::io::Error),

    #[error("no supported audio track found in this file")]
    NoAudioTrack,

    #[error("unsupported audio codec")]
    UnsupportedCodec,

    #[error("audio decoding error: {0}")]
    Decode(String),

    #[error("resampling error: {0}")]
    Resample(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
