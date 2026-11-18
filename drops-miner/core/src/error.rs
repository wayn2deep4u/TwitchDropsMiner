//! The engine's error type.

use std::fmt;

#[derive(Debug, Clone)]
pub enum Error {
    /// Couldn't reach Twitch or got an unexpected answer.
    Http(String),
    /// Twitch's GQL API answered with an error.
    Gql(String),
    /// The saved sign-in no longer works.
    Auth(String),
    /// The engine is shutting down.
    Exit,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(m) => write!(f, "Connection problem: {m}"),
            Error::Gql(m) => write!(f, "Twitch API error: {m}"),
            Error::Auth(m) => write!(f, "Sign-in problem: {m}"),
            Error::Exit => write!(f, "Exiting"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
