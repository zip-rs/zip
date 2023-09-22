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

use std::cmp;
use std::collections::HashMap;
use std::io::{self, prelude::*};
use std::mem;
use std::ops;
use std::path::Path;
use std::sync::Arc;

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
    ///         std::io::copy(&mut file, &mut std::io::stdout())?;
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

pub(crate) enum StreamDropBehavior {
    NoOp,
    DriveToEnd,
}

pub(crate) struct Limiter<S>
where
    S: Read,
{
    source_extent: ops::Range<usize>,
    internal_pos: usize,
    source_stream: S,
    drop_behavior: StreamDropBehavior,
}

impl<S> Limiter<S>
where
    S: Read,
{
    pub(crate) fn take(
        source_pos: usize,
        source_stream: S,
        limit: usize,
        drop_behavior: StreamDropBehavior,
    ) -> Self {
        Self {
            source_extent: ops::Range {
                start: source_pos,
                end: source_pos + limit,
            },
            internal_pos: 0,
            source_stream,
            drop_behavior,
        }
    }

    #[inline]
    fn full_len(&self) -> usize {
        self.source_extent.end - self.source_extent.start
    }

    #[inline]
    fn remaining_len(&self) -> usize {
        self.full_len() - self.internal_pos
    }

    #[inline]
    fn limit_length(&self, requested_length: usize) -> usize {
        cmp::min(self.remaining_len(), requested_length)
    }

    #[inline]
    fn push_cursor(&mut self, len: usize) {
        assert!(len <= self.remaining_len());
        self.internal_pos += len;
    }
}

impl<S> ops::Drop for Limiter<S>
where
    S: Read,
{
    fn drop(&mut self) {
        match self.drop_behavior {
            StreamDropBehavior::DriveToEnd => {
                let mut buf: Vec<u8> = Vec::new();
                self.read_to_end(&mut buf).unwrap();
            }
            StreamDropBehavior::NoOp => (),
        }
    }
}

impl<S> Read for Limiter<S>
where
    S: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let num_bytes_to_read: usize = self.limit_length(buf.len());
        if num_bytes_to_read == 0 {
            return Ok(0);
        }

        let bytes_read = self.source_stream.read(&mut buf[..num_bytes_to_read])?;
        assert!(bytes_read <= num_bytes_to_read);
        if bytes_read > 0 {
            self.push_cursor(bytes_read);
        }
        Ok(bytes_read)
    }
}

impl<S> AsRef<S> for Limiter<S>
where
    S: Read,
{
    fn as_ref(&self) -> &S {
        &self.source_stream
    }
}

impl<S> AsMut<S> for Limiter<S>
where
    S: Read,
{
    fn as_mut(&mut self) -> &mut S {
        &mut self.source_stream
    }
}

impl<S> ops::Deref for Limiter<S>
where
    S: Read,
{
    type Target = S;
    fn deref(&self) -> &S {
        &self.source_stream
    }
}

impl<S> ops::DerefMut for Limiter<S>
where
    S: Read,
{
    fn deref_mut(&mut self) -> &mut S {
        &mut self.source_stream
    }
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum CryptoReader<S>
where
    S: Read,
{
    Plaintext(Limiter<S>),
    ZipCrypto(ZipCryptoReaderValid<Limiter<S>>),
    #[cfg(feature = "aes-crypto")]
    Aes {
        reader: AesReaderValid<Limiter<S>>,
        vendor_version: AesVendorVersion,
    },
}

impl<S> Read for CryptoReader<S>
where
    S: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            CryptoReader::Plaintext(r) => r.read(buf),
            CryptoReader::ZipCrypto(r) => r.read(buf),
            #[cfg(feature = "aes-crypto")]
            CryptoReader::Aes { reader: r, .. } => r.read(buf),
        }
    }
}

impl<S> CryptoReader<S>
where
    S: Read,
{
    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> Limiter<S> {
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

pub(crate) enum ZipFileReader<S>
where
    S: Read,
{
    Raw(Limiter<S>),
    /* TODO: introduce an option to avoid checking CRCs? what about an option that allows for
     * seeking by buffering each entry in memory as it's first read out? */
    Stored(Crc32Reader<CryptoReader<S>>),
    #[cfg(any(
        feature = "deflate",
        feature = "deflate-miniz",
        feature = "deflate-zlib"
    ))]
    Deflated(Crc32Reader<flate2::read::DeflateDecoder<CryptoReader<S>>>),
    #[cfg(feature = "bzip2")]
    Bzip2(Crc32Reader<BzDecoder<CryptoReader<S>>>),
    #[cfg(feature = "zstd")]
    Zstd(Crc32Reader<ZstdDecoder<'static, io::BufReader<CryptoReader<S>>>>),
}

impl<S> Read for ZipFileReader<S>
where
    S: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
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

