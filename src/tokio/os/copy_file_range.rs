use super::{SyscallAvailability, INVALID_FD};
use crate::{cvt, try_libc};

use cfg_if::cfg_if;
use displaydoc::Display;
use libc;
use once_cell::sync::Lazy;
use tokio::{io, task};
use tokio_pipe::{PipeRead, PipeWrite};

use std::{
    future::Future,
    io::{IoSlice, IoSliceMut},
    mem::{self, MaybeUninit},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    pin::Pin,
    ptr,
    task::{ready, Context, Poll, Waker},
};

fn invalid_copy_file_range() -> io::Error {
    let ret = unsafe {
        libc::copy_file_range(
            INVALID_FD,
            ptr::null_mut(),
            INVALID_FD,
            ptr::null_mut(),
            1,
            0,
        )
    };
    assert_eq!(-1, ret);
    io::Error::last_os_error()
}

pub static HAS_COPY_FILE_RANGE: Lazy<SyscallAvailability> = Lazy::new(|| {
    cfg_if! {
        if #[cfg(target_os = "linux")] {
            match invalid_copy_file_range().raw_os_error().unwrap() {
                libc::EBADF => SyscallAvailability::Available,
                errno => SyscallAvailability::FailedProbe(io::Error::from_raw_os_error(errno)),
            }
        } else {
            SyscallAvailability::NotOnThisPlatform
        }
    }
});

pub struct RawArgs<'a> {
    fd: libc::c_int,
    off: Option<&'a mut libc::off64_t>,
}

pub trait CopyFileRangeHandle {
    fn role(&self) -> Role;
    fn as_args(self: Pin<&mut Self>) -> RawArgs<'_>;
}

pub struct MutateInnerOffset {
    role: Role,
    owned_fd: OwnedFd,
    offset: u64,
}

impl MutateInnerOffset {
    pub async fn new(f: impl IntoRawFd, role: Role) -> io::Result<Self> {
        let raw_fd = validate_raw_fd(f.into_raw_fd(), role)?;
        let offset: libc::off64_t =
            task::spawn_blocking(move || unsafe { cvt!(libc::lseek(raw_fd, 0, libc::SEEK_CUR)) })
                .await
                .unwrap()?;
        let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        Ok(Self {
            role,
            owned_fd,
            offset: offset as u64,
        })
    }

    pub fn into_owned(self) -> OwnedFd {
        self.owned_fd
    }
}

impl IntoRawFd for MutateInnerOffset {
    fn into_raw_fd(self) -> RawFd {
        self.into_owned().into_raw_fd()
    }
}

impl From<MutateInnerOffset> for std::fs::File {
    fn from(x: MutateInnerOffset) -> Self {
        x.into_owned().into()
    }
}

impl CopyFileRangeHandle for MutateInnerOffset {
    fn role(&self) -> Role {
        self.role
    }
    fn as_args(self: Pin<&mut Self>) -> RawArgs<'_> {
        RawArgs {
            fd: self.owned_fd.as_raw_fd(),
            off: None,
        }
    }
}

impl std::io::Read for MutateInnerOffset {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.owned_fd.as_raw_fd();
        /* FIXME: make this truly async instead of sync, perf permitting! */
        let num_read =
            unsafe { cvt!(libc::read(fd, mem::transmute(buf.as_mut_ptr()), buf.len())) }?;
        assert!(num_read >= 0);
        Ok(num_read as usize)
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let fd = self.owned_fd.as_raw_fd();
        /* FIXME: make this truly async instead of sync, perf permitting! */
        let num_read = unsafe {
            cvt!(libc::readv(
                fd,
                mem::transmute(bufs.as_ptr()),
                bufs.len() as libc::c_int
            ))
        }?;
        assert!(num_read >= 0);
        Ok(num_read as usize)
    }

    #[inline]
    fn is_read_vectored(&self) -> bool {
        true
    }
}

impl std::io::Seek for MutateInnerOffset {
    fn seek(&mut self, arg: io::SeekFrom) -> io::Result<u64> {
        let (offset, whence): (libc::off64_t, libc::c_int) = match arg {
            io::SeekFrom::Start(pos) => (pos as libc::off64_t, libc::SEEK_SET),
            io::SeekFrom::Current(diff) => (diff, libc::SEEK_CUR),
            io::SeekFrom::End(diff) => (diff, libc::SEEK_END),
        };
        let fd = self.owned_fd.as_raw_fd();
        let new_offset = cvt!(unsafe { libc::lseek(fd, offset, whence) })?;
        self.offset = new_offset as u64;
        Ok(self.offset)
    }
}

