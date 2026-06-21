//! the document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
use crate::source::BlockSource;
use std::sync::Arc;
/// the lines to draw for one screen, already chopped to the terminal width.
#[derive(Debug, PartialEq, Eq)]
pub struct ViewportRender {
    pub rows: Vec<String>,
}
/// a read-only view over a file's bytes.
pub struct Document {
    source: Arc<dyn BlockSource>,
    size: u64,
    config: Config,
}
impl Document {
    pub fn new(source: Arc<dyn BlockSource>, config: Config) -> Self {
        let size = source.size();
        Self {
            source,
            size,
            config,
        }
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    /// builds one screen of `rows` display lines starting at line-start byte
    /// offset `top`, each chopped to `cols` characters. Reads forward only until
    /// the screen is filled, EOF, or a bounded scan budget is hit — so a file
    /// with sparse newlines (or one giant unterminated line) can never pull the
    /// whole file into memory before the first paint.
    pub async fn viewport(
        &self,
        top: u64,
        rows: usize,
        cols: usize,
    ) -> anyhow::Result<ViewportRender> {
        let scan_budget = self
            .config
            .block_size
            .max(rows.saturating_mul(cols).saturating_mul(4));
        let mut buf: Vec<u8> = Vec::new();
        let mut pos = top;
        let mut newlines = 0usize;
        while newlines < rows && pos < self.size && buf.len() < scan_budget {
            let block = self.source.read_block(pos, self.config.block_size).await?;
            if block.is_empty() {
                break;
            }
            pos += block.len() as u64;
            newlines += bytecount_newlines(&block);
            buf.extend_from_slice(&block);
        }
        // nothing more to show once we hit EOF or exhaust the scan budget; a line
        // longer than the budget is rendered chopped and truncates the screen
        // (full long-line handling lands with wrap/navigation in a later plan).
        let stop = pos >= self.size || buf.len() >= scan_budget;
        let mut out = Vec::with_capacity(rows);
        let mut line_start = 0usize;
        for i in 0..buf.len() {
            if buf[i] == b'\n' {
                out.push(make_line(&buf[line_start..i], cols));
                line_start = i + 1;
                if out.len() == rows {
                    break;
                }
            }
        }
        if out.len() < rows && line_start < buf.len() && stop {
            out.push(make_line(&buf[line_start..], cols));
        }
        Ok(ViewportRender { rows: out })
    }
}
fn bytecount_newlines(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}
fn make_line(bytes: &[u8], cols: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.strip_suffix('\r').unwrap_or(&text);
    trimmed.chars().take(cols).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    fn doc(data: &'static [u8], block_size: usize) -> Document {
        let src = Arc::new(MockSource::new(bytes::Bytes::from_static(data)));
        Document::new(src, Config { block_size })
    }
    #[tokio::test]
    async fn returns_first_screen() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d.viewport(0, 2, 80).await.unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn returns_all_lines_when_file_is_short() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d.viewport(0, 10, 80).await.unwrap();
        assert_eq!(
            v.rows,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
    #[tokio::test]
    async fn includes_final_line_without_trailing_newline() {
        let d = doc(b"abc", 1 << 20);
        let v = d.viewport(0, 5, 80).await.unwrap();
        assert_eq!(v.rows, vec!["abc".to_string()]);
    }
    #[tokio::test]
    async fn chops_long_lines_to_width() {
        let d = doc(b"xxxxxxxxxx\n", 1 << 20);
        let v = d.viewport(0, 1, 4).await.unwrap();
        assert_eq!(v.rows, vec!["xxxx".to_string()]);
    }
    #[tokio::test]
    async fn handles_lines_spanning_block_boundaries() {
        let d = doc(b"aaaaaa\nbb\n", 4);
        let v = d.viewport(0, 2, 80).await.unwrap();
        assert_eq!(v.rows, vec!["aaaaaa".to_string(), "bb".to_string()]);
    }
    #[tokio::test]
    async fn strips_trailing_carriage_return() {
        let d = doc(b"a\r\nb\r\n", 1 << 20);
        let v = d.viewport(0, 2, 80).await.unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn empty_file_has_no_rows() {
        let d = doc(b"", 1 << 20);
        let v = d.viewport(0, 5, 80).await.unwrap();
        assert!(v.rows.is_empty());
    }
    #[tokio::test]
    async fn starts_from_nonzero_line_offset() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d.viewport(2, 2, 80).await.unwrap();
        assert_eq!(v.rows, vec!["b".to_string(), "c".to_string()]);
    }
    #[tokio::test]
    async fn long_unterminated_line_does_not_read_whole_file() {
        // a single 4096-byte line with no newline, read through tiny blocks: the
        // viewport must stop at its scan budget rather than pull the whole "file"
        // into memory before the first paint.
        let src = Arc::new(MockSource::new(bytes::Bytes::from(vec![b'x'; 4096])));
        let d = Document::new(src.clone(), Config { block_size: 16 });
        let v = d.viewport(0, 2, 4).await.unwrap();
        assert_eq!(v.rows, vec!["xxxx".to_string()]);
        assert!(
            src.read_count() < 16,
            "read {} blocks of a 4096-byte line; expected a bounded few",
            src.read_count()
        );
    }
}
