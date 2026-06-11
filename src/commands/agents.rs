use crate::checkout::{checkout_dir, checkout_display_path, is_in_place};
use crate::git::git_output;
use crate::model::{KnitProject, RepoEntry};
use crate::output as out;
use crate::store::ActiveBundle;
use crate::store::read_json;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const KNIT_AGENTS_BEGIN: &str = "<!-- BEGIN KNIT AGENTS -->";
const KNIT_AGENTS_END: &str = "<!-- END KNIT AGENTS -->";

pub(crate) fn write_bundle_worktree_agents_md(active: &ActiveBundle) -> Result<Option<PathBuf>> {
    if active.bundle.repos.is_empty() {
        return Ok(None);
    }

    let bundle_root = active.root.join(".knit/worktrees").join(&active.bundle.id);
    if !bundle_root.exists() {
        return Ok(None);
    }

    let path = bundle_root.join("AGENTS.md");
    let section = bundle_worktree_agents_section(active, &bundle_root);
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        upsert_managed_section(&existing, &section)
    } else {
        format!("# AGENTS.md\n\n{section}")
    };
    fs::write(&path, next).with_context(|| {
        format!(
            "failed to write Knit bundle worktree guidance at {}",
            path.display()
        )
    })?;
    Ok(Some(path))
}

pub(crate) fn write_worktree_agents_md(active: &ActiveBundle) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for repo in &active.bundle.repos {
        if is_in_place(repo) {
            continue;
        }
        let Some(checkout) = checkout_dir(active, repo) else {
            continue;
        };
        let path = checkout.join("AGENTS.md");
        let section = worktree_agents_section(active, repo, &checkout);
        let next = if path.exists() {
            let existing = fs::read_to_string(&path)
                .with_context(|| format!("failed to read existing {}", path.display()))?;
            upsert_managed_section(&existing, &section)
        } else {
            format!("# AGENTS.md\n\n{section}")
        };
        fs::write(&path, next).with_context(|| {
            format!(
                "failed to write Knit worktree guidance at {}",
                path.display()
            )
        })?;
        exclude_worktree_agents(&checkout)?;
        paths.push(path);
    }
    Ok(paths)
}

pub(crate) fn print_bundle_worktree_agents_summary(path: Option<&Path>) {
    if let Some(path) = path {
        println!(
            "{} {}",
            out::heading("Bundle AGENTS.md:"),
            out::path(path.display())
        );
    }
}

pub(crate) fn print_worktree_agents_summary(paths: &[PathBuf]) {
    if !paths.is_empty() {
        println!(
            "{} {} repo worktree(s)",
            out::heading("Worktree AGENTS.md:"),
            paths.len()
        );
    }
}

fn bundle_worktree_agents_section(active: &ActiveBundle, bundle_root: &Path) -> String {
    let bundle_root_display = bundle_root
        .strip_prefix(&active.root)
        .unwrap_or(bundle_root)
        .display()
        .to_string();
    let checkouts = active
        .bundle
        .repos
        .iter()
        .map(|repo| format!("- `{}`: `{}`", repo.id, checkout_display_path(repo)))
        .collect::<Vec<_>>()
        .join("\n");
    let runtime_section = worktree_runtime_section(active);

    format!(
        r#"<!-- BEGIN KNIT AGENTS -->
## Knit Bundle Worktree Guide

This directory is the generated worktree root for Knit bundle `{bundle}`.

```txt
{bundle_root}
```

Bundle-scoped Knit commands resolve this bundle automatically from this cwd:

```sh
knit status
knit add
knit commit --all -m "Describe the feature change"
knit push --set-upstream
```

Before editing a path that may have cross-repo coupling, ask Knit which prior bundle work touched it:

```sh
knit related --repo <repo-id> path/inside/repo
knit related --repo <repo-id> path/inside/repo --pull
```

Knit uses Git history to find commits for the path, then expands matching Knit history into the related bundle, commit group, and companion repo commits. Inspect the printed `git show --stat` commands before changing risky areas.
{runtime_section}
For repo-local file reads, edits, tests, and git commands, make the specific repo checkout the actual cwd/workdir.

Tracked checkouts for this bundle:

{checkouts}

Do not edit the original source checkout for feature work unless the bundle was created with `--in-place`.
<!-- END KNIT AGENTS -->
"#,
        bundle = active.bundle.id,
        bundle_root = bundle_root_display,
        runtime_section = runtime_section,
        checkouts = if checkouts.is_empty() {
            "(none)".to_string()
        } else {
            checkouts
        }
    )
}

