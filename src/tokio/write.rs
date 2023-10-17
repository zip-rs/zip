use crate::{
    compression::CompressionMethod,
    result::{ZipError, ZipResult},
    spec,
    tokio::{
        buf_writer::BufWriter,
        read::{read_spec, Shared, ZipArchive},
        stream_impls::deflate,
        utils::map_swap_uninit,
        WrappedPin,
    },
    types::{ZipFileData, DEFAULT_VERSION},
    write::{FileOptions, ZipRawValues, ZipWriterStats},
};

use tokio::io::{self, AsyncSeekExt, AsyncWriteExt};

use std::{
    cmp, mem, ops,
    pin::Pin,
    ptr,
    task::{ready, Context, Poll},
};

#[cfg(any(
    feature = "deflate",
    feature = "deflate-miniz",
    feature = "deflate-zlib"
))]
use flate2::{Compress, Compression};

pub struct StoredWriter<S>(Pin<Box<S>>);

impl<S> StoredWriter<S> {
    pub fn new(inner: Pin<Box<S>>) -> Self {
        Self(inner)
    }

    #[inline]
    fn pin_stream(self: Pin<&mut Self>) -> Pin<&mut S> {
        unsafe { self.get_unchecked_mut() }.0.as_mut()
    }
}

impl<S> WrappedPin<S> for StoredWriter<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        self.0
    }
}

impl<S: io::AsyncWrite> io::AsyncWrite for StoredWriter<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pin_stream().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pin_stream().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pin_stream().poll_shutdown(cx)
    }
}

pub struct DeflatedWriter<S>(deflate::Writer<Compress, BufWriter<S>>);

impl<S> DeflatedWriter<S> {
    #[inline]
    fn pin_stream(self: Pin<&mut Self>) -> Pin<&mut deflate::Writer<Compress, BufWriter<S>>> {
        unsafe { self.map_unchecked_mut(|Self(inner)| inner) }
    }
}

impl<S: io::AsyncWrite> DeflatedWriter<S> {
    pub fn new(compression: Compression, inner: Pin<Box<S>>) -> Self {
        let compress = Compress::new(compression, false);
        let buf_writer = BufWriter::new(inner);
        Self(deflate::Writer::with_state(compress, Box::pin(buf_writer)))
    }
}

impl<S> WrappedPin<S> for DeflatedWriter<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        Pin::into_inner(self.0.unwrap_inner_pin()).unwrap_inner_pin()
    }
}

impl<S: io::AsyncWrite> io::AsyncWrite for DeflatedWriter<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pin_stream().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pin_stream().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pin_stream().poll_shutdown(cx)
    }
}

pub enum ZipFileWrappedWriter<S> {
    Stored(StoredWriter<S>),
    Deflated(DeflatedWriter<S>),
}

enum WrappedProj<'a, S> {
    Stored(Pin<&'a mut StoredWriter<S>>),
    Deflated(Pin<&'a mut DeflatedWriter<S>>),
}

impl<S> ZipFileWrappedWriter<S> {
    #[inline]
    fn project(self: Pin<&mut Self>) -> WrappedProj<'_, S> {
        unsafe {
            let s = self.get_unchecked_mut();
            match s {
                Self::Stored(s) => WrappedProj::Stored(Pin::new_unchecked(s)),
                Self::Deflated(s) => WrappedProj::Deflated(Pin::new_unchecked(s)),
            }
        }
    }
}

impl<S: io::AsyncWrite> io::AsyncWrite for ZipFileWrappedWriter<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.project() {
            WrappedProj::Stored(w) => w.poll_write(cx, buf),
            WrappedProj::Deflated(w) => w.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            WrappedProj::Stored(w) => w.poll_flush(cx),
            WrappedProj::Deflated(w) => w.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            WrappedProj::Stored(w) => w.poll_shutdown(cx),
            WrappedProj::Deflated(w) => w.poll_shutdown(cx),
        }
    }
}

impl<S> WrappedPin<S> for ZipFileWrappedWriter<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        match self {
            Self::Stored(s) => s.unwrap_inner_pin(),
            Self::Deflated(s) => s.unwrap_inner_pin(),
        }
    }
}

enum InnerWriter<S> {
    NoActiveFile(Pin<Box<S>>),
    FileWriter(ZipFileWrappedWriter<S>),
}

enum InnerProj<'a, S> {
    NoActiveFile(Pin<&'a mut S>),
    FileWriter(Pin<&'a mut ZipFileWrappedWriter<S>>),
}

impl<S> InnerWriter<S> {
    #[inline]
    pub(crate) fn project(self: Pin<&mut Self>) -> InnerProj<'_, S> {
        unsafe {
            let s = self.get_unchecked_mut();
            match s {
                Self::NoActiveFile(s) => InnerProj::NoActiveFile(s.as_mut()),
                Self::FileWriter(s) => InnerProj::FileWriter(Pin::new_unchecked(s)),
            }
        }
    }
}

enum WriterWrapResult {
    Stored,
    Deflated(Compression),
}

