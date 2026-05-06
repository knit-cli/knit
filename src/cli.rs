use clap::{Parser, Subcommand};
use std::ffi::OsString;
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
    /// Track local git repositories in the active bundle and materialize checkouts.
    Track {
        /// Paths to local git repositories.
        #[arg(required = true)]
        repo_paths: Vec<PathBuf>,
        /// Override the inferred base branch.
        #[arg(long)]
        base: Option<String>,
        /// Use each original repo checkout directly instead of creating a Knit worktree.
        #[arg(long)]
        in_place: bool,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
    },
    /// Stage file changes inside tracked checkouts, like git add.
    Add {
        /// Limit staging to one or more repo ids or paths. Positional pathspecs then apply inside those repos.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Record only intent to add pathspecs, like git add -N.
        #[arg(short = 'N', long = "intent-to-add")]
        intent_to_add: bool,
        /// Stage modifications/deletions to tracked files only, like git add -u.
        #[arg(short = 'u', long)]
        update: bool,
        /// Optional repo selectors or pathspecs.
        args: Vec<String>,
    },
    /// Stop tracking repositories. Leaves git branches/checkouts in place.
    Untrack {
        /// Repo ids to remove from the active bundle.
        #[arg(required = true)]
        repo_ids: Vec<String>,
    },
    /// Remove repositories from bundle tracking. Alias for untrack.
    Remove {
        /// Repo ids to remove from the active bundle.
        #[arg(required = true)]
        repo_ids: Vec<String>,
    },
    /// Create per-repo worktrees for the active bundle.
    Worktree,
    /// Stage file changes inside tracked checkouts. Alias for add.
    Stage {
        /// Limit staging to one or more repo ids or paths.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Record only intent to add pathspecs, like git add -N.
        #[arg(short = 'N', long = "intent-to-add")]
        intent_to_add: bool,
        /// Stage modifications/deletions to tracked files only, like git add -u.
        #[arg(short = 'u', long)]
        update: bool,
        /// Optional repo selectors or pathspecs.
        args: Vec<String>,
    },
    /// Show status for all repos in the active bundle.
    Status,
    /// Show cross-repo diffs against each repo base.
    Diff {
        /// Show a compact diffstat instead of full patches.
        #[arg(long)]
        stat: bool,
        /// Optional repo ids or paths to limit the diff.
        repos: Vec<String>,
    },
    /// Record git commits that happened outside Knit.
    Sync,
    /// Commit staged changes across tracked checkouts.
    Commit {
        /// Commit message to use in every repo with staged changes.
        #[arg(short, long)]
        message: String,
        /// Stage all tracked worktree changes before committing.
        #[arg(long)]
        stage: bool,
    },
    /// Show bundle ledger entries.
    Log {
        /// Show only the latest N log entries. With no value, defaults to 10.
        #[arg(short = 'n', long = "limit", value_name = "COUNT", num_args = 0..=1, default_missing_value = "10")]
        limit: Option<usize>,
        /// Git-style shorthand for the latest N entries, for example `knit log -2`.
        #[arg(value_name = "-COUNT", allow_hyphen_values = true)]
        shorthand_limit: Option<String>,
    },
    /// Revert a bundle log entry across its affected repos.
    Revert {
        /// Bundle log selector: git commit SHA, node id, commit group id, HEAD, or HEAD~N.
        target: String,
        /// Write or refresh a revert plan. This is the default when --apply is not passed.
        #[arg(long, conflicts_with = "apply")]
        plan: bool,
        /// Apply a previously planned revert.
        #[arg(long)]
        apply: bool,
    },
    /// Run a git command in tracked checkouts.
    Git {
        /// Target repo id or path. Repeat for multiple repos.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Run against every tracked repo.
        #[arg(long)]
        all: bool,
        /// Git arguments to pass through.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Show git show --stat for each commit in a logical group.
    Show {
        /// Commit group id to inspect.
        commit_group_id: String,
    },
}
