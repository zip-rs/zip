//! Error types that can be emitted from this library

use displaydoc::Display;
use thiserror::Error;

use std::error::Error;
use std::fmt;
use std::io;

/// Generic result type with ZipError as its error variant
pub type ZipResult<T> = Result<T, ZipError>;

/// The given password is wrong
#[derive(Debug)]
pub struct InvalidPassword;

impl fmt::Display for InvalidPassword {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "invalid password for file in archive")
    }
}

impl Error for InvalidPassword {}

/// Error type for Zip
#[derive(Debug, Display, Error)]
pub enum ZipError {
    /// i/o error: {0}
    Io(#[from] io::Error),

    /// invalid Zip archive: {0}
    InvalidArchive(&'static str),

    /// unsupported Zip archive: {0}
    UnsupportedArchive(&'static str),

    /// specified file not found in archive
    FileNotFound,
}

impl ZipError {
    /// The text used as an error when a password is required and not supplied
    ///
    /// ```rust,no_run
    /// # use zip::result::ZipError;
    /// # let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&[])).unwrap();
    /// match archive.by_index(1) {
    ///     Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) => eprintln!("a password is needed to unzip this file"),
    ///     _ => (),
    /// }
    /// # ()
    /// ```
    pub const PASSWORD_REQUIRED: &'static str = "Password required to decrypt file";
}

impl From<ZipError> for io::Error {
    fn from(err: ZipError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, err)
    }
}

/// Error type for time parsing
#[derive(Debug)]
pub struct DateTimeRangeError;

impl fmt::Display for DateTimeRangeError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "a date could not be represented within the bounds the MS-DOS date range (1980-2107)"
        )
    }
}

impl Error for DateTimeRangeError {}
