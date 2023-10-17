use crate::compression::CompressionMethod;
use crate::result::{ZipError, ZipResult};
use crate::spec::{self, LocalHeaderBuffer};
use crate::tokio::{
    buf_reader::BufReader, combinators::Limiter, crc32::Crc32Reader, extraction::CompletedPaths,
    stream_impls::deflate, utils::map_take_manual_drop, WrappedPin,
};
use crate::types::ZipFileData;

#[cfg(any(
    feature = "deflate",
    feature = "deflate-miniz",
    feature = "deflate-zlib"
))]
use flate2::Decompress;

use async_stream::try_stream;
use cfg_if::cfg_if;
use futures_core::stream::Stream;
use futures_util::{pin_mut, stream::TryStreamExt};
use indexmap::IndexMap;
use tokio::{
    fs,
    io::{self, AsyncReadExt, AsyncSeekExt},
    sync::{self, mpsc},
    task,
};

use std::{
    cell::UnsafeCell,
    mem::{self, ManuallyDrop},
    num, ops,
    os::unix::io::{AsRawFd, RawFd},
    path::{Path, PathBuf},
    pin::Pin,
    str,
    sync::Arc,
    task::{Context, Poll},
};

pub(crate) trait ReaderWrapper<S> {
    fn construct(data: &ZipFileData, s: Pin<Box<S>>) -> Self
    where
        Self: Sized;
}

pub struct StoredReader<S>(Crc32Reader<S>);

impl<S> StoredReader<S> {
    #[inline]
    fn pin_stream(self: Pin<&mut Self>) -> Pin<&mut Crc32Reader<S>> {
        unsafe { self.map_unchecked_mut(|Self(inner)| inner) }
    }
}

impl<S: io::AsyncRead> io::AsyncRead for StoredReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.pin_stream().poll_read(cx, buf)
    }
}

impl<S> WrappedPin<S> for StoredReader<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        self.0.unwrap_inner_pin()
    }
}

impl<S> ReaderWrapper<S> for StoredReader<S> {
    fn construct(data: &ZipFileData, s: Pin<Box<S>>) -> Self {
        Self(Crc32Reader::new(s, data.crc32, false))
    }
}

pub struct DeflateReader<S>(Crc32Reader<deflate::Reader<Decompress, BufReader<S>>>);

impl<S> DeflateReader<S> {
    #[inline]
    fn pin_stream(
        self: Pin<&mut Self>,
    ) -> Pin<&mut Crc32Reader<deflate::Reader<Decompress, BufReader<S>>>> {
        unsafe { self.map_unchecked_mut(|Self(inner)| inner) }
    }
}

impl<S: io::AsyncRead> io::AsyncRead for DeflateReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.pin_stream().poll_read(cx, buf)
    }
}

impl<S> WrappedPin<S> for DeflateReader<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        Pin::into_inner(Pin::into_inner(self.0.unwrap_inner_pin()).unwrap_inner_pin())
            .unwrap_inner_pin()
    }
}

impl<S: io::AsyncRead> ReaderWrapper<S> for DeflateReader<S> {
    fn construct(data: &ZipFileData, s: Pin<Box<S>>) -> Self {
        let buf_reader = BufReader::with_capacity(num::NonZeroUsize::new(32 * 1024).unwrap(), s);
        let deflater = deflate::Reader::with_state(Decompress::new(false), Box::pin(buf_reader));
        Self(Crc32Reader::new(Box::pin(deflater), data.crc32, false))
    }
}

pub enum ZipFileWrappedReader<S> {
    Stored(StoredReader<S>),
    Deflated(DeflateReader<S>),
}

enum WrappedProj<'a, S> {
    Stored(Pin<&'a mut StoredReader<S>>),
    Deflated(Pin<&'a mut DeflateReader<S>>),
}

impl<S> ZipFileWrappedReader<S> {
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

impl<S: io::AsyncRead> io::AsyncRead for ZipFileWrappedReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            WrappedProj::Stored(r) => r.poll_read(cx, buf),
            WrappedProj::Deflated(r) => r.poll_read(cx, buf),
        }
    }
}

impl<S> WrappedPin<S> for ZipFileWrappedReader<S> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        match self {
            Self::Stored(r) => r.unwrap_inner_pin(),
            Self::Deflated(r) => r.unwrap_inner_pin(),
        }
    }
}

impl<S> WrappedPin<S> for ZipFileWrappedReader<Limiter<S>> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        match self {
            Self::Stored(r) => Pin::into_inner(r.unwrap_inner_pin()).unwrap_inner_pin(),
            Self::Deflated(r) => Pin::into_inner(r.unwrap_inner_pin()).unwrap_inner_pin(),
        }
    }
}

impl<S: io::AsyncRead> ReaderWrapper<S> for ZipFileWrappedReader<S> {
    fn construct(data: &ZipFileData, s: Pin<Box<S>>) -> Self {
        match data.compression_method {
            CompressionMethod::Stored => Self::Stored(StoredReader::<S>::construct(data, s)),
            #[cfg(any(
                feature = "deflate",
                feature = "deflate-miniz",
                feature = "deflate-zlib"
            ))]
            CompressionMethod::Deflated => Self::Deflated(DeflateReader::<S>::construct(data, s)),
            _ => todo!("other compression methods not supported yet!"),
        }
    }
}

