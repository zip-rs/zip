//! Types for reading ZIP archives

#[cfg(feature = "aes-crypto")]
use crate::aes::{AesReader, AesReaderValid};
use crate::compression::CompressionMethod;
use crate::cp437::FromCp437;
use crate::crc32::Crc32Reader;
use crate::result::{InvalidPassword, ZipError, ZipResult};
use crate::spec;
use crate::types::{AesMode, AesVendorVersion, AtomicU64, DateTime, System, ZipFileData};
use crate::zipcrypto::{ZipCryptoReader, ZipCryptoReaderValid, ZipCryptoValidator};

use byteorder::{LittleEndian, ReadBytesExt};
use once_cell::sync::Lazy;

use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, prelude::*};
use std::ops;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::{Arc, RwLock};

#[cfg(any(
    feature = "deflate",
    feature = "deflate-miniz",
    feature = "deflate-zlib"
))]
use flate2::read::DeflateDecoder;

#[cfg(feature = "bzip2")]
use bzip2::read::BzDecoder;

#[cfg(feature = "zstd")]
use zstd::stream::read::Decoder as ZstdDecoder;

/// Provides high level API for reading from a stream.
pub(crate) mod stream;

// Put the struct declaration in a private module to convince rustdoc to display ZipArchive nicely
pub(crate) mod zip_archive {
    /// Extract immutable data from `ZipArchive` to make it cheap to clone
    #[derive(Debug)]
    pub(crate) struct Shared {
        pub(super) files: Vec<super::ZipFileData>,
        pub(super) names_map: super::HashMap<String, usize>,
        pub(super) offset: u64,
        pub(super) comment: Vec<u8>,
    }

    /// ZIP archive reader
    ///
    /// At the moment, this type is cheap to clone if this is the case for the
    /// reader it uses. However, this is not guaranteed by this crate and it may
    /// change in the future.
    ///
    /// ```no_run
    /// use std::io::prelude::*;
    /// fn list_zip_contents(reader: impl Read + Seek) -> zip::result::ZipResult<()> {
    ///     let mut zip = zip::ZipArchive::new(reader)?;
    ///
    ///     for i in 0..zip.len() {
    ///         let mut file = zip.by_index(i)?;
    ///         println!("Filename: {}", file.name());
    ///         std::io::copy(&mut file, &mut std::io::stdout());
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    #[derive(Clone, Debug)]
    pub struct ZipArchive<R> {
        pub(super) reader: R,
        pub(super) shared: super::Arc<Shared>,
    }
}

pub use zip_archive::ZipArchive;
#[allow(clippy::large_enum_variant)]
enum CryptoReader<'a> {
    Plaintext(io::Take<&'a mut dyn Read>),
    ZipCrypto(ZipCryptoReaderValid<io::Take<&'a mut dyn Read>>),
    #[cfg(feature = "aes-crypto")]
    Aes {
        reader: AesReaderValid<io::Take<&'a mut dyn Read>>,
        vendor_version: AesVendorVersion,
    },
}

impl<'a> Read for CryptoReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            CryptoReader::Plaintext(r) => r.read(buf),
            CryptoReader::ZipCrypto(r) => r.read(buf),
            #[cfg(feature = "aes-crypto")]
            CryptoReader::Aes { reader: r, .. } => r.read(buf),
        }
    }
}

impl<'a> CryptoReader<'a> {
    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> io::Take<&'a mut dyn Read> {
        match self {
            CryptoReader::Plaintext(r) => r,
            CryptoReader::ZipCrypto(r) => r.into_inner(),
            #[cfg(feature = "aes-crypto")]
            CryptoReader::Aes { reader: r, .. } => r.into_inner(),
        }
    }

    /// Returns `true` if the data is encrypted using AE2.
    pub fn is_ae2_encrypted(&self) -> bool {
        #[cfg(feature = "aes-crypto")]
        return matches!(
            self,
            CryptoReader::Aes {
                vendor_version: AesVendorVersion::Ae2,
                ..
            }
        );
        #[cfg(not(feature = "aes-crypto"))]
        false
    }
}

enum ZipEntry<'a, R: Read + 'a> {
    Stored(R),
    #[cfg(any(
        feature = "deflate",
        feature = "deflate-miniz",
        feature = "deflate-zlib"
    ))]
    Deflated(flate2::read::DeflateDecoder<R>),
    #[cfg(feature = "bzip2")]
    Bzip2(BzDecoder<R>),
    #[cfg(feature = "zstd")]
    Zstd(ZstdDecoder<'a, io::BufReader<R>>),
}

impl<'a, R: Read + 'a> ZipEntry<'a, R> {
    pub fn from_data(data: &'a ZipFileData, source_handle: R) -> Self {
        match data.compression_method {
            CompressionMethod::Stored => Self::Stored(source_handle),
            #[cfg(any(
                feature = "deflate",
                feature = "deflate-miniz",
                feature = "deflate-zlib"
            ))]
            CompressionMethod::Deflated => Self::Deflated(DeflateDecoder::new(source_handle)),
            #[cfg(feature = "bzip2")]
            CompressionMethod::Bzip2 => Self::Bzip2(BzDecoder::new(source_handle)),
            #[cfg(feature = "zstd")]
            CompressionMethod::Zstd => {
                let zstd_reader = ZstdDecoder::new(source_handle).unwrap();
                Self::Zstd(zstd_reader)
            }
            _ => panic!("Compression method not supported"),
        }
    }
}

impl<'a, R: Read + 'a> Read for ZipEntry<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stored(r) => r.read(buf),
            #[cfg(any(
                feature = "deflate",
                feature = "deflate-miniz",
                feature = "deflate-zlib"
            ))]
            Self::Deflated(r) => r.read(buf),
            #[cfg(feature = "bzip2")]
            Self::Bzip2(r) => r.read(buf),
            #[cfg(feature = "zstd")]
            Self::Zstd(r) => r.read(buf),
        }
    }
}

enum ZipFileReader<'a> {
    NoReader,
    Raw(io::Take<&'a mut dyn io::Read>),
    Stored(Crc32Reader<CryptoReader<'a>>),
    #[cfg(any(
        feature = "deflate",
        feature = "deflate-miniz",
        feature = "deflate-zlib"
    ))]
    Deflated(Crc32Reader<flate2::read::DeflateDecoder<CryptoReader<'a>>>),
    #[cfg(feature = "bzip2")]
    Bzip2(Crc32Reader<BzDecoder<CryptoReader<'a>>>),
    #[cfg(feature = "zstd")]
    Zstd(Crc32Reader<ZstdDecoder<'a, io::BufReader<CryptoReader<'a>>>>),
}

impl<'a> Read for ZipFileReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ZipFileReader::NoReader => panic!("ZipFileReader was in an invalid state"),
            ZipFileReader::Raw(r) => r.read(buf),
            ZipFileReader::Stored(r) => r.read(buf),
            #[cfg(any(
                feature = "deflate",
                feature = "deflate-miniz",
                feature = "deflate-zlib"
            ))]
            ZipFileReader::Deflated(r) => r.read(buf),
            #[cfg(feature = "bzip2")]
            ZipFileReader::Bzip2(r) => r.read(buf),
            #[cfg(feature = "zstd")]
            ZipFileReader::Zstd(r) => r.read(buf),
        }
    }
}

impl<'a> ZipFileReader<'a> {
    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> io::Take<&'a mut dyn Read> {
        match self {
            ZipFileReader::NoReader => panic!("ZipFileReader was in an invalid state"),
            ZipFileReader::Raw(r) => r,
            ZipFileReader::Stored(r) => r.into_inner().into_inner(),
            #[cfg(any(
                feature = "deflate",
                feature = "deflate-miniz",
                feature = "deflate-zlib"
            ))]
            ZipFileReader::Deflated(r) => r.into_inner().into_inner().into_inner(),
            #[cfg(feature = "bzip2")]
            ZipFileReader::Bzip2(r) => r.into_inner().into_inner().into_inner(),
            #[cfg(feature = "zstd")]
            ZipFileReader::Zstd(r) => r.into_inner().finish().into_inner().into_inner(),
        }
    }
}

