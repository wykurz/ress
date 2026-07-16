//! Thin CLI over the ress-filegen library — fixture files for the perf
//! harness and manual reproduction. Size flags are conveniences over
//! `Spec::target_bytes`; explicit flags override the chosen preset's fields.
use clap::Parser;
#[derive(Parser, Debug)]
#[command(about = "deterministic log-like fixture generator")]
struct Cli {
    /// output path (the file is created or truncated)
    out: Option<std::path::PathBuf>,
    /// start from a named preset (see --list-presets)
    #[arg(long)]
    preset: Option<String>,
    /// print available preset names and exit
    #[arg(long)]
    list_presets: bool,
    #[arg(long)]
    seed: Option<u64>,
    /// target size in KiB / MiB / GiB (choose one; overrides the preset's)
    #[arg(long, conflicts_with_all = ["mib", "gib"])]
    kib: Option<u64>,
    #[arg(long, conflicts_with = "gib")]
    mib: Option<u64>,
    #[arg(long)]
    gib: Option<u64>,
    /// inclusive lower bound of a uniformly drawn line length in bytes
    /// (requires --max-len; a drawn length is in [min-len, max-len), so
    /// min-len == max-len is rejected as an empty range, not a fixed length)
    #[arg(long, requires = "max_len")]
    min_len: Option<usize>,
    /// exclusive upper bound of a uniformly drawn line length in bytes
    /// (requires --min-len; see --min-len for the [min-len, max-len) range)
    #[arg(long, requires = "min_len")]
    max_len: Option<usize>,
    #[arg(long, conflicts_with_all = ["min_len", "max_len"])]
    fixed_len: Option<usize>,
    #[arg(long)]
    no_trailing_newline: bool,
    #[arg(long, requires = "mega_len")]
    mega_every: Option<u64>,
    #[arg(long, requires = "mega_every")]
    mega_len: Option<usize>,
    #[arg(long)]
    utf8_fraction: Option<f64>,
}
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.list_presets {
        for name in ress_filegen::PRESETS {
            println!("{name}");
        }
        return Ok(());
    }
    let mut spec = match &cli.preset {
        Some(name) => {
            ress_filegen::preset(name).ok_or_else(|| anyhow::anyhow!("unknown preset: {name}"))?
        }
        None => ress_filegen::preset("uniform-log").expect("default preset exists"),
    };
    if let Some(seed) = cli.seed {
        spec.seed = seed;
    }
    // `<<` never panics on the bits it shifts OUT of a u64 — only a shift
    // amount past the type's own width panics (not the case here, 10/20/30
    // are all well within 64) — so a huge --kib/--mib/--gib value would
    // otherwise wrap silently to some small, unrelated target_bytes instead
    // of erroring. checked_mul catches exactly that: it is the overflow-
    // checked form of `n << k` (equivalent to `n * 2^k` for a non-negative
    // shift), so any value that would lose bits is rejected here, before
    // target_bytes is ever assigned.
    if let Some(k) = cli.kib {
        spec.target_bytes = k
            .checked_mul(1 << 10)
            .ok_or_else(|| anyhow::anyhow!("--kib {k} overflows a u64 byte count"))?;
    }
    if let Some(m) = cli.mib {
        spec.target_bytes = m
            .checked_mul(1 << 20)
            .ok_or_else(|| anyhow::anyhow!("--mib {m} overflows a u64 byte count"))?;
    }
    if let Some(g) = cli.gib {
        spec.target_bytes = g
            .checked_mul(1 << 30)
            .ok_or_else(|| anyhow::anyhow!("--gib {g} overflows a u64 byte count"))?;
    }
    if let Some(n) = cli.fixed_len {
        spec.line_len = ress_filegen::LineLen::Fixed(n);
    } else if let (Some(min), Some(max)) = (cli.min_len, cli.max_len) {
        spec.line_len = ress_filegen::LineLen::Uniform { min, max };
    }
    if cli.no_trailing_newline {
        spec.trailing_newline = false;
    }
    if let (Some(every), Some(len)) = (cli.mega_every, cli.mega_len) {
        spec.mega = Some(ress_filegen::Mega { every, len });
    }
    if let Some(f) = cli.utf8_fraction {
        spec.utf8_fraction = f;
    }
    let out_path = cli
        .out
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("output path required (or --list-presets)"))?;
    // parse -> assemble -> validate -> create: the spec is fully assembled by
    // this point, so a bad spec is rejected here, before the destination is
    // opened (and truncated, if it already exists).
    ress_filegen::validate(&spec)?;
    let file = std::fs::File::create(out_path)?;
    let mut writer = std::io::BufWriter::new(file);
    let stats = ress_filegen::generate(&spec, &mut writer)?;
    std::io::Write::flush(&mut writer)?;
    eprintln!("bytes={} lines={}", stats.bytes, stats.lines);
    Ok(())
}
