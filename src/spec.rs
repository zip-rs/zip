use crate::result::{ZipError, ZipResult};
#[cfg(doc)]
use crate::write::{FileOptions, ZipWriter};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io;
use std::io::prelude::*;

pub const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x04034b50;
pub const CENTRAL_DIRECTORY_HEADER_SIGNATURE: u32 = 0x02014b50;
const CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06054b50;
pub const ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06064b50;
const ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE: u32 = 0x07064b50;

/// The number of bytes necessary to allocate a zip64 record for an individual file.
///
/// If a file larger than this threshold attempts to be written, and [`FileOptions::large_file()`]
/// was not true, then [`ZipWriter`] will raise an [`io::Error`] with [`io::ErrorKind::Other`].
///
/// If the zip file itself is larger than this value, then a zip64 central directory record will be
/// written to the end of the file.
///
///```
/// # fn main() -> Result<(), zip::result::ZipError> {
/// use std::io::{self, Cursor, prelude::*};
/// use zip::{ZipWriter, write::FileOptions};
///
/// let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
/// // Writing an extremely large file for this test is faster without compression.
/// let options = FileOptions::default().compression_method(zip::CompressionMethod::Stored);
///
/// let big_len: usize = (zip::ZIP64_BYTES_THR as usize) + 1;
/// let big_buf = vec![0u8; big_len];
/// zip.start_file("zero.dat", options)?;
/// // This is too big!
/// assert!(zip.write_all(&big_buf[..]).is_err());
/// // Attempting to write anything further to the same zip will fail.
/// assert!(zip.start_file("one.dat", options).is_err());
///
/// // Create a new zip output.
/// let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
/// // This time, create a zip64 record for the file.
/// let options = options.large_file(true);
/// zip.start_file("zero.dat", options)?;
/// // This succeeds because we specified that it could be a large file.
/// assert!(zip.write_all(&big_buf[..]).is_ok());
/// # Ok(())
/// # }
///```
pub const ZIP64_BYTES_THR: u64 = u32::MAX as u64;
/// The number of entries within a single zip necessary to allocate a zip64 central
/// directory record.
///
/// If more than this number of entries is written to a [`ZipWriter`], then [`ZipWriter::finish()`]
/// will write out extra zip64 data to the end of the zip file.
pub const ZIP64_ENTRY_THR: usize = u16::MAX as usize;

pub struct CentralDirectoryEnd {
    pub disk_number: u16,
    pub disk_with_central_directory: u16,
    pub number_of_files_on_this_disk: u16,
    pub number_of_files: u16,
    pub central_directory_size: u32,
    pub central_directory_offset: u32,
    pub zip_file_comment: Vec<u8>,
}

impl CentralDirectoryEnd {
    // Per spec 4.4.1.4 - a CentralDirectoryEnd field might be insufficient to hold the
    // required data. In this case the file SHOULD contain a ZIP64 format record
    // and the field of this record will be set to -1
    pub(crate) fn record_too_small(&self) -> bool {
        self.disk_number == 0xFFFF
            || self.disk_with_central_directory == 0xFFFF
            || self.number_of_files_on_this_disk == 0xFFFF
            || self.number_of_files == 0xFFFF
            || self.central_directory_size == 0xFFFFFFFF
            || self.central_directory_offset == 0xFFFFFFFF
    }

    pub fn parse<T: Read>(reader: &mut T) -> ZipResult<CentralDirectoryEnd> {
        let magic = reader.read_u32::<LittleEndian>()?;
        if magic != CENTRAL_DIRECTORY_END_SIGNATURE {
            return Err(ZipError::InvalidArchive("Invalid digital signature header"));
        }
        let disk_number = reader.read_u16::<LittleEndian>()?;
        let disk_with_central_directory = reader.read_u16::<LittleEndian>()?;
        let number_of_files_on_this_disk = reader.read_u16::<LittleEndian>()?;
        let number_of_files = reader.read_u16::<LittleEndian>()?;
        let central_directory_size = reader.read_u32::<LittleEndian>()?;
        let central_directory_offset = reader.read_u32::<LittleEndian>()?;
        let zip_file_comment_length = reader.read_u16::<LittleEndian>()? as usize;
        let mut zip_file_comment = vec![0; zip_file_comment_length];
        reader.read_exact(&mut zip_file_comment)?;

        Ok(CentralDirectoryEnd {
            disk_number,
            disk_with_central_directory,
            number_of_files_on_this_disk,
            number_of_files,
            central_directory_size,
            central_directory_offset,
            zip_file_comment,
        })
    }