impl WriterWrapResult {
    pub(crate) fn wrap<S: io::AsyncWrite>(self, s: Pin<Box<S>>) -> ZipFileWrappedWriter<S> {
        match self {
            Self::Stored => ZipFileWrappedWriter::Stored(StoredWriter::new(s)),
            Self::Deflated(c) => ZipFileWrappedWriter::Deflated(DeflatedWriter::new(c, s)),
        }
    }
}

impl<S: io::AsyncWrite + io::AsyncSeek> InnerWriter<S> {
    /// Returns `directory_start`.
    pub(crate) async fn finalize(
        self: Pin<&mut Self>,
        files: &[ZipFileData],
        comment: &[u8],
    ) -> ZipResult<u64> {
        match self.project() {
            InnerProj::FileWriter(_) => unreachable!("stream should be unwrapped!"),
            InnerProj::NoActiveFile(mut inner) => {
                let central_start = inner.stream_position().await?;
                for file in files.iter() {
                    write_spec::write_central_directory_header(inner.as_mut(), file).await?;
                }
                let central_size = inner.stream_position().await? - central_start;

                /* If we have to create a zip64 file, generate the appropriate additional footer. */
                if files.len() > spec::ZIP64_ENTRY_THR
                    || central_size.max(central_start) > spec::ZIP64_BYTES_THR
                {
                    let zip64_footer = spec::Zip64CentralDirectoryEnd {
                        version_made_by: DEFAULT_VERSION as u16,
                        version_needed_to_extract: DEFAULT_VERSION as u16,
                        disk_number: 0,
                        disk_with_central_directory: 0,
                        number_of_files_on_this_disk: files.len() as u64,
                        number_of_files: files.len() as u64,
                        central_directory_size: central_size,
                        central_directory_offset: central_start,
                    };
                    zip64_footer.write_async(inner.as_mut()).await?;

                    let zip64_footer = spec::Zip64CentralDirectoryEndLocator {
                        disk_with_central_directory: 0,
                        end_of_central_directory_offset: central_start + central_size,
                        number_of_disks: 1,
                    };
                    zip64_footer.write_async(inner.as_mut()).await?;
                }

                let number_of_files = files.len().min(spec::ZIP64_ENTRY_THR) as u16;
                let footer = spec::CentralDirectoryEnd {
                    disk_number: 0,
                    disk_with_central_directory: 0,
                    zip_file_comment: comment.to_vec(),
                    number_of_files_on_this_disk: number_of_files,
                    number_of_files,
                    central_directory_size: central_size.min(spec::ZIP64_BYTES_THR) as u32,
                    central_directory_offset: central_start.min(spec::ZIP64_BYTES_THR) as u32,
                };
                footer.write_async(inner.as_mut()).await?;

                Ok(central_start)
            }
        }
    }

    pub(crate) async fn initialize_entry(
        self: Pin<&mut Self>,
        raw: ZipRawValues,
        options: FileOptions,
        file_name: String,
        stats: &mut ZipWriterStats,
    ) -> ZipResult<ZipFileData> {
        match self.project() {
            InnerProj::FileWriter(_) => unreachable!("stream should be unwrapped!"),
            InnerProj::NoActiveFile(mut inner) => {
                let header_start = inner.stream_position().await?;

                let mut file = ZipFileData::initialize(raw, options, header_start, file_name);
                write_spec::write_local_file_header(inner.as_mut(), &file).await?;

                let header_end = inner.stream_position().await?;
                stats.start = header_end;
                *file.data_start.get_mut() = header_end;

                stats.bytes_written = 0;
                stats.hasher.reset();

                Ok(file)
            }
        }
    }

