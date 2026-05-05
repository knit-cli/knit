pub mod cli;
pub mod commands;
pub mod git;
pub mod ids;
pub mod model;
pub mod paths;
pub mod status;
pub mod store;
pub mod time;

use anyhow::Result;

pub use cli::{Cli, Commands};

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { title, force } => commands::init_bundle(&title, force),
        Commands::Add {
            repo_paths,
            base,
            no_worktree,
        } => commands::add_repos(&repo_paths, base.as_deref(), !no_worktree),
        Commands::Remove { repo_ids } => commands::remove_repos(&repo_ids),
        Commands::Worktree => commands::create_worktrees(),
        Commands::Stage => commands::stage_all(),
        Commands::Status => commands::show_status(),
        Commands::Commit { message, stage } => commands::commit_staged(&message, stage),
        Commands::Log => commands::show_log(),
        Commands::Show { commit_group_id } => commands::show_group(&commit_group_id),
    }
}
