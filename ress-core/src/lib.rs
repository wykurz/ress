//! The headless ress engine: file I/O and the document/viewport model.
pub mod document;
pub mod line;
pub mod source;
/// Tunable engine parameters. Defaults target high-latency network filesystems.
#[derive(Debug, Clone)]
pub struct Config {
    /// Size of an interactive read in bytes.
    pub block_size: usize,
    /// Columns a tab advances to the next multiple of.
    pub tab_stop: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            tab_stop: 8,
        }
    }
}