    pub(crate) async fn update_header_and_unwrap_stream(
        mut self: Pin<&mut Self>,
        stats: &mut ZipWriterStats,
        file: &mut ZipFileData,
    ) -> ZipResult<()> {
        match self.as_mut().project() {
            InnerProj::NoActiveFile(mut inner) => {
                inner.shutdown().await?;
            }
            InnerProj::FileWriter(mut inner) => {
                /* NB: we need to ensure the compression stream writes out everything it has left
                 * before reclaiming the stream handle! */
                inner.shutdown().await?;
            }
        }

        let s = self.get_mut();

        map_swap_uninit(s, |s| Self::NoActiveFile(s.unwrap_inner_pin()));

        match s {
            Self::NoActiveFile(ref mut inner) => {
                file.crc32 = mem::take(&mut stats.hasher).finalize();
                file.uncompressed_size = stats.bytes_written;

                let file_end = inner.stream_position().await?;
                file.compressed_size = file_end - stats.start;

                write_spec::update_local_file_header(inner.as_mut(), file).await?;
                inner.seek(io::SeekFrom::Start(file_end)).await?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    pub(crate) fn wrap_compressor_stream(
        self: Pin<&mut Self>,
        file: &ZipFileData,
    ) -> ZipResult<()> {
        let s = self.get_mut();
        let wrap_result = Self::try_wrap_writer(file.compression_method, file.compression_level)?;
        map_swap_uninit(s, move |s| match s {
            Self::FileWriter(_) => unreachable!("writer should be unwrapped!"),
            Self::NoActiveFile(s) => Self::FileWriter(wrap_result.wrap(s)),
        });
        Ok(())
    }

    pub(crate) async fn write_extra_data_and_wrap_compressor_stream(
        self: Pin<&mut Self>,
        stats: &mut ZipWriterStats,
        file: &mut ZipFileData,
    ) -> ZipResult<()> {
        let s = self.get_mut();
        match s {
            Self::FileWriter(_) => unreachable!("writer should be unwrapped!"),
            Self::NoActiveFile(inner) => {
                let data_start = file.data_start.get_mut();

                // Append extra data to local file header and keep it for central file header.
                inner.write_all(&file.extra_field).await?;

                // Update final `data_start`.
                let header_end = *data_start + file.extra_field.len() as u64;
                stats.start = header_end;
                *data_start = header_end;

                // Update extra field length in local file header.
                let extra_field_length =
                    if file.large_file { 20 } else { 0 } + file.extra_field.len() as u16;
                inner
                    .seek(io::SeekFrom::Start(file.header_start + 28))
                    .await?;
                inner.write_u16_le(extra_field_length).await?;
                inner.seek(io::SeekFrom::Start(header_end)).await?;
            }
        }

        Pin::new(s).wrap_compressor_stream(file)?;

        Ok(())
    }
}

impl<S: io::AsyncWrite> InnerWriter<S> {
    pub fn try_wrap_writer(
        compression: CompressionMethod,
        compression_level: Option<i32>,
    ) -> ZipResult<WriterWrapResult> {
        match compression {
            CompressionMethod::Stored => {
                if compression_level.is_some() {
                    return Err(ZipError::UnsupportedArchive(
                        "Unsupported compression level",
                    ));
                }
                Ok(WriterWrapResult::Stored)
            }
            CompressionMethod::Deflated => {
                let compression = Compression::new(
                    clamp_opt(
                        compression_level.unwrap_or(Compression::default().level() as i32),
                        deflate_compression_level_range(),
                    )
                    .ok_or(ZipError::UnsupportedArchive(
                        "Unsupported compression level",
                    ))? as u32,
                );
                Ok(WriterWrapResult::Deflated(compression))
            }
            _ => todo!("other compression methods not yet supported!"),
        }
    }
}

fn clamp_opt<T: cmp::Ord + Copy>(value: T, range: ops::RangeInclusive<T>) -> Option<T> {
    if range.contains(&value) {
        Some(value)
    } else {
        None
    }
}

fn deflate_compression_level_range() -> ops::RangeInclusive<i32> {
    let min = Compression::none().level() as i32;
    let max = Compression::best().level() as i32;
    min..=max
}

impl<S> WrappedPin<S> for InnerWriter<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        match self {
            Self::NoActiveFile(s) => s,
            Self::FileWriter(s) => s.unwrap_inner_pin(),
        }
    }
}

/// To a [`Cursor`](std::io::Cursor):
///```
/// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
/// use zip::{write::FileOptions, tokio::{read::ZipArchive, write::ZipWriter}};
/// use std::{io::Cursor, pin::Pin};
/// use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
///
/// let buf = Cursor::new(Vec::new());
/// let mut f = ZipWriter::new(Box::pin(buf));
/// let mut fp = Pin::new(&mut f);
///
/// let opts = FileOptions::default();
/// fp.as_mut().start_file("asdf.txt", opts).await?;
/// fp.write_all(b"hello!").await?;
///
/// let mut f = f.finish_into_readable().await?;
/// let mut f = Pin::new(&mut f);
/// let mut s = String::new();
/// {
///   let mut zf = f.by_name("asdf.txt").await?;
///   zf.read_to_string(&mut s).await?;
/// }
/// assert_eq!(&s, "hello!");
/// # Ok(())
/// # })}
///```
///
/// To a [`File`](tokio::fs::File):
///```
/// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
/// use zip::{write::FileOptions, tokio::{read::ZipArchive, write::ZipWriter}};
/// use std::pin::Pin;
/// use tokio::{fs, io::{self, AsyncReadExt, AsyncWriteExt}};
///
/// let file = fs::File::from_std(tempfile::tempfile()?);
/// let mut f = ZipWriter::new(Box::pin(file));
/// let mut fp = Pin::new(&mut f);
///
/// let opts = FileOptions::default();
/// fp.as_mut().start_file("asdf.txt", opts).await?;
/// fp.write_all(b"hello!").await?;
///
/// let mut f = f.finish_into_readable().await?;
/// let mut f = Pin::new(&mut f);
/// let mut s = String::new();
/// {
///   let mut zf = f.by_name("asdf.txt").await?;
///   zf.read_to_string(&mut s).await?;
/// }
/// assert_eq!(&s, "hello!");
/// # Ok(())
/// # })}
///```
pub struct ZipWriter<S> {
    inner: InnerWriter<S>,
    files: Vec<ZipFileData>,
    stats: ZipWriterStats,
    writing_to_file: bool,
    writing_to_extra_field: bool,
    writing_to_central_extra_field_only: bool,
    writing_raw: bool,
    comment: Vec<u8>,
}

struct WriteProj<'a, S> {
    pub inner: Pin<&'a mut InnerWriter<S>>,
    pub files: &'a mut Vec<ZipFileData>,
    pub stats: &'a mut ZipWriterStats,
    pub writing_to_file: &'a mut bool,
    pub writing_to_extra_field: &'a mut bool,
    pub writing_to_central_extra_field_only: &'a mut bool,
    pub writing_raw: &'a mut bool,
    pub comment: &'a mut Vec<u8>,
}

