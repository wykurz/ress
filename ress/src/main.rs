mod app;
mod cli;
mod render;
mod terminal;
use clap::Parser;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let _guard = cli::init_logging(cli.log_file.as_deref(), cli.verbose)?;
    let source = std::sync::Arc::new(ress_core::source::PreadSource::open(&cli.file)?);
    let document = ress_core::document::Document::new(source, ress_core::Config::default());
    tracing::info!("opened {} ({} bytes)", cli.file.display(), document.size());
    app::run(document).await
}
