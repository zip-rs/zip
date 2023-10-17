pub mod copy_file_range;

#[macro_export]
macro_rules! cvt {
    ($e:expr) => {{
        let ret = $e;
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }};
}

#[macro_export]
macro_rules! try_libc {
    ($e: expr) => {{
        let ret = $e;
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        ret
    }};
}

pub enum SyscallAvailability {
    Available,
    FailedProbe(std::io::Error),
    NotOnThisPlatform,
}

/// Invalid file descriptor.
///
/// Valid file descriptors are guaranteed to be positive numbers (see `open()` manpage)
/// while negative values are used to indicate errors.
/// Thus -1 will never be overlap with a valid open file.
const INVALID_FD: std::os::fd::RawFd = -1;

pub mod subset {
    use crate::{
        tokio::read::{Shared, SharedData},
        types::ZipFileData,
    };

    use std::{cmp, ops, sync::Arc};

    #[derive(Debug)]
    pub struct SharedSubset {
        parent: Arc<Shared>,
        content_range: ops::Range<u64>,
        entry_range: ops::RangeInclusive<usize>,
    }

    impl SharedData for SharedSubset {
        #[inline]
        fn content_range(&self) -> ops::Range<u64> {
            debug_assert!(self.content_range.start <= self.content_range.end);
            debug_assert!(self.content_range.start >= self.parent.offset());
            debug_assert!(self.content_range.end <= self.parent.directory_start());
            self.content_range.clone()
        }
        #[inline]
        fn comment(&self) -> &[u8] {
            self.parent.comment()
        }
        #[inline]
        fn contiguous_entries(&self) -> &indexmap::map::Slice<String, ZipFileData> {
            debug_assert!(self.entry_range.start() <= self.entry_range.end());
            &self.parent.contiguous_entries()[self.entry_range.clone()]
        }
    }

    impl SharedSubset {
        pub fn parent(&self) -> Arc<Shared> {
            self.parent.clone()
        }

        pub fn split_contiguous_chunks(
            parent: Arc<Shared>,
            num_chunks: usize,
        ) -> Box<[SharedSubset]> {
            let chunk_size = cmp::max(1, parent.len() / num_chunks);
            let all_entry_indices: Vec<usize> = (0..parent.len()).collect();
            let chunked_entry_ranges: Vec<ops::RangeInclusive<usize>> = all_entry_indices
                .chunks(chunk_size)
                .map(|chunk_indices| {
                    let min = *chunk_indices.first().unwrap();
                    let max = *chunk_indices.last().unwrap();
                    min..=max
                })
                .collect();
            assert!(chunked_entry_ranges.len() <= num_chunks);

            let parent_slice = parent.contiguous_entries();
            let chunked_slices: Vec<&indexmap::map::Slice<String, ZipFileData>> =
                chunked_entry_ranges
                    .iter()
                    .map(|range| &parent_slice[range.clone()])
                    .collect();

            let chunk_regions: Vec<ops::Range<u64>> = chunked_slices
                .iter()
                .enumerate()
                .map(|(i, chunk)| {
                    fn begin_pos(chunk: &indexmap::map::Slice<String, ZipFileData>) -> u64 {
                        assert!(!chunk.is_empty());
                        chunk.get_index(0).unwrap().1.header_start
                    }
                    if i == 0 {
                        assert_eq!(parent.offset(), begin_pos(chunk));
                    }
                    let beg = begin_pos(chunk);
                    let end = chunked_slices
                        .get(i + 1)
                        .map(|chunk| *chunk)
                        .map(begin_pos)
                        .unwrap_or(parent.directory_start());
                    beg..end
                })
                .collect();

            let subsets: Vec<SharedSubset> = chunk_regions
                .into_iter()
                .zip(chunked_entry_ranges.into_iter())
                .map(|(content_range, entry_range)| SharedSubset {
                    parent: parent.clone(),
                    content_range,
                    entry_range,
                })
                .collect();

            subsets.into_boxed_slice()
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        use crate::{result::ZipResult, tokio::write::ZipWriter, write::FileOptions};
        use std::{io::Cursor, pin::Pin};
        use tokio::io::AsyncWriteExt;

        #[tokio::test]
        async fn test_split_contiguous_chunks() -> ZipResult<()> {
            let options = FileOptions::default();

            let buf = Cursor::new(Vec::new());
            let mut zip = ZipWriter::new(Box::pin(buf));
            let mut zp = Pin::new(&mut zip);
            zp.as_mut().start_file("a.txt", options).await?;
            zp.write_all(b"hello\n").await?;
            let mut src = zip.finish_into_readable().await?;
            let src = Pin::new(&mut src);

            let buf = Cursor::new(Vec::new());
            let mut zip = ZipWriter::new(Box::pin(buf));
            let mut zp = Pin::new(&mut zip);
            zp.as_mut().start_file("b.txt", options).await?;
            zp.write_all(b"hey\n").await?;
            let mut src2 = zip.finish_into_readable().await?;
            let src2 = Pin::new(&mut src2);

            let buf = Cursor::new(Vec::new());
            let mut zip = ZipWriter::new(Box::pin(buf));
            let mut zp = Pin::new(&mut zip);
            zp.as_mut().start_file("c/d.txt", options).await?;
            zp.write_all(b"asdf!\n").await?;
            let mut src3 = zip.finish_into_readable().await?;
            let src3 = Pin::new(&mut src3);

            let prefix = [0u8; 200];
            let mut buf = Cursor::new(Vec::new());
            buf.write_all(&prefix).await?;
            let mut zip = ZipWriter::new(Box::pin(buf));
            let mut zp = Pin::new(&mut zip);
            zp.as_mut().merge_archive(src).await?;
            zp.as_mut().merge_archive(src2).await?;
            zp.merge_archive(src3).await?;
            let result = zip.finish_into_readable().await?;

            let parent = result.shared();
            assert_eq!(parent.len(), 3);
            assert_eq!(parent.offset(), prefix.len() as u64);
            assert_eq!(200..329, parent.content_range());

            let split_result = SharedSubset::split_contiguous_chunks(parent.clone(), 3);
            assert_eq!(split_result.len(), 3);

            assert_eq!(200..243, split_result[0].content_range);
            assert_eq!(1, split_result[0].len());
            assert_eq!(0..=0, split_result[0].entry_range);

            assert_eq!(243..284, split_result[1].content_range);
            assert_eq!(1, split_result[1].len());
            assert_eq!(1..=1, split_result[1].entry_range);

            assert_eq!(284..329, split_result[2].content_range);
            assert_eq!(1, split_result[2].len());
            assert_eq!(2..=2, split_result[2].entry_range);
            assert_eq!(329, parent.directory_start());

            Ok(())
        }
    }
}
pub use subset::SharedSubset;

pub mod mapped_archive {
    use super::{
        copy_file_range::{self, CopyFileRangeHandle, FromGivenOffset, MutateInnerOffset, Role},
        subset::SharedSubset,
    };
    use crate::{
        result::ZipResult,
        tokio::{
            combinators::Limiter,
            read::{Shared, SharedData, ZipArchive},
            WrappedPin,
        },
    };

