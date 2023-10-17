use crate::tokio::WrappedPin;

use tokio::io;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

pub mod stream_adaptors {
    use super::*;
    use crate::tokio::channels::{futurized::*, *};

    use std::{cmp, task::ready};

    pub trait KnownExpanse {
        /* TODO: make this have a parameterized Self::Index type, used e.g. with RangeInclusive or
         * something. */
        fn full_length(&self) -> usize;
    }

    ///```
    /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async {
    /// use std::{io::{SeekFrom, Cursor}, pin::Pin};
    /// use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    /// use zip::tokio::combinators::Limiter;
    ///
    /// let mut buf = Cursor::new(Vec::new());
    /// buf.write_all(b"hello\n").await?;
    /// buf.seek(SeekFrom::Start(1)).await?;
    ///
    /// let mut limited = Limiter::take(1, Box::pin(buf), 3);
    /// let mut s = String::new();
    /// limited.read_to_string(&mut s).await?;
    /// assert_eq!(s, "ell");
    ///
    /// limited.seek(SeekFrom::End(-1)).await?;
    /// s.clear();
    /// limited.read_to_string(&mut s).await?;
    /// assert_eq!(s, "l");
    /// # Ok(())
    /// # })}
    ///```
    #[derive(Debug)]
    pub struct Limiter<S> {
        pub max_len: usize,
        pub internal_pos: usize,
        pub start_pos: u64,
        pub source_stream: Pin<Box<S>>,
    }

    impl<S> Limiter<S> {
        pub fn take(start_pos: u64, source_stream: Pin<Box<S>>, limit: usize) -> Self {
            Self {
                max_len: limit,
                internal_pos: 0,
                start_pos,
                source_stream,
            }
        }

        #[inline]
        fn pin_stream(self: Pin<&mut Self>) -> Pin<&mut S> {
            self.get_mut().source_stream.as_mut()
        }

        #[inline]
        fn remaining_len(&self) -> usize {
            debug_assert!(self.internal_pos <= self.max_len);
            self.max_len - self.internal_pos
        }

        #[inline]
        fn limit_length(&self, requested_length: usize) -> usize {
            cmp::min(self.remaining_len(), requested_length)
        }

        #[inline]
        fn push_cursor(&mut self, len: usize) {
            debug_assert!(len <= self.remaining_len());
            self.internal_pos += len;
        }

        #[inline]
        fn convert_seek_request_to_relative(&self, op: io::SeekFrom) -> i64 {
            let cur = self.internal_pos as u64;
            let new_point = cmp::min(
                self.max_len as u64,
                match op {
                    io::SeekFrom::Start(new_point) => new_point,
                    io::SeekFrom::End(from_end) => {
                        cmp::max(0, self.max_len as i64 + from_end) as u64
                    }
                    io::SeekFrom::Current(from_cur) => cmp::max(0, cur as i64 + from_cur) as u64,
                },
            );
            let diff = new_point as i64 - cur as i64;
            diff
        }

        #[inline]
        fn interpret_new_pos(&mut self, new_pos: u64) {
            assert!(new_pos >= self.start_pos);
            assert!(new_pos <= self.start_pos + self.max_len as u64);
            self.internal_pos = (new_pos - self.start_pos) as usize;
        }
    }

    impl<S> WrappedPin<S> for Limiter<S> {
        fn unwrap_inner_pin(self) -> Pin<Box<S>> {
            self.source_stream
        }
    }

    impl<S> KnownExpanse for Limiter<S> {
        #[inline]
        fn full_length(&self) -> usize {
            self.max_len
        }
    }

    impl<S: io::AsyncRead> io::AsyncRead for Limiter<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            debug_assert!(buf.remaining() > 0);

            let num_bytes_to_read: usize = self.as_mut().limit_length(buf.remaining());
            if num_bytes_to_read == 0 {
                return Poll::Ready(Ok(()));
            }

            buf.initialize_unfilled_to(num_bytes_to_read);
            let mut unfilled_buf = buf.take(num_bytes_to_read);
            match self.as_mut().pin_stream().poll_read(cx, &mut unfilled_buf) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(x) => {
                    let bytes_read = unfilled_buf.filled().len();
                    Poll::Ready(x.map(|()| {
                        assert!(bytes_read <= num_bytes_to_read);
                        if bytes_read > 0 {
                            buf.advance(bytes_read);
                            self.push_cursor(bytes_read);
                        }
                    }))
                }
            }
        }
    }

    impl<S: io::AsyncSeek> io::AsyncSeek for Limiter<S> {
        fn start_seek(self: Pin<&mut Self>, op: io::SeekFrom) -> io::Result<()> {
            let diff = self.convert_seek_request_to_relative(op);
            let s = self.get_mut();
            Pin::new(&mut s.source_stream).start_seek(io::SeekFrom::Current(diff))
        }
        fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
            let s = self.get_mut();
            let result = ready!(Pin::new(&mut s.source_stream).poll_complete(cx));
            if let Ok(ref cur_pos) = result {
                s.interpret_new_pos(*cur_pos);
            }
            Poll::Ready(result)
        }
    }

    impl<S: io::AsyncWrite> io::AsyncWrite for Limiter<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            debug_assert!(!buf.is_empty());

            let num_bytes_to_write: usize = self.limit_length(buf.len());
            if num_bytes_to_write == 0 {
                return Poll::Ready(Ok(0));
            }

            let s = self.get_mut();
            match Pin::new(&mut s.source_stream).poll_write(cx, &buf[..num_bytes_to_write]) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(x) => Poll::Ready(x.map(|bytes_written| {
                    assert!(bytes_written <= num_bytes_to_write);
                    if bytes_written > 0 {
                        s.push_cursor(bytes_written);
                    }
                    bytes_written
                })),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let s = self.get_mut();
            Pin::new(&mut s.source_stream).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let s = self.get_mut();
            Pin::new(&mut s.source_stream).poll_shutdown(cx)
        }
    }

    /* ///``` */
    /* /// # fn main() -> zip::result::ZipResult<()> { tokio_test::block_on(async { */
    /* /// use std::{io::{Cursor, prelude::*}, pin::Pin, sync::Arc}; */
    /* /// use tokio::{io::{self, AsyncReadExt}, fs}; */
    /* /// */
    /* /// let mut buf = Cursor::new(Vec::new()); */
    /* /// buf.write_all(b"hello\n")?; */
    /* /// buf.rewind()?; */
    /* /// let mut f = zip::combinators::AsyncIoAdapter::new(buf); */
    /* /// let mut buf: Vec<u8> = Vec::new(); */
    /* /// f.read_to_end(&mut buf).await?; */
    /* /// assert_eq!(&buf, b"hello\n"); */
    /* /// # Ok(()) */
    /* /// # })} */
    /* ///``` */
    /* pub struct AsyncIoAdapter<S> { */
    /*     inner: Arc<sync::Mutex<Option<S>>>, */
    /*     tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<io::Result<()>>>>>, */
    /*     rx: oneshot::Receiver<io::Result<()>>, */
    /*     ring: RingFuturized, */
    /* } */
    /* impl<S> AsyncIoAdapter<S> { */
    /*     pub fn new(inner: S) -> Self { */
    /*         let (tx, rx) = oneshot::channel::<io::Result<()>>(); */
    /*         Self { */
    /*             inner: Arc::new(sync::Mutex::new(Some(inner))), */
    /*             tx: Arc::new(parking_lot::Mutex::new(Some(tx))), */
    /*             rx, */
    /*             ring: RingFuturized::new(), */
    /*         } */
    /*     } */

    /*     pub fn into_inner(self) -> S { */
    /*         self.inner */
    /*             .try_lock_owned() */
    /*             .expect("there should be no further handles to this mutex") */
    /*             .take() */
    /*             .unwrap() */
    /*     } */
    /* } */

    /* impl<S: std::io::Read> AsyncIoAdapter<S> { */
    /*     fn do_write( */
    /*         mut write_lease: WritePermitFuturized, */
    /*         mut inner: sync::OwnedMappedMutexGuard<Option<S>, S>, */
    /*         tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<io::Result<()>>>>>, */
    /*     ) { */
    /*         assert!(!write_lease.is_empty()); */
    /*         match inner.read(&mut write_lease) { */
    /*             Err(e) => { */
    /*                 /\* If any error occurs, assume no bytes were written, as per std::io::Read */
    /*                  * docs. *\/ */
    /*                 write_lease.truncate(0); */
    /*                 if e.kind() != io::ErrorKind::Interrupted { */
    /*                     /\* Interrupted is the only retryable error according to the docs. *\/ */
    /*                     if let Some(tx) = tx.lock().take() { */
    /*                         tx.send(Err(e)) */
    /*                             .expect("receiver should not have been dropped yet!"); */
    /*                     } */
    /*                 } */
    /*             } */
    /*             Ok(n) => { */
    /*                 write_lease.truncate(n); */
    /*                 /\* If we received 0 after providing a non-empty output buf, assume we are */
    /*                  * OVER! *\/ */
    /*                 if n == 0 { */
    /*                     if let Some(tx) = tx.lock().take() { */
    /*                         tx.send(Ok(())) */
    /*                             .expect("receiver should not have been dropped yet!"); */
    /*                     } */
    /*                 } */
    /*             } */
    /*         } */
    /*     } */
    /* } */

    /* impl<S: std::io::Read + Unpin + Send + 'static> io::AsyncRead for AsyncIoAdapter<S> { */
    /*     fn poll_read( */
    /*         self: Pin<&mut Self>, */
    /*         cx: &mut Context<'_>, */
    /*         buf: &mut io::ReadBuf<'_>, */
    /*     ) -> Poll<io::Result<()>> { */
    /*         debug_assert!(buf.remaining() > 0); */

    /*         let s = self.get_mut(); */

    /*         if let Poll::Ready(read_data) = s.ring.poll_read(cx, buf.remaining()) { */
    /*             debug_assert!(!read_data.is_empty()); */
    /*             buf.put_slice(&**read_data); */
    /*             return Poll::Ready(Ok(())); */
    /*         } */

    /*         if let Poll::Ready(result) = Pin::new(&mut s.rx).poll(cx) { */
    /*             return Poll::Ready( */
    /*                 result.expect("sender should not have been dropped without sending!"), */
    /*             ); */
    /*         } */

    /*         let write_data = ready!(s.ring.poll_write(cx, s.ring.capacity())); */
    /*         /\* FIXME: i'm pretty sure this can cause a segfault with the current */
    /*          * mem::ManuallyDrop::take() strategy and global mutex for Ring instances. *\/ */
    /*         let write_data: WritePermitFuturized<'static> = unsafe { mem::transmute(write_data) }; */
    /*         let tx = s.tx.clone(); */
    /*         if let Ok(inner) = s.inner.clone().try_lock_owned() { */
    /*             let inner = sync::OwnedMutexGuard::map(inner, |inner| inner.as_mut().unwrap()); */
    /*             task::spawn_blocking(move || { */
    /*                 Self::do_write(write_data, inner, tx); */
    /*             }); */
    /*             Poll::Pending */
    /*         } else { */
    /*             let inner = s.inner.clone(); */
    /*             task::spawn(async move { */
    /*                 let inner = sync::OwnedMutexGuard::map(inner.lock_owned().await, |inner| { */
    /*                     inner.as_mut().unwrap() */
    /*                 }); */
    /*                 task::spawn_blocking(move || { */
    /*                     Self::do_write(write_data, inner, tx); */
    /*                 }); */
    /*             }); */
    /*             Poll::Pending */
    /*         } */
    /*     } */
    /* } */

    /* impl<S: std::io::Write> AsyncIoAdapter<S> { */
    /*     fn do_read( */
    /*         mut read_lease: ReadPermitFuturized, */
    /*         mut inner: sync::OwnedMappedMutexGuard<Option<S>, S>, */
    /*         tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<io::Result<()>>>>>, */
    /*     ) { */
    /*         match inner.write(&read_lease) { */
    /*             Err(e) => { */
    /*                 if e.kind() == io::ErrorKind::Interrupted { */
    /*                     read_lease.truncate(0); */
    /*                 } else { */
    /*                     if let Some(tx) = tx.lock().take() { */
    /*                         tx.send(Err(e)) */
    /*                             .expect("receiver should not have been dropped yet!"); */
    /*                     } */
    /*                 } */
    /*             } */
    /*             Ok(n) => { */
    /*                 read_lease.truncate(n); */
    /*                 if n == 0 { */
    /*                     if let Some(tx) = tx.lock().take() { */
    /*                         tx.send(Ok(())) */
    /*                             .expect("receiver should not have been dropped yet!"); */
    /*                     } */
    /*                 } */
    /*             } */
    /*         } */
    /*     } */
    /* } */

    /* impl<S: std::io::Write + Unpin + Send + 'static> io::AsyncWrite for AsyncIoAdapter<S> { */
    /*     fn poll_write( */
    /*         self: Pin<&mut Self>, */
    /*         cx: &mut Context<'_>, */
    /*         buf: &[u8], */
    /*     ) -> Poll<io::Result<usize>> { */
    /*         debug_assert!(!buf.is_empty()); */

    /*         let s = self.get_mut(); */

    /*         if let Poll::Ready(mut write_data) = s.ring.poll_write(cx, buf.len()) { */
    /*             debug_assert!(!write_data.is_empty()); */
    /*             let len = write_data.len(); */
    /*             write_data.copy_from_slice(&buf[..len]); */
    /*             return Poll::Ready(Ok(len)); */
    /*         } */

    /*         if let Poll::Ready(result) = Pin::new(&mut s.rx).poll(cx) { */
    /*             return Poll::Ready( */
    /*                 result */
    /*                     .expect("sender should not have been dropped without sending!") */
    /*                     .map(|()| 0), */
    /*             ); */
    /*         } */

    /*         let tx = s.tx.clone(); */
    /*         let read_data = ready!(s.ring.poll_read(cx, buf.len())); */
    /*         if let Ok(inner) = s.inner.clone().try_lock_owned() { */
    /*             let inner = sync::OwnedMutexGuard::map(inner, |inner| inner.as_mut().unwrap()); */
    /*             task::spawn_blocking(move || { */
    /*                 Self::do_read(read_data, inner, tx); */
    /*             }); */
    /*             Poll::Pending */
    /*         } else { */
    /*             let inner = s.inner.clone(); */
    /*             task::spawn(async move { */
    /*                 let inner = sync::OwnedMutexGuard::map(inner.lock_owned().await, |inner| { */
    /*                     inner.as_mut().unwrap() */
    /*                 }); */
    /*                 task::spawn_blocking(move || { */
    /*                     Self::do_read(read_data, inner, tx); */
    /*                 }); */
    /*             }); */
    /*             Poll::Pending */
    /*         } */
    /*     } */

    /*     fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { */
    /*         let s = self.get_mut(); */

    /*         let tx = s.tx.clone(); */
    /*         match ready!(s.ring.poll_read_until_no_space(cx)) { */
    /*             Some(read_data) => { */
    /*                 if let Ok(inner) = s.inner.clone().try_lock_owned() { */
    /*                     let inner = */
    /*                         sync::OwnedMutexGuard::map(inner, |inner| inner.as_mut().unwrap()); */
    /*                     task::spawn_blocking(move || { */
    /*                         Self::do_read(read_data, inner, tx); */
    /*                     }); */
    /*                     Poll::Pending */
    /*                 } else { */
    /*                     let inner = s.inner.clone(); */
    /*                     task::spawn(async move { */
    /*                         let inner = */
    /*                             sync::OwnedMutexGuard::map(inner.lock_owned().await, |inner| { */
    /*                                 inner.as_mut().unwrap() */
    /*                             }); */
    /*                         task::spawn_blocking(move || { */
    /*                             Self::do_read(read_data, inner, tx); */
    /*                         }); */
    /*                     }); */
    /*                     Poll::Pending */
    /*                 } */
    /*             } */
    /*             None => { */
    /*                 if let Ok(inner) = s.inner.clone().try_lock_owned() { */
    /*                     let mut inner = */
    /*                         sync::OwnedMutexGuard::map(inner, |inner| inner.as_mut().unwrap()); */
    /*                     task::spawn_blocking(move || { */
    /*                         match inner.flush() { */
    /*                             Ok(())  */
    /*                         } */
    /*                         Self::do_read(read_data, inner, tx); */
    /*                     }); */
    /*                     Poll::Pending */
    /*                 } else { */
    /*                     let inner = s.inner.clone(); */
    /*                     task::spawn(async move { */
    /*                         let inner = */
    /*                             sync::OwnedMutexGuard::map(inner.lock_owned().await, |inner| { */
    /*                                 inner.as_mut().unwrap() */
    /*                             }); */
    /*                         task::spawn_blocking(move || { */
    /*                             Self::do_read(read_data, inner, tx); */
    /*                         }); */
    /*                     }); */
    /*                     Poll::Pending */
    /*                 } */
    /*             } */
    /*         } */
    /*     } */

    /*     fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> { */
    /*         self.poll_flush(cx) */
    /*     } */
    /* } */
}
pub use stream_adaptors::{/* AsyncIoAdapter, */ KnownExpanse, Limiter};
