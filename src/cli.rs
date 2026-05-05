use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "knit")]
#[command(about = "Git for cross-repo feature work")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new feature bundle in .knit/.
    Init {
        /// Human-readable feature title.
        title: String,
        /// Replace an existing active bundle with the same slug.
        #[arg(long)]
        force: bool,
    },
    /// Add a local git repository to the active bundle.
    Add {
        /// Path to a local git repository.
        repo_path: PathBuf,
        /// Override the inferred base branch.
        #[arg(long)]
        base: Option<String>,
    },
    /// Create per-repo worktrees for the active bundle.
    Worktree,
    /// Show status for all repos in the active bundle.
    Status,
    /// Commit staged changes across bundle worktrees.
    Commit {
        /// Commit message to use in every repo with staged changes.
        #[arg(short, long)]
        message: String,
    },
    /// Show logical commit groups recorded in the active bundle.
    Log,
    /// Show git show --stat for each commit in a logical group.
    Show {
        /// Commit group id to inspect.
        commit_group_id: String,
    },
}
