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
        /// Write an AGENTS.md tutorial for agents working in this Knit workspace.
        #[arg(long)]
        agents: bool,
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
    /// Inspect the active bundle artifact.
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Add a non-git note to the active bundle ledger.
    Checkpoint {
        /// Checkpoint note to record.
        message: String,
    },
    /// Mark the active bundle closed without mutating git state.
    Close {
        /// Optional reason to record on the close node.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove Knit-generated local state.
    Clean {
        /// Remove stored revert plans.
        #[arg(long)]
        plans: bool,
        /// Remove generated worktrees for the active bundle.
        #[arg(long)]
        worktrees: bool,
        /// Remove all cleanable generated state.
        #[arg(long)]
        all: bool,
        /// Pass --force to git worktree remove.
        #[arg(long)]
        force: bool,
    },
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
    /// Fetch tracked repos without merging.
    Fetch {
        /// Optional repo ids or paths to limit the fetch.
        repos: Vec<String>,
        /// Fetch every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
    },
    /// Pull tracked repos.
    Pull {
        /// Optional repo ids or paths to limit the pull.
        repos: Vec<String>,
        /// Pull every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
        /// Use git pull --rebase instead of the default fast-forward-only pull.
        #[arg(long)]
        rebase: bool,
        /// Allow git pull to run with uncommitted changes.
        #[arg(long)]
        force: bool,
        /// Pull the tracked feature checkouts instead of original/base repo paths.
        #[arg(long)]
        feature: bool,
    },
    /// Push tracked feature branches.
    Push {
        /// Optional repo ids or paths to limit the push.
        repos: Vec<String>,
        /// Push every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
        /// Set each feature branch's upstream to origin/<branch>.
        #[arg(long)]
        set_upstream: bool,
    },
    /// Publish tracked feature branches to a code hosting provider.
    Publish {
        #[command(subcommand)]
        target: PublishCommand,
    },
    /// Plan and execute cross-repo PR landing.
    Land {
        #[command(subcommand)]
        command: LandCommand,
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
    /// Show git details for a bundle log entry.
    Show {
        /// Bundle log selector: git commit SHA, node id, commit group id, HEAD, or HEAD~N.
        target: String,
    },
}

#[derive(Subcommand)]
pub enum BundleCommand {
    /// Print the active bundle file path.
    Path,
    /// Print the active bundle JSON.
    Print,
    /// Validate the active bundle structure.
    Validate,
}

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Publish to GitHub pull requests.
    Github {
        #[command(subcommand)]
        command: GithubPublishCommand,
    },
}

#[derive(Subcommand)]
pub enum LandCommand {
    /// Generate an editable landing plan from recorded publications.
    Plan {
        /// Landing provider to target. GitHub is the only provider implemented.
        #[arg(long, default_value = "github")]
        provider: String,
        /// Write the generated plan to a custom path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace an existing plan file.
        #[arg(long)]
        force: bool,
    },
    /// Execute a landing plan.
    Apply {
        /// Plan file to execute. Defaults to .knit/land-plans/<bundle>.land.json.
        #[arg(long)]
        plan: Option<PathBuf>,
    },
    /// Resume a failed or incomplete landing run.
    Resume {
        /// Run file to resume. Defaults to the latest run.
        #[arg(long)]
        run: Option<PathBuf>,
    },
    /// Show the latest landing run or default plan status.
    Status {
        /// Run file to inspect. Defaults to the latest run.
        #[arg(long)]
        run: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum GithubPublishCommand {
    /// Push feature branches and create missing GitHub PRs.
    Create {
        /// Optional repo ids or paths to limit PR creation.
        repos: Vec<String>,
        /// Create PRs for every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
        /// Create draft PRs.
        #[arg(long)]
        draft: bool,
        /// Explicitly sync cross-links after creation. This is the default.
        #[arg(long, conflicts_with = "no_sync")]
        sync: bool,
        /// Skip the second phase that updates every PR body with cross-links.
        #[arg(long)]
        no_sync: bool,
        /// Set each feature branch's upstream to origin/<branch> while pushing.
        #[arg(long)]
        set_upstream: bool,
    },
    /// Refresh recorded PR metadata and rewrite Knit cross-link blocks.
    Sync {
        /// Optional repo ids or paths to limit PR sync.
        repos: Vec<String>,
        /// Sync every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
    },
    /// Show recorded PRs for the active bundle.
    Status {
        /// Optional repo ids or paths to limit PR status.
        repos: Vec<String>,
        /// Show every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
    },
}
