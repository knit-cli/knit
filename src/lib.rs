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
        Commands::Add { repo_path, base } => commands::add_repo(&repo_path, base.as_deref()),
        Commands::Worktree => commands::create_worktrees(),
        Commands::Status => commands::show_status(),
        Commands::Commit { message } => commands::commit_staged(&message),
        Commands::Log => commands::show_log(),
        Commands::Show { commit_group_id } => commands::show_group(&commit_group_id),
    }
}
