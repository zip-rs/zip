//! Types that specify what is contained in a ZIP.

#[cfg(doc)]
use crate::read::ZipFile;

use crate::write::{FileOptions, ZipRawValues};

use cfg_if::cfg_if;
use num_enum::{FromPrimitive, IntoPrimitive};

use std::convert::TryInto;
use std::ops::{Range, RangeInclusive};
use std::path;

mod ffi {
    pub const S_IFDIR: u32 = 0o0040000;
    pub const S_IFREG: u32 = 0o0100000;
}

cfg_if! {
    if #[cfg(any(
        all(target_arch = "arm", target_pointer_width = "32"),
        target_arch = "mips",
        target_arch = "powerpc"
    ))] {
        mod atomic {
            use crossbeam_utils::sync::ShardedLock;
            pub use std::sync::atomic::Ordering;

            #[derive(Debug, Default)]
            pub struct AtomicU64 {
                value: ShardedLock<u64>,
            }

            impl AtomicU64 {
                pub fn new(v: u64) -> Self {
                    Self {
                        value: ShardedLock::new(v),
                    }
                }
                pub fn get_mut(&mut self) -> &mut u64 {
                    self.value.get_mut().unwrap()
                }
                pub fn load(&self, _: Ordering) -> u64 {
                    *self.value.read().unwrap()
                }
                pub fn store(&self, value: u64, _: Ordering) {
                    *self.value.write().unwrap() = value;
                }
            }
        }
    } else {
        use std::sync::atomic;
    }

}