pub(crate) async fn find_content<S: io::AsyncRead + io::AsyncSeek>(
    data: &ZipFileData,
    mut reader: Pin<Box<S>>,
) -> ZipResult<Limiter<S>> {
    let cur_pos = {
        // Parse local header
        reader.seek(io::SeekFrom::Start(data.header_start)).await?;

        static_assertions::assert_eq_size!([u8; 30], LocalHeaderBuffer);
        let mut info = [0u8; 30];
        reader.read_exact(&mut info[..]).await?;

        let LocalHeaderBuffer {
            magic,
            file_name_length,
            /* NB: zip files have separate local and central extra data records. The length of the
             * local extra field is being parsed here. The value of this field cannot be inferred
             * from the central record data alone. */
            extra_field_length,
            ..
        } = unsafe { mem::transmute(info) };

        if magic != spec::LOCAL_FILE_HEADER_SIGNATURE {
            return Err(ZipError::InvalidArchive("Invalid local file header"));
        }

        let data_start = data.header_start
            + info.len() as u64
            + file_name_length as u64
            + extra_field_length as u64;
        data.data_start.store(data_start);

        reader.seek(io::SeekFrom::Start(data_start)).await?
    };
    Ok(Limiter::take(
        cur_pos,
        reader,
        data.compressed_size as usize,
    ))
}

pub trait SharedData {
    #[inline]
    fn len(&self) -> usize {
        self.contiguous_entries().len()
    }
    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn content_range(&self) -> ops::Range<u64>;
    #[inline]
    fn content_len(&self) -> u64 {
        let r = self.content_range();
        debug_assert!(r.start <= r.end);
        r.end - r.start
    }
    fn comment(&self) -> &[u8];
    fn contiguous_entries(&self) -> &indexmap::map::Slice<String, ZipFileData>;
}

#[derive(Debug)]
pub struct Shared {
    files: IndexMap<String, ZipFileData>,
    offset: u64,
    directory_start: u64,
    comment: Vec<u8>,
}

impl SharedData for Shared {
    #[inline]
    fn content_range(&self) -> ops::Range<u64> {
        ops::Range {
            start: self.offset(),
            end: self.directory_start(),
        }
    }
    #[inline]
    fn comment(&self) -> &[u8] {
        &self.comment
    }
    #[inline]
    fn contiguous_entries(&self) -> &indexmap::map::Slice<String, ZipFileData> {
        self.files.as_slice()
    }
}

impl Shared {
    #[inline]
    pub fn offset(&self) -> u64 {
        self.offset
    }
    #[inline]
    pub fn directory_start(&self) -> u64 {
        self.directory_start
    }