/// A struct for reading a zip file
pub struct ZipFile<'a> {
    data: Cow<'a, ZipFileData>,
    crypto_reader: Option<CryptoReader<'a>>,
    reader: ZipFileReader<'a>,
}

fn find_content<'a>(
    data: &ZipFileData,
    reader: &'a mut (impl Read + Seek),
) -> ZipResult<io::Take<&'a mut dyn Read>> {
    // Parse local header
    reader.seek(io::SeekFrom::Start(data.header_start))?;
    let signature = reader.read_u32::<LittleEndian>()?;
    if signature != spec::LOCAL_FILE_HEADER_SIGNATURE {
        return Err(ZipError::InvalidArchive("Invalid local file header"));
    }

    reader.seek(io::SeekFrom::Current(22))?;
    let file_name_length = reader.read_u16::<LittleEndian>()? as u64;
    let extra_field_length = reader.read_u16::<LittleEndian>()? as u64;
    let magic_and_header = 4 + 22 + 2 + 2;
    let data_start = data.header_start + magic_and_header + file_name_length + extra_field_length;
    data.data_start.store(data_start);

    reader.seek(io::SeekFrom::Start(data_start))?;
    Ok((reader as &mut dyn Read).take(data.compressed_size))
}

#[allow(clippy::too_many_arguments)]
fn make_crypto_reader<'a>(
    compression_method: crate::compression::CompressionMethod,
    crc32: u32,
    last_modified_time: DateTime,
    using_data_descriptor: bool,
    reader: io::Take<&'a mut dyn io::Read>,
    password: Option<&[u8]>,
    aes_info: Option<(AesMode, AesVendorVersion)>,
    #[cfg(feature = "aes-crypto")] compressed_size: u64,
) -> ZipResult<Result<CryptoReader<'a>, InvalidPassword>> {
    #[allow(deprecated)]
    {
        if let CompressionMethod::Unsupported(_) = compression_method {
            return unsupported_zip_error("Compression method not supported");
        }
    }

    let reader = match (password, aes_info) {
        #[cfg(not(feature = "aes-crypto"))]
        (Some(_), Some(_)) => {
            return Err(ZipError::UnsupportedArchive(
                "AES encrypted files cannot be decrypted without the aes-crypto feature.",
            ))
        }
        #[cfg(feature = "aes-crypto")]
        (Some(password), Some((aes_mode, vendor_version))) => {
            match AesReader::new(reader, aes_mode, compressed_size).validate(password)? {
                None => return Ok(Err(InvalidPassword)),
                Some(r) => CryptoReader::Aes {
                    reader: r,
                    vendor_version,
                },
            }
        }
        (Some(password), None) => {
            let validator = if using_data_descriptor {
                ZipCryptoValidator::InfoZipMsdosTime(last_modified_time.timepart())
            } else {
                ZipCryptoValidator::PkzipCrc32(crc32)
            };
            match ZipCryptoReader::new(reader, password).validate(validator)? {
                None => return Ok(Err(InvalidPassword)),
                Some(r) => CryptoReader::ZipCrypto(r),
            }
        }
        (None, Some(_)) => return Ok(Err(InvalidPassword)),
        (None, None) => CryptoReader::Plaintext(reader),
    };
    Ok(Ok(reader))
}

fn make_reader(
    compression_method: CompressionMethod,
    crc32: u32,
    reader: CryptoReader,
) -> ZipFileReader {
    let ae2_encrypted = reader.is_ae2_encrypted();

    match compression_method {
        CompressionMethod::Stored => {
            ZipFileReader::Stored(Crc32Reader::new(reader, crc32, ae2_encrypted))
        }
        #[cfg(any(
            feature = "deflate",
            feature = "deflate-miniz",
            feature = "deflate-zlib"
        ))]
        CompressionMethod::Deflated => {
            let deflate_reader = DeflateDecoder::new(reader);
            ZipFileReader::Deflated(Crc32Reader::new(deflate_reader, crc32, ae2_encrypted))
        }
        #[cfg(feature = "bzip2")]
        CompressionMethod::Bzip2 => {
            let bzip2_reader = BzDecoder::new(reader);
            ZipFileReader::Bzip2(Crc32Reader::new(bzip2_reader, crc32, ae2_encrypted))
        }
        #[cfg(feature = "zstd")]
        CompressionMethod::Zstd => {
            let zstd_reader = ZstdDecoder::new(reader).unwrap();
            ZipFileReader::Zstd(Crc32Reader::new(zstd_reader, crc32, ae2_encrypted))
        }
        _ => panic!("Compression method not supported"),
    }
}

impl<R: Read + io::Seek> ZipArchive<R> {
    /// Get the directory start offset and number of files. This is done in a
    /// separate function to ease the control flow design.
    pub(crate) fn get_directory_counts(
        reader: &mut R,
        footer: &spec::CentralDirectoryEnd,
        cde_start_pos: u64,
    ) -> ZipResult<(u64, u64, usize)> {
        // See if there's a ZIP64 footer. The ZIP64 locator if present will
        // have its signature 20 bytes in front of the standard footer. The
        // standard footer, in turn, is 22+N bytes large, where N is the
        // comment length. Therefore:
        let zip64locator = if reader
            .seek(io::SeekFrom::End(
                -(20 + 22 + footer.zip_file_comment.len() as i64),
            ))
            .is_ok()
        {
            match spec::Zip64CentralDirectoryEndLocator::parse(reader) {
                Ok(loc) => Some(loc),
                Err(ZipError::InvalidArchive(_)) => {
                    // No ZIP64 header; that's actually fine. We're done here.
                    None
                }
                Err(e) => {
                    // Yikes, a real problem
                    return Err(e);
                }
            }
        } else {
            // Empty Zip files will have nothing else so this error might be fine. If
            // not, we'll find out soon.
            None
        };

        match zip64locator {
            None => {
                // Some zip files have data prepended to them, resulting in the
                // offsets all being too small. Get the amount of error by comparing
                // the actual file position we found the CDE at with the offset
                // recorded in the CDE.
                let archive_offset = cde_start_pos
                    .checked_sub(footer.central_directory_size as u64)
                    .and_then(|x| x.checked_sub(footer.central_directory_offset as u64))
                    .ok_or(ZipError::InvalidArchive(
                        "Invalid central directory size or offset",
                    ))?;

                let directory_start = footer.central_directory_offset as u64 + archive_offset;
                let number_of_files = footer.number_of_files_on_this_disk as usize;
                Ok((archive_offset, directory_start, number_of_files))
            }
            Some(locator64) => {
                // If we got here, this is indeed a ZIP64 file.

                if !footer.record_too_small()
                    && footer.disk_number as u32 != locator64.disk_with_central_directory
                {
                    return unsupported_zip_error(
                        "Support for multi-disk files is not implemented",
                    );
                }

                // We need to reassess `archive_offset`. We know where the ZIP64
                // central-directory-end structure *should* be, but unfortunately we
                // don't know how to precisely relate that location to our current
                // actual offset in the file, since there may be junk at its
                // beginning. Therefore we need to perform another search, as in
                // read::CentralDirectoryEnd::find_and_parse, except now we search
                // forward.

                let search_upper_bound = cde_start_pos
                    .checked_sub(60) // minimum size of Zip64CentralDirectoryEnd + Zip64CentralDirectoryEndLocator
                    .ok_or(ZipError::InvalidArchive(
                        "File cannot contain ZIP64 central directory end",
                    ))?;
                let (footer, archive_offset) = spec::Zip64CentralDirectoryEnd::find_and_parse(
                    reader,
                    locator64.end_of_central_directory_offset,
                    search_upper_bound,
                )?;

                if footer.disk_number != footer.disk_with_central_directory {
                    return unsupported_zip_error(
                        "Support for multi-disk files is not implemented",
                    );
                }

                let directory_start = footer
                    .central_directory_offset
                    .checked_add(archive_offset)
                    .ok_or({
                        ZipError::InvalidArchive("Invalid central directory size or offset")
                    })?;

                Ok((
                    archive_offset,
                    directory_start,
                    footer.number_of_files as usize,
                ))
            }
        }
    }

