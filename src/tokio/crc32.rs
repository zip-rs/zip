//! Helper module to compute a CRC32 checksum

use crate::tokio::WrappedPin;

use crc32fast::Hasher;
use tokio::io;

use std::pin::Pin;
use std::task::{ready, Context, Poll};

/// Reader that validates the CRC32 when it reaches the EOF.
pub struct Crc32Reader<R> {
    inner: Pin<Box<R>>,
    hasher: Hasher,
    check: u32,
    /// Signals if `inner` stores aes encrypted data.
    /// AE-2 encrypted data doesn't use crc and sets the value to 0.
    ae2_encrypted: bool,
}

struct Crc32Proj<'a, R> {
    pub inner: Pin<&'a mut R>,
    pub hasher: &'a mut Hasher,
    pub check: &'a mut u32,
    pub ae2_encrypted: &'a mut bool,
}

impl<R> Crc32Reader<R> {
    /// Get a new Crc32Reader which checks the inner reader against checksum.
    /// The check is disabled if `ae2_encrypted == true`.
    pub(crate) fn new(inner: Pin<Box<R>>, checksum: u32, ae2_encrypted: bool) -> Self {
        Crc32Reader {
            inner,
            hasher: Hasher::new(),
            check: checksum,
            ae2_encrypted,
        }
    }

    #[inline]
    fn project(self: Pin<&mut Self>) -> Crc32Proj<'_, R> {
        unsafe {
            let Self {
                inner,
                hasher,
                check,
                ae2_encrypted,
            } = self.get_unchecked_mut();
            Crc32Proj {
                inner: Pin::new_unchecked(inner.as_mut().get_unchecked_mut()),
                hasher,
                check,
                ae2_encrypted,
            }
        }
    }
}

impl<R> WrappedPin<R> for Crc32Reader<R> {
    fn unwrap_inner_pin(self) -> Pin<Box<R>> {
        self.inner
    }
}

impl<R: io::AsyncRead> io::AsyncRead for Crc32Reader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let start = buf.filled().len();

        let me = self.project();

        if let Err(e) = ready!(me.inner.poll_read(cx, buf)) {
            return Poll::Ready(Err(e));
        }

        let written: usize = buf.filled().len() - start;
        if written == 0 {
            return Poll::Ready(
                if !*me.ae2_encrypted && (*me.check != me.hasher.clone().finalize()) {
                    Err(io::Error::new(io::ErrorKind::Other, "Invalid checksum"))
                } else {
                    Ok(())
                },
            );
        }

        me.hasher.update(&buf.filled()[start..]);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::result::ZipResult;

    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_empty_reader() -> ZipResult<()> {
        let data: &[u8] = b"";
        let mut buf = [0; 1];

        let mut reader = Crc32Reader::new(Box::pin(data.clone()), 0, false);
        assert_eq!(reader.read(&mut buf).await?, 0);

        let mut reader = Crc32Reader::new(Box::pin(data), 1, false);
        assert!(reader
            .read(&mut buf)
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid checksum"));
        Ok(())
    }

    #[tokio::test]
    async fn test_byte_by_byte() -> ZipResult<()> {
        let data: &[u8] = b"1234";
        let mut buf = [0; 1];

        let mut reader = Crc32Reader::new(Box::pin(data), 0x9be3e0a3, false);
        assert_eq!(reader.read(&mut buf).await?, 1);
        assert_eq!(reader.read(&mut buf).await?, 1);
        assert_eq!(reader.read(&mut buf).await?, 1);
        assert_eq!(reader.read(&mut buf).await?, 1);
        assert_eq!(reader.read(&mut buf).await?, 0);
        // Can keep reading 0 bytes after the end
        assert_eq!(reader.read(&mut buf).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_zero_read() -> ZipResult<()> {
        let data: &[u8] = b"1234";
        let mut buf = [0; 5];

        let mut reader = Crc32Reader::new(Box::pin(data), 0x9be3e0a3, false);
        assert_eq!(reader.read(&mut buf[..0]).await?, 0);
        assert_eq!(reader.read(&mut buf).await?, 4);

        Ok(())
    }
}