    pub fn find_and_parse<T: Read + io::Seek>(
        reader: &mut T,
    ) -> ZipResult<(CentralDirectoryEnd, u64)> {
        const HEADER_SIZE: u64 = 22;
        const BYTES_BETWEEN_MAGIC_AND_COMMENT_SIZE: u64 = HEADER_SIZE - 6;
        let file_length = reader.seek(io::SeekFrom::End(0))?;

        let search_upper_bound = file_length.saturating_sub(HEADER_SIZE + ::std::u16::MAX as u64);

        if file_length < HEADER_SIZE {
            return Err(ZipError::InvalidArchive("Invalid zip header"));
        }

        let mut pos = file_length - HEADER_SIZE;
        while pos >= search_upper_bound {
            reader.seek(io::SeekFrom::Start(pos))?;
            if reader.read_u32::<LittleEndian>()? == CENTRAL_DIRECTORY_END_SIGNATURE {
                reader.seek(io::SeekFrom::Current(
                    BYTES_BETWEEN_MAGIC_AND_COMMENT_SIZE as i64,
                ))?;
                let cde_start_pos = reader.seek(io::SeekFrom::Start(pos))?;
                return CentralDirectoryEnd::parse(reader).map(|cde| (cde, cde_start_pos));
            }
            pos = match pos.checked_sub(1) {
                Some(p) => p,
                None => break,
            };
        }
        Err(ZipError::InvalidArchive(
            "Could not find central directory end",
        ))
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> ZipResult<()> {
        writer.write_u32::<LittleEndian>(CENTRAL_DIRECTORY_END_SIGNATURE)?;
        writer.write_u16::<LittleEndian>(self.disk_number)?;
        writer.write_u16::<LittleEndian>(self.disk_with_central_directory)?;
        writer.write_u16::<LittleEndian>(self.number_of_files_on_this_disk)?;
        writer.write_u16::<LittleEndian>(self.number_of_files)?;
        writer.write_u32::<LittleEndian>(self.central_directory_size)?;
        writer.write_u32::<LittleEndian>(self.central_directory_offset)?;
        writer.write_u16::<LittleEndian>(self.zip_file_comment.len() as u16)?;
        writer.write_all(&self.zip_file_comment)?;
        Ok(())
    }
}

pub struct Zip64CentralDirectoryEndLocator {
    pub disk_with_central_directory: u32,
    pub end_of_central_directory_offset: u64,
    pub number_of_disks: u32,
}

impl Zip64CentralDirectoryEndLocator {
    pub fn parse<T: Read>(reader: &mut T) -> ZipResult<Zip64CentralDirectoryEndLocator> {
        let magic = reader.read_u32::<LittleEndian>()?;
        if magic != ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE {
            return Err(ZipError::InvalidArchive(
                "Invalid zip64 locator digital signature header",
            ));
        }
        let disk_with_central_directory = reader.read_u32::<LittleEndian>()?;
        let end_of_central_directory_offset = reader.read_u64::<LittleEndian>()?;
        let number_of_disks = reader.read_u32::<LittleEndian>()?;

        Ok(Zip64CentralDirectoryEndLocator {
            disk_with_central_directory,
            end_of_central_directory_offset,
            number_of_disks,
        })
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> ZipResult<()> {
        writer.write_u32::<LittleEndian>(ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE)?;
        writer.write_u32::<LittleEndian>(self.disk_with_central_directory)?;
        writer.write_u64::<LittleEndian>(self.end_of_central_directory_offset)?;
        writer.write_u32::<LittleEndian>(self.number_of_disks)?;
        Ok(())
    }
}

pub struct Zip64CentralDirectoryEnd {
    pub version_made_by: u16,
    pub version_needed_to_extract: u16,
    pub disk_number: u32,
    pub disk_with_central_directory: u32,
    pub number_of_files_on_this_disk: u64,
    pub number_of_files: u64,
    pub central_directory_size: u64,
    pub central_directory_offset: u64,
    //pub extensible_data_sector: Vec<u8>, <-- We don't do anything with this at the moment.
}

impl Zip64CentralDirectoryEnd {
    pub fn find_and_parse<T: Read + io::Seek>(
        reader: &mut T,
        nominal_offset: u64,
        search_upper_bound: u64,
    ) -> ZipResult<(Zip64CentralDirectoryEnd, u64)> {
        let mut pos = nominal_offset;

        while pos <= search_upper_bound {
            reader.seek(io::SeekFrom::Start(pos))?;

            if reader.read_u32::<LittleEndian>()? == ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE {
                let archive_offset = pos - nominal_offset;

                let _record_size = reader.read_u64::<LittleEndian>()?;
                // We would use this value if we did anything with the "zip64 extensible data sector".

                let version_made_by = reader.read_u16::<LittleEndian>()?;
                let version_needed_to_extract = reader.read_u16::<LittleEndian>()?;
                let disk_number = reader.read_u32::<LittleEndian>()?;
                let disk_with_central_directory = reader.read_u32::<LittleEndian>()?;
                let number_of_files_on_this_disk = reader.read_u64::<LittleEndian>()?;
                let number_of_files = reader.read_u64::<LittleEndian>()?;
                let central_directory_size = reader.read_u64::<LittleEndian>()?;
                let central_directory_offset = reader.read_u64::<LittleEndian>()?;

                return Ok((
                    Zip64CentralDirectoryEnd {
                        version_made_by,
                        version_needed_to_extract,
                        disk_number,
                        disk_with_central_directory,
                        number_of_files_on_this_disk,
                        number_of_files,
                        central_directory_size,
                        central_directory_offset,
                    },
                    archive_offset,
                ));
            }

            pos += 1;
        }

        Err(ZipError::InvalidArchive(
            "Could not find ZIP64 central directory end",
        ))
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> ZipResult<()> {
        writer.write_u32::<LittleEndian>(ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE)?;
        writer.write_u64::<LittleEndian>(44)?; // record size
        writer.write_u16::<LittleEndian>(self.version_made_by)?;
        writer.write_u16::<LittleEndian>(self.version_needed_to_extract)?;
        writer.write_u32::<LittleEndian>(self.disk_number)?;
        writer.write_u32::<LittleEndian>(self.disk_with_central_directory)?;
        writer.write_u64::<LittleEndian>(self.number_of_files_on_this_disk)?;
        writer.write_u64::<LittleEndian>(self.number_of_files)?;
        writer.write_u64::<LittleEndian>(self.central_directory_size)?;
        writer.write_u64::<LittleEndian>(self.central_directory_offset)?;
        Ok(())
    }
}
