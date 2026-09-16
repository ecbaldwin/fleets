//! Library error type.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        msg: String,
    },
    UnsupportedSource {
        path: String,
        ext: String,
    },
    /// A `--host`/`--limit` referenced something that doesn't exist, etc.
    NotFound(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "reading {path}: {source}"),
            Error::Parse { path, msg } => write!(f, "parsing {path}: {msg}"),
            Error::UnsupportedSource { path, ext } => {
                write!(f, "unsupported inventory source {path} (extension {ext:?})")
            }
            Error::NotFound(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
