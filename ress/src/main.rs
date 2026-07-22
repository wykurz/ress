mod app;
mod cli;
mod render;
mod terminal;
use clap::Parser;
fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let _guard = cli::init_logging(cli.log_file.as_deref(), cli.verbose)?;
    let source = std::sync::Arc::new(ress_core::source::PreadSource::open_with_read_concurrency(
        &cli.file,
        cli.read_concurrency,
    )?);
    let config = ress_core::Config {
        cache_bytes: (cli.cache_mib as usize) << 20,
        prefetch_depth: cli.prefetch_depth,
        ..ress_core::Config::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        // the document spawns its background indexer at construction, so
        // it must be built inside the runtime.
        let document = ress_core::document::Document::new(source, config);
        let name = cli.file.display().to_string();
        tracing::info!("opened {name} ({} bytes)", document.size());
        app::run(document, name).await
    });
    // a wedged blocking read on a dead network mount must never hold the
    // process hostage after quit; abandon outstanding background fills.
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    result
}
