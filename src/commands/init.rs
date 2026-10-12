use crate::checkout::checkout_dir;
use crate::commands::agents::{
    agent_teamwork_section, print_bundle_worktree_agents_summary, upsert_managed_section,
    write_bundle_worktree_agents_md,
};
use crate::commands::base::BundleBaseMode;
use crate::ids::{expand_repo_selectors, slugify};
use crate::model::{ChangeGroup, KnitConfig, KnitProject, ProjectRepoEntry, ProjectView};
use crate::output as out;
use crate::store::{
    bundle_path as stored_bundle_path, find_knit_root, load_config, load_views, read_json,
    save_active_bundle, save_config, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn init_bundle(title: &str, force: bool, agents: bool) -> Result<()> {
    start_bundle(
        title,
        None,
        &[],
        false,
        None,
        &[],
        &[],
        true,
        false,
        BundleBaseMode::FreshRemote,
        force,
        agents,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn start_bundle(
    title: &str,
    project: Option<&str>,
    repo_ids: &[String],
    all_repos: bool,
    view: Option<&str>,
    include: &[String],
    exclude: &[String],
    materialize: bool,
    in_place: bool,
    base_mode: BundleBaseMode,
    force: bool,
    agents: bool,
    cd: Option<&str>,
) -> Result<()> {
    if all_repos && !repo_ids.is_empty() {
        bail!("Use either --all-repos or --repo, not both.");
    }
    if view.is_some() && (all_repos || !repo_ids.is_empty()) {
        bail!("Use --view for default selection, not together with --repo or --all-repos.");
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).unwrap_or(cwd);
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let bundle_path = stored_bundle_path(&root, &bundle_id);
    let bundle_existed = bundle_path.exists();
    if bundle_existed && !force {
        if agents && cd.is_none() {
            let bundle: ChangeGroup = read_json(&bundle_path)?;
            let active = ActiveBundle::unlocked(root.clone(), bundle_path.clone(), bundle);
            let agents_path = write_agents_md(&root)?;
            let bundle_agents = write_bundle_worktree_agents_md(&active)?;
            println!(
                "{} {}",
                out::heading("AGENTS.md:"),
                out::path(agents_path.display())
            );
            print_bundle_worktree_agents_summary(bundle_agents.as_deref());
            return Ok(());
        }
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            bundle_path.display()
        );
    }

    if !force {
        ensure_no_remote_slug_collision(&root, project, &bundle_id)?;
    }

    fs::create_dir_all(&bundle_dir).context("failed to create .knit/bundles")?;
    fs::create_dir_all(&worktree_dir).context("failed to create .knit/worktrees")?;
    fs::create_dir_all(knit_dir.join("projects")).context("failed to create .knit/projects")?;

    let mut config = if knit_dir.join("config.json").exists() {
        load_config(&root)?
    } else {
        KnitConfig::new(bundle_id.clone())
    };
    let project_id = resolve_start_project(&root, project, &config)?;
    crate::auth::set_project_override(project_id.clone());
    let mut bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now_iso());
    bundle.project_id = project_id.clone();
    write_json(&bundle_path, &bundle)?;

    let setup_result = (|| -> Result<()> {
        // Empty project bundles have no tracking step to save their creation
        // event. Record it here, after repo selection has succeeded.
        if let Some(project_id) = &project_id {
            let selected = select_project_repos(
                &root, project_id, repo_ids, all_repos, view, include, exclude,
            )?;
            if !selected.is_empty() {
                let mut active = ActiveBundle::unlocked(root.clone(), bundle_path.clone(), bundle);
                crate::commands::track::track_project_repos(
                    &mut active,
                    &selected,
                    materialize,
                    in_place,
                    base_mode,
                )?;
                save_active_bundle(&active)?;
            } else {
                crate::history::record_bundle_history(&root, &bundle)?;
            }
        }

        // Do not switch the shared workspace fallback until every recoverable
        // bundle/repo update and requested worktree has been persisted. In
        // particular, an ENOSPC during materialization must not strand the
        // workspace on an incomplete bundle and hide the user's prior context.
        config.active_bundle = Some(bundle_id.clone());
        if let Some(project_id) = &project_id {
            config.active_project = Some(project_id.clone());
        }
        save_config(&root, &config)?;
        Ok(())
    })();

    if let Err(error) = setup_result {
        if rollback_empty_new_bundle(&bundle_path, &worktree_dir, bundle_existed) {
            return Err(error).with_context(|| {
                format!(
                    "Bundle `{bundle_id}` setup failed before any repos were recorded, so its new artifact was removed. The workspace fallback was not switched. Resolve the error and retry the original bundle command."
                )
            });
        }
        return Err(error).with_context(|| {
            format!(
                "Bundle `{bundle_id}` was recorded at {}, but setup did not complete. The workspace fallback was not switched. Inspect it with `knit --bundle {bundle_id} status`; after resolving the error, run `knit --bundle {bundle_id} bundle worktree` to resume any recorded repos. If no repos were recorded, rerun the original bundle command with `--force`.",
                bundle_path.display()
            )
        });
    }

    println!(
        "{} {}",
        out::heading("Active bundle:"),
        out::path(bundle_path.display())
    );

    if agents {
        let agents_path = write_agents_md(&root)?;
        println!(
            "{} {}",
            out::heading("AGENTS.md:"),
            out::path(agents_path.display())
        );
    }

    let bundle: ChangeGroup = read_json(&bundle_path)?;
    let active = ActiveBundle::unlocked(root.clone(), bundle_path.clone(), bundle);
    let bundle_agents = write_bundle_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());

    if let Some(selector) = cd {
        let path = cd_target_dir(&active, selector)?;
        start_shell_in(&active, &path)?;
    }

    Ok(())
}

