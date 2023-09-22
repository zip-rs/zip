use crate::result::{ZipError, ZipResult};
use std::io;
use std::io::prelude::*;

pub const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x04034b50;
pub const CENTRAL_DIRECTORY_HEADER_SIGNATURE: u32 = 0x02014b50;
const CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06054b50;
pub const ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06064b50;
const ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE: u32 = 0x07064b50;

pub const ZIP64_BYTES_THR: u64 = u32::MAX as u64;
pub const ZIP64_ENTRY_THR: usize = u16::MAX as usize;

pub(crate) fn read_u8(reader: &mut impl Read) -> ZipResult<u8> {
    let mut data = [0; 1];
    reader.read_exact(&mut data)?;
    Ok(u8::from_le_bytes(data))
}

pub(crate) fn read_u16(reader: &mut impl Read) -> ZipResult<u16> {
    let mut data = [0; 2];
    reader.read_exact(&mut data)?;
    Ok(u16::from_le_bytes(data))
}

pub(crate) fn read_u32(reader: &mut impl Read) -> ZipResult<u32> {
    let mut data = [0; 4];
    reader.read_exact(&mut data)?;
    Ok(u32::from_le_bytes(data))
}

pub(crate) fn read_u64(reader: &mut impl Read) -> ZipResult<u64> {
    let mut data = [0; 8];
    reader.read_exact(&mut data)?;
    Ok(u64::from_le_bytes(data))
}

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
        let magic = read_u32(reader)?;
        if magic != CENTRAL_DIRECTORY_END_SIGNATURE {
            return Err(ZipError::InvalidArchive("Invalid digital signature header"));
        }
        let disk_number = read_u16(reader)?;
        let disk_with_central_directory = read_u16(reader)?;
        let number_of_files_on_this_disk = read_u16(reader)?;
        let number_of_files = read_u16(reader)?;
        let central_directory_size = read_u32(reader)?;
        let central_directory_offset = read_u32(reader)?;
        let zip_file_comment_length = read_u16(reader)? as usize;
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
            if read_u32(reader)? == CENTRAL_DIRECTORY_END_SIGNATURE {
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
        writer.write_all(&CENTRAL_DIRECTORY_END_SIGNATURE.to_le_bytes())?;
        writer.write_all(&self.disk_number.to_le_bytes())?;
        writer.write_all(&self.disk_with_central_directory.to_le_bytes())?;
        writer.write_all(&self.number_of_files_on_this_disk.to_le_bytes())?;
        writer.write_all(&self.number_of_files.to_le_bytes())?;
        writer.write_all(&self.central_directory_size.to_le_bytes())?;
        writer.write_all(&self.central_directory_offset.to_le_bytes())?;
        writer.write_all(&(self.zip_file_comment.len() as u16).to_le_bytes())?;
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
        let magic = read_u32(reader)?;
        if magic != ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE {
            return Err(ZipError::InvalidArchive(
                "Invalid zip64 locator digital signature header",
            ));
        }
        let disk_with_central_directory = read_u32(reader)?;
        let end_of_central_directory_offset = read_u64(reader)?;
        let number_of_disks = read_u32(reader)?;

        Ok(Zip64CentralDirectoryEndLocator {
            disk_with_central_directory,
            end_of_central_directory_offset,
            number_of_disks,
        })
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> ZipResult<()> {
        writer.write_all(&ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE.to_ne_bytes())?;
        writer.write_all(&self.disk_with_central_directory.to_ne_bytes())?;
        writer.write_all(&self.end_of_central_directory_offset.to_ne_bytes())?;
        writer.write_all(&self.number_of_disks.to_ne_bytes())?;
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

            if read_u32(reader)? == ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE {
                let archive_offset = pos - nominal_offset;

                let _record_size = read_u64(reader)?;
                // We would use this value if we did anything with the "zip64 extensible data sector".

                let version_made_by = read_u16(reader)?;
                let version_needed_to_extract = read_u16(reader)?;
                let disk_number = read_u32(reader)?;
                let disk_with_central_directory = read_u32(reader)?;
                let number_of_files_on_this_disk = read_u64(reader)?;
                let number_of_files = read_u64(reader)?;
                let central_directory_size = read_u64(reader)?;
                let central_directory_offset = read_u64(reader)?;

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
        writer.write_all(&ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE.to_le_bytes())?;
        writer.write_all(&44u64.to_le_bytes())?; // record size
        writer.write_all(&self.version_made_by.to_le_bytes())?;
        writer.write_all(&self.version_needed_to_extract.to_le_bytes())?;
        writer.write_all(&self.disk_number.to_le_bytes())?;
        writer.write_all(&self.disk_with_central_directory.to_le_bytes())?;
        writer.write_all(&self.number_of_files_on_this_disk.to_le_bytes())?;
        writer.write_all(&self.number_of_files.to_le_bytes())?;
        writer.write_all(&self.central_directory_size.to_le_bytes())?;
        writer.write_all(&self.central_directory_offset.to_le_bytes())?;
        Ok(())
    }
}
