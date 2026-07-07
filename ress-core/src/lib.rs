//! The headless ress engine: file I/O and the document/viewport model.
pub mod cache;
pub mod document;
pub mod line;
pub mod prefetch;
pub mod scan;
pub mod source;
/// Tunable engine parameters. Defaults target high-latency network filesystems.
#[derive(Debug, Clone)]
pub struct Config {
    /// Size of an interactive read in bytes.
    pub block_size: usize,
    /// Columns a tab advances to the next multiple of.
    pub tab_stop: usize,
    /// Total bytes of file data the shared block cache may hold.
    pub cache_bytes: usize,
    /// Byte budget for one navigation scan (scroll/goto); exhaustion clamps
    /// (forward) or stays put (backward) instead of reading without limit.
    pub nav_scan_budget: usize,
    /// Blocks to keep warm ahead of the viewport in the scroll direction.
    pub prefetch_depth: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            tab_stop: 8,
            cache_bytes: 256 << 20,
            nav_scan_budget: 8 << 20,
            prefetch_depth: 8,
        }
    }
}
