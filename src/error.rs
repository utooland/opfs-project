use std::fmt;

/// Result type alias for opfs-project operations
pub type Result<T> = std::result::Result<T, OpfsError>;

/// Tri-state integrity verification result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    /// Hash matched successfully
    Verified,
    /// Hash did not match
    Failed,
    /// No hash information was available to verify against
    NoHashAvailable,
}

impl VerifyResult {
    /// Returns `true` only if the hash was verified successfully
    pub fn is_verified(self) -> bool {
        self == Self::Verified
    }

    /// Returns `true` if the hash was present and did not match
    pub fn is_failed(self) -> bool {
        self == Self::Failed
    }
}

/// Crate-level error type for opfs-project
#[derive(Debug)]
pub enum OpfsError {
    /// Underlying I/O error
    Io(std::io::Error),
    /// HTTP request returned a non-success status code
    Http { status: u16, url: String },
    /// Network / transport error from reqwest
    Network(reqwest::Error),
    /// Integrity check (sha512/sha1) failed after download
    IntegrityFailed { package: String, version: String },
    /// A `RwLock` was poisoned
    LockPoisoned,
    /// File or entry not found
    NotFound(String),
    /// Attempted to read a directory as a file
    IsADirectory(String),
    /// Generic error with message
    Other(String),
}

impl fmt::Display for OpfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Http { status, url } => write!(f, "HTTP {status} for {url}"),
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::IntegrityFailed { package, version } => {
                write!(f, "{package}@{version}: integrity check failed")
            }
            Self::LockPoisoned => write!(f, "cache lock poisoned"),
            Self::NotFound(path) => write!(f, "not found: {path}"),
            Self::IsADirectory(path) => write!(f, "is a directory: {path}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for OpfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Network(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OpfsError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<reqwest::Error> for OpfsError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e)
    }
}

impl From<OpfsError> for std::io::Error {
    fn from(e: OpfsError) -> Self {
        match e {
            OpfsError::Io(io) => io,
            OpfsError::NotFound(msg) => std::io::Error::new(std::io::ErrorKind::NotFound, msg),
            OpfsError::IsADirectory(msg) => {
                std::io::Error::new(std::io::ErrorKind::IsADirectory, msg)
            }
            other => std::io::Error::other(other.to_string()),
        }
    }
}