    /// Read a ZIP archive, collecting the files it contains
    ///
    /// This uses the central directory record of the ZIP file, and ignores local file headers
    pub fn new(mut reader: R) -> ZipResult<ZipArchive<R>> {
        let (footer, cde_start_pos) = spec::CentralDirectoryEnd::find_and_parse(&mut reader)?;

        if !footer.record_too_small() && footer.disk_number != footer.disk_with_central_directory {
            return unsupported_zip_error("Support for multi-disk files is not implemented");
        }

        let (archive_offset, directory_start, number_of_files) =
            Self::get_directory_counts(&mut reader, &footer, cde_start_pos)?;

        // If the parsed number of files is greater than the offset then
        // something fishy is going on and we shouldn't trust number_of_files.
        let file_capacity = if number_of_files > cde_start_pos as usize {
            0
        } else {
            number_of_files
        };

        let mut files = Vec::with_capacity(file_capacity);
        let mut names_map = HashMap::with_capacity(file_capacity);

        if reader.seek(io::SeekFrom::Start(directory_start)).is_err() {
            return Err(ZipError::InvalidArchive(
                "Could not seek to start of central directory",
            ));
        }

        for _ in 0..number_of_files {
            let file = central_header_to_zip_file(&mut reader, archive_offset)?;
            names_map.insert(file.file_name.clone(), files.len());
            files.push(file);
        }

        let shared = Arc::new(zip_archive::Shared {
            files,
            names_map,
            offset: archive_offset,
            comment: footer.zip_file_comment,
        });

        Ok(ZipArchive { reader, shared })
    }
    /// Extract a Zip archive into a directory, overwriting files if they
    /// already exist. Paths are sanitized with [`ZipFile::enclosed_name`].
    ///
    /// Extraction is not atomic; If an error is encountered, some of the files
    /// may be left on disk.
    pub fn extract<P: AsRef<Path>>(&mut self, directory: P) -> ZipResult<()> {
        for i in 0..self.len() {
            let mut file = self.by_index(i)?;
            let filepath = file
                .enclosed_name()
                .ok_or(ZipError::InvalidArchive("Invalid file path"))?;

            let outpath = directory.as_ref().join(filepath);

            if file.name().ends_with('/') {
                fs::create_dir_all(&outpath)?;
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        fs::create_dir_all(p)?;
                    }
                }
                let mut outfile = fs::File::create(&outpath)?;
                io::copy(&mut file, &mut outfile)?;
            }
            // Get and Set permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.unix_mode() {
                    fs::set_permissions(&outpath, fs::Permissions::from_mode(mode))?;
                }
            }
        }
        Ok(())
    }

    /// Number of files contained in this zip.
    pub fn len(&self) -> usize {
        self.shared.files.len()
    }

    /// Whether this zip archive contains no files
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the offset from the beginning of the underlying reader that this zip begins at, in bytes.
    ///
    /// Normally this value is zero, but if the zip has arbitrary data prepended to it, then this value will be the size
    /// of that prepended data.
    pub fn offset(&self) -> u64 {
        self.shared.offset
    }

    /// Get the comment of the zip archive.
    pub fn comment(&self) -> &[u8] {
        &self.shared.comment
    }

    /// Returns an iterator over all the file and directory names in this archive.
    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.shared.names_map.keys().map(|s| s.as_str())
    }

    /// Search for a file entry by name, decrypt with given password
    ///
    /// # Warning
    ///
    /// The implementation of the cryptographic algorithms has not
    /// gone through a correctness review, and you should assume it is insecure:
    /// passwords used with this API may be compromised.
    ///
    /// This function sometimes accepts wrong password. This is because the ZIP spec only allows us
    /// to check for a 1/256 chance that the password is correct.
    /// There are many passwords out there that will also pass the validity checks
    /// we are able to perform. This is a weakness of the ZipCrypto algorithm,
    /// due to its fairly primitive approach to cryptography.
    pub fn by_name_decrypt<'a>(
        &'a mut self,
        name: &str,
        password: &[u8],
    ) -> ZipResult<Result<ZipFile<'a>, InvalidPassword>> {
        self.by_name_with_optional_password(name, Some(password))
    }

    /// Search for a file entry by name
    pub fn by_name<'a>(&'a mut self, name: &str) -> ZipResult<ZipFile<'a>> {
        Ok(self.by_name_with_optional_password(name, None)?.unwrap())
    }

    fn by_name_with_optional_password<'a>(
        &'a mut self,
        name: &str,
        password: Option<&[u8]>,
    ) -> ZipResult<Result<ZipFile<'a>, InvalidPassword>> {
        let index = match self.shared.names_map.get(name) {
            Some(index) => *index,
            None => {
                return Err(ZipError::FileNotFound);
            }
        };
        self.by_index_with_optional_password(index, password)
    }

    /// Get a contained file by index, decrypt with given password
    ///
    /// # Warning
    ///
    /// The implementation of the cryptographic algorithms has not
    /// gone through a correctness review, and you should assume it is insecure:
    /// passwords used with this API may be compromised.
    ///
    /// This function sometimes accepts wrong password. This is because the ZIP spec only allows us
    /// to check for a 1/256 chance that the password is correct.
    /// There are many passwords out there that will also pass the validity checks
    /// we are able to perform. This is a weakness of the ZipCrypto algorithm,
    /// due to its fairly primitive approach to cryptography.
    pub fn by_index_decrypt<'a>(
        &'a mut self,
        file_number: usize,
        password: &[u8],
    ) -> ZipResult<Result<ZipFile<'a>, InvalidPassword>> {
        self.by_index_with_optional_password(file_number, Some(password))
    }

    /// Get a contained file by index
    pub fn by_index(&mut self, file_number: usize) -> ZipResult<ZipFile<'_>> {
        Ok(self
            .by_index_with_optional_password(file_number, None)?
            .unwrap())
    }

    /// Get a contained file by index without decompressing it
    pub fn by_index_raw(&mut self, file_number: usize) -> ZipResult<ZipFile<'_>> {
        let reader = &mut self.reader;
        self.shared
            .files
            .get(file_number)
            .ok_or(ZipError::FileNotFound)
            .and_then(move |data| {
                Ok(ZipFile {
                    crypto_reader: None,
                    reader: ZipFileReader::Raw(find_content(data, reader)?),
                    data: Cow::Borrowed(data),
                })
            })
    }

    fn by_index_with_optional_password<'a>(
        &'a mut self,
        file_number: usize,
        mut password: Option<&[u8]>,
    ) -> ZipResult<Result<ZipFile<'a>, InvalidPassword>> {
        let data = self
            .shared
            .files
            .get(file_number)
            .ok_or(ZipError::FileNotFound)?;

        match (password, data.encrypted) {
            (None, true) => return Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)),
            (Some(_), false) => password = None, //Password supplied, but none needed! Discard.
            _ => {}
        }
        let limit_reader = find_content(data, &mut self.reader)?;

        match make_crypto_reader(
            data.compression_method,
            data.crc32,
            data.last_modified_time,
            data.using_data_descriptor,
            limit_reader,
            password,
            data.aes_mode,
            #[cfg(feature = "aes-crypto")]
            data.compressed_size,
        ) {
            Ok(Ok(crypto_reader)) => Ok(Ok(ZipFile {
                crypto_reader: Some(crypto_reader),
                reader: ZipFileReader::NoReader,
                data: Cow::Borrowed(data),
            })),
            Err(e) => Err(e),
            Ok(Err(e)) => Ok(Err(e)),
        }
    }

    /// Unwrap and return the inner reader object
    ///
    /// The position of the reader is undefined.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

#[derive(Debug, Clone)]
struct CompletedPaths {
    seen: HashSet<PathBuf>,
}

