//! the byte-reading seam: `BlockSource` abstracts the file behind the cache,
//! so a future io_uring backend is a drop-in replacement for `PreadSource`.
use bytes::Bytes;
/// reads raw bytes from a fixed, seekable source.
#[async_trait::async_trait]
pub trait BlockSource: Send + Sync {
    /// total size of the source in bytes.
    fn size(&self) -> u64;
    /// reads up to `len` bytes starting at `offset`; returns fewer bytes near
    /// EOF and an empty buffer when `offset >= size`.
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes>;
}

use std::sync::atomic::{AtomicU64, Ordering};
/// an in-memory `BlockSource` for tests: counts reads and can inject latency
/// to simulate a slow network filesystem.
pub struct MockSource {
    data: Bytes,
    reads: AtomicU64,
    latency: std::time::Duration,
}
impl MockSource {
    pub fn new(data: impl Into<Bytes>) -> Self {
        Self {
            data: data.into(),
            reads: AtomicU64::new(0),
            latency: std::time::Duration::ZERO,
        }
    }
    /// adds a fixed per-read delay (simulated network-FS latency).
    pub fn with_latency(mut self, latency: std::time::Duration) -> Self {
        self.latency = latency;
        self
    }
    /// number of `read_block` calls so far.
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}
#[async_trait::async_trait]
impl BlockSource for MockSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        let size = self.data.len() as u64;
        let start = offset.min(size) as usize;
        let end = offset.saturating_add(len as u64).min(size) as usize;
        Ok(self.data.slice(start..end))
    }
}

use std::sync::Arc;
/// a `BlockSource` backed by a real file using positioned reads (`pread`),
/// so concurrent reads never contend a shared file offset. Blocking reads run
/// on tokio's blocking pool; an io_uring backend can replace this later.
pub struct PreadSource {
    file: Arc<std::fs::File>,
    size: u64,
}
impl PreadSource {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            size,
        })
    }
}
#[async_trait::async_trait]
impl BlockSource for PreadSource {
    fn size(&self) -> u64 {
        self.size
    }
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes> {
        let file = self.file.clone();
        let size = self.size;
        let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Bytes> {
            use std::os::unix::fs::FileExt;
            let start = offset.min(size);
            let end = offset.saturating_add(len as u64).min(size);
            let n = (end - start) as usize;
            let mut buf = vec![0u8; n];
            let mut filled = 0;
            while filled < n {
                let read = file.read_at(&mut buf[filled..], start + filled as u64)?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            buf.truncate(filled);
            Ok(Bytes::from(buf))
        })
        .await??;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mock_returns_requested_bytes() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        assert_eq!(src.size(), 10);
        let b = src.read_block(2, 4).await.unwrap();
        assert_eq!(&b[..], b"2345");
    }
    #[tokio::test]
    async fn mock_short_read_at_eof() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let b = src.read_block(8, 100).await.unwrap();
        assert_eq!(&b[..], b"89");
    }
    #[tokio::test]
    async fn mock_empty_past_eof() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let b = src.read_block(50, 4).await.unwrap();
        assert!(b.is_empty());
    }
    #[tokio::test]
    async fn mock_counts_reads() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let _ = src.read_block(0, 4).await.unwrap();
        let _ = src.read_block(4, 4).await.unwrap();
        assert_eq!(src.read_count(), 2);
    }
    #[tokio::test]
    async fn mock_latency_delays_read() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"))
            .with_latency(std::time::Duration::from_millis(20));
        let start = std::time::Instant::now();
        let _ = src.read_block(0, 4).await.unwrap();
        assert!(start.elapsed() >= std::time::Duration::from_millis(20));
    }
    #[tokio::test]
    async fn pread_reads_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let src = PreadSource::open(&path).unwrap();
        assert_eq!(src.size(), 11);
        let b = src.read_block(6, 5).await.unwrap();
        assert_eq!(&b[..], b"world");
    }
    #[tokio::test]
    async fn pread_short_read_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc").unwrap();
        let src = PreadSource::open(&path).unwrap();
        let b = src.read_block(1, 100).await.unwrap();
        assert_eq!(&b[..], b"bc");
    }
    #[tokio::test]
    async fn pread_empty_past_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc").unwrap();
        let src = PreadSource::open(&path).unwrap();
        let b = src.read_block(99, 4).await.unwrap();
        assert!(b.is_empty());
    }
}
