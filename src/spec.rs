use crate::result::{ZipError, ZipResult};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io;
use std::io::prelude::*;

pub const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x04034b50;
pub const CENTRAL_DIRECTORY_HEADER_SIGNATURE: u32 = 0x02014b50;
const CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06054b50;
pub const ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06064b50;
const ZIP64_CENTRAL_DIRECTORY_END_LOCATOR_SIGNATURE: u32 = 0x07064b50;

pub const ZIP64_BYTES_THR: u64 = u32::MAX as u64;
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
        const HEADER_SIZE: usize = 22;

        let file_length = reader.seek(io::SeekFrom::End(0))?;

        let last_chunk_start = reader.seek(io::SeekFrom::End(-(std::cmp::min(file_length as usize, HEADER_SIZE + ::std::u16::MAX as usize) as i64)))?;
        let mut last_chunk = Vec::with_capacity(HEADER_SIZE + ::std::u16::MAX as usize);
        reader.read_to_end(&mut last_chunk)?;

        if last_chunk.len() < HEADER_SIZE {
            return Err(ZipError::InvalidArchive("Invalid zip header"));
        }

        let mut pos = last_chunk.len() - HEADER_SIZE;
        loop {
            if (&last_chunk[pos..]).read_u32::<LittleEndian>()? == CENTRAL_DIRECTORY_END_SIGNATURE {
                let cde_start_pos = last_chunk_start + pos as u64;
                return CentralDirectoryEnd::parse(&mut &last_chunk[pos..]).map(|cde| (cde, cde_start_pos));
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

        const HEADER_SIZE: usize = 56; /* does not include comment */

        let mut buffer = Vec::new();
        while pos <= search_upper_bound {
            reader.seek(io::SeekFrom::Start(pos))?;

            buffer.resize(std::cmp::min(4096, HEADER_SIZE + (search_upper_bound-pos) as usize), 0u8);
            reader.read_exact(&mut buffer)?;
            for i in 0..=buffer.len() - HEADER_SIZE {
                let mut bufreader = &buffer[i..];

                if bufreader.read_u32::<LittleEndian>()? == ZIP64_CENTRAL_DIRECTORY_END_SIGNATURE {
                    let archive_offset = pos + i as u64 - nominal_offset;

                    let _record_size = bufreader.read_u64::<LittleEndian>()?;
                    // We would use this value if we did anything with the "zip64 extensible data sector".

                    let version_made_by = bufreader.read_u16::<LittleEndian>()?;
                    let version_needed_to_extract = bufreader.read_u16::<LittleEndian>()?;
                    let disk_number = bufreader.read_u32::<LittleEndian>()?;
                    let disk_with_central_directory = bufreader.read_u32::<LittleEndian>()?;
                    let number_of_files_on_this_disk = bufreader.read_u64::<LittleEndian>()?;
                    let number_of_files = bufreader.read_u64::<LittleEndian>()?;
                    let central_directory_size = bufreader.read_u64::<LittleEndian>()?;
                    let central_directory_offset = bufreader.read_u64::<LittleEndian>()?;

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
            }
            pos += buffer.len() as u64 - HEADER_SIZE as u64 + 1; /* subtract the HEADER_SIZE in case header spans a chunk boundary */
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

#[cfg(test)]
mod test {
    #[test]
    fn zero_length_zip() {
        use super::CentralDirectoryEnd;
        use std::io;

        let v = vec![];
        let cde = CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&v));
        assert!(cde.is_err());
    }

    #[test]
    fn invalid_cde_too_small() {
        use super::CentralDirectoryEnd;
        use std::io;

        // This is a valid CDE that _just_ fits (though there's nothing in front of it, so the offsets are wrong)
        let v = vec![0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00, 0xe5, 0x00, 0x00, 0x00, 0xd3, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cde = CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&v));
        assert!(cde.is_ok()); // This is ok, the offsets are checked elsewhere.

        // This is the same except the CDE is truncated by 4 bytes
        let cde = CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&v[0..v.len()-4]));
        assert!(cde.is_err());
    }

    #[test]
    fn invalid_cde_missing() {
        use super::CentralDirectoryEnd;
        use std::io;

        let v = [0; 70000]; // something larger than 65536 + CDE size
        let cde = CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&v));
        assert!(cde.is_err());

        let v = [0; 256]; // something smaller than 65536 but larger CDE size
        let cde = CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&v));
        assert!(cde.is_err());
    }

    fn zip64_cde_search(cde_start_pos: usize, total_size: usize) -> u64 {
        use super::Zip64CentralDirectoryEnd;
        use std::io;

        // 56 byte zip64 Central Directory End (extracted manually from tests/data/zip64_demo.zip)
        let cde64 = vec![0x50, 0x4b, 0x06, 0x06, 0x2c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0x03, 0x2d, 0x00,
                         0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                         0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                         0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let mut haystack = vec![];
        haystack.resize(cde_start_pos, 0u8);
        haystack.extend_from_slice(&cde64);
        haystack.resize(total_size, 0u8);
        let (_, offset) = Zip64CentralDirectoryEnd::find_and_parse(&mut io::Cursor::new(&haystack), 0, haystack.len() as u64 - 56).expect("find_and_parse");
        offset
    }

    #[test]
    fn zip64_cde_search_less_than_chunk_at_start() {
        assert_eq!(0, zip64_cde_search(0, 100));
    }

    #[test]
    fn zip64_cde_search_less_than_chunk_in_middle() {
        assert_eq!(100, zip64_cde_search(100, 300));
    }

    #[test]
    fn zip64_cde_search_less_than_chunk_at_end() {
        assert_eq!(100, zip64_cde_search(100, 156));
    }

    #[test]
    fn zip64_cde_search_more_than_chunk_at_chunk_start() {
        assert_eq!(4096, zip64_cde_search(4096, 4200));
    }

    #[test]
    fn zip64_cde_search_more_than_chunk_mid_chunk() {
        assert_eq!(5000, zip64_cde_search(5000, 9000));
    }

    #[test]
    fn zip64_cde_search_more_than_chunk_at_chunk_end() {
        assert_eq!(8192-56, zip64_cde_search(8192-56, 8192));
    }

    #[test]
    fn zip64_cde_search_more_than_chunk_straddling_chunk_end() {
        assert_eq!(4096-30, zip64_cde_search(4096-30, 4200));
    }
}
