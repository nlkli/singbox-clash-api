//! Error type used by every fallible operation in this crate.

use std::error::Error as StdError;
use std::fmt;

/// A specialized [`Result`](std::result::Result) for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while talking to the sing-box controller.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An argument was rejected locally, before any request was sent.
    InvalidInput(String),
    /// Connecting, sending, or receiving failed.
    Transport(Box<dyn StdError + Send + Sync>),
    /// The client-side request timeout elapsed.
    Timeout,
    /// The server answered with a non-success status.
    Api {
        /// HTTP status code.
        status: u16,
        /// The `message` field of the JSON error body, the raw body text,
        /// or the canonical status reason, in that order of preference.
        message: String,
    },
    /// A response body could not be decoded.
    Decode(serde_json::Error),
}

impl Error {
    /// The HTTP status code for [`Error::Api`], otherwise `None`.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// `true` for a `401 Unauthorized` answer (missing or wrong secret).
    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(401)
    }

    /// `true` for a `404 Not Found` answer (unknown proxy or group).
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidInput(msg.into())
    }

    pub(crate) fn transport(err: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Error::Transport(err.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Error::Transport(err) => write!(f, "transport error: {err}"),
            Error::Timeout => f.write_str("request timed out"),
            Error::Api { status, message } => write!(f, "API error {status}: {message}"),
            Error::Decode(err) => write!(f, "cannot decode response: {err}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Error::Transport(err) => Some(err.as_ref()),
            Error::Decode(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Decode(err)
    }
}
