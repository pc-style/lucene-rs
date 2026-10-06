use std::fmt;

/// Errors returned by indexing and searching.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// An index file is malformed or fails its checksum.
    Corrupt(String),
    /// The caller asked for something inconsistent (e.g. a field indexed two different ways).
    IllegalArgument(String),
    /// Another `IndexWriter` holds the index's write lock.
    LockObtainFailed(String),
    /// The query string could not be parsed.
    QueryParse(String),
    /// No commit exists in the directory.
    IndexNotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Corrupt(m) => write!(f, "corrupt index: {m}"),
            Self::IllegalArgument(m) => write!(f, "illegal argument: {m}"),
            Self::LockObtainFailed(m) => write!(f, "lock obtain failed: {m}"),
            Self::QueryParse(m) => write!(f, "cannot parse query: {m}"),
            Self::IndexNotFound(m) => write!(f, "no index found: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}