fn rollback_empty_new_bundle(
    bundle_path: &Path,
    worktree_dir: &Path,
    bundle_existed: bool,
) -> bool {
    if bundle_existed {
        return false;
    }
    let Ok(bundle) = read_json::<ChangeGroup>(bundle_path) else {
        return false;
    };
    if !bundle.repos.is_empty() || fs::remove_file(bundle_path).is_err() {
        return false;
    }

    // This only succeeds for the empty container created by start_bundle. A
    // pre-existing non-empty directory is intentionally preserved for manual
    // inspection instead of being recursively discarded.
    let _ = fs::remove_dir(worktree_dir);
    true
}

/// Resolve the directory a `--cd` shell should start in: the bundle worktree
/// root for an empty selector, or the checkout of the one repo a selector
/// matches. Shared by bundle creation and by entering an existing bundle, so
/// both spellings navigate identically.
pub(crate) fn cd_target_dir(active: &ActiveBundle, selector: &str) -> Result<PathBuf> {
    if active.bundle.repos.is_empty() {
        bail!("Cannot cd into a checkout because the bundle has no tracked repos.");
    }

    if selector.trim().is_empty() {
        let root = active.root.join(".knit/worktrees").join(&active.bundle.id);
        ensure_enterable_worktree_root(active, &root)?;
        return Ok(root);
    }

    let selectors = [selector.to_string()];
    let indexes = crate::repo_selectors::resolve_repo_indexes(active, &selectors, false)?;
    if indexes.len() != 1 {
        bail!("Repo selector `{selector}` matched multiple repos; pass a more specific repo id.");
    }
    checkout_for_repo(active, indexes[0])
}

/// A shell may only start in a bundle worktree root that exists and holds at
/// least one materialized checkout. Creation-time `--cd` always passes
/// (materialization precedes navigation); entering an existing bundle can
/// reach one that was never materialized or whose worktrees were removed, and
/// that gets an actionable error instead of a shell in a missing or empty
/// directory. Nothing is created here — no branches, no worktrees.
fn ensure_enterable_worktree_root(active: &ActiveBundle, root: &Path) -> Result<()> {
    let materialized = active
        .bundle
        .repos
        .iter()
        .any(|repo| checkout_dir(active, repo).is_some());
    if root.is_dir() && materialized {
        return Ok(());
    }
    if crate::commands::bundle::bundle_state(&active.bundle)
        == crate::commands::bundle::BundleStatus::Archived
    {
        bail!(
            "Bundle `{}` is archived and its generated worktrees were removed. Run `knit bundle restore {}` and then `knit --bundle {} bundle worktree` before entering it.",
            active.bundle.id,
            active.bundle.id,
            active.bundle.id
        );
    }
    bail!(
        "Bundle `{}` has no materialized worktree at {}. Run `knit --bundle {} bundle worktree` first.",
        active.bundle.id,
        out::path(root.display()),
        active.bundle.id
    );
}