pub(crate) fn write_project_agents_md(root: &Path, project: &KnitProject) -> Result<PathBuf> {
    let path = root.join("AGENTS.md");
    let section = project_agents_section(project);
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        upsert_project_agents_section(&existing, project, &section)
    } else {
        format!("# AGENTS.md\n\n{section}")
    };

    fs::write(&path, next).with_context(|| {
        format!(
            "failed to write Knit project guidance at {}",
            path.display()
        )
    })?;
    Ok(path)
}

pub(crate) fn upsert_managed_section(existing: &str, section: &str) -> String {
    if let Some(next) =
        upsert_between_markers(existing, KNIT_AGENTS_BEGIN, KNIT_AGENTS_END, section)
    {
        return next;
    }

    append_section(existing, section)
}

fn upsert_project_agents_section(existing: &str, project: &KnitProject, section: &str) -> String {
    let begin = project_agents_begin(&project.id);
    let end = project_agents_end(&project.id);
    if let Some(next) = upsert_between_markers(existing, &begin, &end, section) {
        return next;
    }

    if let Some((start, end)) = legacy_project_section_range(existing, project) {
        let mut next = String::new();
        next.push_str(existing[..start].trim_end());
        if !next.is_empty() {
            next.push_str("\n\n");
        }
        push_section_and_suffix(&mut next, section, &existing[end..]);
        return ensure_trailing_newline(next);
    }

    append_section(existing, section)
}

fn upsert_between_markers(
    existing: &str,
    begin: &str,
    end_marker: &str,
    section: &str,
) -> Option<String> {
    let start = existing.find(begin)?;
    let end_offset = existing[start..].find(end_marker)?;
    let end = start + end_offset + end_marker.len();
    let mut next = String::new();
    next.push_str(&existing[..start]);
    push_section_and_suffix(&mut next, section, &existing[end..]);
    Some(ensure_trailing_newline(next))
}

fn push_section_and_suffix(next: &mut String, section: &str, suffix: &str) {
    next.push_str(section.trim_end());
    if !suffix.is_empty() && !suffix.starts_with('\n') {
        next.push('\n');
    }
    next.push_str(suffix);
}

fn append_section(existing: &str, section: &str) -> String {
    let mut next = existing.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(section);
    ensure_trailing_newline(next)
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn exclude_worktree_agents(checkout: &Path) -> Result<()> {
    let exclude_path = git_output(checkout, ["rev-parse", "--git-path", "info/exclude"])
        .context("failed to locate git exclude file")?;
    let exclude_path = resolve_checkout_path(checkout, exclude_path.trim());
    if let Some(parent) = exclude_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create git exclude parent {}", parent.display()))?;
    }
    let mut text = if exclude_path.exists() {
        fs::read_to_string(&exclude_path)
            .with_context(|| format!("failed to read {}", exclude_path.display()))?
    } else {
        String::new()
    };
    if !text.lines().any(|line| line.trim() == "AGENTS.md") {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("AGENTS.md\n");
        fs::write(&exclude_path, text)
            .with_context(|| format!("failed to write {}", exclude_path.display()))?;
    }
    Ok(())
}

fn resolve_checkout_path(checkout: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        checkout.join(path)
    }
}