impl std::io::Write for MutateInnerOffset {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let fd = self.owned_fd.as_raw_fd();
        let num_written =
            cvt!(unsafe { libc::write(fd, mem::transmute(buf.as_ptr()), buf.len()) })?;
        assert!(num_written > 0);
        Ok(num_written as usize)
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let fd = self.owned_fd.as_raw_fd();
        let num_written = cvt!(unsafe {
            libc::writev(fd, mem::transmute(bufs.as_ptr()), bufs.len() as libc::c_int)
        })?;
        assert!(num_written > 0);
        Ok(num_written as usize)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = cvt!(unsafe { libc::fdatasync(self.owned_fd.as_raw_fd()) })?;
        Ok(())
    }
}

impl io::AsyncRead for MutateInnerOffset {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let fd = self.owned_fd.as_raw_fd();
        /* FIXME: make this truly async instead of sync, perf permitting! */
        let num_read = unsafe {
            cvt!(libc::read(
                fd,
                mem::transmute(buf.initialize_unfilled().as_mut_ptr()),
                buf.remaining(),
            ))
        }?;
        assert!(num_read >= 0);
        buf.set_filled(buf.filled().len() + num_read as usize);
        Poll::Ready(Ok(()))
    }
}

impl io::AsyncSeek for MutateInnerOffset {
    fn start_seek(mut self: Pin<&mut Self>, arg: io::SeekFrom) -> io::Result<()> {
        let (offset, whence): (libc::off64_t, libc::c_int) = match arg {
            io::SeekFrom::Start(pos) => (pos as libc::off64_t, libc::SEEK_SET),
            io::SeekFrom::Current(diff) => (diff, libc::SEEK_CUR),
            io::SeekFrom::End(diff) => (diff, libc::SEEK_END),
        };
        let fd = self.owned_fd.as_raw_fd();
        /* FIXME: make this async/pollable! */
        let new_offset = cvt!(unsafe { libc::lseek(fd, offset, whence) })?;
        self.offset = new_offset as u64;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.offset))
    }
}

impl io::AsyncWrite for MutateInnerOffset {
    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let fd = self.owned_fd.as_raw_fd();
        let num_written = cvt!(unsafe {
            /* FIXME: make this async/pollable! */
            libc::writev(fd, mem::transmute(bufs.as_ptr()), bufs.len() as libc::c_int)
        })?;
        assert!(num_written > 0);
        Poll::Ready(Ok(num_written as usize))
    }

    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let fd = self.owned_fd.as_raw_fd();
        /* FIXME: make this async/pollable! */
        let num_written =
            cvt!(unsafe { libc::write(fd, mem::transmute(buf.as_ptr()), buf.len()) })?;
        assert!(num_written > 0);
        Poll::Ready(Ok(num_written as usize))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cvt!(unsafe { libc::fdatasync(self.owned_fd.as_raw_fd()) })?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[derive(Clone)]
pub struct FromGivenOffset {
    fd: RawFd,
    pub offset: i64,
    role: Role,
}

impl FromGivenOffset {
    pub fn new(f: &impl AsRawFd, role: Role, init: u32) -> io::Result<Self> {
        let raw_fd = f.as_raw_fd();
        let fd = validate_raw_fd(raw_fd, role)?;
        Ok(Self {
            fd,
            role,
            offset: init as i64,
        })
    }

    fn stat_sync(&self) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();

        try_libc!(unsafe { libc::fstat(self.fd, stat.as_mut_ptr()) });

        Ok(unsafe { stat.assume_init() })
    }

    fn len_sync(&self) -> io::Result<libc::off_t> {
        Ok(self.stat_sync()?.st_size)
    }

    pub async fn stat(&self) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();

        let fd = self.fd;
        task::spawn_blocking(move || cvt!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) }))
            .await
            .unwrap()?;

        Ok(unsafe { stat.assume_init() })
    }

    pub async fn len(&self) -> io::Result<libc::off_t> {
        Ok(self.stat().await?.st_size)
    }
}