    use tempfile;
    use tokio::{
        fs,
        io::{self, AsyncSeekExt},
        task,
    };

    use std::{marker::Unpin, os::unix::io::AsRawFd, pin::Pin, sync::Arc};

    async fn split_mapped_archive_impl(
        mut in_handle: impl CopyFileRangeHandle + Unpin,
        split_chunks: Vec<SharedSubset>,
    ) -> ZipResult<Box<[ZipArchive<Limiter<fs::File>, SharedSubset>]>> {
        let mut ret: Vec<ZipArchive<Limiter<fs::File>, SharedSubset>> =
            Vec::with_capacity(split_chunks.len());

        for chunk in split_chunks.into_iter() {
            let cur_len = chunk.content_len() as usize;
            let cur_start = chunk.content_range().start;

            let backing_file = task::spawn_blocking(|| tempfile::tempfile())
                .await
                .unwrap()?;
            let mut out_handle = FromGivenOffset::new(&backing_file, Role::Writable, 0)?;

            assert_eq!(
                cur_len,
                copy_file_range::copy_file_range(
                    Pin::new(&mut in_handle),
                    Pin::new(&mut out_handle),
                    cur_len,
                )
                .await?
            );

            let inner = Limiter::take(
                cur_start,
                Box::pin(fs::File::from_std(backing_file)),
                cur_len,
            );
            ret.push(ZipArchive::mapped(Arc::new(chunk), Box::pin(inner)));
        }

        Ok(ret.into_boxed_slice())
    }

    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use zip::{result::ZipError, tokio::{read::ZipArchive, write::ZipWriter, os}};
    /// use futures_util::{pin_mut, TryStreamExt};
    /// use tokio::{io::{self, AsyncReadExt, AsyncWriteExt}, fs};
    /// use std::{io::Cursor, pin::Pin, sync::Arc, path::PathBuf};
    ///
    /// let f: ZipArchive<_, _> = {
    ///   let buf = fs::File::from_std(tempfile::tempfile()?);
    ///   let mut f = zip::tokio::write::ZipWriter::new(Box::pin(buf));
    ///   let mut fp = Pin::new(&mut f);
    ///   let options = zip::write::FileOptions::default()
    ///     .compression_method(zip::CompressionMethod::Deflated);
    ///   fp.as_mut().start_file("a.txt", options).await?;
    ///   fp.as_mut().write_all(b"hello\n").await?;
    ///   fp.as_mut().start_file("b.txt", options).await?;
    ///   fp.as_mut().write_all(b"hello2\n").await?;
    ///   fp.as_mut().start_file("c.txt", options).await?;
    ///   fp.write_all(b"hello3\n").await?;
    ///   f.finish_into_readable().await?
    /// };
    ///
    /// let split_f: Vec<ZipArchive<_, _>> = os::split_into_mapped_archive(f, 3).await?.into();
    ///
    /// let mut ret: Vec<(PathBuf, String)> = Vec::with_capacity(3);
    ///
    /// for mut mapped_archive in split_f.into_iter() {
    ///   let entries = Pin::new(&mut mapped_archive).entries_stream();
    ///   pin_mut!(entries);
    ///
    ///   while let Some(mut zf) = entries.try_next().await? {
    ///     let name = zf.name()?.to_path_buf();
    ///     let mut contents = String::new();
    ///     zf.read_to_string(&mut contents).await?;
    ///     ret.push((name, contents));
    ///   }
    /// }
    ///
    /// assert_eq!(
    ///   ret,
    ///   vec![
    ///     (PathBuf::from("a.txt"), "hello\n".to_string()),
    ///     (PathBuf::from("b.txt"), "hello2\n".to_string()),
    ///     (PathBuf::from("c.txt"), "hello3\n".to_string()),
    ///   ],
    /// );
    ///
    /// # Ok(())
    /// # })}
    ///```
    pub async fn split_into_mapped_archive(
        archive: ZipArchive<fs::File, Shared>,
        num_chunks: usize,
    ) -> ZipResult<Box<[ZipArchive<Limiter<fs::File>, SharedSubset>]>> {
        let shared = archive.shared();

        let inner: Pin<Box<fs::File>> = archive.unwrap_inner_pin();
        let inner: Box<fs::File> = Pin::into_inner(inner);
        let mut inner: fs::File = *inner;

        inner.seek(io::SeekFrom::Start(shared.offset())).await?;

        let in_handle = MutateInnerOffset::new(inner.into_std().await, Role::Readable).await?;

        let split_chunks: Vec<SharedSubset> =
            SharedSubset::split_contiguous_chunks(shared, num_chunks).into();

        split_mapped_archive_impl(in_handle, split_chunks).await
    }

    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use zip::{result::ZipError, tokio::{read::ZipArchive, write::ZipWriter, os}};
    /// use futures_util::{pin_mut, TryStreamExt};
    /// use tokio::{io::{self, AsyncReadExt, AsyncWriteExt}, fs};
    /// use std::{io::Cursor, pin::Pin, sync::Arc, path::PathBuf};
    ///
    /// let f: ZipArchive<_, _> = {
    ///   let buf = fs::File::from_std(tempfile::tempfile()?);
    ///   let mut f = zip::tokio::write::ZipWriter::new(Box::pin(buf));
    ///   let mut fp = Pin::new(&mut f);
    ///   let options = zip::write::FileOptions::default()
    ///     .compression_method(zip::CompressionMethod::Deflated);
    ///   fp.as_mut().start_file("a.txt", options).await?;
    ///   fp.as_mut().write_all(b"hello\n").await?;
    ///   fp.as_mut().start_file("b.txt", options).await?;
    ///   fp.as_mut().write_all(b"hello2\n").await?;
    ///   fp.as_mut().start_file("c.txt", options).await?;
    ///   fp.write_all(b"hello3\n").await?;
    ///   f.finish_into_readable().await?
    /// };
    ///
    /// let split_f: Vec<ZipArchive<_, _>> = os::split_mapped_archive_ref(&f, 3).await?.into();
    ///
    /// let mut ret: Vec<(PathBuf, String)> = Vec::with_capacity(3);
    ///
    /// for mut mapped_archive in split_f.into_iter() {
    ///   let entries = Pin::new(&mut mapped_archive).entries_stream();
    ///   pin_mut!(entries);
    ///
    ///   while let Some(mut zf) = entries.try_next().await? {
    ///     let name = zf.name()?.to_path_buf();
    ///     let mut contents = String::new();
    ///     zf.read_to_string(&mut contents).await?;
    ///     ret.push((name, contents));
    ///   }
    /// }
    ///
    /// assert_eq!(
    ///   ret,
    ///   vec![
    ///     (PathBuf::from("a.txt"), "hello\n".to_string()),
    ///     (PathBuf::from("b.txt"), "hello2\n".to_string()),
    ///     (PathBuf::from("c.txt"), "hello3\n".to_string()),
    ///   ],
    /// );
    ///
    /// # Ok(())
    /// # })}
    ///```
    pub async fn split_mapped_archive_ref<S: AsRawFd>(
        archive: &ZipArchive<S, Shared>,
        num_chunks: usize,
    ) -> ZipResult<Box<[ZipArchive<Limiter<fs::File>, SharedSubset>]>> {
        let shared = archive.shared();

        let in_handle = FromGivenOffset::new(archive, Role::Readable, shared.offset() as u32)?;

        let split_chunks: Vec<SharedSubset> =
            SharedSubset::split_contiguous_chunks(shared, num_chunks).into();

        split_mapped_archive_impl(in_handle, split_chunks).await
    }
}
pub use mapped_archive::{split_into_mapped_archive, split_mapped_archive_ref};