    #[inline]
    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(|s| s.as_str())
    }

    pub(crate) async fn get_directory_counts<S: io::AsyncRead + io::AsyncSeek>(
        mut reader: Pin<&mut S>,
        footer: &spec::CentralDirectoryEnd,
        cde_end_pos: u64,
    ) -> ZipResult<(u64, u64, usize)> {
        // See if there's a ZIP64 footer. The ZIP64 locator if present will
        // have its signature 20 bytes in front of the standard footer. The
        // standard footer, in turn, is 22+N bytes large, where N is the
        // comment length. Therefore:
        let zip64locator = if reader
            .as_mut()
            .seek(io::SeekFrom::End(
                -(20 + 22 + footer.zip_file_comment.len() as i64),
            ))
            .await
            .is_ok()
        {
            match spec::Zip64CentralDirectoryEndLocator::parse_async(reader.as_mut()).await {
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
                let archive_offset = cde_end_pos
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
                    return Err(ZipError::UnsupportedArchive(
                        "Support for multi-disk files is not implemented",
                    ));
                }

                // We need to reassess `archive_offset`. We know where the ZIP64
                // central-directory-end structure *should* be, but unfortunately we
                // don't know how to precisely relate that location to our current
                // actual offset in the file, since there may be junk at its
                // beginning. Therefore we need to perform another search, as in
                // read::CentralDirectoryEnd::find_and_parse, except now we search
                // forward.

                let search_upper_bound = cde_end_pos
                    .checked_sub(60) // minimum size of Zip64CentralDirectoryEnd + Zip64CentralDirectoryEndLocator
                    .ok_or(ZipError::InvalidArchive(
                        "File cannot contain ZIP64 central directory end",
                    ))?;
                let (footer, archive_offset) =
                    spec::Zip64CentralDirectoryEnd::find_and_parse_async(
                        reader.as_mut(),
                        locator64.end_of_central_directory_offset,
                        search_upper_bound,
                    )
                    .await?;

                if footer.disk_number != footer.disk_with_central_directory {
                    return Err(ZipError::UnsupportedArchive(
                        "Support for multi-disk files is not implemented",
                    ));
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

    pub async fn parse<S: io::AsyncRead + io::AsyncSeek>(
        mut reader: Pin<Box<S>>,
    ) -> ZipResult<(Self, Pin<Box<S>>)> {
        let (footer, cde_end_pos) =
            spec::CentralDirectoryEnd::find_and_parse_async(reader.as_mut()).await?;

        if !footer.record_too_small() && footer.disk_number != footer.disk_with_central_directory {
            return Err(ZipError::UnsupportedArchive(
                "Support for multi-disk files is not implemented",
            ));
        }

        let (archive_offset, directory_start, number_of_files) =
            Self::get_directory_counts(reader.as_mut(), &footer, cde_end_pos).await?;

        // If the parsed number of files is greater than the offset then
        // something fishy is going on and we shouldn't trust number_of_files.
        let file_capacity = if number_of_files > directory_start as usize {
            0
        } else {
            number_of_files
        };

        let mut files = IndexMap::with_capacity(file_capacity);

        reader
            .seek(io::SeekFrom::Start(directory_start))
            .await
            .map_err(|_| {
                ZipError::InvalidArchive("Could not seek to start of central directory")
            })?;

        for i in 0..number_of_files {
            let file =
                read_spec::central_header_to_zip_file(reader.as_mut(), archive_offset).await?;
            if i == 0 {
                assert_eq!(archive_offset, file.header_start);
            }
            assert!(files.insert(file.file_name.clone(), file).is_none());
        }

        Ok((
            Self {
                files,
                offset: archive_offset,
                directory_start,
                comment: footer.zip_file_comment,
            },
            reader,
        ))
    }
}

async fn create_dir_idempotent<P: AsRef<Path>>(dir: P) -> io::Result<Option<()>> {
    match fs::create_dir(dir).await {
        Ok(()) => Ok(Some(())),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
pub struct ZipFile<'a, S, R: WrappedPin<S>, Sh> {
    data: &'a ZipFileData,
    wrapped_reader: ManuallyDrop<R>,
    parent: &'a mut ZipArchive<S, Sh>,
}

impl<'a, S, R: WrappedPin<S>, Sh> ZipFile<'a, S, R, Sh> {
    #[inline]
    pub fn name(&self) -> ZipResult<&Path> {
        self.data
            .enclosed_name()
            .ok_or(ZipError::InvalidArchive("Invalid file path"))
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        /* TODO: '\\' too? */
        self.data.file_name.ends_with('/')
    }

    #[inline]
    pub fn unix_mode(&self) -> Option<u32> {
        self.data.unix_mode()
    }

    #[inline]
    pub fn data(&self) -> &ZipFileData {
        &self.data
    }

    #[inline]
    fn pin_stream(self: Pin<&mut Self>) -> Pin<&mut R> {
        unsafe { self.map_unchecked_mut(|s| &mut *s.wrapped_reader) }
    }
}

impl<'a, S, R: WrappedPin<S>, Sh> ops::Drop for ZipFile<'a, S, R, Sh> {
    fn drop(&mut self) {
        inner_drop(unsafe { Pin::new_unchecked(self) });
        fn inner_drop<'a, S, R: WrappedPin<S>, Sh>(this: Pin<&mut ZipFile<'a, S, R, Sh>>) {
            let ZipFile {
                ref mut wrapped_reader,
                ref mut parent,
                ..
            } = unsafe { this.get_unchecked_mut() };
            let _ = parent
                .reader
                .insert(unsafe { ManuallyDrop::take(wrapped_reader) }.unwrap_inner_pin());
        }
    }
}

impl<'a, S, R: WrappedPin<S> + 'a, Sh> ZipFile<'a, S, R, Sh> {
    pub(crate) fn decode_stream<T: ReaderWrapper<R> + WrappedPin<S>>(
        self,
    ) -> ZipFile<'a, S, T, Sh> {
        let s = UnsafeCell::new(ManuallyDrop::new(self));

        let data: &'a ZipFileData = unsafe { &*s.get() }.data;
        let wrapped_reader: &mut ManuallyDrop<R> = &mut unsafe { &mut *s.get() }.wrapped_reader;
        let parent: &'a mut ZipArchive<S, Sh> = unsafe { &mut *s.get() }.parent;
        let wrapped_reader = map_take_manual_drop(wrapped_reader, move |wrapped_reader: R| {
            T::construct(data, Box::pin(wrapped_reader))
        });

        let data: &'a ZipFileData = unsafe { &*s.get() }.data;
        ZipFile {
            data,
            wrapped_reader,
            parent,
        }
    }
}

impl<'a, S, R: WrappedPin<S> + io::AsyncRead, Sh> io::AsyncRead for ZipFile<'a, S, R, Sh> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.pin_stream().poll_read(cx, buf)
    }
}

#[derive(Debug)]
pub struct ZipArchive<S, Sh> {
    reader: Option<Pin<Box<S>>>,
    shared: Arc<Sh>,
}

impl<S, Sh> ZipArchive<S, Sh> {
    #[inline]
    fn pin_reader_mut_option(self: Pin<&mut Self>) -> &mut Option<Pin<Box<S>>> {
        &mut self.get_mut().reader
    }

    #[inline]
    fn pin_reader_assert(self: Pin<&mut Self>) -> Pin<&mut S> {
        unsafe {
            let s = self.get_unchecked_mut();
            s.reader.as_mut().unwrap().as_mut()
        }
    }