impl<S> ZipWriter<S> {
    pub fn new(inner: Pin<Box<S>>) -> Self {
        Self {
            inner: InnerWriter::NoActiveFile(inner),
            files: Vec::new(),
            stats: Default::default(),
            writing_to_file: false,
            writing_to_extra_field: false,
            writing_to_central_extra_field_only: false,
            writing_raw: false,
            comment: Vec::new(),
        }
    }

    fn for_append(inner: Pin<Box<S>>, files: Vec<ZipFileData>, comment: Vec<u8>) -> Self {
        Self {
            inner: InnerWriter::NoActiveFile(inner),
            files,
            stats: Default::default(),
            writing_to_file: false,
            writing_to_extra_field: false,
            writing_to_central_extra_field_only: false,
            comment,
            writing_raw: true, /* avoid recomputing the last file's header */
        }
    }

    #[inline]
    fn project(self: Pin<&mut Self>) -> WriteProj<'_, S> {
        unsafe {
            let Self {
                inner,
                files,
                stats,
                writing_to_file,
                writing_to_extra_field,
                writing_to_central_extra_field_only,
                writing_raw,
                comment,
            } = self.get_unchecked_mut();
            WriteProj {
                inner: Pin::new_unchecked(inner),
                files,
                stats,
                writing_to_file,
                writing_to_extra_field,
                writing_to_central_extra_field_only,
                writing_raw,
                comment,
            }
        }
    }

    pub fn set_raw_comment(self: Pin<&mut Self>, comment: Vec<u8>) {
        *self.project().comment = comment;
    }

    pub fn set_comment(self: Pin<&mut Self>, comment: impl Into<String>) {
        self.set_raw_comment(comment.into().into());
    }
}

impl<S: io::AsyncWrite + io::AsyncSeek> ZipWriter<S> {
    pub async fn start_file(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        mut options: FileOptions,
    ) -> ZipResult<()> {
        if options.permissions.is_none() {
            options.permissions = Some(0o644);
        }
        *options.permissions.as_mut().unwrap() |= 0o100000;

        self.as_mut().start_entry(name, options, None).await?;

        let me = self.project();

        me.inner.wrap_compressor_stream(me.files.last().unwrap())?;

        *me.writing_to_file = true;

        Ok(())
    }

    pub async fn add_directory(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        mut options: FileOptions,
    ) -> ZipResult<()> {
        if options.permissions.is_none() {
            options.permissions = Some(0o755);
        }
        *options.permissions.as_mut().unwrap() |= 0o40000;
        options.compression_method = CompressionMethod::Stored;

        let mut name_as_string = name.into();
        // Append a slash to the filename if it does not end with it.
        if !name_as_string.ends_with('/') {
            /* TODO: ends_with('\\') as well? */
            name_as_string.push('/');
        }

        self.as_mut()
            .start_entry(name_as_string, options, None)
            .await?;
        self.writing_to_file = false;

        Ok(())
    }

    pub async fn add_symlink(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        target: impl Into<String>,
        mut options: FileOptions,
    ) -> ZipResult<()> {
        if options.permissions.is_none() {
            options.permissions = Some(0o777);
        }
        *options.permissions.as_mut().unwrap() |= 0o120000;
        // The symlink target is stored as file content. And compressing the target path
        // likely wastes space. So always store.
        options.compression_method = CompressionMethod::Stored;

        self.as_mut().start_entry(name, options, None).await?;
        self.writing_to_file = true;
        self.write_all(target.into().as_bytes()).await?;
        self.writing_to_file = false;

        Ok(())
    }

    pub async fn start_file_with_extra_data(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        mut options: FileOptions,
    ) -> ZipResult<u64> {
        if options.permissions.is_none() {
            options.permissions = Some(0o644);
        }
        *options.permissions.as_mut().unwrap() |= 0o100000;

        self.as_mut().start_entry(name, options, None).await?;

        self.writing_to_file = true;
        self.writing_to_extra_field = true;

        Ok(self.files.last().unwrap().data_start.load())
    }

    pub async fn start_file_aligned(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        options: FileOptions,
        align: u16,
    ) -> ZipResult<u64> {
        let data_start = self
            .as_mut()
            .start_file_with_extra_data(name, options)
            .await?;
        let align = align as u64;

        if align > 1 && data_start % align != 0 {
            let pad_length = (align - (data_start + 4) % align) % align;
            let pad = vec![0; pad_length as usize];
            self.write_all(b"za").await?; // 0x617a
            self.write_u16_le(pad.len() as u16).await?;
            self.write_all(&pad).await?;
            assert_eq!(
                self.as_mut().end_local_start_central_extra_data().await? % align,
                0
            );
        }
        let extra_data_end = self.end_extra_data().await?;
        Ok(extra_data_end - data_start)
    }

    async fn start_entry(
        mut self: Pin<&mut Self>,
        name: impl Into<String>,
        options: FileOptions,
        raw_values: Option<ZipRawValues>,
    ) -> ZipResult<()> {
        self.as_mut().finish_file().await?;

        let raw_values = raw_values.unwrap_or_default();

        let mut me = self.project();

        let file = me
            .inner
            .as_mut()
            .initialize_entry(raw_values, options, name.into(), me.stats)
            .await?;
        me.files.push(file);
        Ok(())
    }

