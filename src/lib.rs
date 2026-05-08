pub mod checkout;
pub mod cli;
pub mod commands;
pub mod git;
pub mod ids;
pub mod model;
pub mod output;
pub mod paths;
pub mod repo_selectors;
pub mod selectors;
pub mod status;
pub mod store;
pub mod time;
pub mod tracking;

use anyhow::Result;

pub use cli::{BundleCommand, Cli, Commands, GithubPublishCommand, PublishCommand};

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init {
            title,
            force,
            agents,
        } => commands::init_bundle(&title, force, agents),
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
        Commands::Bundle { command } => match command {
            BundleCommand::Path => commands::bundle_path(),
            BundleCommand::Print => commands::print_bundle(),
            BundleCommand::Validate => commands::validate_bundle(),
        },
        Commands::Checkpoint { message } => commands::record_checkpoint(&message),
        Commands::Close { reason } => commands::close_bundle(reason.as_deref()),
        Commands::Clean {
            plans,
            worktrees,
            all,
            force,
        } => commands::clean_generated(plans, worktrees, all, force),
        Commands::Stage {
            repos,
            intent_to_add,
            update,
            args,
        } => commands::stage_paths(&repos, &args, intent_to_add, update),
        Commands::Status => commands::show_status(),
        Commands::Diff { stat, repos } => commands::show_diff(&repos, stat),
        Commands::Fetch { repos, all } => commands::fetch_repos(&repos, all),
        Commands::Pull {
            repos,
            all,
            rebase,
            force,
            feature,
        } => commands::pull_repos(&repos, all, rebase, force, feature),
        Commands::Push {
            repos,
            all,
            set_upstream,
        } => commands::push_repos(&repos, all, set_upstream),
        Commands::Publish { target } => match target {
            PublishCommand::Github { command } => match command {
                GithubPublishCommand::Create {
                    repos,
                    all,
                    draft,
                    sync,
                    no_sync,
                    set_upstream,
                } => commands::create_github_publications(
                    &repos,
                    all,
                    draft,
                    sync || !no_sync,
                    set_upstream,
                ),
                GithubPublishCommand::Sync { repos, all } => {
                    commands::sync_github_publications(&repos, all)
                }
                GithubPublishCommand::Status { repos, all } => {
                    commands::show_github_publication_status(&repos, all)
                }
            },
        },
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
