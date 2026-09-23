//! Bounded in-memory capture of a guest's stdout/stderr.
//!
//! A [`CaptureFile`] is handed to the WASI environment as stdout or stderr.
//! Writes never block: bytes are appended to a shared buffer until the limit
//! is reached, after which they are dropped and the stream is flagged as
//! truncated. The host reads the result through [`CaptureHandle::snapshot`].

use std::{
    io::{self, SeekFrom},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use virtual_fs::{AsyncRead, AsyncSeek, AsyncWrite, VirtualFile};

/// Shared handle to the captured bytes of one stream.
#[derive(Clone, Debug)]
pub(crate) struct CaptureHandle {
    bytes: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    limit: usize,
}

impl CaptureHandle {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::with_capacity(limit.min(64 * 1024)))),
            truncated: Arc::new(AtomicBool::new(false)),
            limit,
        }
    }

    /// Returns a copy of the captured bytes and whether output was truncated.
    pub(crate) fn snapshot(&self) -> (Vec<u8>, bool) {
        let bytes = self.bytes.lock().expect("capture lock poisoned").clone();
        (bytes, self.truncated.load(Ordering::Acquire))
    }

    fn retain(&self, chunk: &[u8]) {
        let mut buf = self.bytes.lock().expect("capture lock poisoned");
        let room = self.limit.saturating_sub(buf.len());
        let take = room.min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            self.truncated.store(true, Ordering::Release);
        }
    }

    fn len(&self) -> usize {
        self.bytes.lock().expect("capture lock poisoned").len()
    }
}

/// A write-only virtual file backed by a [`CaptureHandle`].
#[derive(Debug)]
pub(crate) struct CaptureFile {
    handle: CaptureHandle,
    cursor: u64,
}

impl CaptureFile {
    pub(crate) fn new(handle: CaptureHandle) -> Self {
        Self { handle, cursor: 0 }
    }
}

impl AsyncWrite for CaptureFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.handle.retain(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for CaptureFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        dst: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let bytes = self.handle.bytes.lock().expect("capture lock poisoned");
        let start = usize::try_from(self.cursor).unwrap_or(usize::MAX);
        if start < bytes.len() {
            let n = dst.remaining().min(bytes.len() - start);
            dst.put_slice(&bytes[start..start + n]);
            drop(bytes);
            self.cursor += n as u64;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for CaptureFile {
    fn start_seek(mut self: Pin<&mut Self>, pos: SeekFrom) -> io::Result<()> {
        let len = self.handle.len() as i128;
        let next = match pos {
            SeekFrom::Start(o) => i128::from(o),
            SeekFrom::End(o) => len + i128::from(o),
            SeekFrom::Current(o) => i128::from(self.cursor) + i128::from(o),
        };
        self.cursor = u64::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))?;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.cursor))
    }
}

impl VirtualFile for CaptureFile {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        self.handle.len() as u64
    }

    fn set_len(&mut self, new_size: u64) -> virtual_fs::Result<()> {
        let new_size = usize::try_from(new_size).map_err(|_| virtual_fs::FsError::InvalidInput)?;
        if new_size > self.handle.limit {
            return Err(virtual_fs::FsError::InvalidInput);
        }
        self.handle
            .bytes
            .lock()
            .map_err(|_| virtual_fs::FsError::Lock)?
            .resize(new_size, 0);
        Ok(())
    }

    fn unlink(&mut self) -> virtual_fs::Result<()> {
        Ok(())
    }

    fn poll_read_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let available = (self.handle.len() as u64).saturating_sub(self.cursor);
        Poll::Ready(Ok(usize::try_from(available).unwrap_or(usize::MAX)))
    }

    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(8192))
    }
}