impl std::io::Read for FromGivenOffset {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let num_read = cvt!(unsafe {
            libc::pread(
                self.fd,
                mem::transmute(buf.as_mut_ptr()),
                buf.len(),
                self.offset,
            )
        })?;
        assert!(num_read >= 0);
        self.offset += num_read as i64;
        Ok(num_read as usize)
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let num_read = cvt!(unsafe {
            libc::preadv(
                self.fd,
                mem::transmute(bufs.as_mut_ptr()),
                bufs.len() as libc::c_int,
                self.offset,
            )
        })?;
        assert!(num_read >= 0);
        self.offset += num_read as i64;
        Ok(num_read as usize)
    }

    #[inline]
    fn is_read_vectored(&self) -> bool {
        true
    }
}

impl std::io::Seek for FromGivenOffset {
    fn seek(&mut self, arg: io::SeekFrom) -> io::Result<u64> {
        self.offset = match arg {
            io::SeekFrom::Start(from_start) => from_start as i64,
            io::SeekFrom::Current(diff) => self.offset + diff,
            io::SeekFrom::End(from_end) => {
                assert!(from_end <= 0);
                let full_len = self.len_sync()?;
                full_len + from_end
            }
        };
        Ok(self.offset as u64)
    }
}

impl std::io::Write for FromGivenOffset {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let num_written = cvt!(unsafe {
            libc::pwrite(
                self.fd,
                mem::transmute(buf.as_ptr()),
                buf.len(),
                self.offset,
            )
        })?;
        assert!(num_written > 0);
        self.offset += num_written as i64;
        Ok(num_written as usize)
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let num_written = cvt!(unsafe {
            libc::pwritev(
                self.fd,
                mem::transmute(bufs.as_ptr()),
                bufs.len() as libc::c_int,
                self.offset,
            )
        })?;
        assert!(num_written > 0);
        self.offset += num_written as i64;
        Ok(num_written as usize)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = cvt!(unsafe { libc::fdatasync(self.fd) })?;
        Ok(())
    }
}

impl io::AsyncRead for FromGivenOffset {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        debug_assert!(buf.remaining() > 0);

        let prev_filled = buf.filled().len();
        /* FIXME: don't block here! */
        let num_read = cvt!(unsafe {
            libc::pread(
                self.fd,
                mem::transmute(buf.initialize_unfilled().as_mut_ptr()),
                buf.remaining(),
                self.offset,
            )
        })?;
        self.offset += num_read as i64;
        buf.set_filled(prev_filled + num_read as usize);

        Poll::Ready(Ok(()))
    }
}

impl io::AsyncSeek for FromGivenOffset {
    fn start_seek(mut self: Pin<&mut Self>, op: io::SeekFrom) -> io::Result<()> {
        self.offset = match op {
            io::SeekFrom::Start(from_start) => from_start as i64,
            io::SeekFrom::Current(diff) => self.offset + diff,
            io::SeekFrom::End(from_end) => {
                assert!(from_end <= 0);
                let full_len = self.len_sync()?;
                full_len + from_end
            }
        };
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        assert!(self.offset >= 0);
        Poll::Ready(Ok(self.offset as u64))
    }
}

impl io::AsyncWrite for FromGivenOffset {
    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        /* FIXME: don't block here! */
        let num_written = cvt!(unsafe {
            libc::pwritev(
                self.fd,
                mem::transmute(bufs.as_ptr()),
                bufs.len() as libc::c_int,
                self.offset,
            )
        })?;
        assert!(num_written > 0);
        self.offset += num_written as i64;

        Poll::Ready(Ok(num_written as usize))
    }

    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        debug_assert!(buf.len() > 0);

        /* FIXME: don't block here! */
        let num_written = cvt!(unsafe {
            libc::pwrite(
                self.fd,
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                self.offset,
            )
        })?;
        assert!(num_written > 0);
        self.offset += num_written as i64;

        Poll::Ready(Ok(num_written as usize))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cvt!(unsafe { libc::fdatasync(self.fd) })?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