impl CompletedPaths {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.seen.contains(path.as_ref())
    }

    pub fn containing_dirs<'a>(
        path: &'a (impl AsRef<Path> + ?Sized),
    ) -> impl Iterator<Item = &'a Path> {
        let is_dir = path.as_ref().to_string_lossy().ends_with('/');
        path.as_ref()
            .ancestors()
            .inspect(|p| {
                if p == &Path::new("/") {
                    unreachable!("did not expect absolute paths")
                }
            })
            .filter_map(move |p| {
                if &p == &path.as_ref() {
                    if is_dir {
                        Some(p)
                    } else {
                        None
                    }
                } else if p == Path::new("") {
                    None
                } else {
                    Some(p)
                }
            })
    }

    pub fn new_containing_dirs_needed<'a>(
        &self,
        path: &'a (impl AsRef<Path> + ?Sized),
    ) -> Vec<&'a Path> {
        let mut ret: Vec<_> = Self::containing_dirs(path)
            /* Assuming we are given ancestors in order from child to parent. */
            .take_while(|p| !self.contains(p))
            .collect();
        /* Get dirs in order from parent to child. */
        ret.reverse();
        ret
    }

    pub fn write_dirs<'a>(&mut self, paths: &[&'a Path]) {
        for path in paths.iter() {
            if !self.contains(path) {
                self.seen.insert(path.to_path_buf());
            }
        }
    }
}

#[derive(Debug)]
#[allow(missing_docs)]
pub enum IntermediateFile {
    Immediate(Arc<RwLock<Box<[u8]>>>, usize),
    Paging(UnsafeCell<fs::File>, PathBuf, usize),
}

unsafe impl Sync for IntermediateFile {}

impl fmt::Display for IntermediateFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let len = self.len();
        match self {
            Self::Immediate(arc, pos) => match str::from_utf8(arc.read().unwrap().as_ref()) {
                Ok(s) => write!(f, "Immediate(@{})[{}](\"{}\")", pos, s.len(), s),
                Err(_) => write!(f, "Immediate[{}](<binary>)", len),
                /* Err(_) => write!( */
                /*     f, */
                /*     "Immediate(@{})[{}](<binary> = \"{}\")", */
                /*     pos, */
                /*     arc.read().unwrap().len(), */
                /*     String::from_utf8_lossy(arc.read().unwrap().as_ref()), */
                /* ), */
            },
            Self::Paging(_, path, len) => write!(f, "Paging[{}]({})", len, path.display()),
        }
    }
}

impl IntermediateFile {
    #[allow(missing_docs)]
    pub fn len(&self) -> usize {
        match self {
            Self::Immediate(arc, _) => arc.read().unwrap().len(),
            Self::Paging(_, _, len) => *len,
        }
    }
    #[allow(missing_docs)]
    pub fn tell(&self) -> io::Result<u64> {
        match self {
            Self::Immediate(_, pos) => Ok(*pos as u64),
            Self::Paging(f, _, _) => {
                let f: &mut fs::File = unsafe { &mut *f.get() };
                Ok(f.stream_position()?)
            }
        }
    }
    #[allow(missing_docs)]
    pub fn immediate(len: usize) -> Self {
        Self::Immediate(Arc::new(RwLock::new(vec![0; len].into_boxed_slice())), 0)
    }
    #[allow(missing_docs)]
    pub fn paging(len: usize) -> io::Result<Self> {
        let f = tempfile::NamedTempFile::with_prefix("intermediate")?;
        let (mut f, path) = f.keep().unwrap();
        f.set_len(len as u64)?;
        f.rewind()?;
        Ok(Self::Paging(UnsafeCell::new(f), path, len))
    }
    #[allow(missing_docs)]
    pub fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut f = fs::File::open(path.as_ref())?;
        let len = f.seek(io::SeekFrom::End(0))?;
        f.rewind()?;
        Ok(Self::Paging(
            UnsafeCell::new(f),
            path.as_ref().to_path_buf(),
            len as usize,
        ))
    }
    #[allow(missing_docs)]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::Immediate(Arc::new(RwLock::new(bytes.into())), 0)
    }
    #[allow(missing_docs)]
    pub fn remove_backing_file(&mut self) -> io::Result<()> {
        match self {
            Self::Immediate(_, _) => Ok(()),
            Self::Paging(_, path, _) => fs::remove_file(path),
        }
    }
}

impl Clone for IntermediateFile {
    fn clone(&self) -> Self {
        let pos = self.tell().unwrap();
        /* eprintln!("cloning! {}", &self); */
        match self {
            Self::Immediate(arc, pos) => Self::Immediate(arc.clone(), *pos),
            Self::Paging(_, path, len) => {
                /* let prev_f: &mut fs::File = unsafe { &mut *prev_f.get() }; */
                /* prev_f.sync_data().unwrap(); */
                let mut new_f = fs::OpenOptions::new().read(true).open(&path).unwrap();
                new_f.seek(io::SeekFrom::Start(pos)).unwrap();
                Self::Paging(UnsafeCell::new(new_f), path.clone(), *len)
            }
        }
    }
}

impl io::Read for IntermediateFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Immediate(arc, pos) => {
                let beg = *pos;
                let full_len = arc.read().unwrap().as_ref().len();
                assert!(full_len >= beg);
                let end = cmp::min(beg + buf.len(), full_len);
                let src = &arc.read().unwrap()[beg..end];
                let cur_len = src.len();
                buf[..cur_len].copy_from_slice(src);
                *pos += cur_len;
                Ok(cur_len)
            }
            Self::Paging(file, _, _) => file.get_mut().read(buf),
        }
    }
}

impl io::Seek for IntermediateFile {
    fn seek(&mut self, pos_arg: io::SeekFrom) -> io::Result<u64> {
        let len = self.len();
        match self {
            Self::Immediate(_, pos) => {
                match pos_arg {
                    io::SeekFrom::Start(s) => {
                        *pos = s as usize;
                    }
                    io::SeekFrom::End(from_end) => {
                        *pos = ((len as isize) + from_end as isize) as usize;
                    }
                    io::SeekFrom::Current(from_cur) => {
                        *pos = ((*pos as isize) + from_cur as isize) as usize;
                    }
                };
                Ok(*pos as u64)
            }
            Self::Paging(file, _, _) => file.get_mut().seek(pos_arg),
        }
    }
}

impl io::Write for IntermediateFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let full_len = self.len();
        match self {
            Self::Immediate(arc, pos) => {
                let beg = *pos;
                assert!(beg <= full_len);
                let end = cmp::min(beg + buf.len(), full_len);
                let cur_len = end - beg;
                arc.write().unwrap()[beg..end].copy_from_slice(&buf[..cur_len]);
                *pos += cur_len;
                Ok(cur_len)
            }
            Self::Paging(file, _, _) => file.get_mut().write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Immediate(_, _) => Ok(()),
            Self::Paging(file, _, _) => file.get_mut().flush(),
        }
    }
}

static NUM_CPUS: Lazy<usize> = Lazy::new(|| match std::thread::available_parallelism() {
    Ok(x) => x.into(),
    /* Default to 2 if any error occurs. */
    Err(_) => 2,
});

fn build_thread_pool(n: Option<usize>, prefix: &str) -> rayon::ThreadPool {
    let prefix = prefix.to_string();
    rayon::ThreadPoolBuilder::new()
        .num_threads(n.unwrap_or(*NUM_CPUS))
        .thread_name(move |i| format!("{}: {}", &prefix, i))
        .build()
        .unwrap()
}