    async fn finish_file(mut self: Pin<&mut Self>) -> ZipResult<()> {
        if self.writing_to_extra_field {
            // Implicitly calling [`ZipWriter::end_extra_data`] for empty files.
            self.as_mut().end_extra_data().await?;
        }

        let mut me = self.project();

        if !*me.writing_raw {
            let file = match me.files.last_mut() {
                None => return Ok(()),
                Some(f) => f,
            };

            me.inner
                .as_mut()
                .update_header_and_unwrap_stream(me.stats, file)
                .await?;
        }

        *me.writing_to_file = false;
        *me.writing_raw = false;
        Ok(())
    }

    pub async fn end_local_start_central_extra_data(mut self: Pin<&mut Self>) -> ZipResult<u64> {
        let data_start = self.as_mut().end_extra_data().await?;
        self.files.last_mut().unwrap().extra_field.clear();
        self.writing_to_extra_field = true;
        self.writing_to_central_extra_field_only = true;
        Ok(data_start)
    }

    pub async fn end_extra_data(self: Pin<&mut Self>) -> ZipResult<u64> {
        if !self.writing_to_extra_field {
            return Err(io::Error::new(io::ErrorKind::Other, "Not writing to extra field").into());
        }

        let mut me = self.project();

        let file = me.files.last_mut().unwrap();

        write_spec::validate_extra_data(file).await?;

        if !*me.writing_to_central_extra_field_only {
            me.inner
                .as_mut()
                .write_extra_data_and_wrap_compressor_stream(me.stats, file)
                .await?;
        }

        let data_start = file.data_start.get_mut();
        *me.writing_to_extra_field = false;
        *me.writing_to_central_extra_field_only = false;
        Ok(*data_start)
    }

    pub async fn finish(mut self) -> ZipResult<Pin<Box<S>>> {
        let _ = Pin::new(&mut self).finalize().await?;
        Pin::new(&mut self).shutdown().await?;
        Ok(self.unwrap_inner_pin())
    }

    async fn finalize(mut self: Pin<&mut Self>) -> ZipResult<u64> {
        self.as_mut().finish_file().await?;

        let me = self.project();
        me.inner.finalize(me.files, me.comment).await
    }

    /// Copy over the entire contents of another archive verbatim.
    ///
    /// This method extracts file metadata from the `source` archive, then simply performs a single
    /// big [`io::copy()`](io::copy) to transfer all the actual file contents without any
    /// decompression or decryption. This is more performant than the equivalent operation of
    /// calling [`Self::raw_copy_file()`] for each entry from the `source` archive in sequence.
    ///
    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use zip::{tokio::{read::ZipArchive, write::ZipWriter}, write::FileOptions};
    /// use tokio::io::{self, AsyncReadExt, AsyncWriteExt, AsyncSeekExt};
    /// use std::{io::Cursor, pin::Pin};
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// zp.as_mut().start_file("a.txt", FileOptions::default()).await?;
    /// zp.write_all(b"hello\n").await?;
    /// let mut src = zip.finish_into_readable().await?;
    /// let src = Pin::new(&mut src);
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// zp.as_mut().start_file("b.txt", FileOptions::default()).await?;
    /// zp.write_all(b"hey\n").await?;
    /// let mut src2 = zip.finish_into_readable().await?;
    /// let src2 = Pin::new(&mut src2);
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// zp.as_mut().merge_archive(src).await?;
    /// zp.merge_archive(src2).await?;
    /// let mut result = zip.finish_into_readable().await?;
    /// let mut zp = Pin::new(&mut result);
    ///
    /// let mut s: String = String::new();
    /// zp.as_mut().by_name("a.txt").await?.read_to_string(&mut s).await?;
    /// assert_eq!(s, "hello\n");
    /// s.clear();
    /// zp.by_name("b.txt").await?.read_to_string(&mut s).await?;
    /// assert_eq!(s, "hey\n");
    /// # Ok(())
    /// # })}
    ///```
    pub async fn merge_archive<R>(
        mut self: Pin<&mut Self>,
        source: Pin<&mut ZipArchive<R, Shared>>,
    ) -> ZipResult<()>
    where
        R: io::AsyncRead + io::AsyncSeek,
    {
        self.as_mut().finish_file().await?;

        /* Ensure we accept the file contents on faith (and avoid overwriting the data).
         * See raw_copy_file_rename(). */
        self.writing_to_file = true;
        self.writing_raw = true;

        let mut me = self.project();

        /* Get the file entries from the source archive. */
        let new_files = match me.inner.as_mut().project() {
            InnerProj::FileWriter(_) => unreachable!("should never merge with unfinished file!"),
            InnerProj::NoActiveFile(writer) => source.merge_contents(writer).await?,
        };
        /* These file entries are now ours! */
        let new_files: Vec<ZipFileData> = new_files.into();
        me.files.extend(new_files.into_iter());

        Ok(())
    }
}

