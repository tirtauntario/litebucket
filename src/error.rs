//! Internal error type shared by storage, metadata, and operational code.
//!
//! Protocol-facing errors live in `s3::error`; these convert into them.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("storage integrity failure: {0}")]
    Integrity(String),
    #[error("service overloaded: {0}")]
    Overloaded(&'static str),
    #[error("the data directory is owned by another process")]
    Locked,
    #[error("metadata commit outcome is unknown")]
    CommitUncertain,
    #[error("mutations are halted pending recovery: {0}")]
    Halted(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn integrity(msg: impl Into<String>) -> Self {
        Self::Integrity(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// True for out-of-space conditions that should surface as retryable capacity errors.
    pub fn is_no_space(&self) -> bool {
        match self {
            Self::Io(e) => matches!(
                e.raw_os_error(),
                Some(code) if code == libc_enospc() || code == libc_edquot()
            ),
            _ => false,
        }
    }
}

fn libc_enospc() -> i32 {
    rustix::io::Errno::NOSPC.raw_os_error()
}

fn libc_edquot() -> i32 {
    rustix::io::Errno::DQUOT.raw_os_error()
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