    #[inline]
    pub fn shared(&self) -> Arc<Sh> {
        self.shared.clone()
    }
}

impl<S: AsRawFd, Sh> AsRawFd for ZipArchive<S, Sh> {
    fn as_raw_fd(&self) -> RawFd {
        self.reader.as_ref().unwrap().as_raw_fd()
    }
}

impl<S: io::AsyncRead + io::AsyncSeek> ZipArchive<S, Shared> {
    pub async fn new(reader: Pin<Box<S>>) -> ZipResult<Self> {
        let (shared, reader) = Shared::parse(reader).await?;
        Ok(Self {
            reader: Some(reader),
            shared: Arc::new(shared),
        })
    }
}

impl<S, Sh> ZipArchive<S, Sh> {
    pub fn mapped(shared: Arc<Sh>, reader: Pin<Box<S>>) -> Self {
        Self {
            reader: Some(reader),
            shared,
        }
    }
}

impl<S, Sh> WrappedPin<S> for ZipArchive<S, Sh> {
    fn unwrap_inner_pin(self) -> Pin<Box<S>> {
        self.reader.unwrap()
    }
}

impl<S: io::AsyncRead + io::AsyncSeek, Sh: SharedData> ZipArchive<S, Sh> {
    pub async fn by_index(
        self: Pin<&mut Self>,
        index: usize,
    ) -> ZipResult<Pin<Box<ZipFile<'_, S, ZipFileWrappedReader<Limiter<S>>, Sh>>>> {
        let raw_entry: Pin<Box<ZipFile<'_, S, Limiter<S>, Sh>>> = self.by_index_raw(index).await?;
        let decoded_entry: Pin<Box<ZipFile<'_, S, ZipFileWrappedReader<Limiter<S>>, Sh>>> =
            Box::pin(Pin::into_inner(raw_entry).decode_stream());
        Ok(decoded_entry)
    }

    pub async fn by_index_raw(
        self: Pin<&mut Self>,
        index: usize,
    ) -> ZipResult<Pin<Box<ZipFile<'_, S, Limiter<S>, Sh>>>> {
        let s = UnsafeCell::new(self);
        let data = match unsafe { &*s.get() }
            .shared
            .contiguous_entries()
            .get_index(index)
        {
            None => {
                return Err(ZipError::FileNotFound);
            }
            Some((_, data)) => data,
        };

        let limited_reader = find_content(
            data,
            unsafe { &mut *s.get() }
                .as_mut()
                .pin_reader_mut_option()
                .take()
                .unwrap(),
        )
        .await?;

        Ok(Box::pin(ZipFile {
            data,
            wrapped_reader: ManuallyDrop::new(limited_reader),
            parent: unsafe { &mut *s.get() },
        }))
    }

    pub fn raw_entries_stream(
        self: Pin<&mut Self>,
    ) -> impl Stream<Item = ZipResult<Pin<Box<ZipFile<'_, S, Limiter<S>, Sh>>>>> + '_ {
        let len = self.shared.len();
        let s = std::cell::UnsafeCell::new(self);
        /* FIXME: make this a stream with a known length! */
        try_stream! {
            for i in 0..len {
                let f = Pin::new(unsafe { &mut **s.get() }).by_index_raw(i).await?;
                yield f;
            }
        }
    }

    pub fn entries_stream(
        self: Pin<&mut Self>,
    ) -> impl Stream<Item = ZipResult<Pin<Box<ZipFile<'_, S, ZipFileWrappedReader<Limiter<S>>, Sh>>>>> + '_
    {
        use futures_util::StreamExt;

        self.raw_entries_stream()
            .map(|result| result.map(|entry| Box::pin(Pin::into_inner(entry).decode_stream())))
    }
}

