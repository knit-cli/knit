use clap::{Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "knit")]
#[command(version)]
#[command(about = "Git for cross-repo feature work")]
pub struct Cli {
    /// Resolve commands against this bundle instead of cwd or workspace context.
    #[arg(long, global = true, value_name = "BUNDLE")]
    pub bundle: Option<String>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum FetchMode {
    /// Fetch both git repos and knit bundles (default).
    All,
    /// Fetch git repos only.
    Git,
    /// Fetch knit bundles only.
    Knit,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a reusable project repo template (like `git init`, for a project).
    Init {
        /// Project name.
        name: String,
        /// Write or refresh project-specific AGENTS.md guidance.
        #[arg(long)]
        agents: bool,
    },
    /// Manage reusable project repo templates.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Inspect project source checkouts, configured bases, and open bundles.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Manage your saved per-project views (named bundle shapes).
    View {
        #[command(subcommand)]
        command: ViewCommand,
    },
    /// Manage sync remotes.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Clone a remote project export into a local Knit workspace.
    Clone {
        /// Project to clone: `owner/slug`, a bare slug, or a project id. Use the
        /// `owner/slug` form (owner is a username or org slug) when a slug is not
        /// unique across owners.
        project: String,
        /// Directory to create. Defaults to the project slug.
        target: Option<PathBuf>,
        /// Named sync remote. Defaults to the configured sync remote.
        #[arg(long)]
        remote: Option<String>,
        /// Remote base URL. Required when the remote is not configured.
        #[arg(long)]
        url: Option<String>,
        /// Remote token. Prefer KNIT_REMOTE_<NAME>_TOKEN or KNIT_REMOTE_TOKEN.
        #[arg(long)]
        token: Option<String>,
        /// Bundle to make active after clone. Defaults to the latest open exported bundle.
        #[arg(long = "active-bundle")]
        active_bundle: Option<String>,
        /// Only write project and bundle JSON; do not create feature worktrees.
        #[arg(long)]
        no_worktree: bool,
        /// Print a machine-readable clone result document to stdout. Progress
        /// lines move to stderr.
        #[arg(long)]
        json: bool,
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
    /// Show the resolved bundle, create one (`knit bundle "feature title"`), or manage it.
    #[command(args_conflicts_with_subcommands = true)]
    Bundle {
        /// Title of a new bundle to create. With no title and no subcommand, shows the current bundle.
        title: Option<String>,
        /// Project template to use. Defaults to the active project when present.
        #[arg(long)]
        project: Option<String>,
        /// Project repo id to include. Repeat to include several repos.
        #[arg(long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Include every project repo, including observed repos.
        #[arg(long)]
        all_repos: bool,
        /// Apply a saved view (named bundle shape). Conflicts with --repo/--all-repos.
        #[arg(long, value_name = "NAME")]
        view: Option<String>,
        /// Add a project repo on top of the resolved set. Repeatable.
        #[arg(long = "include", value_name = "REPO")]
        include: Vec<String>,
        /// Drop a repo from the resolved set. Repeatable.
        #[arg(long = "exclude", value_name = "REPO")]
        exclude: Vec<String>,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
        /// Use each original repo checkout directly instead of creating a Knit worktree.
        #[arg(long)]
        in_place: bool,
        /// Do not fetch bases; prefer cached origin/<base>, then the local base.
        #[arg(long, conflicts_with = "from_local_base")]
        offline: bool,
        /// Start from local configured base branches without fetching origin.
        #[arg(long)]
        from_local_base: bool,
        /// Replace an existing bundle with the same slug.
        #[arg(long)]
        force: bool,
        /// Write an AGENTS.md tutorial for agents working in this Knit workspace.
        #[arg(long)]
        agents: bool,
        /// Start a shell in .knit/worktrees/<bundle>. Pass a repo selector to cd into that repo checkout instead.
        #[arg(long, value_name = "REPO", num_args = 0..=1, default_missing_value = "", conflicts_with = "no_worktree")]
        cd: Option<String>,
        #[command(subcommand)]
        command: Option<BundleCommand>,
    },
    /// Switch the fallback bundle for this workspace.
    Switch {
        /// Bundle id to make active.
        bundle: String,
        /// Set the workspace fallback bundle.
        #[arg(long)]
        workspace: bool,
    },
    /// Remove Knit-generated local state.
    Clean {
        /// Remove stored revert plans.
        #[arg(long)]
        plans: bool,
        /// Remove generated worktrees for the resolved bundle.
        #[arg(long)]
        worktrees: bool,
        /// Clean selected generated state for archived bundles.
        #[arg(long)]
        archived: bool,
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
    /// Fetch tracked repos and/or bundles.
    Fetch {
        /// Optional repo ids or paths to limit the git fetch.
        repos: Vec<String>,
        /// Fetch mode: --all (default), --git only, --knit only.
        #[arg(long, default_value = "all", value_name = "MODE")]
        mode: FetchMode,
        /// Named sync remote for knit fetch. Defaults to the configured sync remote.
        #[arg(long, value_name = "REMOTE")]
        remote: Option<String>,
        /// (Deprecated, use --git) Skip sync remote sync.
        #[arg(long, hide = true)]
        no_remote: bool,
    },
    /// Pull tracked repos. With no flags: inside a bundle pulls that bundle's
    /// checkouts; at the workspace base pulls every project source checkout and
    /// open bundle, reporting each.
    Pull {
        /// Optional repo ids or paths to limit a single-bundle pull.
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
        /// Deprecated alias for --current.
        #[arg(long, hide = true)]
        main: bool,
        /// Fetch and safely fast-forward the active project's configured base branches.
        #[arg(long)]
        base: bool,
        /// Update the active project's source checkouts on their current branches.
        #[arg(long)]
        current: bool,
        /// Update every open bundle's checkouts from its remote artifact.
        #[arg(long)]
        bundles: bool,
        /// Also pull the current bundle artifact from a named sync remote.
        #[arg(long, value_name = "REMOTE")]
        remote: Option<String>,
        /// Skip configured sync remote sync for this pull.
        #[arg(long)]
        no_remote: bool,
        /// Union-merge the bundle ledger when the local and remote artifacts
        /// have diverged, instead of keeping local and warning.
        #[arg(long)]
        merge: bool,
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
        /// Force push, refusing when the remote branch moved since the last
        /// fetch. The safe way to push rewritten feature-branch history.
        #[arg(long)]
        force_with_lease: bool,
        /// Force push unconditionally. Prefer --force-with-lease.
        #[arg(long, conflicts_with = "force_with_lease")]
        force: bool,
        /// Also push the bundle artifact to these sync remotes. Repeat to push to multiple remotes and override push-sync config.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
        /// Skip the remote bundle sync for this push.
        #[arg(long)]
        no_remote: bool,
    },
    /// Record and inspect named check verdicts on the bundle ledger.
    Check {
        #[command(subcommand)]
        command: CheckCommand,
    },
    /// Record a cross-repo known-good marker: annotated git tags `knit/<name>` on each repo's origin base.
    #[command(args_conflicts_with_subcommands = true)]
    Tag {
        /// Tag name (creates `knit/<name>` in every tracked repo). With no name and no subcommand, lists tags.
        name: Option<String>,
        /// Extra note stored in the tag annotation and ledger node.
        #[arg(short = 'm', long = "message", value_name = "NOTE")]
        message: Option<String>,
        /// Limit tagging to one or more repo ids or paths (weakens the cross-repo claim).
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Create local tags and the ledger node without pushing to origin.
        #[arg(long)]
        no_push: bool,
        /// Record the ledger node only; do not create git tags.
        #[arg(long, conflicts_with = "no_push")]
        no_git: bool,
        #[command(subcommand)]
        command: Option<TagCommand>,
    },
    /// Run a project command inside resolved bundle checkouts.
    Run {
        /// Configured project command name. Omit when passing a raw command after --.
        name: Option<String>,
        /// Target repo id or path. Overrides configured repos for named commands.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Run against every tracked repo.
        #[arg(long)]
        all: bool,
        /// List configured commands for the active bundle's project.
        #[arg(long)]
        list: bool,
        /// Overwrite an existing docker-compose.knit.yml (`knit run eject`).
        #[arg(long)]
        force: bool,
        /// With `knit run down`, also remove bundle-owned volumes and locally
        /// built Compose images. External volumes and explicitly tagged images
        /// are preserved.
        #[arg(long)]
        purge: bool,
        /// Raw command to execute, for example `knit run -r web -- docker compose up`.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Publish tracked feature branches to a code hosting provider.
    #[command(visible_alias = "request")]
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
    /// Cherry-pick commits recorded in another Knit bundle into the resolved bundle.
    #[command(name = "cherrypick", alias = "cherry-pick")]
    CherryPick {
        /// Source bundle id to copy recorded commits from.
        #[arg(long = "from", value_name = "BUNDLE")]
        from_bundle: String,
        /// Source bundle selectors: node id, commit group id, git SHA, HEAD, or HEAD~N.
        #[arg(required = true)]
        targets: Vec<String>,
        /// Limit cherry-picks to one or more repo ids or paths.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Show what would be cherry-picked without changing git or bundle state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Reconcile local Knit state, or sync artifacts with the configured remotes.
    ///
    /// Bare `knit sync` records git commits made outside Knit into the bundle
    /// ledger (a local-only reconcile). The `push`/`pull` subcommands are the one
    /// way to move bundle, history, and view artifacts to and from the sync remotes.
    Sync {
        #[command(subcommand)]
        command: Option<SyncCommand>,
    },
    /// Show and sync project-wide commit history.
    History {
        #[command(subcommand)]
        command: Option<HistoryCommand>,
    },
    /// Find Knit bundles related to paths touched in Git history.
    Related {
        /// Paths to inspect. Paths are repo-relative unless they include a project repo id prefix.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Repo id. Defaults to the repo containing cwd or a repo id prefix in the path.
        #[arg(short = 'r', long = "repo")]
        repo: Option<String>,
        /// Project id. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
        /// Maximum related Knit instances to show.
        #[arg(short = 'n', long = "limit", default_value_t = 10)]
        limit: usize,
        /// Maximum Git commits to inspect for the path query.
        #[arg(long = "commit-limit", default_value_t = 200)]
        commit_limit: usize,
        /// Pull Knit history from a remote before querying.
        #[arg(long)]
        pull: bool,
        /// Named sync remote used with --pull.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Commit staged changes across tracked checkouts.
    Commit {
        /// Commit message to use in every repo with staged changes.
        #[arg(short, long)]
        message: String,
        /// Stage every tracked change first, then commit, like `git commit -a`.
        #[arg(short = 'a', long = "all", alias = "stage")]
        all: bool,
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
pub enum SyncCommand {
    /// Push artifacts to the sync remotes. With no target flags, pushes bundle, history,
    /// views, and architecture for the resolved project/bundle; the
    /// knowledge-graph viz slice moves only with an explicit `--kg`.
    Push {
        #[command(flatten)]
        targets: SyncTargetArgs,
        /// Named sync remote(s). Repeat for several. Defaults to configured sync remotes.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
        /// Force-push bundle artifacts, overwriting the remote ledger only when it
        /// still matches the state fetched for the lease. Applies to bundle targets only.
        #[arg(long)]
        force_with_lease: bool,
        /// Force-push bundle artifacts unconditionally. Prefer --force-with-lease.
        #[arg(long, conflicts_with = "force_with_lease")]
        force: bool,
    },
    /// Pull artifacts from the sync remotes. With no target flags, pulls bundle, history,
    /// and views for the resolved project/bundle.
    Pull {
        #[command(flatten)]
        targets: SyncTargetArgs,
        /// Named sync remote(s). Repeat for several. Defaults to configured sync remotes.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
    },
}

#[derive(clap::Args, Clone, Debug)]
pub struct SyncTargetArgs {
    /// Sync the bundle artifact for the resolved bundle.
    #[arg(long)]
    pub bundles: bool,
    /// Sync project commit history events.
    #[arg(long)]
    pub history: bool,
    /// Sync your saved views for the project.
    #[arg(long)]
    pub views: bool,
    /// Sync the project architecture artifact (produced by `urdir kg architecture`).
    #[arg(long)]
    pub architecture: bool,
    /// Sync the knowledge-graph viz slice (produced by `urdir kg viz`). The
    /// slice is often several MB, so it moves only with this explicit flag —
    /// never as part of `--all` or a bare invocation.
    #[arg(long)]
    pub kg: bool,
    /// Sync every routine artifact family (bundles, history, views,
    /// architecture). This is also the default with no target flags. The
    /// knowledge-graph viz slice needs an explicit `--kg`.
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// Show local project history.
    List {
        /// Show only the latest N events.
        #[arg(short = 'n', long = "limit", default_value_t = 20)]
        limit: usize,
        /// Limit to a repo id.
        #[arg(long)]
        repo: Option<String>,
        /// Limit to a bundle id.
        #[arg(long)]
        bundle: Option<String>,
        /// Project id. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Rebuild local project history from bundle ledgers.
    Refresh {
        /// Project id. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum BundleCommand {
    /// Materialize per-repo worktrees for the resolved bundle.
    Worktree,
    /// Pull one bundle from the sync remote: refresh its artifact, fetch its
    /// feature branches, and materialize fast-forwarded worktrees.
    Pull {
        /// Remote bundle slug to pull.
        slug: String,
        /// Print a machine-readable JSON document to stdout. Progress lines
        /// move to stderr.
        #[arg(long)]
        json: bool,
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
        /// Do not fetch bases; prefer cached origin/<base>, then the local base.
        #[arg(long, conflicts_with = "from_local_base")]
        offline: bool,
        /// Start from local configured base branches without fetching origin.
        #[arg(long)]
        from_local_base: bool,
        /// Only update the bundle; do not create branches or worktrees.
        #[arg(long)]
        no_worktree: bool,
    },
    /// Remove repos from the current bundle, tearing down their worktrees.
    Remove {
        /// Repo ids to remove from the current bundle.
        #[arg(required = true)]
        repos: Vec<String>,
        /// Keep the generated worktree on disk (tracking removal only).
        #[arg(long, conflicts_with = "delete_branch")]
        keep_worktree: bool,
        /// Also delete the local feature branch after removing the worktree.
        #[arg(long)]
        delete_branch: bool,
        /// Discard uncommitted or unpushed work when tearing down.
        #[arg(long)]
        force: bool,
    },
    /// Reshape the current bundle to match a saved view.
    ApplyView {
        /// Saved view name to apply.
        name: String,
        /// Keep generated worktrees for repos the view drops (tracking removal only).
        #[arg(long, conflicts_with = "delete_branch")]
        keep_worktree: bool,
        /// Also delete local feature branches for repos the view drops.
        #[arg(long)]
        delete_branch: bool,
        /// Discard uncommitted or unpushed work when tearing down dropped repos.
        #[arg(long)]
        force: bool,
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
    /// Delete dead bundles whose PRs are merged, closed, missing, or absent.
    Prune {
        /// Apply pruning after listing candidate bundles.
        #[arg(long)]
        apply: bool,
        /// Refresh recorded PR states from GitHub before deciding. This is the default.
        #[arg(long, conflicts_with = "no_refresh")]
        refresh: bool,
        /// Use only cached recorded PR states without querying GitHub.
        #[arg(long)]
        no_refresh: bool,
        /// Treat bundles whose only uncommitted work is untracked files as dead work (the untracked files are discarded when worktrees are removed).
        #[arg(long)]
        untracked: bool,
        /// Report every bundle's prune status, including ones that are kept.
        #[arg(long)]
        report: bool,
        /// Remove all cleanup targets: worktrees, local branches, forced local branch deletion, origin branches, and remote bundle records.
        #[arg(long)]
        all: bool,
        /// Remove generated worktrees for each pruned bundle and orphaned worktree dirs.
        #[arg(long)]
        worktrees: bool,
        /// Pass --force to git worktree remove and discard uncommitted work in orphan worktree dirs.
        #[arg(long, requires = "worktrees")]
        force: bool,
        /// Delete local feature branches for each pruned bundle after generated worktrees are removed.
        #[arg(long)]
        branches: bool,
        /// Use `git branch -D` instead of `git branch -d` for local feature branches.
        #[arg(long = "force-branches", requires = "branches")]
        force_branches: bool,
        /// Delete matching feature branches from origin.
        #[arg(long = "remote-branches", requires = "branches")]
        remote_branches: bool,
        /// Delete matching remote bundle records.
        #[arg(long = "remote-bundles")]
        remote_bundles: bool,
        /// Also prune finished (landed/archived) bundle artifacts. By default
        /// finished bundles are history and only open dead work is pruned.
        #[arg(long)]
        archived: bool,
    },
    /// Mark a bundle done: archive its artifact and remove generated worktrees, keeping branches.
    Archive {
        /// Bundle id to archive.
        bundle: String,
        /// Optional reason to record on the archive node.
        #[arg(long)]
        reason: Option<String>,
        /// Keep generated worktrees on disk.
        #[arg(long = "keep-worktrees")]
        keep_worktrees: bool,
        /// Pass --force to git worktree remove.
        #[arg(long)]
        force: bool,
    },
    /// Restore an archived bundle to the open state.
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
        /// Delete matching feature branches from origin.
        #[arg(long = "remote-branches", requires = "branches")]
        remote_branches: bool,
    },
    /// Print the resolved bundle file path.
    Path,
    /// Print the resolved bundle JSON.
    Print,
    /// Validate the resolved bundle structure.
    Validate,
}

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    /// Show source checkout and configured-base state for the active project.
    Status,
}

#[derive(Subcommand)]
pub enum ProjectCommand {
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
        /// Write or refresh project-specific AGENTS.md guidance.
        #[arg(long)]
        agents: bool,
    },
    /// Change only a project repo's configured base branch.
    SetBase {
        /// Stable repo id inside the project.
        repo_id: String,
        /// New configured base branch.
        branch: String,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// List projects in this workspace.
    List,
    /// Print a project JSON artifact.
    Show {
        /// Project name. Defaults to the active project.
        name: Option<String>,
    },
    /// Remove a project template JSON artifact, or specific repos from it.
    Remove {
        /// Project name.
        name: String,
        /// Remove only these repo ids from the project, leaving the template.
        /// Repeat for several. Required unless `--force` removes the template.
        #[arg(long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Required to remove the whole project artifact (ignored with --repo).
        #[arg(long)]
        force: bool,
    },
    /// Push the project JSON shape and repositories to a sync remote.
    Push {
        /// Project name. Defaults to the active project.
        name: Option<String>,
        /// Named sync remote. Defaults to the configured sync remote.
        #[arg(long)]
        remote: Option<String>,
        /// Delete remote repository records whose id is no longer in the local
        /// project shape, so the remote converges on the local repo set.
        #[arg(long)]
        prune: bool,
    },
    /// Write or refresh project-specific AGENTS.md guidance.
    Agents {
        /// Project name. Defaults to the active project.
        name: Option<String>,
    },
    /// Pull runtime/landing/commands from a repo-local knit.project.json.
    Pull {
        /// Project name. Defaults to the active project.
        name: Option<String>,
        /// Repo id that contains knit.project.json, usually the stack repo.
        #[arg(long)]
        repo: String,
        /// Refresh project AGENTS.md after pulling.
        #[arg(long)]
        agents: bool,
    },
    /// Manage named commands that `knit run` can execute.
    Command {
        #[command(subcommand)]
        command: ProjectRunCommandCli,
    },
}

#[derive(Subcommand)]
pub enum ProjectRunCommandCli {
    /// Add or replace a named project command.
    Set {
        /// Command name used by `knit run <name>`.
        name: String,
        /// Repo id to run in. Repeat for multiple repos.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Subdirectory inside each resolved checkout.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Environment variable to set, as KEY=VALUE. Repeat for several variables.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Command and args to execute.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// List project commands.
    List,
    /// Remove a project command.
    Remove {
        /// Command name.
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ViewCommand {
    /// List your saved views for a project (`*` marks the default).
    List {
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Print a view, or with --repos the repos it resolves to.
    Show {
        /// View name. Omit to print the whole views document.
        name: Option<String>,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
        /// Print the resolved repo set instead of the raw view.
        #[arg(long)]
        repos: bool,
    },
    /// Create or replace a saved view.
    Save {
        /// View name.
        name: String,
        /// Repo id to add on top of the project default. Repeatable.
        #[arg(long = "include", value_name = "REPO")]
        include: Vec<String>,
        /// Repo id to drop from the project default. Repeatable.
        #[arg(long = "exclude", value_name = "REPO")]
        exclude: Vec<String>,
        /// Derive include/exclude from the current bundle's repos.
        #[arg(long)]
        from_bundle: bool,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Add repos to a view's include list.
    Include {
        /// View name.
        name: String,
        /// Repo ids to add.
        #[arg(required = true)]
        repos: Vec<String>,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Add repos to a view's exclude list.
    Exclude {
        /// View name.
        name: String,
        /// Repo ids to exclude.
        #[arg(required = true)]
        repos: Vec<String>,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Drop repos from both a view's include and exclude lists.
    Unset {
        /// View name.
        name: String,
        /// Repo ids to remove from the view.
        #[arg(required = true)]
        repos: Vec<String>,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Set or clear the default view applied by new bundles.
    Default {
        /// View name to make the default.
        name: Option<String>,
        /// Clear the default view.
        #[arg(long, conflicts_with = "name")]
        clear: bool,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Delete a saved view.
    Rm {
        /// View name.
        name: String,
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
    /// Open the views file in $EDITOR.
    Edit {
        /// Project name. Defaults to the active project.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// Add or replace a named sync remote.
    Add {
        /// Remote name, for example `hosted`.
        name: String,
        /// Remote base URL, for example `http://localhost:4000` or `https://host.example`.
        url: String,
        /// Optional remote token. Prefer KNIT_REMOTE_<NAME>_TOKEN or KNIT_REMOTE_TOKEN for shared workspaces.
        #[arg(long)]
        token: Option<String>,
        /// Store this remote in the user-level Knit config instead of the workspace.
        #[arg(long)]
        global: bool,
    },
    /// List configured remotes.
    List {
        /// Show only user-level remotes.
        #[arg(long)]
        global: bool,
    },
    /// Show a configured remote.
    Show {
        /// Remote name.
        name: String,
        /// Show only the user-level remote.
        #[arg(long)]
        global: bool,
    },
    /// Remove a configured remote.
    Remove {
        /// Remote name.
        name: String,
        /// Remove the user-level remote instead of the workspace remote.
        #[arg(long)]
        global: bool,
    },
    /// List the remote projects visible to the resolved remote token.
    Projects {
        /// Named sync remote. Defaults to the configured sync remote.
        #[arg(long)]
        remote: Option<String>,
        /// Print a machine-readable JSON document to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Store or clear a token for a remote.
    Token {
        /// Remote name.
        name: String,
        /// Token value. Omit with --clear.
        token: Option<String>,
        /// Remove the stored token.
        #[arg(long)]
        clear: bool,
        /// Store or clear the token in the user-level Knit config.
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show Knit config (global, workspace, and effective when inside a workspace).
    Show {
        /// Show only the user-level config.
        #[arg(long)]
        global: bool,
    },
    /// Set a Knit config value.
    Set {
        /// Config key: advice, stealth, auto-tag, push-sync, sync-remote, or sync-remotes.
        key: String,
        /// Config value.
        value: String,
        /// Store the value in the user-level Knit config instead of the workspace.
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
pub enum SchemaCommand {
    /// Print a bundled JSON Schema.
    Print {
        /// Schema name: bundle, project, merge-run, land-plan, land-run, config.
        name: String,
    },
}

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Push feature branches and create missing review objects (auto-detects each repo's host).
    Create {
        /// Optional repo ids or paths to limit creation.
        repos: Vec<String>,
        /// Read a bundle JSON artifact from this path instead of the local Knit workspace.
        #[arg(long)]
        from_artifact: Option<PathBuf>,
        /// Write the updated bundle JSON artifact to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Skip pushing feature branches. Branches must already exist on the remote.
        #[arg(long)]
        no_push: bool,
        /// Override base branch. Use once for all repos or repeat as REPO=BRANCH.
        #[arg(long = "base", value_name = "BRANCH|REPO=BRANCH")]
        bases: Vec<String>,
        /// Create review objects for every tracked repo instead of only repos with recorded work.
        #[arg(long)]
        all: bool,
        /// Create draft review objects.
        #[arg(long)]
        draft: bool,
        /// Replace recorded merged/closed review objects with a fresh review round.
        #[arg(long)]
        renew: bool,
        /// Explicitly sync cross-links after creation. This is the default.
        #[arg(long, conflicts_with = "no_sync")]
        sync: bool,
        /// Skip the second phase that updates every review body with cross-links.
        #[arg(long)]
        no_sync: bool,
        /// Set each feature branch's upstream to origin/<branch> while pushing.
        #[arg(long)]
        set_upstream: bool,
        /// Also push the bundle artifact to these sync remotes. Repeat to push to multiple remotes and override push-sync config.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
        /// Skip the remote bundle sync for this publish.
        #[arg(long)]
        no_remote: bool,
        /// Limit publishing to repos hosted on this provider (github, gitlab, forgejo). Default: every repo's own host.
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Shorthand for `--provider github`.
        #[arg(long, conflicts_with = "provider")]
        github: bool,
    },
    /// Refresh recorded review metadata and rewrite Knit cross-link blocks.
    Sync {
        /// Optional repo ids or paths to limit the sync.
        repos: Vec<String>,
        /// Read a bundle JSON artifact from this path instead of the local Knit workspace.
        #[arg(long)]
        from_artifact: Option<PathBuf>,
        /// Write the updated bundle JSON artifact to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Sync every tracked repo instead of only repos with recorded work or publications.
        #[arg(long)]
        all: bool,
        /// Limit the sync to repos hosted on this provider (github, gitlab, forgejo). Default: every repo's own host.
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Shorthand for `--provider github`.
        #[arg(long, conflicts_with = "provider")]
        github: bool,
    },
    /// Show recorded review objects for the resolved bundle.
    Status {
        /// Optional repo ids or paths to limit the status.
        repos: Vec<String>,
        /// Show every tracked repo. This is the default when no repos are passed.
        #[arg(long)]
        all: bool,
        /// Fetch live mergeability, checks, and review state from the host.
        #[arg(long)]
        live: bool,
        /// Limit the status to repos hosted on this provider (github, gitlab, forgejo).
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Shorthand for `--provider github`.
        #[arg(long, conflicts_with = "provider")]
        github: bool,
    },
}

#[derive(Subcommand)]
pub enum CheckCommand {
    /// Run the project command of the same name and record the verdict.
    Run {
        /// Configured project command name (e.g. `ci`).
        name: String,
        /// Target repo id or path. Overrides configured repos.
        #[arg(short = 'r', long = "repo", value_name = "REPO")]
        repos: Vec<String>,
        /// Run against every tracked repo.
        #[arg(long)]
        all: bool,
    },
    /// Record a verdict computed elsewhere (another tool or a human).
    Record {
        /// Check name (e.g. `ci`, `functional`).
        name: String,
        /// Record the check as green.
        #[arg(long, conflicts_with = "fail")]
        pass: bool,
        /// Record the check as red.
        #[arg(long)]
        fail: bool,
        /// Optional human-readable detail stored with the verdict.
        #[arg(long)]
        detail: Option<String>,
    },
    /// Show the latest verdict per check and whether it is still fresh.
    Status,
}

#[derive(Subcommand)]
pub enum TagCommand {
    /// List `knit/*` tags across repos, marking partial sets.
    List,
    /// Show per-repo local/remote SHAs, the annotation subject, and ledger provenance.
    Show {
        /// Tag name without the `knit/` prefix.
        name: String,
    },
}

#[derive(Subcommand)]
pub enum LandCommand {
    /// Generate an editable landing plan from recorded publications.
    Plan {
        /// Landing provider to target. GitHub is the only provider implemented.
        #[arg(long)]
        provider: Option<String>,
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
        /// Land even when required checks are missing, red, or stale.
        #[arg(long)]
        skip_checks: bool,
        /// Keep generated bundle worktrees after a successful land.
        #[arg(long = "keep-worktrees")]
        keep_worktrees: bool,
        /// Read a bundle JSON artifact from this path and land PRs without a local Knit workspace.
        #[arg(long)]
        from_artifact: Option<PathBuf>,
        /// Write the updated bundle JSON artifact to this path.
        /// When omitted, the updated artifact is printed to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also push the landed bundle artifact to these sync remotes. Repeat to push to multiple remotes and override push-sync config.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
        /// Skip the remote bundle sync after landing.
        #[arg(long, conflicts_with = "remote")]
        no_remote: bool,
        /// After a successful land, record a cross-repo known-good tag of the resulting main. Optionally name it; defaults to the bundle slug.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        tag: Option<String>,
        /// Do not auto-tag even when the `auto-tag` config default is on.
        #[arg(long, conflicts_with = "tag")]
        no_tag: bool,
    },
    /// Create revert PRs for the merge steps a failed landing run completed.
    Rollback {
        /// Run file to roll back. Defaults to the latest run.
        #[arg(long)]
        run: Option<PathBuf>,
        /// Create the revert PRs. Without this flag the rollback is only previewed.
        #[arg(long)]
        apply: bool,
    },
    /// Resume a failed or incomplete landing run.
    Resume {
        /// Resume even when required checks are missing, red, or stale.
        #[arg(long)]
        skip_checks: bool,
        /// Run file to resume. Defaults to the latest run.
        #[arg(long)]
        run: Option<PathBuf>,
        /// Also push the landed bundle artifact to these sync remotes. Repeat to push to multiple remotes and override push-sync config.
        #[arg(long, value_name = "REMOTE")]
        remote: Vec<String>,
        /// Skip the remote bundle sync after landing.
        #[arg(long, conflicts_with = "remote")]
        no_remote: bool,
    },
    /// Show the latest landing run or default plan status.
    Status {
        /// Run file to inspect. Defaults to the latest run.
        #[arg(long)]
        run: Option<PathBuf>,
    },
    /// Preflight each recorded PR's live landing readiness before applying.
    Check,
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