fn checkout_for_repo(active: &ActiveBundle, index: usize) -> Result<PathBuf> {
    let repo = &active.bundle.repos[index];
    checkout_dir(active, repo).with_context(|| {
        format!(
            "{} has no materialized checkout. Run `knit bundle worktree` first.",
            repo.id
        )
    })
}

/// Start the user's shell in `path` with the bundle context exported. The one
/// navigation helper shared by creation-time `--cd` and by entering an
/// existing bundle with `knit bundle --cd`.
pub(crate) fn start_shell_in(active: &ActiveBundle, path: &Path) -> Result<()> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(default_shell);
    println!("{} {}", out::heading("cd:"), out::path(path.display()));
    let status = Command::new(&shell)
        .current_dir(path)
        .env("KNIT_ROOT", &active.root)
        .env("KNIT_BUNDLE", &active.bundle.id)
        .status()
        .with_context(|| {
            format!(
                "failed to start shell {} in {}",
                PathBuf::from(&shell).display(),
                path.display()
            )
        })?;
    if !status.success() {
        bail!("shell exited with {status}");
    }
    Ok(())
}

fn default_shell() -> std::ffi::OsString {
    if cfg!(windows) {
        std::ffi::OsString::from("cmd")
    } else {
        std::ffi::OsString::from("/bin/sh")
    }
}

/// Refuse to create a bundle whose slug already exists on the sync
/// remote for this project: two users independently picking the same title
/// would otherwise share one remote record and one `knit/<slug>` branch
/// namespace across every repo. Best-effort by design — no workspace config,
/// no sync remote, no token, or an unreachable remote all skip the check so
/// creation keeps working offline. `--force` skips it explicitly.
fn ensure_no_remote_slug_collision(
    root: &Path,
    project: Option<&str>,
    bundle_id: &str,
) -> Result<()> {
    if !root.join(".knit/config.json").exists() {
        return Ok(());
    }
    let Ok(config) = crate::store::load_effective_config(root) else {
        return Ok(());
    };
    let Some(project_id) = project
        .map(slugify)
        .or_else(|| config.active_project.clone())
    else {
        return Ok(());
    };
    match crate::commands::remote::remote_bundle_lifecycle(&config, &project_id, bundle_id) {
        Ok(Some(state)) => bail!(
            "Bundle `{bundle_id}` already exists on the sync remote for project `{project_id}` ({state}). Join the existing bundle with `knit fetch` and `knit --bundle {bundle_id} bundle worktree`, pick another title, or pass --force to create a local bundle with the same id anyway."
        ),
        // Offline, missing remote/token, or a server error must never block
        // local bundle creation; the push-time ledger gate still protects the
        // remote record itself.
        Ok(None) | Err(_) => Ok(()),
    }
}

fn resolve_start_project(
    root: &Path,
    project: Option<&str>,
    config: &KnitConfig,
) -> Result<Option<String>> {
    let Some(project_id) = project
        .map(slugify)
        .or_else(|| config.active_project.clone())
    else {
        return Ok(None);
    };
    let path = root
        .join(".knit/projects")
        .join(format!("{project_id}.project.json"));
    if !path.exists() {
        bail!("Project {} does not exist.", out::repo(&project_id));
    }
    Ok(Some(project_id))
}