impl<S: io::AsyncRead + io::AsyncSeek> ZipArchive<S, Shared> {
    pub async fn by_name(
        self: Pin<&mut Self>,
        name: &str,
    ) -> ZipResult<Pin<Box<ZipFile<'_, S, ZipFileWrappedReader<Limiter<S>>, Shared>>>> {
        let index = match self.shared.files.get_index_of(name) {
            None => {
                return Err(ZipError::FileNotFound);
            }
            Some(n) => n,
        };
        self.by_index(index).await
    }

    pub(crate) async fn merge_contents<W: io::AsyncWrite + io::AsyncSeek>(
        mut self: Pin<&mut Self>,
        mut w: Pin<&mut W>,
    ) -> ZipResult<Box<[ZipFileData]>> {
        use rayon::prelude::*;

        let mut new_files: Box<[ZipFileData]> = self
            .shared
            .files
            .par_values()
            .cloned()
            .collect::<Vec<_>>()
            .into_boxed_slice();
        if new_files.is_empty() {
            return Ok(new_files);
        }
        /* The first file header will probably start at the beginning of the file, but zip doesn't
         * enforce that, and executable zips like PEX files will have a shebang line so will
         * definitely be greater than 0.
         *
         * assert_eq!(0, new_files[0].header_start); // Avoid this.
         */

        let new_initial_header_start = w.stream_position().await?;

        /* Push back file header starts for all entries in the covered files. */
        new_files
            .par_iter_mut()
            .map(|f| {
                /* This is probably the only really important thing to change. */
                f.header_start = f.header_start.checked_add(new_initial_header_start).ok_or(
                    ZipError::InvalidArchive(
                        "new header start from merge would have been too large",
                    ),
                )?;
                /* This is only ever used internally to cache metadata lookups (i
                t's not part of the
                 * zip spec), and 0 is the sentinel value. */
                f.central_header_start = 0;
                /* This is an atomic variable so it can be updated from another thread in the
                 * implementation (which is good!). */
                let new_data_start = f
                    .data_start
                    /* NB: it's annoying there's no .checked_fetch_add(), but we don't need it here
                     * because nothing else has any reference to this data. */
                    .load()
                    .checked_add(new_initial_header_start)
                    .ok_or(ZipError::InvalidArchive(
                        "new data start from merge would have been too large",
                    ))?;
                f.data_start.store(new_data_start);
                Ok(())
            })
            .collect::<Result<(), ZipError>>()?;

        let shared = Arc::clone(&self.shared);

        /* Rewind to the beginning of the file.
         *
         * NB: we *could* decide to start copying from shared.offset instead, which
         * would avoid copying over e.g. any pex shebangs or other file contents that start before
         * the first zip file entry. However, zip files actually shouldn't care about garbage data
         * in *between* real entries, since the central directory header records the correct start
         * location of each, and keeping track of that math is more complicated logic that will only
         * rarely be used, since most zips that get merged together are likely to be produced
         * specifically for that purpose (and therefore are unlikely to have a shebang or other
         * preface). Finally, this preserves any data that might actually be desired.
         */
        self.as_mut()
            .pin_reader_assert()
            .seek(io::SeekFrom::Start(0))
            .await?;
        /* Find the end of the file data. */
        let length_to_read = shared.directory_start as usize;

        let inner = self.as_mut().pin_reader_mut_option().take().unwrap();
        /* Produce an AsyncRead that reads bytes up until the start of the central directory
         * header. */
        let mut limited_raw = Limiter::take(0, inner, length_to_read);
        io::copy(&mut limited_raw, &mut w).await?;

        let _ = self
            .pin_reader_mut_option()
            .insert(limited_raw.unwrap_inner_pin());

        /* Return the files we've just written to the data stream. */
        Ok(new_files)
    }

    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use std::{io::Cursor, pin::Pin, sync::Arc};
    /// use tokio::{io::{self, AsyncReadExt, AsyncWriteExt}, fs};
    ///
    /// let mut f = {
    ///   let buf = Cursor::new(Vec::new());
    ///   let mut f = zip::tokio::write::ZipWriter::new(Box::pin(buf));
    ///   let mut fp = Pin::new(&mut f);
    ///   let options = zip::write::FileOptions::default()
    ///     .compression_method(zip::CompressionMethod::Deflated);
    ///   fp.as_mut().start_file("a/b.txt", options).await?;
    ///   fp.write_all(b"hello\n").await?;
    ///   f.finish_into_readable().await?
    /// };
    ///
    /// let t = tempfile::tempdir()?;
    ///
    /// let root = t.path();
    /// Pin::new(&mut f).extract_simple(Arc::new(root.to_path_buf())).await?;
    /// let msg = fs::read_to_string(root.join("a/b.txt")).await?;
    /// assert_eq!(&msg, "hello\n");
    /// # Ok(())
    /// # })}
    ///```
    pub async fn extract_simple(self: Pin<&mut Self>, root: Arc<PathBuf>) -> ZipResult<()> {
        fs::create_dir_all(&*root).await?;

        let entries = self.entries_stream();
        pin_mut!(entries);

        while let Some(mut file) = entries.try_next().await? {
            let name = file.name()?;
            let outpath = root.join(name);

            if CompletedPaths::is_dir(name) {
                fs::create_dir_all(&outpath).await?;
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        fs::create_dir_all(p).await?;
                    }
                }
                let mut outfile = fs::File::create(&outpath).await?;
                io::copy(&mut file, &mut outfile).await?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.data().unix_mode() {
                    fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode)).await?;
                }
            }
        }
        Ok(())
    }

    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use std::{io::Cursor, pin::Pin, sync::Arc};
    /// use tokio::{io::{self, AsyncReadExt, AsyncWriteExt}, fs};
    ///
    /// let mut f = {
    ///   let buf = Cursor::new(Vec::new());
    ///   let mut f = zip::tokio::write::ZipWriter::new(Box::pin(buf));
    ///   let mut fp = Pin::new(&mut f);
    ///   let options = zip::write::FileOptions::default()
    ///     .compression_method(zip::CompressionMethod::Deflated);
    ///   fp.as_mut().start_file("a/b.txt", options).await?;
    ///   fp.write_all(b"hello\n").await?;
    ///   f.finish_into_readable().await?
    /// };
    ///
    /// let t = tempfile::tempdir()?;
    ///
    /// let root = t.path();
    /// Pin::new(&mut f).extract(Arc::new(root.to_path_buf())).await?;
    /// let msg = fs::read_to_string(root.join("a/b.txt")).await?;
    /// assert_eq!(&msg, "hello\n");
    /// # Ok(())
    /// # })}
    ///```
    pub async fn extract(self: Pin<&mut Self>, root: Arc<PathBuf>) -> ZipResult<()> {
        fs::create_dir_all(&*root).await?;

        let names: Vec<&Path> = self
            .shared
            .file_names()
            .map(|name| Path::new::<str>(unsafe { mem::transmute(name) }))
            .collect();

        let paths = Arc::new(sync::RwLock::new(CompletedPaths::new()));
        let (path_tx, path_rx) = mpsc::unbounded_channel::<&Path>();
        let (compressed_tx, compressed_rx) = mpsc::unbounded_channel::<(&Path, Box<[u8]>)>();
        let (paired_tx, paired_rx) = mpsc::unbounded_channel::<(&Path, Box<[u8]>)>();

        /* (1) Before we even start reading from the file handle, we know what our output paths are
         *     going to be from the ZipFileData, so create any necessary subdirectory structures. */
        let root2 = root.clone();
        let paths2 = paths.clone();
        let dirs_task = task::spawn(async move {
            use futures_util::{stream, StreamExt};

            let path_tx = &path_tx;
            stream::iter(names.into_iter())
                .map(Ok)
                .try_for_each_concurrent(None, move |name| {
                    let root2 = root2.clone();
                    let paths2 = paths2.clone();
                    async move {
                        /* dbg!(&name); */
                        let new_dirs = paths2.read().await.new_containing_dirs_needed(&name);
                        for dir in new_dirs.into_iter() {
                            if paths2.read().await.contains(&dir) {
                                continue;
                            }
                            let full_dir = root2.join(&dir);
                            if create_dir_idempotent(full_dir).await?.is_some() {
                                paths2.write().await.confirm_dir(dir);
                            }
                        }

                        path_tx.send(name).unwrap();

                        Ok::<_, ZipError>(())
                    }
                })
                .await?;

            Ok::<_, ZipError>(())
        });

        /* (2) Match up the uncompressed buffers with open file handles to extract to! */
        let shared = self.shared.clone();
        let matching_task = task::spawn(async move {
            use futures_util::{select, FutureExt};
            use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};

            let mut path_rx = UnboundedReceiverStream::new(path_rx);

            let mut compressed_rx = UnboundedReceiverStream::new(compressed_rx);

            let mut remaining_unmatched_paths: IndexMap<&Path, (bool, Option<Box<[u8]>>)> = shared
                .files
                .values()
                .map(|data| {
                    data.enclosed_name()
                        .ok_or(ZipError::InvalidArchive("Invalid file path"))
                        .map(|name| (name, (false, None)))
                })
                .collect::<ZipResult<IndexMap<&Path, _>>>()?;

            let mut stopped_path = false;
            let mut stopped_compressed = false;
            loop {
                let (name, val) = select! {
                    x = path_rx.next().fuse() => match x {
                        Some(name) => {
                            let val = remaining_unmatched_paths.get_mut(&name).unwrap();
                            assert_eq!(val.0, false);
                            val.0 = true;
                            (name, val)
                        },
                        None => {
                            stopped_path = true;
                            continue;
                        },
                    },
                    x = compressed_rx.next().fuse() => match x {
                        Some((name, buf)) => {
                            let val = remaining_unmatched_paths.get_mut(&name).unwrap();
                            assert!(val.1.is_none());
                            let _ = val.1.insert(buf);
                            (name, val)
                        },
                        None => {
                            stopped_compressed = true;
                            continue;
                        },
                    },
                    complete => break,
                };
                /* dbg!(&name); */
                if val.0 && val.1.is_some() {
                    let buf = mem::take(&mut val.1).unwrap();
                    remaining_unmatched_paths.remove(&name).unwrap();
                    paired_tx.send((name, buf)).unwrap();
                }
                if stopped_path && stopped_compressed {
                    break;
                }
                if remaining_unmatched_paths.is_empty() {
                    break;
                }
            }

            Ok::<_, ZipError>(())
        });

        /* (3) Attempt to offload decompression to as many threads as possible. */
        let shared = self.shared.clone();
        let root2 = root.clone();
        let decompress_task = task::spawn(async move {
            use futures_util::StreamExt;
            use tokio_stream::wrappers::UnboundedReceiverStream;

            let paired_rx = UnboundedReceiverStream::new(paired_rx);
            paired_rx
                .map(Ok)
                .try_for_each_concurrent(None, move |(name, buf)| {
                    let shared = shared.clone();
                    let root2 = root2.clone();
                    async move {
                        /* dbg!(&name); */
                        let data = shared.files.get(CompletedPaths::path_str(&name)).unwrap();

                        /* Get the file to write to. */
                        let full_path = root2.join(&name);
                        let mut handle = fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(full_path)
                            .await?;
                        /* Set the length, in case this improves performance writing to the handle
                         * just below. */
                        handle.set_len(data.uncompressed_size).await?;

                        let uncompressed_size = data.uncompressed_size as usize;
                        /* We already know *exactly* how many bytes we will need to read out
                         * (because this info is recorded in the zip file entryu), so we can
                         * allocate exactly that much to minimize allocation as well as
                         * blocking on memory availability for the decompressor. */
                        let mut wrapped = BufReader::with_capacity(
                            num::NonZeroUsize::new(uncompressed_size).unwrap(),
                            Box::pin(ZipFileWrappedReader::construct(
                                data,
                                Box::pin(buf.as_ref()),
                            )),
                        );

                        assert_eq!(
                            uncompressed_size as u64,
                            /* NB: This appears to be faster than calling .read_to_end() and
                             * .write_all() with an intermediate buffer for some reason! */
                            io::copy_buf(&mut wrapped, &mut handle).await?
                        );

                        cfg_if! {
                            if #[cfg(unix)] {
                                use std::os::unix::fs::PermissionsExt;

                                if let Some(mode) = data.unix_mode() {
                                    handle
                                        .set_permissions(std::fs::Permissions::from_mode(mode))
                                        .await?;
                                }
                                handle.sync_all().await?;
                            } else {
                                handle.sync_data().await?;
                            }
                        }

                        Ok::<_, ZipError>(())
                    }
                })
                .await?;

            Ok::<_, ZipError>(())
        });

        /* (4) In order, scan off the raw memory for every file entry into a Box<[u8]> to avoid
         *     interleaving decompression with read i/o. */
        let entries = self.raw_entries_stream();
        pin_mut!(entries);

        while let Some(mut file) = entries.try_next().await? {
            let name: &'static Path = unsafe { mem::transmute(file.name()?) };
            /* dbg!(&name); */
            if CompletedPaths::is_dir(&name) {
                continue;
            }
            let compressed_size = file.data.compressed_size as usize;

            let mut compressed_contents: Vec<u8> = Vec::with_capacity(compressed_size);
            assert_eq!(
                compressed_size,
                file.read_to_end(&mut compressed_contents).await?
            );
            compressed_tx
                .send((name, compressed_contents.into_boxed_slice()))
                .unwrap();
        }
        mem::drop(compressed_tx);

        dirs_task.await.expect("panic in subtask")?;
        matching_task.await.expect("panic in subtask")?;
        decompress_task.await.expect("panic in subtask")?;

        Ok(())
    }
}