impl AsRawFd for FromGivenOffset {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl CopyFileRangeHandle for FromGivenOffset {
    fn role(&self) -> Role {
        self.role
    }
    fn as_args(self: Pin<&mut Self>) -> RawArgs {
        let Self {
            fd, ref mut offset, ..
        } = self.get_mut();
        RawArgs {
            fd: fd.as_raw_fd(),
            off: Some(offset),
        }
    }
}

#[inline]
fn convert_option_ptr<T>(mut p: Option<&mut T>) -> *mut T {
    if let Some(ref mut val) = p {
        &mut **val
    } else {
        ptr::null_mut()
    }
}

pub async fn iter_copy_file_range(
    src: Pin<&mut impl CopyFileRangeHandle>,
    dst: Pin<&mut impl CopyFileRangeHandle>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(src.role(), Role::Readable);
    let RawArgs {
        fd: fd_in,
        off: off_in,
    } = src.as_args();
    let off_in = convert_option_ptr(off_in);
    let off_in = off_in as usize;

    assert_eq!(dst.role(), Role::Writable);
    let RawArgs {
        fd: fd_out,
        off: off_out,
    } = dst.as_args();
    let off_out = convert_option_ptr(off_out);
    let off_out = off_out as usize;

    /* These must always be set to 0 for now. */
    const FUTURE_FLAGS: libc::c_uint = 0;
    let written: libc::ssize_t = task::spawn_blocking(move || {
        let off_in = off_in as *mut libc::off64_t;
        let off_out = off_out as *mut libc::off64_t;
        cvt!(unsafe { libc::copy_file_range(fd_in, off_in, fd_out, off_out, len, FUTURE_FLAGS) })
    })
    .await
    .unwrap()?;
    assert!(written >= 0);
    Ok(written as usize)
}

pub async fn iter_splice_from_pipe(
    mut src: Pin<&mut PipeRead>,
    dst: Pin<&mut impl CopyFileRangeHandle>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(dst.role(), Role::Writable);
    let RawArgs {
        fd: fd_out,
        off: off_out,
    } = dst.as_args();

    src.splice_to_blocking_fd(fd_out, off_out, len, false).await
}

pub async fn splice_from_pipe(
    mut src: Pin<&mut PipeRead>,
    mut dst: Pin<&mut impl CopyFileRangeHandle>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_splice_from_pipe(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

pub async fn iter_splice_to_pipe(
    src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut PipeWrite>,
    len: usize,
) -> io::Result<usize> {
    assert_eq!(src.role(), Role::Readable);
    let RawArgs {
        fd: fd_in,
        off: off_in,
    } = src.as_args();

    dst.splice_from_blocking_fd(fd_in, off_in, len).await
}

pub async fn splice_to_pipe(
    mut src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut PipeWrite>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_splice_to_pipe(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

pub async fn copy_file_range(
    mut src: Pin<&mut impl CopyFileRangeHandle>,
    mut dst: Pin<&mut impl CopyFileRangeHandle>,
    full_len: usize,
) -> io::Result<usize> {
    let mut remaining = full_len;

    while remaining > 0 {
        let cur_written = iter_copy_file_range(src.as_mut(), dst.as_mut(), remaining).await?;
        assert!(cur_written <= remaining);
        if cur_written == 0 {
            return Ok(full_len - remaining);
        }
        remaining -= cur_written;
    }
    Ok(full_len)
}

fn check_regular_file(fd: RawFd) -> io::Result<()> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();

    try_libc!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) });

    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) == libc::S_IFREG {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Fd is not a regular file",
        ))
    }
}

fn get_status_flags(fd: RawFd) -> io::Result<libc::c_int> {
    Ok(try_libc!(unsafe { libc::fcntl(fd, libc::F_GETFL) }))
}

#[derive(Copy, Clone, Debug, Display, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Role {
    /// fd has the read capability
    Readable,
    /// fd has the write capability
    Writable,
}

