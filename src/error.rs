use failure::Fail;
use std::io;
use std::string::FromUtf8Error;

#[derive(Debug, Fail)]
pub enum KvsError {
    #[fail(display = "IO error: {}", _0)]
    Io(#[cause] io::Error),

    #[fail(display = "serde_json error: {}", _0)]
    Serde(#[cause] serde_json::Error),

    #[fail(display = "Key not found")]
    KeyNotFount,

    #[fail(display = "Unexpected command type")]
    UnexpectedCommandType,

    #[fail(display = "UTF-8 error: {}", _0)]
    Utf8(#[cause] FromUtf8Error),

    #[fail(display = "sled error: {}", _0)]
    Sled(#[cause] sled::Error),

    #[fail(display = "display = {}", _0)]
    StringError(String)
}

impl From<io::Error> for KvsError {
    fn from(error: io::Error) -> KvsError {
        KvsError::Io(error)
    }
}

impl From<serde_json::Error> for KvsError {
    fn from(error: serde_json::Error) -> KvsError {
        KvsError::Serde(error)
    }
}

impl  From<FromUtf8Error> for KvsError {
    fn from(error: FromUtf8Error) -> KvsError {
        KvsError::Utf8(error)
    }
}

impl From<sled::Error> for KvsError {
    fn from(error: sled::Error) -> KvsError {
        KvsError::Sled(error)
    }
}

pub type Result<T> = std::result::Result<T, KvsError>;