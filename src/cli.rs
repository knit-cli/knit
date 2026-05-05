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
    /// Add local git repositories to the active bundle and materialize worktrees.
    Add {
        /// Paths to local git repositories.
        #[arg(required = true)]
        repo_paths: Vec<PathBuf>,
        /// Override the inferred base branch.
        #[arg(long)]
        base: Option<String>,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
    },
    /// Remove repositories from bundle tracking. Leaves git branches/worktrees in place.
    Remove {
        /// Repo ids to remove from the active bundle.
        #[arg(required = true)]
        repo_ids: Vec<String>,
    },
    /// Create per-repo worktrees for the active bundle.
    Worktree,
    /// Stage all latest changes in tracked worktrees.
    Stage,
    /// Show status for all repos in the active bundle.
    Status,
    /// Commit staged changes across bundle worktrees.
    Commit {
        /// Commit message to use in every repo with staged changes.
        #[arg(short, long)]
        message: String,
        /// Stage all tracked worktree changes before committing.
        #[arg(long)]
        stage: bool,
    },
    /// Show logical commit groups recorded in the active bundle.
    Log,
    /// Show git show --stat for each commit in a logical group.
    Show {
        /// Commit group id to inspect.
        commit_group_id: String,
    },
}