impl Role {
    fn allowed_modes(&self) -> &'static [libc::c_int] {
        static READABLE: &'static [libc::c_int] = &[libc::O_RDONLY, libc::O_RDWR];
        static WRITABLE: &'static [libc::c_int] = &[libc::O_WRONLY, libc::O_RDWR];
        match self {
            Self::Readable => READABLE,
            Self::Writable => WRITABLE,
        }
    }

    fn check_append(&self, flags: libc::c_int) -> io::Result<()> {
        if let Self::Writable = self {
            if (flags & libc::O_APPEND) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Writable Fd was set for append!",
                ));
            }
        }
        Ok(())
    }

    fn errmsg(&self) -> &'static str {
        static READABLE: &'static str = "Fd is not readable!";
        static WRITABLE: &'static str = "Fd is not writable!";
        match self {
            Self::Readable => READABLE,
            Self::Writable => WRITABLE,
        }
    }

    pub(crate) fn validate_flags(&self, flags: libc::c_int) -> io::Result<()> {
        let access_mode = flags & libc::O_ACCMODE;

        if !self.allowed_modes().contains(&access_mode) {
            return Err(io::Error::new(io::ErrorKind::Other, self.errmsg()));
        }
        self.check_append(flags)?;

        Ok(())
    }
}

fn validate_raw_fd(fd: RawFd, role: Role) -> io::Result<RawFd> {
    check_regular_file(fd)?;

    let status_flags = get_status_flags(fd)?;
    role.validate_flags(status_flags)?;

    Ok(fd)
}

#[cfg(test)]
mod test {
    use super::*;

    use std::fs;

    #[test]
    fn check_copy_file_range() {
        assert!(matches!(
            *HAS_COPY_FILE_RANGE,
            SyscallAvailability::Available
        ));
    }

