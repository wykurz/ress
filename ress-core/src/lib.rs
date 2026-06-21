//! The headless ress engine: file I/O and the document/viewport model.
pub mod document;
pub mod source;
/// Tunable engine parameters. Defaults target high-latency network filesystems.
#[derive(Debug, Clone)]
pub struct Config {
    /// size of an interactive read in bytes.
    pub block_size: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
        }
    }
}