impl<S> ZipArchive<S, Shared> {
    pub(crate) fn from_finalized_writer(
        files: Vec<ZipFileData>,
        comment: Vec<u8>,
        stream: Pin<Box<S>>,
        directory_start: u64,
    ) -> ZipResult<Self> {
        use rayon::prelude::*;

        /* This is where the whole file starts. */
        if let Some(initial_offset) = files.first().map(|d| d.header_start) {
            let files: IndexMap<String, ZipFileData> = files
                .into_par_iter()
                .map(|d| (d.file_name.clone(), d))
                .collect();
            let shared = Shared {
                files,
                offset: initial_offset,
                directory_start,
                comment,
            };
            Ok(Self {
                reader: Some(stream),
                shared: Arc::new(shared),
            })
        } else {
            /* We currently require at least 1 file in order to determine the `initial_offset`. */
            Err(ZipError::InvalidArchive(
                "attempt to finalize empty zip writer into readable",
            ))
        }
    }
}

pub(crate) mod read_spec {
    use crate::{
        compression::CompressionMethod,
        result::{ZipError, ZipResult},
        spec::{self, CentralDirectoryHeaderBuffer},
        types::ZipFileData,
    };

    use byteorder::{ByteOrder, LittleEndian};
    use tokio::io::{self, AsyncReadExt, AsyncSeekExt};