    #[test]
    fn check_readable_writable_file() {
        let f = tempfile::tempfile().unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Readable).unwrap();
        validate_raw_fd(fd, Role::Writable).unwrap();
    }

    #[test]
    fn check_only_writable() {
        let td = tempfile::tempdir().unwrap();
        let f = fs::OpenOptions::new()
            .create_new(true)
            .read(false)
            .write(true)
            .open(td.path().join("asdf.txt"))
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Writable).unwrap();
        assert!(validate_raw_fd(fd, Role::Readable).is_err());
    }

    #[test]
    fn check_only_readable() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("asdf.txt");
        fs::write(&p, b"wow!").unwrap();

        let f = fs::OpenOptions::new()
            .read(true)
            .write(false)
            .open(&p)
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        validate_raw_fd(fd, Role::Readable).unwrap();
        assert!(validate_raw_fd(fd, Role::Writable).is_err());
    }

    #[test]
    fn check_no_append() {
        let td = tempfile::tempdir().unwrap();

        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(td.path().join("asdf.txt"))
            .unwrap();
        let fd: RawFd = f.as_raw_fd();

        assert!(validate_raw_fd(fd, Role::Writable).is_err());
        assert!(validate_raw_fd(fd, Role::Readable).is_err());
    }

    #[tokio::test]
    async fn read_ref_into_write_owned() {
        use std::io::{Read, Seek};

        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("asdf.txt");
        fs::write(&p, b"wow!").unwrap();

        let in_file = fs::File::open(&p).unwrap();
        let mut src = FromGivenOffset::new(&in_file, Role::Readable, 0).unwrap();
        let sp = Pin::new(&mut src);

        let p2 = td.path().join("asdf2.txt");
        let out_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            /* Need this to read the output file contents at the end! */
            .read(true)
            .open(&p2)
            .unwrap();
        let mut dst = MutateInnerOffset::new(out_file, Role::Writable)
            .await
            .unwrap();
        let dp = Pin::new(&mut dst);

        /* Explicit offset begins at 0. */
        assert_eq!(0, sp.offset);

        /* 4 bytes were written. */
        assert_eq!(
            4,
            /* NB: 5 bytes were requested! */
            copy_file_range(sp, dp, 5).await.unwrap()
        );
        assert_eq!(4, src.offset);

        let mut dst: fs::File = dst.into_owned().into();
        assert_eq!(4, dst.stream_position().unwrap());
        dst.rewind().unwrap();
        let mut s = String::new();
        dst.read_to_string(&mut s).unwrap();
        assert_eq!(&s, "wow!");
    }

    #[tokio::test]
    async fn test_splice_blocking() {
        use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

        let mut in_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        in_file.write_all(b"hello").await.unwrap();
        in_file.rewind().await.unwrap();
        let mut in_file = MutateInnerOffset::new(in_file.into_std().await, Role::Readable)
            .await
            .unwrap();

        let mut out_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let mut out_file_handle = FromGivenOffset::new(&out_file, Role::Writable, 0).unwrap();

        let (mut r, mut w) = tokio_pipe::pipe().unwrap();

        let w_task = tokio::spawn(async move {
            assert_eq!(
                5,
                splice_to_pipe(Pin::new(&mut in_file), Pin::new(&mut w), 6)
                    .await
                    .unwrap()
            );

            let in_file: fs::File = in_file.into();
            let mut in_file = tokio::fs::File::from_std(in_file);
            assert_eq!(5, in_file.stream_position().await.unwrap());
        });

        let r_task = tokio::spawn(async move {
            assert_eq!(
                5,
                splice_from_pipe(Pin::new(&mut r), Pin::new(&mut out_file_handle), 6)
                    .await
                    .unwrap()
            );
            assert_eq!(out_file_handle.offset, 5);
        });

        tokio::try_join!(w_task, r_task).unwrap();

        assert_eq!(0, out_file.stream_position().await.unwrap());
        let mut s = String::new();
        out_file.read_to_string(&mut s).await.unwrap();
        assert_eq!(&s, "hello");
    }

    #[test]
    fn test_has_vectored_write() {
        use tokio::io::AsyncWrite;

        let f = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        assert!(!f.is_write_vectored());

        let f = std::io::Cursor::new(Vec::new());
        assert!(f.is_write_vectored());
    }

    #[tokio::test]
    async fn test_wrappers_have_vectored_write() {
        use tokio::io::AsyncWrite;

        let f = tempfile::tempfile().unwrap();
        let f = MutateInnerOffset::new(f, Role::Writable).await.unwrap();
        assert!(f.is_write_vectored());

        let f = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        assert!(!f.is_write_vectored());
        let f = FromGivenOffset::new(&f, Role::Writable, 0).unwrap();
        assert!(f.is_write_vectored());
    }

    #[tokio::test]
    async fn test_io_copy_for_owned_wrapper() {
        use std::io::prelude::*;

        let mut f = MutateInnerOffset::new(tempfile::tempfile().unwrap(), Role::Writable)
            .await
            .unwrap();
        f.write_all(b"hello").unwrap();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        let mut f_in = MutateInnerOffset::new(f, Role::Readable).await.unwrap();

        let mut f_out = MutateInnerOffset::new(tempfile::tempfile().unwrap(), Role::Writable)
            .await
            .unwrap();

        std::io::copy(&mut f_in, &mut f_out).unwrap();

        let mut f: std::fs::File = f_out.into();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(&s, "hello");
    }

    #[tokio::test]
    async fn test_io_copy_for_non_owned_wrapper() {
        use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

        let in_backing_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let mut f_in = FromGivenOffset::new(&in_backing_file, Role::Writable, 0).unwrap();
        f_in.write_all(b"hello").await.unwrap();
        let mut f_in = FromGivenOffset::new(&in_backing_file, Role::Readable, 0).unwrap();

        let out_backing_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let mut f_out = FromGivenOffset::new(&out_backing_file, Role::Writable, 0).unwrap();

        io::copy(&mut Pin::new(&mut f_in), &mut Pin::new(&mut f_out))
            .await
            .unwrap();

        f_out.seek(io::SeekFrom::Start(0)).await.unwrap();

        let mut s = String::new();
        f_out.read_to_string(&mut s).await.unwrap();
        assert_eq!(&s, "hello");
    }

    #[tokio::test]
    async fn test_cloneable_wrapper() {
        use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

        let backing_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let mut f1 = FromGivenOffset::new(&backing_file, Role::Writable, 0).unwrap();
        let mut f1b = f1.clone();
        f1.write_all(b"hell").await.unwrap();
        f1b.seek(io::SeekFrom::Current(4)).await.unwrap();
        f1b.write_all(b"o").await.unwrap();

        let mut f2 = FromGivenOffset::new(&backing_file, Role::Readable, 0).unwrap();
        let mut f3 = f2.clone();

        let mut s = String::new();
        f2.read_to_string(&mut s).await.unwrap();
        assert_eq!(&s, "hello");
        s.clear();
        assert_eq!(&s, "");
        f3.read_to_string(&mut s).await.unwrap();
        assert_eq!(&s, "hello");
    }
}
