use anyhow::Result;
use clap::Parser;
use knit::{run, Cli};

fn main() -> Result<()> {
    run(Cli::parse())
}