impl<R: Read + io::Seek + Send + Sync + Clone> ZipArchive<R> {
    /// Extract a Zip archive into a directory, overwriting files if they
    /// already exist. Paths are sanitized with [`ZipFile::enclosed_name`].
    ///
    /// Extraction is not atomic; If an error is encountered, some of the files
    /// may be left on disk.
    pub fn extract_pipelined<P: AsRef<Path>>(&self, directory: P) -> ZipResult<()> {
        use rayon::prelude::*;

        use std::sync::mpsc;

        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;

        let (paths_tx, paths_rx) = mpsc::channel::<&Path>();
        let (dirs_task_tx, dirs_task_rx) = mpsc::channel::<ZipResult<()>>();
        let (stops_prior_tx, stops_prior_rx) = mpsc::sync_channel::<Vec<(&ZipFileData, &Path)>>(1);
        let (stops_tx, stops_rx) =
            mpsc::sync_channel::<(&ZipFileData, &Path, IntermediateFile)>(200);
        let (processed_tx, processed_rx) =
            mpsc::sync_channel::<(&ZipFileData, &Path, IntermediateFile)>(200);

        static TOP_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| build_thread_pool(Some(64), "TOP"));
        static STOPS_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| build_thread_pool(None, "stops"));
        static READER_POOL: Lazy<rayon::ThreadPool> =
            Lazy::new(|| build_thread_pool(None, "reader"));
        static WRITER_POOL: Lazy<rayon::ThreadPool> =
            Lazy::new(|| build_thread_pool(None, "writer"));
        static EXTRACTOR_POOL: Lazy<rayon::ThreadPool> =
            Lazy::new(|| build_thread_pool(None, "extractor"));
        static DIR_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| build_thread_pool(None, "dir"));

        let completed_paths = Arc::new(RwLock::new(CompletedPaths::new()));
        let completed_paths2 = Arc::clone(&completed_paths);

        let shared = &self.shared;
        /* eprintln!("here1"); */
        let reader = self.reader.clone();

        let dirs_task_tx2 = dirs_task_tx.clone();
        TOP_POOL.in_place_scope(move |s| {
            let directory = directory;
            let directory2 = directory.clone();

            let dirs_task_tx3 = dirs_task_tx2.clone();
            /* (1) Collect a plan of where we'll need to seek and read in the underlying reader. */
            s.spawn(move |_| {
                dirs_task_tx3
                    .send(STOPS_POOL.install(move || {
                        let entries: Vec<_> = shared
                            .files
                            .par_iter()
                            .map(|data| {
                                data.enclosed_name()
                                    .ok_or(ZipError::InvalidArchive("Invalid file path"))
                                    .map(|relative_path| (data, relative_path))
                            })
                            .collect::<Result<Vec<_>, ZipError>>()?;

                        let stops: Vec<_> = entries
                            .into_par_iter()
                            .inspect(move |(_, relative_path)| {
                                paths_tx.send(relative_path).expect("paths_rx hung up!");
                            })
                            .filter(|(_, relative_path)| {
                                !relative_path.to_string_lossy().ends_with('/')
                            })
                            .collect();

                        stops_prior_tx
                            .try_send(stops)
                            .expect("expected send to work without blocking");
                        Ok::<_, ZipError>(())
                    }))
                    .expect("dirs_task_rx hung up! -1")
            });

            let dirs_task_tx3 = dirs_task_tx2.clone();
            s.spawn(move |_| {
                dirs_task_tx3
                    .send(READER_POOL.install(move || {
                        let stops = stops_prior_rx.recv().expect("stops_prior_tx hung up!");

                        /* (2) Execute the seek plan by splitting up the reader's extent into N contiguous
                         *     chunks. */
                        let mut chunk_size = stops.len() / *NUM_CPUS;
                        if chunk_size == 0 {
                            chunk_size = stops.len();
                        }

                        /* eprintln!("here2"); */
                        stops
                            .par_chunks(chunk_size)
                            .map(|chunk| (chunk.to_vec(), reader.clone()))
                            .try_for_each(move |(chunk, mut reader)| {
                                for (data, relative_path) in chunk.into_iter() {
                                    /* eprintln!("%%%%%%%%%"); */
                                    /* dbg!(relative_path); */

                                    let mut reader = find_content(data, &mut reader)?;

                                    /* eprintln!("2: %%%%%%%%%"); */
                                    /* reader.seek(io::SeekFrom::Start(start))?; */
                                    /* reader.read_exact(buf)?; */

                                    /* eprintln!("buf.len() = {}", buf.len()); */
                                    /* eprintln!( */
                                    /*     "buf[..20] = {:?}", */
                                    /*     &buf[..20], */
                                    /*     /\* String::from_utf8_lossy(&buf[..20]), *\/ */
                                    /* ); */

                                    /* eprintln!("3: %%%%%%%%%"); */
                                    const SPOOL_THRESHOLD: usize = 2_000;
                                    let len = data.uncompressed_size as usize;
                                    let mut outfile = if len < SPOOL_THRESHOLD {
                                        IntermediateFile::immediate(len)
                                    } else {
                                        IntermediateFile::paging(len)?
                                    };
                                    /* eprintln!("4: %%%%%%%%%"); */
                                    io::copy(&mut reader, &mut outfile)?;
                                    /* eprintln!("5: %%%%%%%%%"); */
                                    outfile.rewind()?;

                                    /* eprintln!("@{}", &outfile); */

                                    match stops_tx.send((data, relative_path, outfile)) {
                                        Ok(()) => {
                                            /* eprintln!("DONE: %% {}", relative_path.display()); */
                                        }
                                        Err(mpsc::SendError((_, relative_path, _))) => {
                                            panic!(
                                                "stops_rx hung up! was: {}",
                                                relative_path.display(),
                                            );
                                        }
                                    }
                                }
                                Ok::<_, ZipError>(())
                            })?;
                        Ok(())
                    }))
                    .expect("dirs_task_rx hung up!0");
            });

            s.spawn(move |_| {
                /* (0) create dirs/??? */
                dirs_task_tx
                    .send(DIR_POOL.install(move || {
                        let completed_paths2 = Arc::clone(&completed_paths);
                        paths_rx
                            .into_iter()
                            .par_bridge()
                            .map(move |relative_path| {
                                completed_paths2
                                    .read()
                                    .unwrap()
                                    .new_containing_dirs_needed(relative_path)
                            })
                            .filter(|new_dirs| !new_dirs.is_empty())
                            .try_for_each(move |new_dirs| {
                                for d in new_dirs.iter() {
                                    let outpath = directory2.join(d);
                                    match fs::create_dir(outpath) {
                                        Ok(()) => (),
                                        Err(e) => {
                                            if e.kind() == io::ErrorKind::AlreadyExists {
                                                /* ignore */
                                            } else {
                                                return Err(e.into());
                                            }
                                        }
                                    }
                                }

                                completed_paths.write().unwrap().write_dirs(&new_dirs[..]);
                                Ok::<_, ZipError>(())
                            })
                    }))
                    .expect("dirs_task_rx hung up!1");
            });

            let dirs_task_tx3 = dirs_task_tx2.clone();
            s.spawn(move |_| {
                dirs_task_tx2
                    .send(WRITER_POOL.install(move || {
                        /* dbg!("wtf"); */
                        stops_rx.into_iter().par_bridge().try_for_each(
                            move |(data, relative_path, source_handle)| {
                                /* eprintln!("0: @@@@@@"); */
                                /* eprintln!( */
                                /*     "@: {}/{}/{}", */
                                /*     relative_path.display(), */
                                /*     data.compressed_size, */
                                /*     &source_handle, */
                                /* ); */

                                let mut decompress_reader =
                                    ZipEntry::from_data(data, source_handle);

                                /* eprintln!("1: @@@@@@@@"); */

                                const UNCOMPRESSED_SPOOL_THRESHOLD: usize = 100_000;
                                let len = data.uncompressed_size as usize;
                                let mut outfile = if len < UNCOMPRESSED_SPOOL_THRESHOLD {
                                    IntermediateFile::immediate(len)
                                } else {
                                    IntermediateFile::paging(len)?
                                };
                                /* NB: this may decompress, which may take a lot of cpu! */
                                io::copy(&mut decompress_reader, &mut outfile)?;
                                /* eprintln!("2: @@@@@@@@"); */
                                outfile.rewind()?;

                                /* decompress_reader.into_inner().remove_backing_file()?; */

                                /* eprintln!("+++++++++"); */

                                processed_tx
                                    .send((data, relative_path, outfile))
                                    .expect("processed_rx hung up!");

                                /* eprintln!("#########"); */

                                Ok::<_, ZipError>(())
                            },
                        )?;

                        /* eprintln!("huh???"); */

                        Ok::<_, ZipError>(())
                    }))
                    .expect("dirs_task_rx hung up!2");
            });

            s.spawn(move |_| {
                let directory = directory; /* Move. */
                /* (4) extract/??? */
                dirs_task_tx3
                    .send(EXTRACTOR_POOL.install(move || {
                        processed_rx.into_iter().par_bridge().try_for_each(
                            move |(data, relative_path, mut file)| {
                                let outpath = directory.join(relative_path);
                                /* dbg!(&outpath); */
                                let mut outfile = match fs::File::create(&outpath) {
                                    Ok(f) => f,
                                    Err(e) => {
                                        if e.kind() == io::ErrorKind::NotFound {
                                            /* Somehow, the containing dir didn't
                                             * exist. Let's make it ourself and
                                             * enter it into the registry. */
                                            let new_dirs = completed_paths2
                                                .read()
                                                .unwrap()
                                                .new_containing_dirs_needed(&relative_path);
                                            /* dbg!(&new_dirs); */

                                            for d in new_dirs.iter() {
                                                let outpath = directory.join(d);
                                                match fs::create_dir(outpath) {
                                                    Ok(()) => (),
                                                    Err(e) => {
                                                        if e.kind() == io::ErrorKind::AlreadyExists
                                                        {
                                                            /* ignore */
                                                        } else {
                                                            return Err(e.into());
                                                        }
                                                    }
                                                }
                                            }

                                            if !new_dirs.is_empty() {
                                                completed_paths2
                                                    .write()
                                                    .unwrap()
                                                    .write_dirs(&new_dirs[..]);
                                            }

                                            fs::File::create(&outpath)?
                                        } else {
                                            return Err(e.into());
                                        }
                                    }
                                };
                                /* eprintln!("&&&&&&&&&&"); */
                                io::copy(&mut file, &mut outfile)?;
                                file.remove_backing_file()?;
                                // Set permissions
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    if let Some(mode) = data.unix_mode() {
                                        outfile
                                            .set_permissions(fs::Permissions::from_mode(mode))?;
                                    }
                                }
                                Ok::<_, ZipError>(())
                            },
                        )
                    }))
                    .expect("dirs_task_rx hung up!3");
            });
            Ok::<_, ZipError>(())
        })?;
        for result in dirs_task_rx.into_iter() {
            result?;
        }
        Ok(())
    }
}

