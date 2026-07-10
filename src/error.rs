use std::fmt;

#[derive(Debug)]
pub enum Error {
    Msg(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Toml(String),
    Http(u16, serde_json::Value),
    Auth(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Msg(s) => write!(f, "{s}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Toml(s) => write!(f, "toml: {s}"),
            Error::Http(code, body) => write!(f, "upstream {code}: {body}"),
            Error::Auth(s) => write!(f, "auth: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Msg(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn anthropic_error(status: u16, err_type: &str, message: &str) -> (u16, serde_json::Value) {
    (
        status,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": err_type,
                "message": message,
            }
        }),
    )
}