    use std::{mem, pin::Pin, slice};

    /// Parse a central directory entry to collect the information for the file.
    pub async fn central_header_to_zip_file<R: io::AsyncRead + io::AsyncSeek>(
        mut reader: Pin<&mut R>,
        archive_offset: u64,
    ) -> ZipResult<ZipFileData> {
        use crate::cp437::FromCp437;
        use crate::types::{AtomicU64, DateTime};

        let central_header_start = reader.stream_position().await?;

        static_assertions::assert_eq_size!([u8; 46], CentralDirectoryHeaderBuffer);
        let mut info = [0u8; 46];
        reader.read_exact(&mut info[..]).await?;

        let CentralDirectoryHeaderBuffer {
            magic,
            version_made_by,
            /* version_needed: _, */
            flag,
            compression_method,
            last_modified_time_timepart,
            last_modified_time_datepart,
            crc32,
            compressed_size,
            uncompressed_size,
            file_name_length,
            extra_field_length,
            file_comment_length,
            /* disk_number_start: _, */
            /* _internal_file_attributes, */
            external_attributes,
            header_start,
            ..
        } = unsafe { mem::transmute(info) };

        if magic != spec::CENTRAL_DIRECTORY_HEADER_SIGNATURE {
            return Err(ZipError::InvalidArchive("Invalid Central Directory header"));
        }

        let encrypted = flag & 1 == 1;
        let is_utf8 = flag & (1 << 11) != 0;
        let using_data_descriptor = flag & (1 << 3) != 0;

        let mut file_name_raw = vec![0; file_name_length as usize];
        reader.read_exact(&mut file_name_raw).await?;
        let mut extra_field = vec![0; extra_field_length as usize];
        reader.read_exact(&mut extra_field).await?;
        let mut file_comment_raw = vec![0; file_comment_length as usize];
        reader.read_exact(&mut file_comment_raw).await?;

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
            system: ((version_made_by >> 8) as u8).into(),
            version_made_by: version_made_by as u8,
            encrypted,
            using_data_descriptor,
            compression_method: {
                #[allow(deprecated)]
                CompressionMethod::from_u16(compression_method)
            },
            compression_level: None,
            last_modified_time: DateTime::from_msdos(
                last_modified_time_datepart,
                last_modified_time_timepart,
            ),
            crc32,
            compressed_size: compressed_size as u64,
            uncompressed_size: uncompressed_size as u64,
            file_name,
            file_name_raw,
            extra_field,
            file_comment,
            header_start: header_start as u64,
            central_header_start,
            data_start: AtomicU64::new(0),
            external_attributes,
            large_file: false,
            aes_mode: None,
        };