fn unsupported_zip_error<T>(detail: &'static str) -> ZipResult<T> {
    Err(ZipError::UnsupportedArchive(detail))
}

/// Parse a central directory entry to collect the information for the file.
pub(crate) fn central_header_to_zip_file<R: Read + io::Seek>(
    reader: &mut R,
    archive_offset: u64,
) -> ZipResult<ZipFileData> {
    let central_header_start = reader.stream_position()?;

    // Parse central header
    let signature = reader.read_u32::<LittleEndian>()?;
    if signature != spec::CENTRAL_DIRECTORY_HEADER_SIGNATURE {
        Err(ZipError::InvalidArchive("Invalid Central Directory header"))
    } else {
        central_header_to_zip_file_inner(reader, archive_offset, central_header_start)
    }
}

/// Parse a central directory entry to collect the information for the file.
fn central_header_to_zip_file_inner<R: Read>(
    reader: &mut R,
    archive_offset: u64,
    central_header_start: u64,
) -> ZipResult<ZipFileData> {
    let version_made_by = reader.read_u16::<LittleEndian>()?;
    let _version_to_extract = reader.read_u16::<LittleEndian>()?;
    let flags = reader.read_u16::<LittleEndian>()?;
    let encrypted = flags & 1 == 1;
    let is_utf8 = flags & (1 << 11) != 0;
    let using_data_descriptor = flags & (1 << 3) != 0;
    let compression_method = reader.read_u16::<LittleEndian>()?;
    let last_mod_time = reader.read_u16::<LittleEndian>()?;
    let last_mod_date = reader.read_u16::<LittleEndian>()?;
    let crc32 = reader.read_u32::<LittleEndian>()?;
    let compressed_size = reader.read_u32::<LittleEndian>()?;
    let uncompressed_size = reader.read_u32::<LittleEndian>()?;
    let file_name_length = reader.read_u16::<LittleEndian>()? as usize;
    let extra_field_length = reader.read_u16::<LittleEndian>()? as usize;
    let file_comment_length = reader.read_u16::<LittleEndian>()? as usize;
    let _disk_number = reader.read_u16::<LittleEndian>()?;
    let _internal_file_attributes = reader.read_u16::<LittleEndian>()?;
    let external_file_attributes = reader.read_u32::<LittleEndian>()?;
    let offset = reader.read_u32::<LittleEndian>()? as u64;
    let mut file_name_raw = vec![0; file_name_length];
    reader.read_exact(&mut file_name_raw)?;
    let mut extra_field = vec![0; extra_field_length];
    reader.read_exact(&mut extra_field)?;
    let mut file_comment_raw = vec![0; file_comment_length];
    reader.read_exact(&mut file_comment_raw)?;

    let file_name = match is_utf8 {
        true => String::from_utf8_lossy(&file_name_raw).into_owned(),
        false => file_name_raw.clone().from_cp437(),
    };
    let file_comment = match is_utf8 {
        true => String::from_utf8_lossy(&file_comment_raw).into_owned(),
        false => file_comment_raw.from_cp437(),
    };

    // Construct the result
    let mut result = ZipFileData {
        system: System::from_u8((version_made_by >> 8) as u8),
        version_made_by: version_made_by as u8,
        encrypted,
        using_data_descriptor,
        compression_method: {
            #[allow(deprecated)]
            CompressionMethod::from_u16(compression_method)
        },
        compression_level: None,
        last_modified_time: DateTime::from_msdos(last_mod_date, last_mod_time),
        crc32,
        compressed_size: compressed_size as u64,
        uncompressed_size: uncompressed_size as u64,
        file_name,
        file_name_raw,
        extra_field,
        file_comment,
        header_start: offset,
        central_header_start,
        data_start: AtomicU64::new(0),
        external_attributes: external_file_attributes,
        large_file: false,
        aes_mode: None,
    };

    match parse_extra_field(&mut result) {
        Ok(..) | Err(ZipError::Io(..)) => {}
        Err(e) => return Err(e),
    }

    let aes_enabled = result.compression_method == CompressionMethod::AES;
    if aes_enabled && result.aes_mode.is_none() {
        return Err(ZipError::InvalidArchive(
            "AES encryption without AES extra data field",
        ));
    }

    // Account for shifted zip offsets.
    result.header_start = result
        .header_start
        .checked_add(archive_offset)
        .ok_or(ZipError::InvalidArchive("Archive header is too large"))?;

    Ok(result)
}

