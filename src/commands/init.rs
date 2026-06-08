use crate::checkout::checkout_dir;
use crate::commands::agents::{
    print_bundle_worktree_agents_summary, print_worktree_agents_summary, upsert_managed_section,
    write_bundle_worktree_agents_md, write_worktree_agents_md,
};
use crate::ids::slugify;
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
        title, None, &[], false, None, &[], &[], true, false, force, agents, None,
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
    if bundle_path.exists() && !force {
        if agents && cd.is_none() {
            let bundle: ChangeGroup = read_json(&bundle_path)?;
            let active = ActiveBundle::unlocked(root.clone(), bundle_path.clone(), bundle);
            let agents_path = write_agents_md(&root)?;
            let bundle_agents = write_bundle_worktree_agents_md(&active)?;
            let worktree_agents = write_worktree_agents_md(&active)?;
            println!(
                "{} {}",
                out::heading("AGENTS.md:"),
                out::path(agents_path.display())
            );
            print_bundle_worktree_agents_summary(bundle_agents.as_deref());
            print_worktree_agents_summary(&worktree_agents);
            return Ok(());
        }
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            bundle_path.display()
        );
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
    let mut bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now_iso());
    bundle.project_id = project_id.clone();
    write_json(&bundle_path, &bundle)?;

    config.active_bundle = Some(bundle_id.clone());
    if let Some(project_id) = &project_id {
        config.active_project = Some(project_id.clone());
    }
    save_config(&root, &config)?;

    if let Some(project_id) = &project_id {
        let selected =
            select_project_repos(&root, project_id, repo_ids, all_repos, view, include, exclude)?;
        if !selected.is_empty() {
            let mut active = ActiveBundle::unlocked(root.clone(), bundle_path.clone(), bundle);
            crate::commands::track::track_project_repos(
                &mut active,
                &selected,
                materialize,
                in_place,
            )?;
            save_active_bundle(&active)?;
        }
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
    let worktree_agents = write_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    print_worktree_agents_summary(&worktree_agents);

    if let Some(selector) = cd {
        let path = cd_target_dir(&active, selector)?;
        start_shell_in(&active, &path)?;
    }

    Ok(())
}