impl<S> ZipFileReader<S>
where
    S: Read,
{
    pub fn convert_to_raw(&mut self) {
        /* Do nothing if already raw. */
        if let Self::Raw(_) = self {
            return;
        }
        /* Create another instance on the stack so we can call .into_inner(). */
        let mut other = mem::MaybeUninit::<Self>::zeroed();
        mem::swap(self, unsafe { &mut *other.as_mut_ptr() });
        mem::forget(mem::replace(
            self,
            Self::Raw(unsafe { other.assume_init() }.into_inner()),
        ));
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> Limiter<S> {
        match self {
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
pub struct ZipFile<S>
where
    S: Read,
{
    /* TODO: use some shared ref like Arc here! */
    data: ZipFileData,
    wrapped_reader: ZipFileReader<S>,
}

fn find_content<S>(data: &ZipFileData, mut reader: S) -> ZipResult<Limiter<S>>
where
    S: Read + io::Seek,
{
    // Parse local header
    reader.seek(io::SeekFrom::Start(data.header_start))?;
    let signature = reader.read_u32::<LittleEndian>()?;
    if signature != spec::LOCAL_FILE_HEADER_SIGNATURE {
        return Err(ZipError::InvalidArchive("Invalid local file header"));
    }

    reader.seek(io::SeekFrom::Current(22))?;
    let file_name_length = reader.read_u16::<LittleEndian>()? as u64;
    /* NB: zip files have separate local and central extra data records. The length of the local
     * extra field is being parsed here. The value of this field cannot be inferred from the
     * central record data. */
    let extra_field_length = reader.read_u16::<LittleEndian>()? as u64;
    let magic_and_header = 4 + 22 + 2 + 2;
    let data_start = data.header_start + magic_and_header + file_name_length + extra_field_length;
    data.data_start.store(data_start);

    reader.seek(io::SeekFrom::Start(data_start))?;
    Ok(Limiter::take(
        data_start as usize,
        reader,
        data.compressed_size as usize,
        StreamDropBehavior::NoOp,
    ))
}

#[allow(clippy::too_many_arguments)]
fn make_crypto_reader<S>(
    compression_method: crate::compression::CompressionMethod,
    crc32: u32,
    last_modified_time: DateTime,
    using_data_descriptor: bool,
    reader: Limiter<S>,
    password: Option<&[u8]>,
    aes_info: Option<(AesMode, AesVendorVersion)>,
    #[cfg(feature = "aes-crypto")] compressed_size: u64,
) -> ZipResult<Result<CryptoReader<S>, InvalidPassword>>
where
    S: Read,
{
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

fn make_reader<S>(
    compression_method: CompressionMethod,
    crc32: u32,
    reader: CryptoReader<S>,
) -> ZipFileReader<S>
where
    S: Read,
{
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
        use std::fs;

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
    pub fn by_name_decrypt(
        &mut self,
        name: &str,
        password: &[u8],
    ) -> ZipResult<Result<ZipFile<&mut R>, InvalidPassword>> {
        self.by_name_with_optional_password(name, Some(password))
    }

    /// Search for a file entry by name
    pub fn by_name(&mut self, name: &str) -> ZipResult<ZipFile<&mut R>> {
        Ok(self.by_name_with_optional_password(name, None)?.unwrap())
    }

    fn by_name_with_optional_password(
        &mut self,
        name: &str,
        password: Option<&[u8]>,
    ) -> ZipResult<Result<ZipFile<&mut R>, InvalidPassword>> {
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
    pub fn by_index_decrypt(
        &mut self,
        file_number: usize,
        password: &[u8],
    ) -> ZipResult<Result<ZipFile<&mut R>, InvalidPassword>> {
        self.by_index_with_optional_password(file_number, Some(password))
    }

    /// Get a contained file by index
    pub fn by_index(&mut self, file_number: usize) -> ZipResult<ZipFile<&mut R>> {
        Ok(self
            .by_index_with_optional_password(file_number, None)?
            .unwrap())
    }

    /// Get a contained file by index without decompressing it
    pub fn by_index_raw(&mut self, file_number: usize) -> ZipResult<ZipFile<&mut R>> {
        let reader = &mut self.reader;
        self.shared
            .files
            .get(file_number)
            .ok_or(ZipError::FileNotFound)
            .and_then(move |data| {
                Ok(ZipFile::new(
                    data.clone(),
                    ZipFileReader::Raw(find_content(data, reader)?),
                ))
            })
    }

    fn by_index_with_optional_password(
        &mut self,
        file_number: usize,
        mut password: Option<&[u8]>,
    ) -> ZipResult<Result<ZipFile<&mut R>, InvalidPassword>> {
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
            Ok(Ok(crypto_reader)) => {
                let wrapped_reader =
                    make_reader(data.compression_method, data.crc32, crypto_reader);
                Ok(Ok(ZipFile::new(data.clone(), wrapped_reader)))
            }
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
impl<S> ZipFile<S>
where
    S: Read,
{
    pub(crate) fn new(data: ZipFileData, source: ZipFileReader<S>) -> Self {
        Self {
            data,
            wrapped_reader: source,
        }
    }

    fn get_reader(&mut self) -> &mut ZipFileReader<S> {
        &mut self.wrapped_reader
    }

    pub(crate) fn get_raw_reader(&mut self) -> &mut ZipFileReader<S> {
        self.wrapped_reader.convert_to_raw();
        self.get_reader()
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

impl<S> Read for ZipFile<S>
where
    S: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.get_reader().read(buf)
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
pub fn read_zipfile_from_stream<S: io::Read>(mut reader: S) -> ZipResult<Option<ZipFile<S>>> {
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

    let limit_reader = Limiter::take(
        0,
        reader,
        result.compressed_size as usize,
        StreamDropBehavior::DriveToEnd,
    );

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

    Ok(Some(ZipFile::new(
        result,
        make_reader(result_compression_method, result_crc32, crypto_reader),
    )))
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
