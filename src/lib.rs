pub mod checkout;
pub mod cli;
pub mod commands;
pub mod git;
pub mod ids;
pub mod model;
pub mod output;
pub mod paths;
pub mod selectors;
pub mod status;
pub mod store;
pub mod time;
pub mod tracking;

use anyhow::Result;

pub use cli::{Cli, Commands};

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { title, force } => commands::init_bundle(&title, force),
        Commands::Track {
            repo_paths,
            base,
            in_place,
            no_worktree,
        } => commands::track_repos(&repo_paths, base.as_deref(), !no_worktree, in_place),
        Commands::Add {
            repos,
            intent_to_add,
            update,
            args,
        } => commands::stage_paths(&repos, &args, intent_to_add, update),
        Commands::Untrack { repo_ids } => commands::remove_repos(&repo_ids),
        Commands::Remove { repo_ids } => commands::remove_repos(&repo_ids),
        Commands::Worktree => commands::create_worktrees(),
        Commands::Stage {
            repos,
            intent_to_add,
            update,
            args,
        } => commands::stage_paths(&repos, &args, intent_to_add, update),
        Commands::Status => commands::show_status(),
        Commands::Diff { stat, repos } => commands::show_diff(&repos, stat),
        Commands::Sync => commands::sync_bundle(),
        Commands::Commit { message, stage } => commands::commit_staged(&message, stage),
        Commands::Log {
            limit,
            shorthand_limit,
        } => commands::show_log(limit, shorthand_limit.as_deref()),
        Commands::Revert {
            target,
            plan: _,
            apply,
        } => commands::revert_target(&target, apply),
        Commands::Git { repos, all, args } => commands::run_git(&args, &repos, all),
        Commands::Show { target } => commands::show_target(&target),
    }
}