fn parse_extra_field(file: &mut ZipFileData) -> ZipResult<()> {
    let mut reader = io::Cursor::new(&file.extra_field);

    while (reader.position() as usize) < file.extra_field.len() {
        let kind = reader.read_u16::<LittleEndian>()?;
        let len = reader.read_u16::<LittleEndian>()?;
        let mut len_left = len as i64;
        match kind {
            // Zip64 extended information extra field
            0x0001 => {
                if file.uncompressed_size == spec::ZIP64_BYTES_THR {
                    file.large_file = true;
                    file.uncompressed_size = reader.read_u64::<LittleEndian>()?;
                    len_left -= 8;
                }
                if file.compressed_size == spec::ZIP64_BYTES_THR {
                    file.large_file = true;
                    file.compressed_size = reader.read_u64::<LittleEndian>()?;
                    len_left -= 8;
                }
                if file.header_start == spec::ZIP64_BYTES_THR {
                    file.header_start = reader.read_u64::<LittleEndian>()?;
                    len_left -= 8;
                }
            }
            0x9901 => {
                // AES
                if len != 7 {
                    return Err(ZipError::UnsupportedArchive(
                        "AES extra data field has an unsupported length",
                    ));
                }
                let vendor_version = reader.read_u16::<LittleEndian>()?;
                let vendor_id = reader.read_u16::<LittleEndian>()?;
                let aes_mode = reader.read_u8()?;
                let compression_method = reader.read_u16::<LittleEndian>()?;

                if vendor_id != 0x4541 {
                    return Err(ZipError::InvalidArchive("Invalid AES vendor"));
                }
                let vendor_version = match vendor_version {
                    0x0001 => AesVendorVersion::Ae1,
                    0x0002 => AesVendorVersion::Ae2,
                    _ => return Err(ZipError::InvalidArchive("Invalid AES vendor version")),
                };
                match aes_mode {
                    0x01 => file.aes_mode = Some((AesMode::Aes128, vendor_version)),
                    0x02 => file.aes_mode = Some((AesMode::Aes192, vendor_version)),
                    0x03 => file.aes_mode = Some((AesMode::Aes256, vendor_version)),
                    _ => return Err(ZipError::InvalidArchive("Invalid AES encryption strength")),
                };
                file.compression_method = {
                    #[allow(deprecated)]
                    CompressionMethod::from_u16(compression_method)
                };
            }
            _ => {
                // Other fields are ignored
            }
        }

        // We could also check for < 0 to check for errors
        if len_left > 0 {
            reader.seek(io::SeekFrom::Current(len_left))?;
        }
    }
    Ok(())
}

/// Methods for retrieving information on zip files
impl<'a> ZipFile<'a> {
    fn get_reader(&mut self) -> &mut ZipFileReader<'a> {
        if let ZipFileReader::NoReader = self.reader {
            let data = &self.data;
            let crypto_reader = self.crypto_reader.take().expect("Invalid reader state");
            self.reader = make_reader(data.compression_method, data.crc32, crypto_reader)
        }
        &mut self.reader
    }

    pub(crate) fn get_raw_reader(&mut self) -> &mut dyn Read {
        if let ZipFileReader::NoReader = self.reader {
            let crypto_reader = self.crypto_reader.take().expect("Invalid reader state");
            self.reader = ZipFileReader::Raw(crypto_reader.into_inner())
        }
        &mut self.reader
    }

    /// Get the version of the file
    pub fn version_made_by(&self) -> (u8, u8) {
        (
            self.data.version_made_by / 10,
            self.data.version_made_by % 10,
        )
    }

    /// Get the name of the file
    ///
    /// # Warnings
    ///
    /// It is dangerous to use this name directly when extracting an archive.
    /// It may contain an absolute path (`/etc/shadow`), or break out of the
    /// current directory (`../runtime`). Carelessly writing to these paths
    /// allows an attacker to craft a ZIP archive that will overwrite critical
    /// files.
    ///
    /// You can use the [`ZipFile::enclosed_name`] method to validate the name
    /// as a safe path.
    pub fn name(&self) -> &str {
        &self.data.file_name
    }

    /// Get the name of the file, in the raw (internal) byte representation.
    ///
    /// The encoding of this data is currently undefined.
    pub fn name_raw(&self) -> &[u8] {
        &self.data.file_name_raw
    }

    /// Get the name of the file in a sanitized form. It truncates the name to the first NULL byte,
    /// removes a leading '/' and removes '..' parts.
    #[deprecated(
        since = "0.5.7",
        note = "by stripping `..`s from the path, the meaning of paths can change.
                `mangled_name` can be used if this behaviour is desirable"
    )]
    pub fn sanitized_name(&self) -> ::std::path::PathBuf {
        self.mangled_name()
    }

    /// Rewrite the path, ignoring any path components with special meaning.
    ///
    /// - Absolute paths are made relative
    /// - [`ParentDir`]s are ignored
    /// - Truncates the filename at a NULL byte
    ///
    /// This is appropriate if you need to be able to extract *something* from
    /// any archive, but will easily misrepresent trivial paths like
    /// `foo/../bar` as `foo/bar` (instead of `bar`). Because of this,
    /// [`ZipFile::enclosed_name`] is the better option in most scenarios.
    ///
    /// [`ParentDir`]: `Component::ParentDir`
    pub fn mangled_name(&self) -> ::std::path::PathBuf {
        self.data.file_name_sanitized()
    }

    /// Ensure the file path is safe to use as a [`Path`].
    ///
    /// - It can't contain NULL bytes
    /// - It can't resolve to a path outside the current directory
    ///   > `foo/../bar` is fine, `foo/../../bar` is not.
    /// - It can't be an absolute path
    ///
    /// This will read well-formed ZIP files correctly, and is resistant
    /// to path-based exploits. It is recommended over
    /// [`ZipFile::mangled_name`].
    pub fn enclosed_name(&self) -> Option<&Path> {
        self.data.enclosed_name()
    }

    /// Get the comment of the file
    pub fn comment(&self) -> &str {
        &self.data.file_comment
    }

    /// Get the compression method used to store the file
    pub fn compression(&self) -> CompressionMethod {
        self.data.compression_method
    }

    /// Get the size of the file, in bytes, in the archive
    pub fn compressed_size(&self) -> u64 {
        self.data.compressed_size
    }

    /// Get the size of the file, in bytes, when uncompressed
    pub fn size(&self) -> u64 {
        self.data.uncompressed_size
    }

    /// Get the time the file was last modified
    pub fn last_modified(&self) -> DateTime {
        self.data.last_modified_time
    }
    /// Returns whether the file is actually a directory
    pub fn is_dir(&self) -> bool {
        self.name()
            .chars()
            .rev()
            .next()
            .map_or(false, |c| c == '/' || c == '\\')
    }

    /// Returns whether the file is a regular file
    pub fn is_file(&self) -> bool {
        !self.is_dir()
    }

    /// Get unix mode for the file
    pub fn unix_mode(&self) -> Option<u32> {
        self.data.unix_mode()
    }

    /// Get the CRC32 hash of the original file
    pub fn crc32(&self) -> u32 {
        self.data.crc32
    }

    /// Get the extra data of the zip header for this file
    pub fn extra_data(&self) -> &[u8] {
        &self.data.extra_field
    }

    /// Get the starting offset of the data of the compressed file
    pub fn data_start(&self) -> u64 {
        self.data.data_start.load()
    }

    /// Get the starting offset of the zip header for this file
    pub fn header_start(&self) -> u64 {
        self.data.header_start
    }
    /// Get the starting offset of the zip header in the central directory for this file
    pub fn central_header_start(&self) -> u64 {
        self.data.central_header_start
    }
}

impl<'a> Read for ZipFile<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.get_reader().read(buf)
    }
}

impl<'a> Drop for ZipFile<'a> {
    fn drop(&mut self) {
        // self.data is Owned, this reader is constructed by a streaming reader.
        // In this case, we want to exhaust the reader so that the next file is accessible.
        if let Cow::Owned(_) = self.data {
            let mut buffer = [0; 1 << 16];

            // Get the inner `Take` reader so all decryption, decompression and CRC calculation is skipped.
            let mut reader: std::io::Take<&mut dyn std::io::Read> = match &mut self.reader {
                ZipFileReader::NoReader => {
                    let innerreader = ::std::mem::replace(&mut self.crypto_reader, None);
                    innerreader.expect("Invalid reader state").into_inner()
                }
                reader => {
                    let innerreader = ::std::mem::replace(reader, ZipFileReader::NoReader);
                    innerreader.into_inner()
                }
            };

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => (),
                    Err(e) => {
                        panic!("Could not consume all of the output of the current ZipFile: {e:?}")
                    }
                }
            }
        }
    }
}