fn cd_target_dir(active: &ActiveBundle, selector: &str) -> Result<PathBuf> {
    if active.bundle.repos.is_empty() {
        bail!("Cannot cd into a checkout because the bundle has no tracked repos.");
    }

    if selector.trim().is_empty() {
        return Ok(active.root.join(".knit/worktrees").join(&active.bundle.id));
    }

    let selectors = [selector.to_string()];
    let indexes = crate::repo_selectors::resolve_repo_indexes(active, &selectors, false)?;
    if indexes.len() != 1 {
        bail!("Repo selector `{selector}` matched multiple repos; pass a more specific repo id.");
    }
    checkout_for_repo(active, indexes[0])
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

fn start_shell_in(active: &ActiveBundle, path: &Path) -> Result<()> {
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
    resolve_view_repos(
        &project,
        repo_ids,
        all_repos,
        view.as_ref(),
        include,
        exclude,
    )
}

/// Resolve which named view to apply: an explicit `--view` name (which must
/// exist), otherwise the user's saved default view, otherwise none.
pub(crate) fn resolve_active_view(
    root: &Path,
    project_id: &str,
    view_name: Option<&str>,
) -> Result<Option<ProjectView>> {
    let views = load_views(root, project_id)?;
    match view_name {
        Some(name) => {
            let name = slugify(name);
            let view = views.views.get(&name).cloned().with_context(|| {
                format!(
                    "Project {} has no saved view named {}. Create it with `knit view save {name}`.",
                    out::repo(project_id),
                    out::repo(&name)
                )
            })?;
            Ok(Some(view))
        }
        // A dangling default is ignored rather than blocking `bundle start`.
        None => Ok(views
            .default_view
            .as_ref()
            .and_then(|name| views.views.get(name).cloned())),
    }
}

/// Resolve a project's repo set for a bundle, applying (in order): the explicit
/// `--repo`/`--all-repos` set or the `includeByDefault` set plus the active
/// view's include/exclude deltas, then ad-hoc `include`/`exclude` overrides.
/// Results preserve project order and are de-duplicated.
pub(crate) fn resolve_view_repos(
    project: &KnitProject,
    repo_ids: &[String],
    all_repos: bool,
    view: Option<&ProjectView>,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<ProjectRepoEntry>> {
    use std::collections::BTreeSet;
    let mut selected: BTreeSet<String> = BTreeSet::new();

    if !repo_ids.is_empty() {
        for repo_id in repo_ids {
            selected.insert(project_repo(project, repo_id)?.id.clone());
        }
    } else if all_repos {
        for repo in &project.repos {
            selected.insert(repo.id.clone());
        }
    } else {
        for repo in &project.repos {
            if repo.include_by_default {
                selected.insert(repo.id.clone());
            }
        }
        if let Some(view) = view {
            for repo_id in &view.include {
                selected.insert(project_repo(project, repo_id)?.id.clone());
            }
            for repo_id in &view.exclude {
                selected.remove(&project_repo(project, repo_id)?.id);
            }
        }
    }

    // Ad-hoc flags apply on top in every mode, so `--all-repos --exclude sej`
    // and `--view backend --include gloss` both work.
    for repo_id in include {
        selected.insert(project_repo(project, repo_id)?.id.clone());
    }
    for repo_id in exclude {
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

fn write_agents_md(root: &Path) -> Result<std::path::PathBuf> {
    let path = root.join("AGENTS.md");
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        upsert_managed_section(&existing, agents_section())
    } else {
        format!("# AGENTS.md\n\n{}", agents_section())
    };

    fs::write(&path, next)
        .with_context(|| format!("failed to write Knit agent tutorial at {}", path.display()))?;
    Ok(path)
}

fn agents_section() -> &'static str {
    r#"<!-- BEGIN KNIT AGENTS -->
## Knit Workspace Guide

This is a Knit workspace. Knit coordinates feature work that spans one or more Git repositories and records the work in `.knit/bundles/<slug>.bundle.json`.

## Knit Workflow

Start by checking which bundle this folder resolves to:

```sh
knit bundle
knit status
knit log
```

Projects are reusable repo templates. Most ongoing work should start from a project:

```sh
knit project init my-project
knit project add backend ../backend
knit project add frontend ../frontend
knit project add docs ../docs --observe
knit project command set dev --repo frontend -- docker compose up
knit bundle "feature title"
```

A bundle is the cross-repo analogue of a git branch: `knit bundle "feature title"`
creates one (like `git branch <name>`), `knit bundle` alone shows the current one, and
`knit bundle start ... <flags>` is the long form when you need `--project`/`--repo`/`--view`/`--cd`.

For ad-hoc bundles, create a bundle and add local repositories directly:

```sh
knit bundle "feature title"
knit bundle add ../backend ../frontend ../scraper
```

For parallel work, use separate bundles. The same repo can appear in many bundles; each bundle gets its own `knit/<bundle>` branch and `.knit/worktrees/<bundle>/<repo>/` checkout:

```sh
knit bundle start "feature a" --repo backend
knit bundle start "feature b" --repo backend
```

Use `knit bundle start "feature title" --cd` to create the bundle from the current workspace project's default repos and immediately start your shell in `.knit/worktrees/<bundle>`. That bundle worktree root gets its own `AGENTS.md` with bundle-wide guidance. Pass `--project` when you want a project other than the current one, pass `--repo` only when you want to limit which repos are included, and pass a `--cd` value such as `--cd backend` only when you want a specific repo checkout instead.

Each user can save named views (bundle shapes) as include/exclude deltas over the project's default repo set, then start from them or reshape a live bundle. Views are per-user config under `.knit/views/<project>.views.json`:

```sh
knit view save backend --exclude frontend,docs
knit view default backend
knit bundle "feature title"                    # uses the default view
knit bundle start "feature title" --view frontend --include docs
knit bundle add docs                           # materialize a repo into the live bundle
knit bundle remove frontend                    # tear down its worktree
knit bundle apply-view backend                 # reshape the live bundle to a saved view
```

For coding agents in the source workspace, moving into a checkout means each shell/tool call must actually run with that checkout as its cwd/workdir. A narrated `cd`, or a `cd` from a previous non-persistent shell command, is not enough. If this agent is working on one feature, open the generated worktree folder and keep tool calls rooted there. If several agents or features are active, open a separate folder or agent rooted at each new worktree. From the source workspace, use explicit `--bundle <bundle>` on bundle-scoped Knit commands for the feature being changed:

```sh
knit --bundle feature-a status
knit --bundle feature-a add
knit --bundle feature-a commit --stage -m "Describe the feature change"
```

Do not use bare `knit switch <bundle>` from the workspace root to recover context. Root-level switching requires `--workspace` so changing the shared fallback is always deliberate.

When more than one open bundle exists, Knit refuses source-root status and mutating commands that would use the shared workspace fallback. Use `knit --bundle <bundle> ...` from the source workspace or run the command from the intended worktree.

Make code changes inside Knit checkouts, usually under:

```txt
.knit/worktrees/<bundle>/<repo>/
```

Inspect, stage, and commit cross-repo work:

```sh
knit --bundle feature-a diff
knit --bundle feature-a add
knit --bundle feature-a commit -m "Describe the feature change"
```

For a one-step stage and commit:

```sh
knit --bundle feature-a commit --stage -m "Describe the feature change"
```

Push the bundle's feature branches after committing:

```sh
knit --bundle feature-a push --set-upstream
```

Push the bundle to one or more KnitHub remotes so it appears in hosted dashboards:

```sh
knit --bundle feature-a push --remote local --remote knithub
```

Publish PRs against their intended GitHub base branch:

```sh
knit publish github create
knit publish github create --base release
knit publish github create --base backend=stable --base frontend=main
knit publish github status
```

Publish from a bundle artifact JSON (no local worktrees; branches must already exist on GitHub):

```sh
knit publish github create --from-artifact bundle.json --out bundle.published.json --no-push
knit publish github sync --from-artifact bundle.published.json --out bundle.published.json
```

When the PRs are approved and the user says to land, merge, release, ship, or continue after review, start landing through Knit:

```sh
knit land
```

Inspect or edit the plan, then execute it explicitly:

```sh
knit land check
knit land apply
knit land status
knit land sync
```

`knit land check` is a read-only preflight: it shows each recorded PR's live state, mergeability, checks, review decision, and a landing verdict, so you can tell whether `knit land apply` will succeed before running it. When it reports a `conflict`, run `knit land update` to merge the base in and resolve, then land again. `knit publish status --live` shows the same live columns.

Land from a bundle artifact JSON (merge-only, no local workspace):

```sh
knit land apply --from-artifact bundle.published.json --out bundle.landed.json
```

Bare `knit land` creates or shows the default plan and stops. It never merges PRs, deploys, waits, or runs plan commands. `knit land apply` executes the plan and lands each recorded PR into its GitHub PR base branch, then executes any generated or edited deployment steps. When push-sync is enabled, a successful land also syncs the updated bundle artifact to configured KnitHub remotes; use `knit land sync` to push the landed artifact later. Project JSON can define a default `landing` template with merge priority and deployments, while `.knit/land-plans/<bundle>.land.json` remains the editable per-bundle plan. A PR with no required checks has passed Knit’s required-check gate. Do not use `gh pr merge` for Knit-owned bundles. Do not use `knit merge --into main` as a substitute for PR landing unless the user explicitly asks for direct branch integration instead of PR landing.

Use `knit merge` for local integration into staging branches or compatibility bundles:

```sh
knit merge feature-a --into staging --fetch
knit bundle compat feature-a feature-b --title "feature a b compat"
knit merge feature-a --into feature-a-b-compat
knit merge feature-b --into feature-a-b-compat --manual
knit merge status
knit merge --continue
knit merge push
```

Use `knit bundle split` or `knit cherrypick` to move selected recorded commits out of a messy bundle and into a fresh one:

```sh
knit bundle split feature-a HEAD~1 --title "feature a clean follow-up"
knit cherrypick --from feature-a --repo backend abc123
```

## Useful Commands

- `knit bundle` shows the resolved bundle and where it came from.
- `knit bundle "Feature title"` creates a bundle (the git-branch-style shorthand).
- `knit bundle start "Feature title" --cd` is the long form that also accepts `--project`/`--repo`/`--view`/`--cd`.
- `knit bundle add <repo-or-project-repo>` adds repos to the current bundle and materializes their worktrees (`--no-worktree` to skip).
- `knit bundle remove <repo>...` removes repos from the current bundle and tears down their worktrees (`--keep-worktree` to only untrack, `--delete-branch` to also drop the feature branch, `--force` to discard dirty/unpushed work).
- `knit bundle apply-view <name>` reshapes the current bundle to match a saved view.
- `knit view save <name> [--include <repo>]... [--exclude <repo>]...` saves a per-user bundle shape; `knit view default <name>` makes it the default for `knit bundle start`.
- `knit view list`, `knit view show [name] [--repos]`, `knit view edit`, `knit view rm <name>` manage saved views; `knit view push`/`knit view pull` sync them to KnitHub.
- `knit bundle compat <bundle> <bundle>` creates an ordinary compatibility bundle from source bundle repos.
- `knit bundle split <bundle> <selector>...` creates a fresh bundle and cherry-picks selected source commits into it.
- `knit cherrypick --from <bundle> <selector>...` cherry-picks selected source bundle commits into the resolved bundle.
- `knit bundle path` prints the resolved bundle file.
- `knit bundle validate` checks the bundle artifact.
- `knit bundle list` shows workspace bundles.
- `knit bundle archive <bundle>` marks completed bundle artifacts as archived.
- `knit bundle restore <bundle>` makes an archived bundle available again.
- `knit bundle delete <bundle> --force` moves the bundle artifact to `.knit/deleted/bundles/` and preserves git state.
- `knit bundle delete <bundle> --force --worktrees --branches --force-branches` discards generated worktrees and local feature branches for that bundle.
- `knit bundle delete <bundle> --force --worktrees --branches --force-branches --remote-branches` also deletes the matching feature branches from `origin`.
- `knit bundle prune` refreshes GitHub PR states and lists clean dead-work bundles with no recorded open PRs.
- `knit bundle prune --no-refresh` performs the same scan using cached recorded PR states only.
- `knit bundle prune --apply --worktrees --branches` is the short form for deleting dead bundle artifacts and their generated local state.
- `knit bundle prune --apply --all` removes dead bundle artifacts, generated and orphaned worktrees, local feature branches, matching `origin` branches, and matching KnitHub remote bundle records.
- Remote bundle cleanup uses the configured KnitHub sync remote and requires a token with `bundle:delete`.
- `knit switch <bundle>` changes the workspace or folder fallback bundle (`--workspace`/`--here` to target one explicitly).
- `knit project remove <project> --force` removes a reusable project template artifact.
- `knit run <project-command>` runs a configured command inside the resolved bundle checkout.
- `knit run --repo <repo> -- <command>` runs a one-off command inside a tracked checkout.
- `knit merge <bundle> --into <branch-or-bundle>` merges a bundle into a local target with rollback by default.
- `knit merge <bundle> --into <branch> --fetch --push` refreshes and pushes branch targets after all local merges succeed.
- `knit merge status` and `knit merge show` inspect recorded merge runs.
- `knit merge <bundle> --into <branch-or-bundle> --manual` leaves conflicts for manual resolution, followed by `knit merge --continue` or `knit merge --abort`.
- `knit land` creates or shows the landing plan; `knit land apply` executes it.
- `knit land check` previews each recorded PR's live landing readiness (state, mergeable, checks, review, verdict) without mutating anything; `knit publish status --live` shows the same columns.
- `knit land sync` pushes the current landed bundle artifact to configured KnitHub remotes.
- `knit doctor` checks workspace JSON, stale locks, and missing paths.
- `knit migrate --check` reports additive JSON migrations; `knit migrate` applies them.
- `knit config set advice false` disables sparse `Next:` advice.
- `knit config set sync-remotes local,knithub` makes push-sync upload bundle artifacts to multiple KnitHub remotes.
- `knit show HEAD` explains the latest bundle ledger entry.
- `knit sync` records Git commits made outside Knit.
- `knit push --set-upstream` pushes every tracked feature branch in the resolved bundle to `origin` and sets upstream tracking.
- `knit push --remote local --remote knithub` pushes the resolved bundle to both configured KnitHub remotes so it is visible in hosted dashboards.
- `knit git --all status --short` runs Git across tracked checkouts.
- `knit checkpoint "note"` records non-Git progress in the bundle ledger.
- `knit bundle close --reason "merged"` marks the bundle closed without deleting branches or worktrees.
- `knit status` still shows a closed bundle's worktrees and branches while they remain on disk.
- `knit clean --closed --worktrees` removes generated worktrees for closed bundles while preserving local feature branches.

Knit resolves bundle context from `--bundle`, then `KNIT_BUNDLE`, then generated worktree cwd, then folder context, then workspace fallback. Inside `.knit/worktrees/<bundle>/<repo>/`, agents do not need to run `knit switch`.

## Knit And Gloss

Knit owns authoring: worktrees, feature branches, commits, sync, reverts, and the bundle ledger. Gloss reads Knit bundles later to prepare review plans, explanations, and UI views.

When using Gloss from this workspace, the active Knit bundle can usually be discovered automatically:

```sh
gloss prepare
gloss view
```
<!-- END KNIT AGENTS -->
"#
}
