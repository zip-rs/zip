/* Taken from https://docs.rs/tokio/latest/src/tokio/io/util/buf_reader.rs.html to fix a few
 * issues. */

use crate::tokio::WrappedPin;

#[cfg(doc)]
use tokio::io::AsyncRead;
use tokio::io::{self, AsyncBufRead};

use std::{
    cmp, fmt,
    num::NonZeroUsize,
    pin::Pin,
    task::{ready, Context, Poll},
};

// used by `BufReader` and `BufWriter`
// https://github.com/rust-lang/rust/blob/master/library/std/src/sys_common/io.rs#L1
const DEFAULT_BUF_SIZE: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(8 * 1024) };

/// The `BufReader` struct adds buffering to any reader.
///
/// It can be excessively inefficient to work directly with a [`AsyncRead`]
/// instance. A `BufReader` performs large, infrequent reads on the underlying
/// [`AsyncRead`] and maintains an in-memory buffer of the results.
///
/// `BufReader` can improve the speed of programs that make *small* and
/// *repeated* read calls to the same file or network socket. It does not
/// help when reading very large amounts at once, or reading just one or a few
/// times. It also provides no advantage when reading from a source that is
/// already in memory, like a `Vec<u8>`.
///
/// When the `BufReader` is dropped, the contents of its buffer will be
/// discarded. Creating multiple instances of a `BufReader` on the same
/// stream can cause data loss.
///
///```
/// # fn main() -> std::io::Result<()> { tokio_test::block_on(async {
/// use zip::tokio::buf_reader::BufReader;
/// use tokio::io::AsyncReadExt;
/// use std::{io::Cursor, pin::Pin};
///
/// let msg = "hello";
/// let buf = Cursor::new(msg.as_bytes());
/// let mut buf_reader = BufReader::new(Box::pin(buf));
///
/// let mut s = String::new();
/// buf_reader.read_to_string(&mut s).await?;
/// assert_eq!(&s, &msg);
/// # Ok(())
/// # })}
///```
pub struct BufReader<R> {
    inner: Pin<Box<R>>,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

struct BufProj<'a, R> {
    pub inner: Pin<&'a mut R>,
    pub buf: &'a mut Box<[u8]>,
}

impl<R> BufReader<R> {
    /// Creates a new `BufReader` with a default buffer capacity. The default is currently 8 KB,
    /// but may change in the future.
    pub fn new(inner: Pin<Box<R>>) -> Self {
        Self::with_capacity(DEFAULT_BUF_SIZE, inner)
    }

    /// Creates a new `BufReader` with the specified buffer capacity.
    pub fn with_capacity(capacity: NonZeroUsize, inner: Pin<Box<R>>) -> Self {
        let buffer = vec![0; capacity.into()];
        Self {
            inner,
            buf: buffer.into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    #[inline]
    pub fn capacity(&self) -> NonZeroUsize {
        unsafe { NonZeroUsize::new_unchecked(self.buf.len()) }
    }

    #[inline]
    fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut R> {
        self.project().inner
    }

    #[inline]
    fn project(self: Pin<&mut Self>) -> BufProj<'_, R> {
        unsafe {
            let Self { inner, buf, .. } = self.get_unchecked_mut();
            BufProj {
                inner: Pin::new_unchecked(inner.as_mut().get_unchecked_mut()),
                buf,
            }
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.cap == self.pos
    }

    /// Returns a reference to the internally buffered data.
    ///
    /// Unlike `fill_buf`, this will not attempt to fill the buffer if it is empty.
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    /// Invalidates all data in the internal buffer.
    #[inline]
    fn discard_buffer(&mut self) {
        self.pos = 0;
        self.cap = 0;
    }

    #[inline]
    fn reset_buffer(&mut self, len: usize) {
        self.pos = 0;
        self.cap = len;
    }

    #[inline]
    fn request_is_larger_than_buffer(&self, buf: &io::ReadBuf<'_>) -> bool {
        buf.remaining() >= self.capacity().get()
    }

    #[inline]
    fn should_bypass_buffer(&self, buf: &io::ReadBuf<'_>) -> bool {
        self.is_empty() && self.request_is_larger_than_buffer(buf)
    }
}

impl<R> WrappedPin<R> for BufReader<R> {
    /// Consumes this `BufReader`, returning the underlying reader.
    ///
    /// Note that any leftover data in the internal buffer is lost.
    fn unwrap_inner_pin(self) -> Pin<Box<R>> {
        self.inner
    }
}

impl<R: io::AsyncRead> BufReader<R> {
    fn bypass_buffer(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let res = ready!(self.as_mut().get_pin_mut().poll_read(cx, buf));
        self.discard_buffer();
        Poll::Ready(res)
    }

    fn reset_to_single_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let len = {
            let me = self.as_mut().project();
            let mut buf = io::ReadBuf::new(me.buf);
            ready!(me.inner.poll_read(cx, &mut buf))?;
            buf.filled().len()
        };

        self.reset_buffer(len);
        Poll::Ready(Ok(()))
    }
}

impl<R: io::AsyncRead> io::AsyncRead for BufReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // If we don't have any buffered data and we're doing a massive read
        // (larger than our internal buffer), bypass our internal buffer
        // entirely.
        if self.should_bypass_buffer(buf) {
            return self.bypass_buffer(cx, buf);
        }
        let rem = ready!(self.as_mut().poll_fill_buf(cx))?;
        let amt = cmp::min(rem.len(), buf.remaining());
        buf.put_slice(&rem[..amt]);
        self.consume(amt);
        Poll::Ready(Ok(()))
    }
}

impl<R: io::AsyncRead> io::AsyncBufRead for BufReader<R> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        if self.is_empty() {
            ready!(self.as_mut().reset_to_single_read(cx))?;
        }
        let buf: &[u8] = self.into_ref().get_ref().buffer();
        Poll::Ready(Ok(buf))
    }

    #[inline]
    fn consume(self: Pin<&mut Self>, amt: usize) {
        if amt == 0 {
            return;
        }
        let me = self.get_mut();
        me.pos = cmp::min(me.pos + amt, me.cap);
    }
}

impl<R: fmt::Debug> fmt::Debug for BufReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufReader")
            .field("reader", &self.inner)
            .field(
                "buffer",
                &format_args!("{}/{}", self.cap - self.pos, self.buf.len()),
            )
            .finish()
    }
}
