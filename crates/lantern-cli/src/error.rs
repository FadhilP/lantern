use std::io;
use std::path::Path;

/// The class of an [`Error`]. It decides the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Refresh or query failure: unreadable rule, file, or Git index, or an exceeded limit.
    Failure,
    /// Bad flag, empty query, invalid regex, or a path that escapes the workspace.
    Usage,
    /// The root moved outside the indexed directory.
    RootMoved,
    /// The cursor belongs to an older state of the index.
    StaleCursor,
    /// Content kept changing, or the root lock timed out. A retry can succeed.
    Temporary,
}

impl ErrorKind {
    /// The process exit code for this kind.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Failure => 1,
            Self::Usage => 2,
            Self::RootMoved => 3,
            Self::StaleCursor => 4,
            Self::Temporary => 75,
        }
    }

    /// The stable code string that error documents carry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Usage => "usage",
            Self::RootMoved => "root_moved",
            Self::StaleCursor => "stale_cursor",
            Self::Temporary => "temporary",
        }
    }
}

/// An error from any Lantern operation, with the [`ErrorKind`] that decides its exit code.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
#[must_use]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// An error of `kind` with a lowercase message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub(crate) fn failure(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failure, message)
    }

    pub(crate) fn temporary(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Temporary, message)
    }

    pub(crate) fn io(path: &Path, error: &io::Error) -> Self {
        Self::failure(format!("{}: {error}", path.display()))
    }

    /// The class of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        let message = format!("database error: {error}");
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::temporary(message)
            }
            _ => Self::failure(message),
        }
    }
}

/// The result type of every Lantern operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;