/// Read ZipFile structures from a non-seekable reader.
///
/// This is an alternative method to read a zip file. If possible, use the ZipArchive functions
/// as some information will be missing when reading this manner.
///
/// Reads a file header from the start of the stream. Will return `Ok(Some(..))` if a file is
/// present at the start of the stream. Returns `Ok(None)` if the start of the central directory
/// is encountered. No more files should be read after this.
///
/// The Drop implementation of ZipFile ensures that the reader will be correctly positioned after
/// the structure is done.
///
/// Missing fields are:
/// * `comment`: set to an empty string
/// * `data_start`: set to 0
/// * `external_attributes`: `unix_mode()`: will return None
pub fn read_zipfile_from_stream<'a, R: io::Read>(
    reader: &'a mut R,
) -> ZipResult<Option<ZipFile<'_>>> {
    let signature = reader.read_u32::<LittleEndian>()?;

    match signature {
        spec::LOCAL_FILE_HEADER_SIGNATURE => (),
        spec::CENTRAL_DIRECTORY_HEADER_SIGNATURE => return Ok(None),
        _ => return Err(ZipError::InvalidArchive("Invalid local file header")),
    }

    let version_made_by = reader.read_u16::<LittleEndian>()?;
    let flags = reader.read_u16::<LittleEndian>()?;
    let encrypted = flags & 1 == 1;
    let is_utf8 = flags & (1 << 11) != 0;
    let using_data_descriptor = flags & (1 << 3) != 0;
    #[allow(deprecated)]
    let compression_method = CompressionMethod::from_u16(reader.read_u16::<LittleEndian>()?);
    let last_mod_time = reader.read_u16::<LittleEndian>()?;
    let last_mod_date = reader.read_u16::<LittleEndian>()?;
    let crc32 = reader.read_u32::<LittleEndian>()?;
    let compressed_size = reader.read_u32::<LittleEndian>()?;
    let uncompressed_size = reader.read_u32::<LittleEndian>()?;
    let file_name_length = reader.read_u16::<LittleEndian>()? as usize;
    let extra_field_length = reader.read_u16::<LittleEndian>()? as usize;

    let mut file_name_raw = vec![0; file_name_length];
    reader.read_exact(&mut file_name_raw)?;
    let mut extra_field = vec![0; extra_field_length];
    reader.read_exact(&mut extra_field)?;

    let file_name = match is_utf8 {
        true => String::from_utf8_lossy(&file_name_raw).into_owned(),
        false => file_name_raw.clone().from_cp437(),
    };

    let mut result = ZipFileData {
        system: System::from_u8((version_made_by >> 8) as u8),
        version_made_by: version_made_by as u8,
        encrypted,
        using_data_descriptor,
        compression_method,
        compression_level: None,
        last_modified_time: DateTime::from_msdos(last_mod_date, last_mod_time),
        crc32,
        compressed_size: compressed_size as u64,
        uncompressed_size: uncompressed_size as u64,
        file_name,
        file_name_raw,
        extra_field,
        file_comment: String::new(), // file comment is only available in the central directory
        // header_start and data start are not available, but also don't matter, since seeking is
        // not available.
        header_start: 0,
        data_start: AtomicU64::new(0),
        central_header_start: 0,
        // The external_attributes field is only available in the central directory.
        // We set this to zero, which should be valid as the docs state 'If input came
        // from standard input, this field is set to zero.'
        external_attributes: 0,
        large_file: false,
        aes_mode: None,
    };

    match parse_extra_field(&mut result) {
        Ok(..) | Err(ZipError::Io(..)) => {}
        Err(e) => return Err(e),
    }

    if encrypted {
        return unsupported_zip_error("Encrypted files are not supported");
    }
    if using_data_descriptor {
        return unsupported_zip_error("The file length is not available in the local header");
    }

    let limit_reader = (reader as &'a mut dyn io::Read).take(result.compressed_size);

    let result_crc32 = result.crc32;
    let result_compression_method = result.compression_method;
    let crypto_reader = make_crypto_reader(
        result_compression_method,
        result_crc32,
        result.last_modified_time,
        result.using_data_descriptor,
        limit_reader,
        None,
        None,
        #[cfg(feature = "aes-crypto")]
        result.compressed_size,
    )?
    .unwrap();

    Ok(Some(ZipFile {
        data: Cow::Owned(result),
        crypto_reader: None,
        reader: make_reader(result_compression_method, result_crc32, crypto_reader),
    }))
}

#[cfg(test)]
mod test {
    #[test]
    fn invalid_offset() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/invalid_offset.zip"));
        let reader = ZipArchive::new(io::Cursor::new(v));
        assert!(reader.is_err());
    }

    #[test]
    fn invalid_offset2() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/invalid_offset2.zip"));
        let reader = ZipArchive::new(io::Cursor::new(v));
        assert!(reader.is_err());
    }

    #[test]
    fn zip64_with_leading_junk() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/zip64_demo.zip"));
        let reader = ZipArchive::new(io::Cursor::new(v)).unwrap();
        assert_eq!(reader.len(), 1);
    }

    #[test]
    fn zip_contents() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/mimetype.zip"));
        let mut reader = ZipArchive::new(io::Cursor::new(v)).unwrap();
        assert_eq!(reader.comment(), b"");
        assert_eq!(reader.by_index(0).unwrap().central_header_start(), 77);
    }

    #[test]
    fn zip_read_streaming() {
        use super::read_zipfile_from_stream;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/mimetype.zip"));
        let mut reader = io::Cursor::new(v);
        loop {
            if read_zipfile_from_stream(&mut reader).unwrap().is_none() {
                break;
            }
        }
    }

    #[test]
    fn zip_clone() {
        use super::ZipArchive;
        use std::io::{self, Read};

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/mimetype.zip"));
        let mut reader1 = ZipArchive::new(io::Cursor::new(v)).unwrap();
        let mut reader2 = reader1.clone();

        let mut file1 = reader1.by_index(0).unwrap();
        let mut file2 = reader2.by_index(0).unwrap();

        let t = file1.last_modified();
        assert_eq!(
            (
                t.year(),
                t.month(),
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            ),
            (1980, 1, 1, 0, 0, 0)
        );

        let mut buf1 = [0; 5];
        let mut buf2 = [0; 5];
        let mut buf3 = [0; 5];
        let mut buf4 = [0; 5];

        file1.read_exact(&mut buf1).unwrap();
        file2.read_exact(&mut buf2).unwrap();
        file1.read_exact(&mut buf3).unwrap();
        file2.read_exact(&mut buf4).unwrap();

        assert_eq!(buf1, buf2);
        assert_eq!(buf3, buf4);
        assert_ne!(buf1, buf3);
    }

    #[test]
    fn file_and_dir_predicates() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("../tests/data/files_and_dirs.zip"));
        let mut zip = ZipArchive::new(io::Cursor::new(v)).unwrap();

        for i in 0..zip.len() {
            let zip_file = zip.by_index(i).unwrap();
            let full_name = zip_file.enclosed_name().unwrap();
            let file_name = full_name.file_name().unwrap().to_str().unwrap();
            assert!(
                (file_name.starts_with("dir") && zip_file.is_dir())
                    || (file_name.starts_with("file") && zip_file.is_file())
            );
        }
    }

    /// test case to ensure we don't preemptively over allocate based on the
    /// declared number of files in the CDE of an invalid zip when the number of
    /// files declared is more than the alleged offset in the CDE
    #[test]
    fn invalid_cde_number_of_files_allocation_smaller_offset() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!(
            "../tests/data/invalid_cde_number_of_files_allocation_smaller_offset.zip"
        ));
        let reader = ZipArchive::new(io::Cursor::new(v));
        assert!(reader.is_err());
    }

    /// test case to ensure we don't preemptively over allocate based on the
    /// declared number of files in the CDE of an invalid zip when the number of
    /// files declared is less than the alleged offset in the CDE
    #[test]
    fn invalid_cde_number_of_files_allocation_greater_offset() {
        use super::ZipArchive;
        use std::io;

        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!(
            "../tests/data/invalid_cde_number_of_files_allocation_greater_offset.zip"
        ));
        let reader = ZipArchive::new(io::Cursor::new(v));
        assert!(reader.is_err());
    }
}