fn worktree_agents_section(active: &ActiveBundle, repo: &RepoEntry, checkout: &Path) -> String {
    let checkout_display = checkout
        .strip_prefix(&active.root)
        .unwrap_or(checkout)
        .display()
        .to_string();
    let repo_worktrees = active
        .bundle
        .repos
        .iter()
        .filter(|repo| !is_in_place(repo))
        .filter_map(|repo| {
            repo.worktree_path
                .as_ref()
                .map(|path| (repo.id.as_str(), path))
        })
        .map(|(repo_id, path)| format!("- `{repo_id}`: `{path}`"))
        .collect::<Vec<_>>()
        .join("\n");
    let runtime_section = worktree_runtime_section(active);

    format!(
        r#"<!-- BEGIN KNIT AGENTS -->
## Knit Worktree Guide

This checkout belongs to Knit bundle `{bundle}` and repo `{repo}`.

```txt
{checkout}
```

Make this folder the actual cwd/workdir for repo-local tool calls. Because this cwd is inside the generated worktree, bundle-scoped Knit commands resolve this bundle automatically:

```sh
knit status
knit add
knit commit --all -m "Describe the feature change"
knit push --set-upstream
```

Before editing a path that may have cross-repo coupling, ask Knit which prior bundle work touched it:

```sh
knit related path/inside/repo
knit related --repo <repo-id> path/inside/repo
knit related --repo <repo-id> path/inside/repo --pull
```

Knit uses Git history to find commits for the path, then expands matching Knit history into the related bundle, commit group, and companion repo commits. Inspect the printed `git show --stat` commands before changing risky areas.
{runtime_section}
Sibling worktrees for this bundle:

{repo_worktrees}

Do not edit the original source checkout for feature work unless the bundle was created with `--in-place`.
<!-- END KNIT AGENTS -->
"#,
        bundle = active.bundle.id,
        repo = repo.id,
        checkout = checkout_display,
        runtime_section = runtime_section,
        repo_worktrees = if repo_worktrees.is_empty() {
            "(none)".to_string()
        } else {
            repo_worktrees
        }
    )
}

fn worktree_runtime_section(active: &ActiveBundle) -> String {
    let Some(project) = load_bundle_project(active) else {
        return String::new();
    };
    let Some(runtime) = &project.runtime else {
        return String::new();
    };
    let stack_repo = runtime.stack_repo.as_deref().unwrap_or("<stack-repo>");
    if active.bundle.repos.iter().all(|repo| repo.id != stack_repo) {
        return String::new();
    }

    format!(
        r#"
When the project defines a bundle runtime, start the bundle stack from a stack worktree checkout such as `{stack_repo}`:

```sh
knit run up
knit run status
knit run down
```

`knit run up` lifts the stack repo's compose shape into an isolated instance: bundle worktrees substituted for source paths, free host ports allocated, run as compose project `knit-run-{bundle}`. A compose file named `docker-compose.knit.yml` or referencing `${{KNIT_*}}` variables is instead run as-is with Knit's environment contract injected. Run state lands in `.knit/runtime-runs/{bundle}/state.json` after a successful start; `knit run down` cleans up by compose project label even when an `up` failed partway. Use `knit run status` for the live URLs; do not guess ports from an older run.

"#
        ,
        stack_repo = stack_repo,
        bundle = active.bundle.id,
    )
}

fn load_bundle_project(active: &ActiveBundle) -> Option<KnitProject> {
    let project_id = active.bundle.project_id.as_deref()?;
    let path = active.root.join(".knit/projects").join(format!("{project_id}.project.json"));
    read_json(&path).ok()
}

