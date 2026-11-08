use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Timeout,
    InvalidHost(String),
    InsecureRefused,
    Protocol(String),
    Server(String),
    Auth(String),
    Db(rusqlite::Error),
    Json(serde_json::Error),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "network error: {e}"),
            Error::Timeout => f.write_str("the server took too long to respond"),
            Error::InvalidHost(h) => write!(f, "invalid host name: {h}"),
            Error::InsecureRefused => f.write_str(
                "refusing to use an unencrypted connection; choose TLS or STARTTLS, or explicitly allow insecure connections",
            ),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Server(m) => write!(f, "server error: {m}"),
            Error::Auth(m) => write!(f, "authentication failed: {m}"),
            Error::Db(e) => write!(f, "database error: {e}"),
            Error::Json(e) => write!(f, "serialization error: {e}"),
            Error::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Db(e) => Some(e),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