fn select_project_repos(
    root: &Path,
    project_id: &str,
    repo_ids: &[String],
    all_repos: bool,
    view_name: Option<&str>,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<ProjectRepoEntry>> {
    let project = crate::commands::project::load_project_by_id(root, project_id)?;
    let view = resolve_active_view(root, project_id, view_name)?;
    let selected = resolve_view_repos(
        &project,
        repo_ids,
        all_repos,
        view.as_ref().map(|(name, view)| (name.as_str(), view)),
        include,
        exclude,
    )?;
    ensure_repos_cloned(root, &selected)?;
    Ok(selected)
}

/// Refuse a bundle that would track a project repo with no checkout on this
/// machine. In a scoped workspace the fix is to extend the scope view and
/// pull; elsewhere the project entry points at a path that has gone missing.
fn ensure_repos_cloned(root: &Path, selected: &[ProjectRepoEntry]) -> Result<()> {
    let missing: Vec<&ProjectRepoEntry> = selected
        .iter()
        .filter(|repo| !Path::new(&repo.path).exists())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = missing.iter().map(|repo| repo.id.as_str()).collect();
    let scope_view = root
        .join(".knit/config.json")
        .exists()
        .then(|| load_config(root).ok())
        .flatten()
        .and_then(|config| config.scope_view);
    match scope_view {
        Some(view) => bail!(
            "Repo {} is not cloned in this workspace (scoped to view {}). Run `knit view include {view} {}` and then `knit pull` to add it, or leave it out with `--exclude`.",
            ids.join(", "),
            out::repo(&view),
            ids.join(" ")
        ),
        None => bail!(
            "Repo {} has no checkout at its recorded path: {}. Clone it there or fix the project entry with `knit project add`.",
            ids.join(", "),
            missing
                .iter()
                .map(|repo| repo.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve which named view to apply: an explicit `--view` name (which must
/// exist), otherwise the user's saved default view, otherwise none. Returns
/// the view together with its name so warnings can point at the right view.
/// Resolution uses the effective overlay: the user's personal views over the
/// project's admin-managed shared templates, personal winning by name.
pub(crate) fn resolve_active_view(
    root: &Path,
    project_id: &str,
    view_name: Option<&str>,
) -> Result<Option<(String, ProjectView)>> {
    let views = load_views(root, project_id)?;
    match view_name {
        Some(name) => {
            let name = slugify(name);
            let view = views.effective_view(&name).cloned().with_context(|| {
                format!(
                    "Project {} has no view named {} (saved or shared). Create it with `knit view save {name}`, or refresh shared templates with `knit sync pull --views`.",
                    out::repo(project_id),
                    out::repo(&name)
                )
            })?;
            Ok(Some((name, view)))
        }
        // A dangling default is ignored rather than blocking `bundle start`.
        None => Ok(views.default_view.as_ref().and_then(|name| {
            views
                .effective_view(name)
                .cloned()
                .map(|view| (name.clone(), view))
        })),
    }
}

/// Resolve a project's repo set for a bundle, applying (in order): the explicit
/// `--repo`/`--all-repos` set or the view's seed set (`includeByDefault`, or
/// empty for a `base: none` view) plus the view's include/exclude deltas, then
/// ad-hoc `include`/`exclude` overrides. Results preserve project order and are
/// de-duplicated.
pub(crate) fn resolve_view_repos(
    project: &KnitProject,
    repo_ids: &[String],
    all_repos: bool,
    view: Option<(&str, &ProjectView)>,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<ProjectRepoEntry>> {
    use std::collections::BTreeSet;
    let repo_ids = expand_repo_selectors(repo_ids);
    let include = expand_repo_selectors(include);
    let exclude = expand_repo_selectors(exclude);
    let mut selected: BTreeSet<String> = BTreeSet::new();

    if !repo_ids.is_empty() {
        for repo_id in &repo_ids {
            selected.insert(project_repo(project, repo_id)?.id.clone());
        }
    } else if all_repos {
        for repo in &project.repos {
            selected.insert(repo.id.clone());
        }
    } else {
        // An absolute view (`base: none`) seeds empty; its include list is the
        // complete shape and never absorbs default-set changes.
        if view.is_none_or(|(_, view)| view.base.is_default()) {
            for repo in &project.repos {
                if repo.include_by_default {
                    selected.insert(repo.id.clone());
                }
            }
        }
        if let Some((view_name, view)) = view {
            // Saved views are user data that can outlive project membership
            // changes (a repo removed after the view was saved, or a synced
            // teammate view naming a repo this project never tracked), so
            // dangling entries are skipped with a warning instead of
            // blocking bundle creation.
            for repo_id in &view.include {
                if let Some(repo) = project_repo_opt(project, repo_id) {
                    selected.insert(repo.id.clone());
                } else {
                    warn_dangling_view_repo(project, view_name, repo_id);
                }
            }
            for repo_id in &view.exclude {
                if let Some(repo) = project_repo_opt(project, repo_id) {
                    selected.remove(&repo.id);
                } else {
                    warn_dangling_view_repo(project, view_name, repo_id);
                }
            }
        }
    }

    // Ad-hoc flags apply on top in every mode, so `--all-repos --exclude sej`
    // and `--view backend --include gloss` both work.
    for repo_id in &include {
        selected.insert(project_repo(project, repo_id)?.id.clone());
    }
    for repo_id in &exclude {
        selected.remove(&project_repo(project, repo_id)?.id);
    }

    Ok(project
        .repos
        .iter()
        .filter(|repo| selected.contains(&repo.id))
        .cloned()
        .collect())
}

fn project_repo<'a>(project: &'a KnitProject, repo_id: &str) -> Result<&'a ProjectRepoEntry> {
    let repo_id = slugify(repo_id);
    project
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| {
            format!(
                "Project {} has no repo named {}.",
                out::repo(&project.id),
                out::repo(&repo_id)
            )
        })
}

/// Like [`project_repo`] but tolerant: view deltas may reference repos the
/// project no longer tracks, which callers surface as a warning rather than
/// an error.
fn project_repo_opt<'a>(project: &'a KnitProject, repo_id: &str) -> Option<&'a ProjectRepoEntry> {
    let repo_id = slugify(repo_id);
    project.repos.iter().find(|repo| repo.id == repo_id)
}

fn warn_dangling_view_repo(project: &KnitProject, view_name: &str, repo_id: &str) {
    crate::human!(
        "{} saved view {} references repo {}, which project {} no longer tracks; skipping. \
         Remove it with `knit view unset {} {}`.",
        out::warn("warning:"),
        out::repo(view_name),
        out::repo(repo_id),
        out::repo(&project.id),
        view_name,
        repo_id
    );
}

fn write_agents_md(root: &Path) -> Result<std::path::PathBuf> {
    let path = root.join("AGENTS.md");
    let section = agents_section();
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        upsert_managed_section(&existing, &section)
    } else {
        format!("# AGENTS.md\n\n{section}")
    };

    fs::write(&path, next)
        .with_context(|| format!("failed to write Knit agent tutorial at {}", path.display()))?;
    Ok(path)
}

/// Refresh every generated section in the workspace root AGENTS.md without
/// requiring a bundle creation side effect. The workspace guide is written
/// first so the project generator can see its shared teamwork guidance and
/// avoid duplicating it inside the project section.
pub fn refresh_agents(project: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project.map(slugify).or(config.active_project);
    let project = project_id
        .as_deref()
        .map(|project_id| read_json::<KnitProject>(&crate::store::project_path(&root, project_id)))
        .transpose()?;

    let agents_path = write_agents_md(&root)?;
    println!(
        "{} {}",
        out::heading("Workspace AGENTS.md:"),
        out::path(agents_path.display())
    );
    if let Some(project) = project {
        let project_agents = crate::commands::agents::write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(project_agents.display())
        );
    }
    Ok(())
}

