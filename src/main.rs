use anyhow::{Context, Result};
use clap::Parser;
use knit::{run, Cli};

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Windows gives the main thread a 1 MiB stack (Unix mains get 8 MiB), and
    // knit's recursive descents (serde_json parsing, directory walks) can
    // exceed that. Run the CLI on a worker thread with an explicit stack size
    // so behavior matches across platforms.
    std::thread::Builder::new()
        .name("knit".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || run(cli))
        .context("failed to spawn knit worker thread")?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}
