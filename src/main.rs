use anyhow::{Context, Result};
use clap::Parser;
use knit::{run, Cli};

fn main() -> Result<()> {
    // Windows gives the main thread a 1 MiB stack (Unix mains get 8 MiB).
    // Both clap parsing of a large CLI in debug builds and knit's recursive
    // descents (serde_json parsing, directory walks) can exceed that, so the
    // entire CLI — including argument parsing — runs on a worker thread with
    // an explicit stack size to make behavior uniform across platforms.
    std::thread::Builder::new()
        .name("knit".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| run(Cli::parse()))
        .context("failed to spawn knit worker thread")?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}