impl<S: io::AsyncRead + io::AsyncWrite + io::AsyncSeek> ZipWriter<S> {
    /// Initializes the archive from an existing ZIP archive, making it ready for append.
    ///
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use zip::{tokio::{read::ZipArchive, write::ZipWriter}, write::FileOptions};
    /// use tokio::io::{self, AsyncReadExt, AsyncWriteExt, AsyncSeekExt};
    /// use std::{io::Cursor, pin::Pin};
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// zp.as_mut().start_file("a.txt", FileOptions::default()).await?;
    /// zp.write_all(b"hello\n").await?;
    /// let src = zip.finish().await?;
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// zp.as_mut().start_file("b.txt", FileOptions::default()).await?;
    /// zp.write_all(b"hey\n").await?;
    /// let mut src2 = zip.finish_into_readable().await?;
    /// let src2 = Pin::new(&mut src2);
    ///
    /// let mut zip = ZipWriter::new_append(src)?;
    /// let mut zp = Pin::new(&mut zip);
    /// zp.merge_archive(src2).await?;
    /// let mut result = zip.finish_into_readable().await?;
    /// let mut zp = Pin::new(&mut result);
    ///
    /// let mut s: String = String::new();
    /// {
    ///   use zip::tokio::read::SharedData;
    ///   assert_eq!(zp.shared().len(), 2);
    /// }
    /// zp.as_mut().by_name("a.txt").await?.read_to_string(&mut s).await?;
    /// assert_eq!(s, "hello\n");
    /// s.clear();
    /// zp.by_name("b.txt").await?.read_to_string(&mut s).await?;
    /// assert_eq!(s, "hey\n");
    ///
    /// # Ok(())
    /// # })}
    ///```
    pub async fn new_append(mut readwriter: Pin<Box<S>>) -> ZipResult<Self> {
        let (footer, cde_end_pos) =
            spec::CentralDirectoryEnd::find_and_parse_async(readwriter.as_mut()).await?;

        if footer.disk_number != footer.disk_with_central_directory {
            return Err(ZipError::UnsupportedArchive(
                "Support for multi-disk files is not implemented",
            ));
        }

        let (archive_offset, directory_start, number_of_files) =
            Shared::get_directory_counts(readwriter.as_mut(), &footer, cde_end_pos).await?;

        readwriter
            .seek(io::SeekFrom::Start(directory_start))
            .await
            .map_err(|_| {
                ZipError::InvalidArchive("Could not seek to start of central directory")
            })?;

        let mut files: Vec<ZipFileData> = Vec::with_capacity(number_of_files);
        for _ in 0..number_of_files {
            let file =
                read_spec::central_header_to_zip_file(readwriter.as_mut(), archive_offset).await?;
            files.push(file);
        }

        /* seek directory_start to overwrite it */
        readwriter
            .seek(io::SeekFrom::Start(directory_start))
            .await?;

        Ok(Self::for_append(readwriter, files, footer.zip_file_comment))
    }

    /// Write the zip file into the backing stream, then produce a readable archive of that data.
    ///
    /// This method avoids parsing the central directory records at the end of the stream for
    /// a slight performance improvement over running [`ZipArchive::new()`] on the output of
    /// [`Self::finish()`].
    ///
    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use zip::{tokio::{read::ZipArchive, write::ZipWriter}, write::FileOptions};
    /// use tokio::io::{self, AsyncReadExt, AsyncWriteExt, AsyncSeekExt};
    /// use std::{io::Cursor, pin::Pin};
    ///
    /// let buf = Cursor::new(Vec::new());
    /// let mut zip = ZipWriter::new(Box::pin(buf));
    /// let mut zp = Pin::new(&mut zip);
    /// let options = FileOptions::default();
    /// zp.as_mut().start_file("a.txt", options).await?;
    /// zp.write_all(b"hello\n").await?;
    ///
    /// let mut zip = zip.finish_into_readable().await?;
    /// let mut zp = Pin::new(&mut zip);
    /// let mut s: String = String::new();
    /// zp.by_name("a.txt").await?.read_to_string(&mut s).await?;
    /// assert_eq!(s, "hello\n");
    /// # Ok(())
    /// # })}
    ///```
    pub async fn finish_into_readable(mut self) -> ZipResult<ZipArchive<S, Shared>> {
        let directory_start = Pin::new(&mut self).finalize().await?;
        Pin::new(&mut self).shutdown().await?;
        let files = mem::take(&mut self.files);
        let comment = mem::take(&mut self.comment);
        let inner = self.unwrap_inner_pin();
        ZipArchive::from_finalized_writer(files, comment, inner, directory_start)
    }
}

impl<S: io::AsyncWrite + io::AsyncSeek> io::AsyncWrite for ZipWriter<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.writing_to_file {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "No file has been started",
            )));
        }
        let mut me = self.project();
        match me.inner.as_mut().project() {
            InnerProj::NoActiveFile(_) => {
                assert!(*me.writing_to_extra_field);
                let field = Pin::new(&mut me.files.last_mut().unwrap().extra_field);
                field.poll_write(cx, buf)
            }
            InnerProj::FileWriter(wrapped) => {
                let num_written = ready!(wrapped.poll_write(cx, buf))?;
                me.stats.update(&buf[..num_written]);
                if me.stats.bytes_written > spec::ZIP64_BYTES_THR
                    && !me.files.last_mut().unwrap().large_file
                {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Large file option has not been set",
                    )));
                }
                Poll::Ready(Ok(num_written))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project().inner.project() {
            InnerProj::NoActiveFile(inner) => inner.poll_flush(cx),
            InnerProj::FileWriter(wrapped) => wrapped.poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        match self.project().inner.project() {
            InnerProj::NoActiveFile(inner) => inner.poll_shutdown(cx),
            InnerProj::FileWriter(wrapped) => wrapped.poll_shutdown(cx),
        }
    }
}

