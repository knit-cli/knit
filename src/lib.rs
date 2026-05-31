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

use anyhow::{bail, Result};

pub use cli::{
    BundleCommand, Cli, Commands, ConfigCommand, GithubPublishCommand, LandCommand, OrgCommand,
    ProjectCommand, ProjectRunCommandCli, PublishCommand, RemoteCommand, SchemaCommand,
    WorkItemCommand,
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
            ProjectCommand::Init { name, agents } => commands::init_project(&name, agents),
            ProjectCommand::Add {
                repo_id,
                repo_path,
                base,
                observe,
                agents,
            } => commands::add_project_repo(&repo_id, &repo_path, base.as_deref(), observe, agents),
            ProjectCommand::List => commands::list_projects(),
            ProjectCommand::Show { name } => commands::show_project(name.as_deref()),
            ProjectCommand::SetOrg { org } => commands::set_project_org(&org),
            ProjectCommand::Remove { name, force } => commands::remove_project(&name, force),
            ProjectCommand::Push { name, remote } => {
                commands::push_project_to_remote(name.as_deref(), &remote)
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
        Commands::Org { command } => match command {
            OrgCommand::Init { name } => commands::init_org(&name),
            OrgCommand::List => commands::list_orgs(),
            OrgCommand::Show { name } => commands::show_org(&name),
            OrgCommand::AddRepo {
                org,
                repo_id,
                repo_path,
                base,
            } => commands::add_org_repo(&org, &repo_id, &repo_path, base.as_deref()),
        },
        Commands::WorkItem { command } => match command {
            WorkItemCommand::Add {
                title,
                kind,
                description,
                project,
                org,
                repo_hints,
                depends_on,
                labels,
                acceptance_criteria,
                priority,
            } => commands::add_work_item(
                &title,
                &kind,
                description.as_deref(),
                project.as_deref(),
                org.as_deref(),
                &repo_hints,
                &depends_on,
                &labels,
                &acceptance_criteria,
                priority.as_deref(),
            ),
            WorkItemCommand::List { project, all } => {
                commands::list_work_items(project.as_deref(), all)
            }
            WorkItemCommand::Show { id } => commands::show_work_item(&id),
            WorkItemCommand::Update {
                id,
                title,
                description,
                planning_status,
                execution_status,
                lane,
                rank,
                planning_rationale,
                planner,
                target,
                last_outcome,
                depends_on,
                repo_hints,
                bundle_ids,
            } => commands::update_work_item(
                &id,
                title.as_deref(),
                description.as_deref(),
                planning_status.as_deref(),
                execution_status.as_deref(),
                lane.as_deref(),
                rank,
                planning_rationale.as_deref(),
                planner.as_deref(),
                target.as_deref(),
                last_outcome.as_deref(),
                &depends_on,
                &repo_hints,
                &bundle_ids,
            ),
            WorkItemCommand::Approve { id } => commands::approve_work_item(&id),
            WorkItemCommand::Export { project, all } => {
                commands::export_work_items(project.as_deref(), all)
            }
            WorkItemCommand::Start { id, target } => {
                commands::start_work_item(&id, target.as_deref())
            }
        },
        Commands::Remote { command } => match command {
            RemoteCommand::Add { name, url, token } => {
                commands::add_remote(&name, &url, token.as_deref())
            }
            RemoteCommand::List => commands::list_remotes(),
            RemoteCommand::Show { name } => commands::show_remote(&name),
            RemoteCommand::Remove { name } => commands::remove_remote(&name),
            RemoteCommand::Token { name, token, clear } => {
                commands::set_remote_token(&name, token.as_deref(), clear)
            }
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
            &remote,
            url.as_deref(),
            token.as_deref(),
            active_bundle.as_deref(),
            !no_worktree,
        ),
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
        Commands::Untrack { repo_ids, repos } => {
            let repo_ids = remove_repo_ids(repo_ids, repos)?;
            commands::remove_repos(&repo_ids)
        }
        Commands::Remove { repo_ids, repos } => {
            let repo_ids = remove_repo_ids(repo_ids, repos)?;
            commands::remove_repos(&repo_ids)
        }
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
            Some(BundleCommand::Remove { repo_ids, repos }) => {
                let repo_ids = remove_repo_ids(repo_ids, repos)?;
                commands::remove_repos(&repo_ids)
            }
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
                let branches = all || branches;
                let force_branches = all || force_branches;
                let remote_branches = all || remote_branches;
                let remote_bundles = all || remote_bundles;
                commands::prune_merged_bundles(
                    apply || force,
                    refresh,
                    untracked,
                    report,
                    worktrees,
                    branches,
                    force_branches,
                    remote_branches,
                    remote_bundles,
                )
            }
            Some(BundleCommand::Switch {
                bundle,
                workspace,
                here,
            }) => commands::switch_bundle(&bundle, workspace, here),
            Some(BundleCommand::Close { reason }) => commands::close_bundle(reason.as_deref()),
            Some(BundleCommand::Archive { bundle }) => commands::archive_bundle(&bundle),
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
            Some(BundleCommand::Split {
                source,
                targets,
                title,
                repos,
                force,
            }) => commands::split_bundle(&source, title.as_deref(), &targets, &repos, force),
            Some(BundleCommand::Path) => commands::bundle_path(),
            Some(BundleCommand::Print) => commands::print_bundle(),
            Some(BundleCommand::Validate) => commands::validate_bundle(),
            Some(BundleCommand::Push { remote, project }) => {
                commands::push_bundle_to_remote(&remote, project.as_deref())
            }
        },
        Commands::Switch {
            bundle,
            workspace,
            here,
        } => commands::switch_bundle(&bundle, workspace, here),
        Commands::Checkpoint { message } => commands::record_checkpoint(&message),
        Commands::Close { reason } => commands::close_bundle(reason.as_deref()),
        Commands::Prune {
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
        } => {
            let refresh = refresh || !no_refresh;
            let worktrees = all || worktrees;
            let branches = all || branches;
            let force_branches = all || force_branches;
            let remote_branches = all || remote_branches;
            let remote_bundles = all || remote_bundles;
            commands::prune_merged_bundles(
                apply || force,
                refresh,
                untracked,
                report,
                worktrees,
                branches,
                force_branches,
                remote_branches,
                remote_bundles,
            )
        }
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
        ),
        Commands::Push {
            repos,
            all,
            set_upstream,
            remote,
            no_remote,
        } => commands::push_repos(&repos, all, set_upstream, remote.as_deref(), no_remote),
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
            } => match from_artifact {
                Some(path) => commands::create_publications_from_artifact(
                    &path,
                    out.as_deref(),
                    &repos,
                    all,
                    draft,
                    &bases,
                    sync || !no_sync,
                    !no_push,
                ),
                None => commands::create_publications(
                    &repos,
                    all,
                    draft,
                    &bases,
                    sync || !no_sync,
                    set_upstream,
                    remote.as_deref(),
                    no_remote,
                ),
            },
            PublishCommand::Sync {
                repos,
                from_artifact,
                out,
                all,
            } => match from_artifact {
                Some(path) => {
                    commands::sync_publications_from_artifact(&path, out.as_deref(), &repos, all)
                }
                None => commands::sync_publications(&repos, all),
            },
            PublishCommand::Status { repos, all } => {
                commands::show_publication_status(&repos, all)
            }
            PublishCommand::Github { command } => match command {
                GithubPublishCommand::Create {
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
                } => match from_artifact {
                    Some(path) => commands::create_publications_from_artifact(
                        &path,
                        out.as_deref(),
                        &repos,
                        all,
                        draft,
                        &bases,
                        sync || !no_sync,
                        !no_push,
                    ),
                    None => commands::create_publications(
                        &repos,
                        all,
                        draft,
                        &bases,
                        sync || !no_sync,
                        set_upstream,
                        None,
                        false,
                    ),
                },
                GithubPublishCommand::Sync {
                    repos,
                    from_artifact,
                    out,
                    all,
                } => match from_artifact {
                    Some(path) => {
                        commands::sync_publications_from_artifact(&path, out.as_deref(), &repos, all)
                    }
                    None => commands::sync_publications(&repos, all),
                },
                GithubPublishCommand::Status { repos, all } => {
                    commands::show_publication_status(&repos, all)
                }
            },
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
            }) => match from_artifact {
                Some(path) => commands::apply_land_from_artifact(&path, out.as_deref()),
                None => commands::apply_land_plan(plan.as_deref(), remote.as_deref(), no_remote),
            },
            Some(LandCommand::Resume {
                run,
                remote,
                no_remote,
            }) => commands::resume_land_run(run.as_deref(), remote.as_deref(), no_remote),
            Some(LandCommand::Sync { remote }) => commands::sync_landed_bundle(remote.as_deref()),
            Some(LandCommand::Status { run }) => commands::show_land_status(run.as_deref()),
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
        Commands::Reset {
            soft,
            mixed: _,
            hard,
            commit,
            repos,
            all,
        } => commands::reset_checkouts(soft, hard, commit.as_deref(), &repos, all),
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

fn remove_repo_ids(repo_ids: Vec<String>, repos: Vec<String>) -> Result<Vec<String>> {
    let repo_ids = repo_ids.into_iter().chain(repos).collect::<Vec<_>>();
    if repo_ids.is_empty() {
        bail!("Pass at least one repo id, for example `knit bundle remove --repo backend`.");
    }
    Ok(repo_ids)
}
