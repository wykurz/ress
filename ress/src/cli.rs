//! Command-line interface and logging setup.
use clap::Parser;
/// A fast pager for huge files.
#[derive(Parser, Debug)]
#[command(name = "ress", version, about = "a fast pager for huge files")]
pub struct Cli {
    /// File to view.
    pub file: std::path::PathBuf,
    /// Write debug logs to this file.
    #[arg(long)]
    pub log_file: Option<std::path::PathBuf>,
    /// Increase verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Cache size for file blocks, in MiB.
    #[arg(long, default_value_t = 256)]
    pub cache_mib: u32,
    /// Blocks to prefetch ahead of the viewport (0 disables).
    #[arg(long, default_value_t = 8)]
    pub prefetch_depth: usize,
    /// Max concurrent OS-level file reads (bounds wedged-mount thread pile-up; min 1).
    #[arg(long, default_value_t = ress_core::source::DEFAULT_READ_CONCURRENCY)]
    pub read_concurrency: usize,
}

/// Maps the `-v` count to a tracing level filter string.
pub fn level_str(verbose: u8) -> &'static str {
    match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    }
}
/// Initializes file logging when `--log-file` is set. Returns a guard that must
/// be kept alive for the non-blocking writer to flush. The TUI owns the screen,
/// so logs never go to stdout/stderr.
pub fn init_logging(
    log_file: Option<&std::path::Path>,
    verbose: u8,
) -> anyhow::Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let Some(path) = log_file else {
        return Ok(None);
    };
    let file = std::fs::File::create(path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_env("RESS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level_str(verbose)));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .init();
    Ok(Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_file_argument() {
        let cli = Cli::try_parse_from(["ress", "foo.log"]).unwrap();
        assert_eq!(cli.file, std::path::PathBuf::from("foo.log"));
        assert_eq!(cli.verbose, 0);
        assert!(cli.log_file.is_none());
    }
    #[test]
    fn parses_verbosity_and_log_file() {
        let cli = Cli::try_parse_from(["ress", "-vv", "--log-file", "/tmp/r.log", "f"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert_eq!(cli.log_file, Some(std::path::PathBuf::from("/tmp/r.log")));
    }
    #[test]
    fn requires_a_file() {
        assert!(Cli::try_parse_from(["ress"]).is_err());
    }
    #[test]
    fn maps_verbosity_to_level() {
        assert_eq!(level_str(0), "warn");
        assert_eq!(level_str(1), "info");
        assert_eq!(level_str(2), "debug");
        assert_eq!(level_str(9), "trace");
    }
    #[test]
    fn parses_cache_and_prefetch_flags() {
        let cli = Cli::try_parse_from([
            "ress",
            "--cache-mib",
            "64",
            "--prefetch-depth",
            "2",
            "--read-concurrency",
            "4",
            "f",
        ])
        .unwrap();
        assert_eq!(cli.cache_mib, 64);
        assert_eq!(cli.prefetch_depth, 2);
        assert_eq!(cli.read_concurrency, 4);
    }
    #[test]
    fn cache_and_prefetch_have_defaults() {
        let cli = Cli::try_parse_from(["ress", "f"]).unwrap();
        assert_eq!(cli.cache_mib, 256);
        assert_eq!(cli.prefetch_depth, 8);
        assert_eq!(
            cli.read_concurrency,
            ress_core::source::DEFAULT_READ_CONCURRENCY
        );
        assert_eq!(ress_core::source::DEFAULT_READ_CONCURRENCY, 16); // pinned: README documents this default
    }
}
