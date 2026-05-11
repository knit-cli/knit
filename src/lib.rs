pub mod advice;
pub mod checkout;
pub mod cli;
pub mod commands;
pub mod git;
pub mod ids;
pub mod model;
pub mod output;
pub mod paths;
pub mod providers;
pub mod repo_selectors;
pub mod selectors;
pub mod status;
pub mod store;
pub mod time;
pub mod tracking;

use anyhow::Result;

pub use cli::{
    BundleCommand, Cli, Commands, ConfigCommand, GithubPublishCommand, LandCommand, ProjectCommand,
    PublishCommand, SchemaCommand,
};

pub fn run(cli: Cli) -> Result<()> {
    store::set_bundle_override(cli.bundle);
    match cli.command {
        Commands::Init {
            title,
            force,
            agents,
        } => commands::init_bundle(&title, force, agents),
        Commands::Project { command } => match command {
            ProjectCommand::Init { name } => commands::init_project(&name),
            ProjectCommand::Add {
                repo_id,
                repo_path,
                base,
                observe,
            } => commands::add_project_repo(&repo_id, &repo_path, base.as_deref(), observe),
            ProjectCommand::List => commands::list_projects(),
            ProjectCommand::Show { name } => commands::show_project(name.as_deref()),
        },
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
            None => commands::show_current_bundle(),
            Some(BundleCommand::Start {
                title,
                project,
                repos,
                all_repos,
                no_worktree,
                in_place,
                force,
                agents,
            }) => commands::start_bundle(
                &title,
                project.as_deref(),
                &repos,
                all_repos,
                !no_worktree,
                in_place,
                force,
                agents,
            ),
            Some(BundleCommand::Add {
                repos,
                base,
                in_place,
                no_worktree,
            }) => commands::track_repo_selectors(&repos, base.as_deref(), !no_worktree, in_place),
            Some(BundleCommand::Remove { repo_ids }) => commands::remove_repos(&repo_ids),
            Some(BundleCommand::List {
                all,
                archived,
                deleted,
            }) => commands::list_bundles(all, archived, deleted),
            Some(BundleCommand::Switch {
                bundle,
                workspace,
                here,
            }) => commands::switch_bundle(&bundle, workspace, here),
            Some(BundleCommand::Close { reason }) => commands::close_bundle(reason.as_deref()),
            Some(BundleCommand::Archive { bundle }) => commands::archive_bundle(&bundle),
            Some(BundleCommand::Restore { bundle }) => commands::restore_bundle(&bundle),
            Some(BundleCommand::Delete { bundle, force }) => {
                commands::delete_bundle(&bundle, force)
            }
            Some(BundleCommand::Compat {
                sources,
                title,
                project,
                all_repos,
                no_worktree,
                in_place,
                force,
            }) => commands::create_compat_bundle(
                &sources,
                title.as_deref(),
                project.as_deref(),
                all_repos,
                !no_worktree,
                in_place,
                force,
            ),
            Some(BundleCommand::Path) => commands::bundle_path(),
            Some(BundleCommand::Print) => commands::print_bundle(),
            Some(BundleCommand::Validate) => commands::validate_bundle(),
        },
        Commands::Switch {
            bundle,
            workspace,
            here,
        } => commands::switch_bundle(&bundle, workspace, here),
        Commands::Checkpoint { message } => commands::record_checkpoint(&message),
        Commands::Close { reason } => commands::close_bundle(reason.as_deref()),
        Commands::Clean {
            plans,
            worktrees,
            closed,
            merge_worktrees,
            all,
            force,
        } => commands::clean_generated(plans, worktrees, closed, merge_worktrees, all, force),
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
                    bases,
                    all,
                    draft,
                    sync,
                    no_sync,
                    set_upstream,
                } => commands::create_github_publications(
                    &repos,
                    all,
                    draft,
                    &bases,
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
        Commands::Land { command } => match command {
            LandCommand::Plan {
                provider,
                out,
                force,
            } => commands::generate_land_plan(&provider, out.as_deref(), force),
            LandCommand::Apply { plan } => commands::apply_land_plan(plan.as_deref()),
            LandCommand::Resume { run } => commands::resume_land_run(run.as_deref()),
            LandCommand::Status { run } => commands::show_land_status(run.as_deref()),
            LandCommand::Update {
                repos,
                all,
                push,
                set_upstream,
                continue_merge,
            } => commands::update_land_branches(&repos, all, push, set_upstream, continue_merge),
        },
        Commands::Merge {
            source,
            into,
            manual,
            fetch,
            push,
            set_upstream,
            run,
            repos,
            continue_run,
            abort,
        } => commands::merge_command(
            source.as_deref(),
            into.as_deref(),
            manual,
            fetch,
            push,
            set_upstream,
            run.as_deref(),
            &repos,
            continue_run,
            abort,
        ),
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
        Commands::Config { command } => match command {
            ConfigCommand::Set { key, value } => commands::set_config_value(&key, &value),
        },
        Commands::Schema { command } => match command {
            SchemaCommand::Print { name } => commands::print_schema(&name),
        },
        Commands::Doctor => commands::doctor_workspace(),
        Commands::Migrate { check } => commands::migrate_workspace(check),
    }
}