fn agents_section() -> String {
    format!(
        r#"<!-- BEGIN KNIT AGENTS -->
## Knit Workspace Guide

This is a Knit workspace. Knit treats several Git repositories like one monorepo: a **bundle** is one
feature branch `knit/<bundle>` across the repositories a change touches, checked out together under
`.knit/worktrees/<bundle>/<repo>/` and recorded in `.knit/bundles/<bundle>.bundle.json`. Edit, build and
test in those worktrees, never in the source checkouts at the workspace root.

```sh
knit bundle list                      # open bundles; add --archived for finished ones
knit --bundle <b> status              # branches, commits and reviews of one bundle
knit bundle "Feature title"           # new bundle with the project's default repos (slug: feature-title)
knit bundle "Fix" --repo <repo>       # only some repos; `knit bundle add <repo>` adds one later
knit --bundle <b> diff                # the whole change across repositories
knit --bundle <b> commit --all -m "…" # one commit in every changed repo, recorded as one group
knit --bundle <b> publish create      # push and open one cross-linked review per changed repo
knit --bundle <b> push                # update open reviews after more commits
knit --bundle <b> rebase              # replay onto the latest bases; on a conflict fix it, `git add`,
                                      #   `git rebase --continue` in that repo, then `knit --bundle <b> rebase --continue`
knit --bundle <b> land                # plan merging the reviews; `knit --bundle <b> land apply` runs it
knit log --all [--repo <repo>]        # history of past bundles and their per-repo commits
```

- Run bundle commands from the workspace root with `--bundle <b>`, or from inside a bundle worktree without
  it. With several open bundles, root commands without `--bundle` are refused.
- Plain `git` works inside any worktree checkout for inspection (log, diff, status).
- Land through `knit land`; do not merge Knit-owned reviews with `gh pr merge` or by hand.
- Refresh this guide with `knit agents`. For anything else, use `knit <command> --help`.

{agent_teamwork_section}<!-- END KNIT AGENTS -->
"#,
        agent_teamwork_section = agent_teamwork_section("## Agent Teamwork")
    )
}