/* impl<S> ops::Drop for ZipWriter<S> { */
/*     fn drop(&mut self) { */
/*         unreachable!("must call .finish()!"); */
/*     } */
/* } */

impl<S> WrappedPin<S> for ZipWriter<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        let inner: InnerWriter<S> = unsafe { ptr::read(&self.inner) };
        mem::forget(self);
        inner.unwrap_inner_pin()
    }
}

pub(crate) mod write_spec {
    use crate::{
        result::ZipResult,
        spec::{self, CentralDirectoryHeaderBuffer, LocalHeaderBuffer},
        types::ZipFileData,
    };

    use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    use std::{io::IoSlice, mem, pin::Pin};

    pub async fn validate_extra_data(file: &ZipFileData) -> ZipResult<()> {
        if file.extra_field.len() > spec::ZIP64_ENTRY_THR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Extra data exceeds extra field",
            )
            .into());
        }

        let mut data = file.extra_field.as_slice();

        while !data.is_empty() {
            let left = data.len();
            if left < 4 {
                return Err(
                    io::Error::new(io::ErrorKind::Other, "Incomplete extra data header").into(),
                );
            }

            let mut buf = [0u8; 4];
            data.read_exact(&mut buf[..]).await?;
            let (kind, size): (u16, u16) = unsafe { mem::transmute(buf) };

            let left = left - 4;

            if kind == 0x0001 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "No custom ZIP64 extra data allowed",
                )
                .into());
            }

            #[cfg(not(feature = "unreserved"))]
            {
                if kind <= 31 || crate::write::EXTRA_FIELD_MAPPING.contains(&kind) {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "Extra data header ID {kind:#06} requires crate feature \"unreserved\"",
                        ),
                    )
                    .into());
                }
            }

            if size > left as u16 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Extra data size exceeds extra field",
                )
                .into());
            }

            data = &data[size as usize..];
        }

        Ok(())
    }

    pub async fn update_local_file_header<S: io::AsyncWrite + io::AsyncSeek>(
        mut writer: Pin<&mut S>,
        file: &ZipFileData,
    ) -> ZipResult<()> {
        const CRC32_OFFSET: u64 = 14;
        writer
            .seek(io::SeekFrom::Start(file.header_start + CRC32_OFFSET))
            .await?;

        if file.large_file {
            writer.write_u32_le(file.crc32).await?;
            update_local_zip64_extra_field(writer.as_mut(), file).await?;
        } else {
            // check compressed size as well as it can also be slightly larger than uncompressed
            if file.compressed_size > spec::ZIP64_BYTES_THR {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Large file option has not been set",
                )
                .into());
            }
            let buf: [u32; 3] = [
                file.crc32,
                file.compressed_size as u32,
                file.uncompressed_size as u32,
            ];
            let buf: [u8; 12] = unsafe { mem::transmute(buf) };
            writer.write_all(&buf[..]).await?;
        };
        Ok(())
    }

    async fn update_local_zip64_extra_field<S: io::AsyncWrite + io::AsyncSeek>(
        mut writer: Pin<&mut S>,
        file: &ZipFileData,
    ) -> ZipResult<()> {
        let zip64_extra_field = file.header_start + 30 + file.file_name.as_bytes().len() as u64;
        writer
            .seek(io::SeekFrom::Start(zip64_extra_field + 4))
            .await?;

        let buf: [u64; 2] = [file.uncompressed_size, file.compressed_size];
        let buf: [u8; 16] = unsafe { mem::transmute(buf) };
        writer.write_all(&buf[..]).await?;
        // Excluded fields:
        // u32: disk start number
        Ok(())
    }

    pub async fn write_local_file_header<S: io::AsyncWrite>(
        mut writer: Pin<&mut S>,
        file: &ZipFileData,
    ) -> ZipResult<()> {
        let (compressed_size, uncompressed_size): (u32, u32) = if file.large_file {
            (spec::ZIP64_BYTES_THR as u32, spec::ZIP64_BYTES_THR as u32)
        } else {
            (file.compressed_size as u32, file.uncompressed_size as u32)
        };
        #[allow(deprecated)]
        let block: [u8; 30] = unsafe {
            mem::transmute(LocalHeaderBuffer {
                magic: spec::LOCAL_FILE_HEADER_SIGNATURE,
                version_needed_to_extract: file.version_needed(),
                flag: if !file.file_name.is_ascii() {
                    1u16 << 11
                } else {
                    0
                } | if file.encrypted { 1u16 << 0 } else { 0 },
                compression_method: file.compression_method.to_u16(),
                last_modified_time_timepart: file.last_modified_time.timepart(),
                last_modified_time_datepart: file.last_modified_time.datepart(),
                crc32: file.crc32,
                compressed_size,
                uncompressed_size,
                file_name_length: file.file_name.as_bytes().len() as u16,
                extra_field_length: if file.large_file { 20 } else { 0 }
                    + file.extra_field.len() as u16,
            })
        };

        let maybe_extra_field = if file.large_file {
            // This entry in the Local header MUST include BOTH original
            // and compressed file size fields.
            assert!(file.uncompressed_size > spec::ZIP64_BYTES_THR);
            assert!(file.compressed_size > spec::ZIP64_BYTES_THR);
            Some(get_central_zip64_extra_field(file))
        } else {
            None
        };

        let fname = file.file_name.as_bytes();

        if writer.is_write_vectored() {
            /* TODO: zero-copy!! */
            let block = IoSlice::new(&block);
            let fname = IoSlice::new(&fname);
            if let Some(extra_block) = maybe_extra_field {
                let extra_field = IoSlice::new(&extra_block);
                writer.write_vectored(&[block, fname, extra_field]).await?;
            } else {
                writer.write_vectored(&[block, fname]).await?;
            }
        } else {
            /* If no special vector write support, just perform a series of normal writes. */
            writer.write_all(&block).await?;
            writer.write_all(&fname).await?;
            if let Some(extra_block) = maybe_extra_field {
                writer.write_all(&extra_block).await?;
            }
        }

        Ok(())
    }

    pub async fn write_central_directory_header<S: io::AsyncWrite>(
        mut writer: Pin<&mut S>,
        file: &ZipFileData,
    ) -> ZipResult<()> {
        let zip64_extra_field = get_central_zip64_extra_field(file);

        #[allow(deprecated)]
        let block: [u8; 46] = unsafe {
            mem::transmute(CentralDirectoryHeaderBuffer {
                magic: spec::CENTRAL_DIRECTORY_HEADER_SIGNATURE,
                version_made_by: (file.system as u16) << 8 | (file.version_made_by as u16),
                version_needed: file.version_needed(),
                flag: if !file.file_name.is_ascii() {
                    1u16 << 11
                } else {
                    0
                } | if file.encrypted { 1u16 << 0 } else { 0 },
                compression_method: file.compression_method.to_u16(),
                last_modified_time_timepart: file.last_modified_time.timepart(),
                last_modified_time_datepart: file.last_modified_time.datepart(),
                crc32: file.crc32,
                compressed_size: file.compressed_size.min(spec::ZIP64_BYTES_THR) as u32,
                uncompressed_size: file.uncompressed_size.min(spec::ZIP64_BYTES_THR) as u32,
                file_name_length: file.file_name.as_bytes().len() as u16,
                extra_field_length: zip64_extra_field.len() as u16,
                file_comment_length: 0,
                disk_number_start: 0,
                internal_attributes: 0,
                external_attributes: file.external_attributes,
                header_start: file.header_start.min(spec::ZIP64_BYTES_THR) as u32,
            })
        };

        let fname = file.file_name.as_bytes();

        if writer.is_write_vectored() {
            /* TODO: zero-copy!! */
            let block = IoSlice::new(&block);
            let fname = IoSlice::new(&fname);
            let z64_extra = IoSlice::new(&zip64_extra_field);
            let extra = IoSlice::new(&file.extra_field);
            writer
                .write_vectored(&[block, fname, z64_extra, extra])
                .await?;
        } else {
            writer.write_all(&block).await?;
            // file name
            writer.write_all(&fname).await?;
            // zip64 extra field
            writer.write_all(&zip64_extra_field).await?;
            // extra field
            writer.write_all(&file.extra_field).await?;
            // file comment
            // <none>
        }

        Ok(())
    }

    fn get_central_zip64_extra_field(file: &ZipFileData) -> Vec<u8> {
        // The order of the fields in the zip64 extended
        // information record is fixed, but the fields MUST
        // only appear if the corresponding Local or Central
        // directory record field is set to 0xFFFF or 0xFFFFFFFF.
        let mut ret: Vec<u8> = Vec::new();
        let mut size: u16 = 0;
        let uncompressed_size = file.uncompressed_size > spec::ZIP64_BYTES_THR;
        let compressed_size = file.compressed_size > spec::ZIP64_BYTES_THR;
        let header_start = file.header_start > spec::ZIP64_BYTES_THR;

        let zip64_kind: u16 = 0x0001;

        if uncompressed_size {
            size += 8;
        }
        if compressed_size {
            size += 8;
        }
        if header_start {
            size += 8;
        }
        if size > 0 {
            ret.extend_from_slice(&zip64_kind.to_le_bytes());
            ret.extend_from_slice(&size.to_le_bytes());
            size += 4;

            if uncompressed_size {
                ret.extend_from_slice(&file.uncompressed_size.to_le_bytes());
            }
            if compressed_size {
                ret.extend_from_slice(&file.compressed_size.to_le_bytes());
            }
            if header_start {
                ret.extend_from_slice(&file.header_start.to_le_bytes());
            }
            // Excluded fields:
            // u32: disk start number
        }

        assert_eq!(size as usize, ret.len());

        ret
    }
}