fn project_runtime_agents_section(project: &KnitProject) -> String {
    let Some(runtime) = &project.runtime else {
        return String::new();
    };

    let stack_repo = runtime.stack_repo.as_deref().unwrap_or("<stack-repo>");
    let compose_file = runtime
        .compose_file
        .clone()
        .unwrap_or_else(|| "docker-compose.knit.yml or docker-compose.yml".to_string());
    let profile_path = runtime.profile_path.as_deref().unwrap_or("/");
    let config_file = &runtime.project_config_file;
    let database = runtime.database.clone().unwrap_or_default();
    let database_mode = database.mode;
    let database_detail = if database_mode == crate::model::DatabaseMode::Bundle {
        format!(
            "mode `{database_mode}` with database `{name}` on host port `{port}` (started by the bundle runtime compose file)",
            name = database
                .name_template
                .as_deref()
                .unwrap_or("app_{bundleId}"),
            port = database.port_base.unwrap_or(5437),
        )
    } else {
        format!(
            "mode `{database_mode}` using `{name}` on localhost:`{port}` (must already be running before `knit run up`)",
            name = database.name,
            port = database.port,
        )
    };
    let ports = runtime.ports.clone().unwrap_or_default();

    format!(
        r#"
### Bundle runtime

This project defines a native Knit bundle runtime. The committed source of truth lives in `{config_file}` inside `{stack_repo}`; sync it into the workspace with:

```sh
knit project pull --repo {stack_repo}
knit project agents
```

From a bundle worktree that includes `{stack_repo}`:

```sh
knit run up
knit run status
knit run down
```

Runtime behavior:

- Runs `{compose_file}` from the `{stack_repo}` checkout as isolated compose project `knit-run-<bundle>`; run state is recorded in `.knit/runtime-runs/<bundle>/state.json` after a successful start (`knit run down` cleans up by project label even without it)
- A plain compose file is lifted automatically: the shape the repos run on `main`, with paths into tracked repos remapped to bundle worktrees and published host ports reallocated; a compose file named `docker-compose.knit.yml` or referencing `${{KNIT_*}}` variables is instead run as-is with Knit's environment contract injected (`KNIT_CHECKOUT_<repo>`, `KNIT_REV_<repo>`, `KNIT_PORT_<service>`, `KNIT_DB_*`); `runtime.mode` in the project config forces a mode
- Builds the stack from bundle worktrees, not the source checkout on `main`
- Allocates free host ports (contract mode pools: {port_pools}, step `{step}`)
- A project command configured as `up`, `down`, or `status` takes precedence over these runtime verbs
- Database (contract mode): {database_detail}
- Opens `{profile_path}` on the frontend port after `knit run status`

Shared database mode attaches bundle stacks to an existing dev database. Bundle database mode activates the compose file's `bundle-db` profile so a dedicated database container starts per runtime with its own empty database.

"#
        ,
        config_file = config_file,
        stack_repo = stack_repo,
        compose_file = compose_file,
        profile_path = profile_path,
        port_pools = ports
            .service_bases()
            .iter()
            .map(|(service, base)| format!("{service} `{base}`"))
            .collect::<Vec<_>>()
            .join(", "),
        step = ports.step,
        database_detail = database_detail,
    )
}

fn project_agents_section(project: &KnitProject) -> String {
    let begin = project_agents_begin(&project.id);
    let end = project_agents_end(&project.id);
    let default_repos = project
        .repos
        .iter()
        .filter(|repo| repo.include_by_default)
        .map(|repo| format!("- `{}`", repo.id))
        .collect::<Vec<_>>();
    let observed_repos = project
        .repos
        .iter()
        .filter(|repo| !repo.include_by_default)
        .map(|repo| format!("- `{}`", repo.id))
        .collect::<Vec<_>>();

    let default_repos = if default_repos.is_empty() {
        "- (none)".to_string()
    } else {
        default_repos.join("\n")
    };
    let observed_section = if observed_repos.is_empty() {
        String::new()
    } else {
        format!(
            "\nObserved repos are available by id but are not included in default bundle starts:\n\n{}\n",
            observed_repos.join("\n")
        )
    };
    let landing_section = project_landing_agents_section(project);
    let runtime_section = project_runtime_agents_section(project);

    format!(
        r#"{begin}
## Knit Project: {project_id}

This workspace has a reusable Knit project named `{project_id}`.

Use Knit as the source of truth for repo ids, paths, bases, and default/observed status:

```sh
knit project show {project_id}
```

Start most new `{project_id}` work with:

```sh
knit bundle "feature title" --project {project_id}
```

That command adds these default repos from the project data:

{default_repos}
{observed_section}
For narrower or unusual work, inspect the project first and then choose repo ids deliberately:

```sh
knit project show {project_id}
knit bundle "feature title" --project {project_id} --repo <repo-id>
```

Before changing a file or subsystem, use project history to find Knit-managed work that previously touched it and see any cross-repo companion commits:

```sh
knit related --repo <repo-id> path/inside/repo
knit related <repo-id>/path/inside/repo
knit related --repo <repo-id> path/inside/repo --pull
```

Use `--pull` when you want to refresh the local history ledger from KnitHub first. The command joins Git's file history with Knit history; Git remains the source of truth for file diffs, and Knit supplies the bundle/commit-group context.
{runtime_section}{landing_section}
{end}
"#,
        begin = begin,
        end = end,
        project_id = project.id,
        default_repos = default_repos,
        observed_section = observed_section,
        runtime_section = runtime_section,
        landing_section = landing_section
    )
}

