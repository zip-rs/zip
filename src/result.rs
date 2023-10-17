//! Error types that can be emitted from this library

use displaydoc::Display;
use thiserror::Error;

use std::error::Error;
use std::fmt;
use std::io;
use std::num::TryFromIntError;
use std::ops::{Range, RangeInclusive};

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
#[derive(Debug, Display, Error)]
pub enum DateTimeRangeError {
    /// year {0} was not in range {1:?}
    InvalidYear(u16, RangeInclusive<u16>),
    /// month {0} was not in range {1:?}
    InvalidMonth(u8, RangeInclusive<u8>),
    /// day {0} was not in range {1:?}
    InvalidDay(u8, RangeInclusive<u8>),
    /// hour {0} was not in range {1:?}
    InvalidHour(u8, Range<u8>),
    /// minute {0} was not in range {1:?}
    InvalidMinute(u8, Range<u8>),
    /// second {0} was not in range {1:?}
    InvalidSecond(u8, RangeInclusive<u8>),
    /// failed to convert {0}: {1}
    NumericConversion(&'static str, #[source] TryFromIntError),
}