        match parse_extra_field(&mut result).await {
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

    async fn parse_extra_field(file: &mut ZipFileData) -> ZipResult<()> {
        let mut reader = std::io::Cursor::new(&file.extra_field);

        while (reader.position() as usize) < file.extra_field.len() {
            let mut buf = [0u8; 32];

            reader.read_exact(&mut buf[..]).await?;

            static_assertions::assert_eq_size!([u8; 32], (u16, u16, u64, u64, u64));

            LittleEndian::from_slice_u16(unsafe {
                slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u16, 14)
            });

            let args: (u16, u16, u64, u64, u64) = unsafe { mem::transmute(buf) };
            let (kind, len, uncompressed_size, compressed_size, header_start) = args;

            let mut len_left = len as i64;
            match kind {
                // Zip64 extended information extra field
                0x0001 => {
                    if file.uncompressed_size >= spec::ZIP64_BYTES_THR {
                        file.large_file = true;
                        file.uncompressed_size = uncompressed_size;
                        len_left -= 8;
                    }
                    if file.compressed_size >= spec::ZIP64_BYTES_THR {
                        file.large_file = true;
                        file.compressed_size = compressed_size;
                        len_left -= 8;
                    }
                    if file.header_start == spec::ZIP64_BYTES_THR {
                        file.header_start = header_start;
                        len_left -= 8;
                    }
                }
                _ => {
                    // Other fields are ignored
                }
            }

            assert!(len_left >= 0);
            if len_left > 0 {
                reader.seek(io::SeekFrom::Current(len_left)).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        compression::CompressionMethod,
        tokio::{combinators::KnownExpanse, write::ZipWriter},
        write::FileOptions,
    };

    use tokio::io::AsyncWriteExt;

    use std::io::Cursor;

    #[tokio::test]
    async fn test_find_content() -> ZipResult<()> {
        let f = {
            let buf = Cursor::new(Vec::new());
            let mut f = ZipWriter::new(Box::pin(buf));
            let mut fp = Pin::new(&mut f);
            let options = FileOptions::default().compression_method(CompressionMethod::Stored);
            fp.as_mut().start_file("a/b.txt", options).await?;
            fp.write_all(b"hello\n").await?;
            f.finish_into_readable().await?
        };

        assert_eq!(1, f.shared.len());
        let data = f
            .shared
            .contiguous_entries()
            .get_index(0)
            .unwrap()
            .1
            .clone();
        assert_eq!("a/b.txt", &data.file_name);

        let mut limited = find_content(&data, f.unwrap_inner_pin()).await?;

        let mut buf = String::new();
        limited.read_to_string(&mut buf).await?;
        assert_eq!(&buf, "hello\n");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_reader() -> ZipResult<()> {
        let f = {
            let buf = Cursor::new(Vec::new());
            let mut f = ZipWriter::new(Box::pin(buf));
            let mut fp = Pin::new(&mut f);
            let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
            fp.as_mut().start_file("a/b.txt", options).await?;
            fp.write_all(b"hello\n").await?;
            f.finish_into_readable().await?
        };

        assert_eq!(1, f.shared.len());
        let data = f
            .shared
            .contiguous_entries()
            .get_index(0)
            .unwrap()
            .1
            .clone();
        assert_eq!(data.crc32, 909783072);
        assert_eq!("a/b.txt", &data.file_name);

        let mut limited = find_content(&data, f.unwrap_inner_pin()).await?;

        let mut buf: Vec<u8> = Vec::new();
        io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await?;
        /* This is compressed, so it should NOT match! */
        assert_ne!(&buf, b"hello\n");
        assert_eq!(buf.len(), limited.full_length());
        assert_eq!(buf.len(), data.compressed_size as usize);
        assert_eq!(b"hello\n".len(), data.uncompressed_size as usize);

        io::AsyncSeekExt::rewind(&mut limited).await?;
        /* This stream should decode the compressed content! */
        let mut decoded = ZipFileWrappedReader::construct(&data, Box::pin(limited));
        let mut buf = String::new();
        io::AsyncReadExt::read_to_string(&mut decoded, &mut buf).await?;
        assert_eq!(&buf, "hello\n");

        Ok(())
    }
}