cfg_if! {
    if #[cfg(feature = "time")] {
        use crate::result::DateTimeRangeError;
        use time::{
            Date, Month, OffsetDateTime, UtcOffset, PrimitiveDateTime, Time,
            error::ComponentRange,
        };
    } else {
        use std::time::SystemTime;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum System {
    Dos = 0,
    Unix = 3,
    #[num_enum(default)]
    Unknown,
}

/// Representation of a moment in time.
///
/// Zip files use an old format from DOS to store timestamps,
/// with its own set of peculiarities.
/// For example, it has a resolution of 2 seconds!
///
/// A [`DateTime`] can be stored directly in a zipfile with [`FileOptions::last_modified_time`],
/// or read from one with [`ZipFile::last_modified`]
///
/// # Warning
///
/// Because there is no timezone associated with the [`DateTime`], they should ideally only
/// be used for user-facing descriptions. This also means [`DateTime::to_time`] returns an
/// [`OffsetDateTime`] (which is the equivalent of chrono's `NaiveDateTime`).
///
/// Modern zip files store more precise timestamps, which are ignored by [`crate::read::ZipArchive`],
/// so keep in mind that these timestamps are unreliable. [We're working on this](https://github.com/zip-rs/zip/issues/156#issuecomment-652981904).
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl ::std::default::Default for DateTime {
    fn default() -> DateTime {
        Self::zero()
    }
}

impl DateTime {
    /// Constructs a 'default' datetime of 1980-01-01 00:00:00
    pub const fn zero() -> Self {
        Self {
            year: 1980,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }

    /// The allowed range for years in a zip file's timestamp.
    pub const YEAR_RANGE: RangeInclusive<u16> = 1980..=2107;
    /// The allowed range for months in a zip file's timestamp.
    pub const MONTH_RANGE: RangeInclusive<u8> = 1..=12;
    /// The allowed range for days in a zip file's timestamp.
    pub const DAY_RANGE: RangeInclusive<u8> = 1..=31;
    /// The allowed range for hours in a zip file's timestamp.
    pub const HOUR_RANGE: Range<u8> = 0..24;
    /// The allowed range for minutes in a zip file's timestamp.
    pub const MINUTE_RANGE: Range<u8> = 0..60;
    /// The allowed range for seconds in a zip file's timestamp.
    pub const SECOND_RANGE: RangeInclusive<u8> = 0..=60;

    fn check_year(year: u16) -> Result<(), DateTimeRangeError> {
        if Self::YEAR_RANGE.contains(&year) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidYear(year, Self::YEAR_RANGE))
        }
    }

    fn check_month(month: u8) -> Result<(), DateTimeRangeError> {
        if Self::MONTH_RANGE.contains(&month) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidMonth(month, Self::MONTH_RANGE))
        }
    }

    fn check_day(day: u8) -> Result<(), DateTimeRangeError> {
        if Self::DAY_RANGE.contains(&day) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidDay(day, Self::DAY_RANGE))
        }
    }

    fn check_hour(hour: u8) -> Result<(), DateTimeRangeError> {
        if Self::HOUR_RANGE.contains(&hour) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidHour(hour, Self::HOUR_RANGE))
        }
    }

    fn check_minute(minute: u8) -> Result<(), DateTimeRangeError> {
        if Self::MINUTE_RANGE.contains(&minute) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidMinute(
                minute,
                Self::MINUTE_RANGE,
            ))
        }
    }

    fn check_second(second: u8) -> Result<(), DateTimeRangeError> {
        if Self::SECOND_RANGE.contains(&second) {
            Ok(())
        } else {
            Err(DateTimeRangeError::InvalidSecond(
                second,
                Self::SECOND_RANGE,
            ))
        }
    }

    /// Converts an msdos (u16, u16) pair to a DateTime object
    pub fn from_msdos(datepart: u16, timepart: u16) -> DateTime {
        let seconds = (timepart & 0b0000000000011111) << 1;
        let minutes = (timepart & 0b0000011111100000) >> 5;
        let hours = (timepart & 0b1111100000000000) >> 11;
        let days = datepart & 0b0000000000011111;
        let months = (datepart & 0b0000000111100000) >> 5;
        let years = (datepart & 0b1111111000000000) >> 9;

        DateTime {
            year: years + 1980,
            month: months as u8,
            day: days as u8,
            hour: hours as u8,
            minute: minutes as u8,
            second: seconds as u8,
        }
    }

    /// Constructs a DateTime from a specific date and time
    ///
    /// The bounds are:
    /// * year: [1980, 2107]
    /// * month: [1, 12]
    /// * day: [1, 31]
    /// * hour: [0, 23]
    /// * minute: [0, 59]
    /// * second: [0, 60]
    #[allow(clippy::result_unit_err)]
    #[deprecated(note = "use DateTime::parse_from_date_and_time() instead")]
    pub fn from_date_and_time(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<DateTime, ()> {
        Self::parse_from_date_and_time(year, month, day, hour, minute, second).map_err(|_| ())
    }

    /// Constructs a DateTime from a specific date and time
    ///
    /// The bounds are:
    /// * year ([`Self::YEAR_RANGE`]): [1980, 2107]
    /// * month ([`Self::MONTH_RANGE`]): [1, 12]
    /// * day ([`Self::DAY_RANGE`]): [1, 31]
    /// * hour ([`Self::HOUR_RANGE`]): [0, 23]
    /// * minute ([`Self::MINUTE_RANGE`]): [0, 59]
    /// * second ([`Self::SECOND_RANGE`]): [0, 60]
    ///
    ///```
    /// use zip::{DateTime, result::DateTimeRangeError};
    ///
    /// assert!(DateTime::parse_from_date_and_time(1980, 1, 1, 0, 0, 0).is_ok());
    /// assert!(matches![
    ///   DateTime::parse_from_date_and_time(1979, 1, 1, 0, 0, 0),
    ///   Err(DateTimeRangeError::InvalidYear(1979, _)),
    /// ]);
    ///```
    pub fn parse_from_date_and_time(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<DateTime, DateTimeRangeError> {
        Self::check_year(year)?;
        Self::check_month(month)?;
        Self::check_day(day)?;
        Self::check_hour(hour)?;
        Self::check_minute(minute)?;
        Self::check_second(second)?;
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    /// Converts a OffsetDateTime object to a DateTime
    ///
    /// Returns `Err` when this object is out of bounds
    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    #[allow(clippy::result_unit_err)]
    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    #[deprecated(note = "use `DateTime::try_from()`")]
    pub fn from_time(dt: OffsetDateTime) -> Result<DateTime, ()> {
        dt.try_into().map_err(|_err| ())
    }

    /// Gets the time portion of this datetime in the msdos representation
    pub fn timepart(&self) -> u16 {
        ((self.second as u16) >> 1) | ((self.minute as u16) << 5) | ((self.hour as u16) << 11)
    }

    /// Gets the date portion of this datetime in the msdos representation
    pub fn datepart(&self) -> u16 {
        (self.day as u16) | ((self.month as u16) << 5) | ((self.year - 1980) << 9)
    }

    /// Converts the DateTime to a OffsetDateTime structure
    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    #[deprecated(note = "use `DateTime::to_time_with_offset()`")]
    pub fn to_time(&self) -> Result<OffsetDateTime, ComponentRange> {
        self.to_time_with_offset(UtcOffset::UTC)
    }

    /// Converts the DateTime to a OffsetDateTime structure, given a UTC offset.
    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn to_time_with_offset(&self, offset: UtcOffset) -> Result<OffsetDateTime, ComponentRange> {
        let date =
            Date::from_calendar_date(self.year as i32, Month::try_from(self.month)?, self.day)?;
        let time = Time::from_hms(self.hour, self.minute, self.second)?;
        Ok(PrimitiveDateTime::new(date, time).assume_offset(offset))
    }

    /// Get the year. There is no epoch, i.e. 2018 will be returned as 2018.
    pub fn year(&self) -> u16 {
        self.year
    }

    /// Get the month, where 1 = january and 12 = december
    ///
    /// # Warning
    ///
    /// When read from a zip file, this may not be a reasonable value
    pub fn month(&self) -> u8 {
        self.month
    }

    /// Get the day
    ///
    /// # Warning
    ///
    /// When read from a zip file, this may not be a reasonable value
    pub fn day(&self) -> u8 {
        self.day
    }

    /// Get the hour
    ///
    /// # Warning
    ///
    /// When read from a zip file, this may not be a reasonable value
    pub fn hour(&self) -> u8 {
        self.hour
    }

    /// Get the minute
    ///
    /// # Warning
    ///
    /// When read from a zip file, this may not be a reasonable value
    pub fn minute(&self) -> u8 {
        self.minute
    }

    /// Get the second
    ///
    /// # Warning
    ///
    /// When read from a zip file, this may not be a reasonable value
    pub fn second(&self) -> u8 {
        self.second
    }
}

#[cfg(feature = "time")]
#[cfg_attr(docsrs, doc(cfg(feature = "time")))]
impl TryFrom<OffsetDateTime> for DateTime {
    type Error = DateTimeRangeError;

    fn try_from(dt: OffsetDateTime) -> Result<Self, Self::Error> {
        let year: u16 = dt
            .year()
            .try_into()
            .map_err(|e| DateTimeRangeError::NumericConversion("year", e))?;
        let month: u8 = dt.month().into();
        let day: u8 = dt.day().into();
        let hour: u8 = dt.hour().into();
        let minute: u8 = dt.minute().into();
        let second: u8 = dt.second().into();

        Self::parse_from_date_and_time(year, month, day, hour, minute, second)
    }
}

pub const DEFAULT_VERSION: u8 = 46;

/// A type like `AtomicU64` except it implements `Clone` and has predefined
/// ordering.
///
/// It uses `Relaxed` ordering because it is not used for synchronisation.
#[derive(Debug)]
pub struct AtomicU64(atomic::AtomicU64);

impl AtomicU64 {
    pub fn new(v: u64) -> Self {
        Self(atomic::AtomicU64::new(v))
    }

    pub fn load(&self) -> u64 {
        self.0.load(atomic::Ordering::Relaxed)
    }

    pub fn store(&self, val: u64) {
        self.0.store(val, atomic::Ordering::Relaxed)
    }

    pub fn get_mut(&mut self) -> &mut u64 {
        self.0.get_mut()
    }
}

impl Clone for AtomicU64 {
    fn clone(&self) -> Self {
        Self(atomic::AtomicU64::new(self.load()))
    }
}

/// Structure representing a ZIP file.
#[derive(Debug, Clone)]
pub struct ZipFileData {
    /// Compatibility of the file attribute information
    pub system: System,
    /// Specification version
    pub version_made_by: u8,
    /// True if the file is encrypted.
    pub encrypted: bool,
    /// True if the file uses a data-descriptor section
    pub using_data_descriptor: bool,
    /// Compression method used to store the file
    pub compression_method: crate::compression::CompressionMethod,
    /// Compression level to store the file
    pub compression_level: Option<i32>,
    /// Last modified time. This will only have a 2 second precision.
    pub last_modified_time: DateTime,
    /// CRC32 checksum
    pub crc32: u32,
    /// Size of the file in the ZIP
    pub compressed_size: u64,
    /// Size of the file when extracted
    pub uncompressed_size: u64,
    /// Name of the file
    pub file_name: String,
    /// Raw file name. To be used when file_name was incorrectly decoded.
    pub file_name_raw: Vec<u8>,
    /// Extra field usually used for storage expansion
    pub extra_field: Vec<u8>,
    /// File comment
    pub file_comment: String,
    /// Specifies where the local header of the file starts
    pub header_start: u64,
    /// Specifies where the central header of the file starts
    ///
    /// Note that when this is not known, it is set to 0
    pub central_header_start: u64,
    /// Specifies where the compressed data of the file starts
    pub data_start: AtomicU64,
    /// External file attributes
    pub external_attributes: u32,
    /// Reserve local ZIP64 extra field
    pub large_file: bool,
    /// AES mode if applicable
    pub aes_mode: Option<(AesMode, AesVendorVersion)>,
}

impl ZipFileData {
    pub(crate) fn initialize(
        raw: ZipRawValues,
        options: FileOptions,
        header_start: u64,
        file_name: String,
    ) -> Self {
        let permissions = options.permissions.unwrap_or(0o100644);
        Self {
            system: System::Unix,
            version_made_by: DEFAULT_VERSION,
            encrypted: options.encrypt_with.is_some(),
            using_data_descriptor: false,
            compression_method: options.compression_method,
            compression_level: options.compression_level,
            last_modified_time: options.last_modified_time,
            crc32: raw.crc32,
            compressed_size: raw.compressed_size,
            uncompressed_size: raw.uncompressed_size,
            file_name,
            file_name_raw: Vec::new(), // Never used for saving
            extra_field: Vec::new(),
            file_comment: String::new(),
            header_start,
            data_start: AtomicU64::new(0),
            central_header_start: 0,
            external_attributes: permissions << 16,
            large_file: options.large_file,
            aes_mode: None,
        }
    }

    pub fn file_name_sanitized(&self) -> ::std::path::PathBuf {
        let no_null_filename = match self.file_name.find('\0') {
            Some(index) => &self.file_name[0..index],
            None => &self.file_name,
        }
        .to_string();

        // zip files can contain both / and \ as separators regardless of the OS
        // and as we want to return a sanitized PathBuf that only supports the
        // OS separator let's convert incompatible separators to compatible ones
        let separator = ::std::path::MAIN_SEPARATOR;
        let opposite_separator = match separator {
            '/' => '\\',
            _ => '/',
        };
        let filename =
            no_null_filename.replace(&opposite_separator.to_string(), &separator.to_string());

        ::std::path::Path::new(&filename)
            .components()
            .filter(|component| matches!(*component, ::std::path::Component::Normal(..)))
            .fold(::std::path::PathBuf::new(), |mut path, ref cur| {
                path.push(cur.as_os_str());
                path
            })
    }

    pub(crate) fn enclosed_name(&self) -> Option<&path::Path> {
        if self.file_name.contains('\0') {
            return None;
        }
        let path = path::Path::new(&self.file_name);
        let mut depth = 0usize;
        for component in path.components() {
            match component {
                path::Component::Prefix(_) | path::Component::RootDir => return None,
                path::Component::ParentDir => depth = depth.checked_sub(1)?,
                path::Component::Normal(_) => depth += 1,
                path::Component::CurDir => (),
            }
        }
        Some(path)
    }

    /// Get unix mode for the file
    #[inline]
    pub(crate) fn unix_mode(&self) -> Option<u32> {
        if self.external_attributes == 0 {
            return None;
        }

        match self.system {
            System::Unix => Some(self.external_attributes >> 16),
            System::Dos => {
                // Interpret MS-DOS directory bit
                let mut mode = if 0x10 == (self.external_attributes & 0x10) {
                    ffi::S_IFDIR | 0o0775
                } else {
                    ffi::S_IFREG | 0o0664
                };
                if 0x01 == (self.external_attributes & 0x01) {
                    // Read-only bit; strip write permissions
                    mode &= 0o0555;
                }
                Some(mode)
            }
            _ => None,
        }
    }

    pub fn zip64_extension(&self) -> bool {
        self.uncompressed_size > 0xFFFFFFFF
            || self.compressed_size > 0xFFFFFFFF
            || self.header_start > 0xFFFFFFFF
    }

    pub fn version_needed(&self) -> u16 {
        // higher versions matched first
        match (self.zip64_extension(), self.compression_method) {
            #[cfg(feature = "bzip2")]
            (_, crate::compression::CompressionMethod::Bzip2) => 46,
            (true, _) => 45,
            _ => 20,
        }
    }
}

/// The encryption specification used to encrypt a file with AES.
///
/// According to the [specification](https://www.winzip.com/win/en/aes_info.html#winzip11) AE-2
/// does not make use of the CRC check.
#[derive(Copy, Clone, Debug)]
pub enum AesVendorVersion {
    Ae1,
    Ae2,
}

/// AES variant used.
#[derive(Copy, Clone, Debug)]
pub enum AesMode {
    Aes128,
    Aes192,
    Aes256,
}

#[cfg(feature = "aes-crypto")]
#[cfg_attr(docsrs, doc(cfg(feature = "aes-crypto")))]
impl AesMode {
    pub fn salt_length(&self) -> usize {
        self.key_length() / 2
    }

    pub fn key_length(&self) -> usize {
        match self {
            Self::Aes128 => 16,
            Self::Aes192 => 24,
            Self::Aes256 => 32,
        }
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod test {
    #[test]
    fn system() {
        use super::System;
        assert_eq!(u8::from(System::Dos), 0u8);
        assert_eq!(System::Dos as u8, 0u8);
        assert_eq!(System::Unix as u8, 3u8);
        assert_eq!(u8::from(System::Unix), 3u8);
        assert_eq!(System::from(0), System::Dos);
        assert_eq!(System::from(3), System::Unix);
        assert_eq!(u8::from(System::Unknown), 4u8);
        assert_eq!(System::Unknown as u8, 4u8);
    }

    #[test]
    fn sanitize() {
        use super::*;
        let file_name = "/path/../../../../etc/./passwd\0/etc/shadow".to_string();
        let data = ZipFileData {
            system: System::Dos,
            version_made_by: 0,
            encrypted: false,
            using_data_descriptor: false,
            compression_method: crate::compression::CompressionMethod::Stored,
            compression_level: None,
            last_modified_time: DateTime::default(),
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            file_name: file_name.clone(),
            file_name_raw: file_name.into_bytes(),
            extra_field: Vec::new(),
            file_comment: String::new(),
            header_start: 0,
            data_start: AtomicU64::new(0),
            central_header_start: 0,
            external_attributes: 0,
            large_file: false,
            aes_mode: None,
        };
        assert_eq!(
            data.file_name_sanitized(),
            ::std::path::PathBuf::from("path/etc/passwd")
        );
    }

    #[test]
    #[allow(clippy::unusual_byte_groupings)]
    fn datetime_default() {
        use super::DateTime;
        let dt = DateTime::default();
        assert_eq!(dt.timepart(), 0);
        assert_eq!(dt.datepart(), 0b0000000_0001_00001);
    }

    #[test]
    #[allow(clippy::unusual_byte_groupings)]
    fn datetime_max() {
        use super::DateTime;
        let dt = DateTime::parse_from_date_and_time(2107, 12, 31, 23, 59, 60).unwrap();
        assert_eq!(dt.timepart(), 0b10111_111011_11110);
        assert_eq!(dt.datepart(), 0b1111111_1100_11111);
    }

    #[test]
    fn datetime_bounds() {
        use super::DateTime;

        assert!(DateTime::parse_from_date_and_time(2000, 1, 1, 23, 59, 60).is_ok());
        assert!(DateTime::parse_from_date_and_time(2000, 1, 1, 24, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(2000, 1, 1, 0, 60, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(2000, 1, 1, 0, 0, 61).is_err());

        assert!(DateTime::parse_from_date_and_time(2107, 12, 31, 0, 0, 0).is_ok());
        assert!(DateTime::parse_from_date_and_time(1980, 1, 1, 0, 0, 0).is_ok());
        assert!(DateTime::parse_from_date_and_time(1979, 1, 1, 0, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(1980, 0, 1, 0, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(1980, 1, 0, 0, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(2108, 12, 31, 0, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(2107, 13, 31, 0, 0, 0).is_err());
        assert!(DateTime::parse_from_date_and_time(2107, 12, 32, 0, 0, 0).is_err());
    }

    #[cfg(feature = "time")]
    use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

    #[cfg(feature = "time")]
    #[test]
    fn datetime_try_from_bounds() {
        use std::convert::TryFrom;

        use super::DateTime;
        use time::macros::datetime;

        // 1979-12-31 23:59:59
        assert!(DateTime::try_from(datetime!(1979-12-31 23:59:59 UTC)).is_err());

        // 1980-01-01 00:00:00
        assert!(DateTime::try_from(datetime!(1980-01-01 00:00:00 UTC)).is_ok());

        // 2107-12-31 23:59:59
        assert!(DateTime::try_from(datetime!(2107-12-31 23:59:59 UTC)).is_ok());

        // 2108-01-01 00:00:00
        assert!(DateTime::try_from(datetime!(2108-01-01 00:00:00 UTC)).is_err());
    }

    #[test]
    fn time_conversion() {
        use super::DateTime;
        let dt = DateTime::from_msdos(0x4D71, 0x54CF);
        assert_eq!(dt.year(), 2018);
        assert_eq!(dt.month(), 11);
        assert_eq!(dt.day(), 17);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 38);
        assert_eq!(dt.second(), 30);

        #[cfg(feature = "time")]
        {
            let offset_time = dt.to_time_with_offset(UtcOffset::UTC).unwrap();
            assert_eq!(dt.to_time().unwrap(), offset_time);
            assert_eq!(
                offset_time.format(&Rfc3339).unwrap(),
                "2018-11-17T10:38:30Z"
            );
        }
    }

    #[test]
    fn time_out_of_bounds() {
        use super::DateTime;
        let dt = DateTime::from_msdos(0xFFFF, 0xFFFF);
        assert_eq!(dt.year(), 2107);
        assert_eq!(dt.month(), 15);
        assert_eq!(dt.day(), 31);
        assert_eq!(dt.hour(), 31);
        assert_eq!(dt.minute(), 63);
        assert_eq!(dt.second(), 62);

        #[cfg(feature = "time")]
        {
            assert!(dt.to_time().is_err());
            assert!(dt.to_time_with_offset(UtcOffset::UTC).is_err());
        }

        let dt = DateTime::from_msdos(0x0000, 0x0000);
        assert_eq!(dt.year(), 1980);
        assert_eq!(dt.month(), 0);
        assert_eq!(dt.day(), 0);
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 0);

        #[cfg(feature = "time")]
        {
            assert!(dt.to_time().is_err());
            assert!(dt.to_time_with_offset(UtcOffset::UTC).is_err());
        }
    }

    #[cfg(feature = "time")]
    #[test]
    fn time_at_january() {
        use super::DateTime;
        use std::convert::TryFrom;

        // 2020-01-01 00:00:00
        let clock = OffsetDateTime::from_unix_timestamp(1_577_836_800).unwrap();

        assert!(DateTime::try_from(clock).is_ok());
    }
}