fn project_landing_agents_section(project: &KnitProject) -> String {
    let Some(landing) = &project.landing else {
        return String::new();
    };

    let merge_order = if landing.merge.repo_order.is_empty() {
        "(bundle repo order)".to_string()
    } else {
        landing
            .merge
            .repo_order
            .iter()
            .map(|repo| format!("- `{repo}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let merge_method = landing.merge.method.unwrap_or_default();
    let required_checks = landing.merge.required_checks_only.unwrap_or(true);
    let deployments = if landing.deployments.is_empty() {
        "- (none)".to_string()
    } else {
        landing
            .deployments
            .iter()
            .map(|deployment| {
                let repo = deployment
                    .repo_id
                    .as_deref()
                    .map(|repo| format!(" repo `{repo}`"))
                    .unwrap_or_default();
                let mode = deployment.mode.unwrap_or(if deployment.command.is_empty() {
                    crate::model::DeployMode::Push
                } else {
                    crate::model::DeployMode::Command
                });
                let checkout = deployment
                    .checkout
                    .as_ref()
                    .map(|checkout| {
                        let remote = checkout.remote.as_deref().unwrap_or("origin");
                        let update = checkout.update.unwrap_or_default();
                        format!(" from `{remote}/{}` with `{update}`", checkout.branch)
                    })
                    .unwrap_or_default();
                let command = if deployment.command.is_empty() {
                    String::new()
                } else {
                    format!(": `{}`", deployment.command.join(" "))
                };
                format!(
                    "- `{id}`{repo} uses `{mode}`{checkout}{command}",
                    id = deployment.id
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"
This project defines a default landing template. `knit land` expands it into `.knit/land-plans/<bundle>.land.json`; inspect or edit that per-bundle plan before `knit land apply`.

Configured landing merge order:

{merge_order}

Merge defaults: method `{merge_method}`, required checks only `{required_checks}`.

Configured deployment steps:

{deployments}

Do not use `gh pr merge` for Knit-owned bundles. Use `knit land`, then `knit land apply` after reviewing the generated plan.
"#
    )
}

fn project_agents_begin(project_id: &str) -> String {
    format!("<!-- BEGIN KNIT PROJECT AGENTS: {project_id} -->")
}

fn project_agents_end(project_id: &str) -> String {
    format!("<!-- END KNIT PROJECT AGENTS: {project_id} -->")
}

fn legacy_project_section_range(existing: &str, project: &KnitProject) -> Option<(usize, usize)> {
    let heading = format!("## {} Knit Project", humanize_project_id(&project.id));
    let start = existing.find(&heading)?;
    let search_start = start + heading.len();
    let end = ["\n<!-- BEGIN ", "\n## "]
        .iter()
        .filter_map(|marker| {
            existing[search_start..]
                .find(marker)
                .map(|offset| search_start + offset)
        })
        .min()
        .unwrap_or(existing.len());
    Some((start, end))
}

fn humanize_project_id(project_id: &str) -> String {
    project_id
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
