use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "knit")]
#[command(about = "Git for cross-repo feature work")]
pub struct Cli {
    /// Resolve commands against this bundle instead of cwd or workspace context.
    #[arg(long, global = true, value_name = "BUNDLE")]
    pub bundle: Option<String>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new feature bundle in .knit/.
    Init {
        /// Human-readable feature title.
        title: String,
        /// Replace an existing bundle with the same slug.
        #[arg(long)]
        force: bool,
        /// Write an AGENTS.md tutorial for agents working in this Knit workspace.
        #[arg(long)]
        agents: bool,
    },
    /// Manage reusable project repo templates.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Track local git repositories in the resolved bundle and materialize checkouts.
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
        /// Repo ids to remove from the resolved bundle.
        repo_ids: Vec<String>,
        /// Repo ids to remove from the resolved bundle.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
    },
    /// Remove repositories from bundle tracking. Alias for untrack.
    Remove {
        /// Repo ids to remove from the resolved bundle.
        repo_ids: Vec<String>,
        /// Repo ids to remove from the resolved bundle.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
    },
    /// Create per-repo worktrees for the resolved bundle.
    Worktree,
    /// Inspect the resolved bundle artifact.
    Bundle {
        #[command(subcommand)]
        command: Option<BundleCommand>,
    },
    /// Switch the fallback bundle for this workspace or folder.
    Switch {
        /// Bundle id to make active.
        bundle: String,
        /// Set the workspace fallback bundle.
        #[arg(long, conflicts_with = "here")]
        workspace: bool,
        /// Set the fallback bundle for the current folder.
        #[arg(long)]
        here: bool,
    },
    /// Add a non-git note to the resolved bundle ledger.
    Checkpoint {
        /// Checkpoint note to record.
        message: String,
    },
    /// Mark the resolved bundle closed without mutating git state.
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
        /// Remove generated worktrees for the resolved bundle.
        #[arg(long)]
        worktrees: bool,
        /// Clean selected generated state for closed and archived bundles.
        #[arg(long)]
        closed: bool,
        /// Remove clean merge worktrees for completed merge runs.
        #[arg(long = "merge-worktrees")]
        merge_worktrees: bool,
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
    /// Show status for all repos in the resolved bundle.
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
    /// Create or show the landing plan. Use `knit land apply` to execute it.
    Land {
        #[command(subcommand)]
        command: Option<LandCommand>,
    },
    /// Merge a bundle or branch into another branch or bundle.
    Merge {
        /// Bundle id or git ref to merge. Omit with --continue or --abort.
        source: Option<String>,
        /// Target branch or bundle id to merge into.
        #[arg(long)]
        into: Option<String>,
        /// Leave conflicts for manual resolution instead of rolling back this merge run.
        #[arg(long)]
        manual: bool,
        /// Fetch origin/<target> before preparing branch-target merge checkouts.
        #[arg(long)]
        fetch: bool,
        /// Push branch-target merge checkouts after the full run succeeds.
        #[arg(long)]
        push: bool,
        /// Set upstream while pushing branch-target merge checkouts.
        #[arg(long)]
        set_upstream: bool,
        /// Merge run id or path for status/show/push actions.
        #[arg(long)]
        run: Option<String>,
        /// Repo ids to push when using `knit merge push`.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Continue the latest manual merge run after conflicts have been resolved and committed.
        #[arg(long = "continue")]
        continue_run: bool,
        /// Abort the latest manual merge run and roll back successful steps from that run.
        #[arg(long)]
        abort: bool,
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
    /// Manage Knit workspace config.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print bundled JSON schemas.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Validate Knit workspace state and report repairable issues.
    Doctor,
    /// Upgrade workspace JSON files to the current additive schema.
    Migrate {
        /// Report required migrations without writing files.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
pub enum BundleCommand {
    /// Create a new feature bundle.
    Start {
        /// Human-readable feature title.
        title: String,
        /// Project template to use. Defaults to the active project when present.
        #[arg(long)]
        project: Option<String>,
        /// Project repo id to include. Repeat to include several repos.
        #[arg(long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Include every project repo, including observed repos.
        #[arg(long)]
        all_repos: bool,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
        /// Use each original repo checkout directly instead of creating a Knit worktree.
        #[arg(long)]
        in_place: bool,
        /// Replace an existing bundle with the same slug.
        #[arg(long)]
        force: bool,
        /// Write an AGENTS.md tutorial for agents working in this Knit workspace.
        #[arg(long)]
        agents: bool,
    },
    /// Add repos or project repo ids to the current bundle.
    Add {
        /// Paths to local git repositories or project repo ids.
        #[arg(required = true)]
        repos: Vec<String>,
        /// Override the inferred base branch for raw repo paths.
        #[arg(long)]
        base: Option<String>,
        /// Use each original repo checkout directly instead of creating a Knit worktree.
        #[arg(long)]
        in_place: bool,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
    },
    /// Remove repos from bundle tracking.
    Remove {
        /// Repo ids to remove from the current bundle.
        repo_ids: Vec<String>,
        /// Repo ids to remove from the current bundle.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
    },
    /// List bundles in the workspace.
    List {
        /// Include every bundle state.
        #[arg(long)]
        all: bool,
        /// Include archived bundles.
        #[arg(long)]
        archived: bool,
        /// Include deleted bundles.
        #[arg(long)]
        deleted: bool,
    },
    /// Switch the fallback bundle for this workspace or folder.
    Switch {
        /// Bundle id to make active.
        bundle: String,
        /// Set the workspace fallback bundle.
        #[arg(long, conflicts_with = "here")]
        workspace: bool,
        /// Set the fallback bundle for the current folder.
        #[arg(long)]
        here: bool,
    },
    /// Mark the current bundle closed without mutating git state.
    Close {
        /// Optional reason to record on the close node.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Mark a bundle archived while keeping its JSON artifact.
    Archive {
        /// Bundle id to archive.
        bundle: String,
    },
    /// Restore an archived bundle to open or closed state.
    Restore {
        /// Bundle id to restore.
        bundle: String,
    },
    /// Move a bundle JSON artifact to .knit/deleted/bundles/.
    Delete {
        /// Bundle id to delete.
        bundle: String,
        /// Required to delete a bundle artifact.
        #[arg(long)]
        force: bool,
        /// Remove clean generated worktrees for this bundle before deleting the artifact.
        #[arg(long)]
        worktrees: bool,
        /// Delete local feature branches for this bundle after generated worktrees are removed.
        #[arg(long)]
        branches: bool,
        /// Use `git branch -D` instead of `git branch -d` for local feature branches.
        #[arg(long = "force-branches", requires = "branches")]
        force_branches: bool,
    },
    /// Create a compatibility bundle from the union of repos in source bundles.
    Compat {
        /// Source bundle ids to make compatible.
        #[arg(required = true)]
        sources: Vec<String>,
        /// Title for the compatibility bundle. Defaults to a title from the source ids.
        #[arg(long)]
        title: Option<String>,
        /// Use a specific project template instead of source bundle repo metadata.
        #[arg(long)]
        project: Option<String>,
        /// Include every project repo when --project is used.
        #[arg(long)]
        all_repos: bool,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
        /// Use each original repo checkout directly instead of creating a Knit worktree.
        #[arg(long)]
        in_place: bool,
        /// Replace an existing bundle with the same slug.
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved bundle file path.
    Path,
    /// Print the resolved bundle JSON.
    Print,
    /// Validate the resolved bundle structure.
    Validate,
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// Create a project repo template.
    Init {
        /// Project name.
        name: String,
    },
    /// Add or update a repo in the active project.
    Add {
        /// Stable repo id inside the project.
        repo_id: String,
        /// Path to a local git repository.
        repo_path: PathBuf,
        /// Override the inferred base branch.
        #[arg(long)]
        base: Option<String>,
        /// Keep this repo out of default bundle starts.
        #[arg(long)]
        observe: bool,
    },
    /// List projects in this workspace.
    List,
    /// Print a project JSON artifact.
    Show {
        /// Project name. Defaults to the active project.
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Set a Knit config value.
    Set {
        /// Config key. Currently only `advice`.
        key: String,
        /// Config value.
        value: String,
    },
}

#[derive(Subcommand)]
pub enum SchemaCommand {
    /// Print a bundled JSON Schema.
    Print {
        /// Schema name: bundle, project, contexts, merge-run, land-plan, land-run, config.
        name: String,
    },
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
    /// Merge current PR base branches into feature branches and record the integration.
    Update {
        /// Optional repo ids or paths to limit the update.
        repos: Vec<String>,
        /// Update every tracked repo with a recorded PR. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
        /// Push feature branches after successful local updates.
        #[arg(long)]
        push: bool,
        /// Set each feature branch's upstream to origin/<branch> while pushing.
        #[arg(long)]
        set_upstream: bool,
        /// Record already-resolved local branch movements as a land update without running git merge.
        #[arg(long)]
        continue_merge: bool,
    },
}

#[derive(Subcommand)]
pub enum GithubPublishCommand {
    /// Push feature branches and create missing GitHub PRs.
    Create {
        /// Optional repo ids or paths to limit PR creation.
        repos: Vec<String>,
        /// Override PR base branch. Use once for all repos or repeat as REPO=BRANCH.
        #[arg(long = "base", value_name = "BRANCH|REPO=BRANCH")]
        bases: Vec<String>,
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
    /// Show recorded PRs for the resolved bundle.
    Status {
        /// Optional repo ids or paths to limit PR status.
        repos: Vec<String>,
        /// Show every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
    },
}
