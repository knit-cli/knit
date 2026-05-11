use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig};
use crate::output as out;
use crate::store::{
    bundle_path as stored_bundle_path, find_knit_root, load_config, save_active_bundle,
    save_config, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

const KNIT_AGENTS_BEGIN: &str = "<!-- BEGIN KNIT AGENTS -->";
const KNIT_AGENTS_END: &str = "<!-- END KNIT AGENTS -->";

pub fn init_bundle(title: &str, force: bool, agents: bool) -> Result<()> {
    start_bundle(title, None, &[], false, true, false, force, agents)
}

pub fn start_bundle(
    title: &str,
    project: Option<&str>,
    repo_ids: &[String],
    all_repos: bool,
    materialize: bool,
    in_place: bool,
    force: bool,
    agents: bool,
) -> Result<()> {
    if all_repos && !repo_ids.is_empty() {
        bail!("Use either --all-repos or --repo, not both.");
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).unwrap_or(cwd);
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let bundle_path = stored_bundle_path(&root, &bundle_id);

    if bundle_path.exists() && !force {
        if agents {
            let agents_path = write_agents_md(&root)?;
            println!(
                "{} {}",
                out::heading("AGENTS.md:"),
                out::path(agents_path.display())
            );
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
        let selected = select_project_repos(&root, project_id, repo_ids, all_repos)?;
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

    Ok(())
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
) -> Result<Vec<crate::model::ProjectRepoEntry>> {
    let project = crate::commands::project::load_project_by_id(root, project_id)?;
    if all_repos {
        return Ok(project.repos);
    }
    if repo_ids.is_empty() {
        return Ok(project
            .repos
            .into_iter()
            .filter(|repo| repo.include_by_default)
            .collect());
    }

    let mut selected = Vec::new();
    for repo_id in repo_ids {
        let repo_id = slugify(repo_id);
        let Some(repo) = project.repos.iter().find(|repo| repo.id == repo_id) else {
            bail!(
                "Project {} has no repo named {}.",
                out::repo(project_id),
                out::repo(&repo_id)
            );
        };
        selected.push(repo.clone());
    }
    Ok(selected)
}

fn write_agents_md(root: &Path) -> Result<std::path::PathBuf> {
    let path = root.join("AGENTS.md");
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        upsert_agents_section(&existing)
    } else {
        format!("# AGENTS.md\n\n{}", agents_section())
    };

    fs::write(&path, next)
        .with_context(|| format!("failed to write Knit agent tutorial at {}", path.display()))?;
    Ok(path)
}

fn upsert_agents_section(existing: &str) -> String {
    if let Some(start) = existing.find(KNIT_AGENTS_BEGIN) {
        if let Some(end_offset) = existing[start..].find(KNIT_AGENTS_END) {
            let end = start + end_offset + KNIT_AGENTS_END.len();
            let mut next = String::new();
            next.push_str(&existing[..start]);
            next.push_str(agents_section().trim_end());
            next.push_str(&existing[end..]);
            return ensure_trailing_newline(next);
        }
    }

    let mut next = existing.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(agents_section());
    ensure_trailing_newline(next)
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
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
knit bundle start "feature title"
```

For ad-hoc bundles, start a bundle and add local repositories directly:

```sh
knit bundle start "feature title"
knit bundle add ../backend ../frontend ../scraper
```

For parallel work, use separate bundles. The same repo can appear in many bundles; each bundle gets its own `knit/<bundle>` branch and `.knit/worktrees/<bundle>/<repo>/` checkout:

```sh
knit bundle start "feature a" --repo backend
knit bundle start "feature b" --repo backend
```

Make code changes inside Knit checkouts, usually under:

```txt
.knit/worktrees/<bundle>/<repo>/
```

Inspect, stage, and commit cross-repo work:

```sh
knit diff
knit add
knit commit -m "Describe the feature change"
```

For a one-step stage and commit:

```sh
knit commit --stage -m "Describe the feature change"
```

Publish PRs against their intended GitHub base branch:

```sh
knit publish github create
knit publish github create --base release
knit publish github create --base backend=stable --base frontend=main
knit publish github status
```

After the PRs are approved, land through Knit:

```sh
knit land plan
knit land apply
knit land status
```

`knit land` lands each recorded PR into its GitHub PR base branch. Do not use `gh pr merge` for Knit-owned bundles. Do not use `knit merge --into main` as a substitute for PR landing unless the user explicitly asks for direct branch integration instead of PR landing.

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

## Useful Commands

- `knit bundle` shows the resolved bundle and where it came from.
- `knit bundle start "Feature title"` creates a bundle.
- `knit bundle add <repo-or-project-repo>` adds repos to the current bundle.
- `knit bundle compat <bundle> <bundle>` creates an ordinary compatibility bundle from source bundle repos.
- `knit bundle path` prints the resolved bundle file.
- `knit bundle validate` checks the bundle artifact.
- `knit bundle list` shows workspace bundles.
- `knit bundle archive <bundle>` marks completed bundle artifacts as archived.
- `knit bundle restore <bundle>` makes an archived bundle available again.
- `knit bundle delete <bundle> --force` moves the bundle artifact to `.knit/deleted/bundles/` and preserves git state.
- `knit bundle delete <bundle> --force --worktrees --branches --force-branches` discards generated worktrees and local feature branches for that bundle.
- `knit bundle switch <bundle>` changes the workspace or folder fallback bundle.
- `knit merge <bundle> --into <branch-or-bundle>` merges a bundle into a local target with rollback by default.
- `knit merge <bundle> --into <branch> --fetch --push` refreshes and pushes branch targets after all local merges succeed.
- `knit merge status` and `knit merge show` inspect recorded merge runs.
- `knit merge <bundle> --into <branch-or-bundle> --manual` leaves conflicts for manual resolution, followed by `knit merge --continue` or `knit merge --abort`.
- `knit land plan` and `knit land apply` are the post-approval PR landing path.
- `knit doctor` checks workspace JSON, stale locks, and missing paths.
- `knit migrate --check` reports additive JSON migrations; `knit migrate` applies them.
- `knit config set advice false` disables sparse `Next:` advice.
- `knit switch <bundle>` is the short alias for bundle switching.
- `knit show HEAD` explains the latest bundle ledger entry.
- `knit sync` records Git commits made outside Knit.
- `knit git --all status --short` runs Git across tracked checkouts.
- `knit checkpoint "note"` records non-Git progress in the bundle ledger.
- `knit close --reason "merged"` marks the bundle closed without deleting branches or worktrees.

Knit resolves bundle context from `--bundle`, then `KNIT_BUNDLE`, then generated worktree cwd, then folder context, then workspace fallback. Inside `.knit/worktrees/<bundle>/<repo>/`, agents do not need to run `knit switch`.

Aliases such as `knit init "feature title"` and `knit track ../backend` are also supported.

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
