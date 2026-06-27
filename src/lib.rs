pub mod advice;
pub mod checkout;
pub mod cli;
pub mod commands;
pub mod git;
pub mod history;
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
    BundleCommand, CheckCommand, Cli, Commands, ConfigCommand, HistoryCommand, LandCommand,
    ProjectCommand, ProjectRunCommandCli, PublishCommand, RemoteCommand, SchemaCommand,
    SyncCommand, ViewCommand,
};

pub fn run(cli: Cli) -> Result<()> {
    store::set_bundle_override(cli.bundle);
    match cli.command {
        Commands::Init { name, agents } => commands::init_project(&name, agents),
        Commands::Project { command } => match command {
            ProjectCommand::Add {
                repo_id,
                repo_path,
                base,
                observe,
                agents,
            } => commands::add_project_repo(&repo_id, &repo_path, base.as_deref(), observe, agents),
            ProjectCommand::List => commands::list_projects(),
            ProjectCommand::Show { name } => commands::show_project(name.as_deref()),
            ProjectCommand::Remove { name, force } => commands::remove_project(&name, force),
            ProjectCommand::Push { name, remote } => {
                commands::push_project_to_remote(name.as_deref(), remote.as_deref())
            }
            ProjectCommand::Agents { name } => commands::refresh_project_agents(name.as_deref()),
            ProjectCommand::Pull { name, repo, agents } => {
                commands::pull_project_config(name.as_deref(), &repo, agents)
            }
            ProjectCommand::Command { command } => match command {
                ProjectRunCommandCli::Set {
                    name,
                    repos,
                    cwd,
                    env,
                    command,
                } => {
                    commands::set_project_run_command(&name, &repos, cwd.as_deref(), &env, &command)
                }
                ProjectRunCommandCli::List => commands::list_project_run_commands(),
                ProjectRunCommandCli::Remove { name } => {
                    commands::remove_project_run_command(&name)
                }
            },
        },
        Commands::View { command } => match command {
            ViewCommand::List { project } => commands::list_views(project.as_deref()),
            ViewCommand::Show {
                name,
                project,
                repos,
            } => commands::show_view(name.as_deref(), project.as_deref(), repos),
            ViewCommand::Save {
                name,
                include,
                exclude,
                from_bundle,
                project,
            } => commands::save_view(&name, &include, &exclude, from_bundle, project.as_deref()),
            ViewCommand::Include {
                name,
                repos,
                project,
            } => commands::view_include(&name, &repos, project.as_deref()),
            ViewCommand::Exclude {
                name,
                repos,
                project,
            } => commands::view_exclude(&name, &repos, project.as_deref()),
            ViewCommand::Unset {
                name,
                repos,
                project,
            } => commands::view_unset(&name, &repos, project.as_deref()),
            ViewCommand::Default {
                name,
                clear,
                project,
            } => commands::set_default_view(name.as_deref(), clear, project.as_deref()),
            ViewCommand::Rm { name, project } => commands::remove_view(&name, project.as_deref()),
            ViewCommand::Edit { project } => commands::edit_views(project.as_deref()),
        },
        Commands::Remote { command } => match command {
            RemoteCommand::Add {
                name,
                url,
                token,
                global,
            } => commands::add_remote(&name, &url, token.as_deref(), global),
            RemoteCommand::List { global } => commands::list_remotes(global),
            RemoteCommand::Show { name, global } => commands::show_remote(&name, global),
            RemoteCommand::Remove { name, global } => commands::remove_remote(&name, global),
            RemoteCommand::Token {
                name,
                token,
                clear,
                global,
            } => commands::set_remote_token(&name, token.as_deref(), clear, global),
        },
        Commands::Clone {
            project,
            target,
            remote,
            url,
            token,
            active_bundle,
            no_worktree,
        } => commands::clone_project_from_remote(
            &project,
            target.as_deref(),
            remote.as_deref(),
            url.as_deref(),
            token.as_deref(),
            active_bundle.as_deref(),
            !no_worktree,
        ),
        Commands::Add {
            repos,
            intent_to_add,
            update,
            args,
        } => commands::stage_paths(&repos, &args, intent_to_add, update),
        Commands::Bundle {
            title,
            project,
            repos,
            all_repos,
            view,
            include,
            exclude,
            no_worktree,
            in_place,
            force,
            agents,
            cd,
            command,
        } => match command {
            None => match title {
                Some(title) => commands::start_bundle(
                    &title,
                    project.as_deref(),
                    &repos,
                    all_repos,
                    view.as_deref(),
                    &include,
                    &exclude,
                    !no_worktree,
                    in_place,
                    force,
                    agents,
                    cd.as_deref(),
                ),
                None => commands::show_current_bundle(),
            },
            Some(BundleCommand::Worktree) => commands::create_worktrees(),
            Some(BundleCommand::Add {
                repos,
                base,
                in_place,
                no_worktree,
            }) => commands::track_repo_selectors(&repos, base.as_deref(), !no_worktree, in_place),
            Some(BundleCommand::Remove {
                repos,
                keep_worktree,
                delete_branch,
                force,
            }) => commands::bundle_exclude(&repos, keep_worktree, delete_branch, force),
            Some(BundleCommand::ApplyView {
                name,
                keep_worktree,
                delete_branch,
                force,
            }) => commands::bundle_apply_view(&name, keep_worktree, delete_branch, force),
            Some(BundleCommand::List {
                all,
                archived,
                deleted,
            }) => commands::list_bundles(all, archived, deleted),
            Some(BundleCommand::Prune {
                apply,
                force,
                refresh,
                no_refresh,
                untracked,
                report,
                all,
                worktrees,
                branches,
                force_branches,
                remote_branches,
                remote_bundles,
            }) => {
                let refresh = refresh || !no_refresh;
                let worktrees = all || worktrees;
                let force = all || force;
                let branches = all || branches;
                let force_branches = all || force_branches;
                let remote_branches = all || remote_branches;
                let remote_bundles = all || remote_bundles;
                commands::prune_merged_bundles(
                    apply,
                    refresh,
                    untracked,
                    report,
                    worktrees,
                    force,
                    branches,
                    force_branches,
                    remote_branches,
                    remote_bundles,
                )
            }
            Some(BundleCommand::Archive {
                bundle,
                reason,
                keep_worktrees,
                force,
            }) => commands::archive_bundle(&bundle, reason.as_deref(), keep_worktrees, force),
            Some(BundleCommand::Restore { bundle }) => commands::restore_bundle(&bundle),
            Some(BundleCommand::Delete {
                bundle,
                force,
                worktrees,
                branches,
                force_branches,
                remote_branches,
            }) => commands::delete_bundle(
                &bundle,
                force,
                worktrees,
                branches,
                force_branches,
                remote_branches,
                false,
                None,
            ),
            Some(BundleCommand::Path) => commands::bundle_path(),
            Some(BundleCommand::Print) => commands::print_bundle(),
            Some(BundleCommand::Validate) => commands::validate_bundle(),
        },
        Commands::Switch { bundle, workspace } => commands::switch_bundle(&bundle, workspace),
        Commands::Clean {
            plans,
            worktrees,
            archived,
            merge_worktrees,
            all,
            force,
        } => commands::clean_generated(plans, worktrees, archived, merge_worktrees, all, force),
        Commands::Status => commands::show_status(),
        Commands::Diff { stat, repos } => commands::show_diff(&repos, stat),
        Commands::Fetch {
            repos,
            mode,
            remote,
            no_remote,
        } => commands::fetch_repos(&repos, mode, remote.as_deref(), no_remote),
        Commands::Pull {
            repos,
            all,
            rebase,
            force,
            feature,
            main,
            bundles,
            remote,
            no_remote,
            merge,
        } => commands::pull(
            &repos,
            all,
            rebase,
            force,
            feature,
            main,
            bundles,
            remote.as_deref(),
            no_remote,
            merge,
        ),
        Commands::Push {
            repos,
            all,
            set_upstream,
            remote,
            no_remote,
        } => commands::push_repos(&repos, all, set_upstream, &remote, no_remote),
        Commands::Check { command } => match command {
            CheckCommand::Run { name, repos, all } => commands::run_check(&name, &repos, all),
            CheckCommand::Record {
                name,
                pass,
                fail,
                detail,
            } => {
                if pass == fail {
                    anyhow::bail!("Pass exactly one of --pass or --fail.");
                }
                commands::record_check(&name, pass, detail.as_deref())
            }
            CheckCommand::Status => commands::show_check_status(),
        },
        Commands::Run {
            name,
            repos,
            all,
            list,
            args,
        } => commands::run_project_command(name.as_deref(), &repos, all, list, &args),
        Commands::Publish { target } => match target {
            PublishCommand::Create {
                repos,
                from_artifact,
                out,
                no_push,
                bases,
                all,
                draft,
                sync,
                no_sync,
                set_upstream,
                remote,
                no_remote,
                provider,
                github,
            } => {
                let provider = effective_publish_provider(provider, github);
                match from_artifact {
                    Some(path) => commands::create_publications_from_artifact(
                        &path,
                        out.as_deref(),
                        &repos,
                        all,
                        draft,
                        &bases,
                        sync || !no_sync,
                        !no_push,
                        provider.as_deref(),
                    ),
                    None => commands::create_publications(
                        &repos,
                        all,
                        draft,
                        &bases,
                        sync || !no_sync,
                        set_upstream,
                        &remote,
                        no_remote,
                        provider.as_deref(),
                    ),
                }
            }
            PublishCommand::Sync {
                repos,
                from_artifact,
                out,
                all,
                provider,
                github,
            } => {
                let provider = effective_publish_provider(provider, github);
                match from_artifact {
                    Some(path) => commands::sync_publications_from_artifact(
                        &path,
                        out.as_deref(),
                        &repos,
                        all,
                        provider.as_deref(),
                    ),
                    None => commands::sync_publications(&repos, all, provider.as_deref()),
                }
            }
            PublishCommand::Status {
                repos,
                all,
                live,
                provider,
                github,
            } => {
                let provider = effective_publish_provider(provider, github);
                commands::show_publication_status(&repos, all, live, provider.as_deref())
            }
        },
        Commands::Land { command } => match command {
            None => commands::land_default(),
            Some(LandCommand::Plan {
                provider,
                out,
                force,
            }) => commands::generate_land_plan(provider.as_deref(), out.as_deref(), force),
            Some(LandCommand::Apply {
                plan,
                from_artifact,
                out,
                remote,
                no_remote,
                skip_checks,
                keep_worktrees,
            }) => match from_artifact {
                Some(path) => commands::apply_land_from_artifact(&path, out.as_deref()),
                None => commands::apply_land_plan(
                    plan.as_deref(),
                    &remote,
                    no_remote,
                    skip_checks,
                    keep_worktrees,
                ),
            },
            Some(LandCommand::Rollback { run, apply }) => {
                commands::rollback_land_run(run.as_deref(), apply)
            }
            Some(LandCommand::Resume {
                run,
                remote,
                no_remote,
                skip_checks,
            }) => commands::resume_land_run(run.as_deref(), &remote, no_remote, skip_checks),
            Some(LandCommand::Status { run }) => commands::show_land_status(run.as_deref()),
            Some(LandCommand::Check) => commands::check_landing(),
            Some(LandCommand::Update {
                repos,
                all,
                push,
                set_upstream,
                continue_merge,
            }) => commands::update_land_branches(&repos, all, push, set_upstream, continue_merge),
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
        Commands::CherryPick {
            from_bundle,
            targets,
            repos,
            dry_run,
        } => commands::cherrypick_from_bundle(&from_bundle, &targets, &repos, dry_run),
        Commands::Sync { command } => match command {
            None => commands::sync_bundle(),
            Some(SyncCommand::Push { targets, remote }) => {
                let targets = commands::remote::SyncTargets::resolve(
                    targets.bundles,
                    targets.history,
                    targets.views,
                    targets.all,
                );
                commands::remote::sync_push(targets, &remote)
            }
            Some(SyncCommand::Pull { targets, remote }) => {
                let targets = commands::remote::SyncTargets::resolve(
                    targets.bundles,
                    targets.history,
                    targets.views,
                    targets.all,
                );
                commands::remote::sync_pull(targets, &remote)
            }
        },
        Commands::History { command } => match command {
            None => commands::show_history(None, 20, None, None),
            Some(HistoryCommand::List {
                limit,
                repo,
                bundle,
                project,
            }) => commands::show_history(
                project.as_deref(),
                limit,
                repo.as_deref(),
                bundle.as_deref(),
            ),
            Some(HistoryCommand::Refresh { project }) => {
                commands::refresh_history(project.as_deref())
            }
        },
        Commands::Related {
            paths,
            repo,
            project,
            limit,
            commit_limit,
            pull,
            remote,
        } => commands::show_related_history(
            project.as_deref(),
            repo.as_deref(),
            &paths,
            limit,
            commit_limit,
            pull,
            remote.as_deref(),
        ),
        Commands::Commit { message, all } => commands::commit_staged(&message, all),
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
            ConfigCommand::Show { global } => commands::show_config(global),
            ConfigCommand::Set { key, value, global } => {
                commands::set_config_value(&key, &value, global)
            }
        },
        Commands::Schema { command } => match command {
            SchemaCommand::Print { name } => commands::print_schema(&name),
        },
        Commands::Doctor => commands::doctor_workspace(),
        Commands::Migrate { check } => commands::migrate_workspace(check),
    }
}

/// Resolve the effective publish provider filter: `--github` is sugar for
/// `--provider github`. `clap` already rejects passing both at once.
fn effective_publish_provider(provider: Option<String>, github: bool) -> Option<String> {
    if github {
        Some("github".to_string())
    } else {
        provider
    }
}
