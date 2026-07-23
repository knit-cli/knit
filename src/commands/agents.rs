use crate::checkout::checkout_display_path;
use crate::model::KnitProject;
use crate::output as out;
use crate::store::read_json;
use crate::store::ActiveBundle;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const KNIT_AGENTS_BEGIN: &str = "<!-- BEGIN KNIT AGENTS -->";
const KNIT_AGENTS_END: &str = "<!-- END KNIT AGENTS -->";
const AGENT_TEAMWORK_SENTINEL: &str = "If the harness provides subagents or agent teams";

pub(crate) fn agent_teamwork_section(heading: &str) -> String {
    format!(
        r#"{heading}

{sentinel}, use them deliberately:

- The main agent keeps architect ownership: understand the issue, define boundaries, identify cross-repo interfaces, and prepare enough context that delegated agents can work without hidden assumptions.
- For multi-repo work, split tasks by repo or by clearly stated interface boundary. Include the expected contract, files, commands, and acceptance checks in each handoff.
- Delegate only the smallest independent task to the minimum capable subagent/model the harness supports. Do not spawn broad or speculative parallel work.
- When subagents report back, the main agent integrates the work, runs the normal Knit/git/project tests, and fixes or redirects the procedure if verification fails.
- If subagents are not available, proceed as a single agent while preserving the same boundary-setting and verification discipline.

"#,
        sentinel = AGENT_TEAMWORK_SENTINEL
    )
}

/// Generated bundle guidance is written only here, at the bundle worktree
/// root (a parent of every repo checkout), never inside a repo checkout:
/// repos that track their own AGENTS.md would commit the bundle-specific
/// section and conflict on every publish, and `.git/info/exclude` cannot
/// hide changes to tracked files.
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

pub(crate) fn print_bundle_worktree_agents_summary(path: Option<&Path>) {
    if let Some(path) = path {
        crate::human!(
            "{} {}",
            out::heading("Bundle AGENTS.md:"),
            out::path(path.display())
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
    let checks_section = worktree_checks_section(active);
    let teamwork_section = agent_teamwork_section("## Agent Teamwork");

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

{teamwork_section}
Before editing a path that may have cross-repo coupling, ask Knit which prior bundle work touched it:

```sh
knit related --repo <repo-id> path/inside/repo
knit related --repo <repo-id> path/inside/repo --pull
```

Knit uses Git history to find commits for the path, then expands matching Knit history into the related bundle, commit group, and companion repo commits. Inspect the printed `git show --stat` commands before changing risky areas.
{runtime_section}{checks_section}For repo-local file reads, edits, tests, and git commands, make the specific repo checkout the actual cwd/workdir.

Tracked checkouts for this bundle:

{checkouts}

Do not edit the original source checkout for feature work unless the bundle was created with `--in-place`.
<!-- END KNIT AGENTS -->
"#,
        bundle = active.bundle.id,
        bundle_root = bundle_root_display,
        teamwork_section = teamwork_section,
        runtime_section = runtime_section,
        checks_section = checks_section,
        checkouts = if checkouts.is_empty() {
            "(none)".to_string()
        } else {
            checkouts
        }
    )
}

pub(crate) fn write_project_agents_md(root: &Path, project: &KnitProject) -> Result<PathBuf> {
    let path = root.join("AGENTS.md");
    let next = if path.exists() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("failed to read existing {}", path.display()))?;
        let include_teamwork =
            !existing_outside_project_section(&existing, project).contains(AGENT_TEAMWORK_SENTINEL);
        let section = project_agents_section(project, include_teamwork);
        upsert_project_agents_section(&existing, project, &section)
    } else {
        let section = project_agents_section(project, true);
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

fn existing_outside_project_section(existing: &str, project: &KnitProject) -> String {
    let begin = project_agents_begin(&project.id);
    let end_marker = project_agents_end(&project.id);
    if let Some(start) = existing.find(&begin) {
        if let Some(end_offset) = existing[start..].find(&end_marker) {
            let end = start + end_offset + end_marker.len();
            let mut next = String::new();
            next.push_str(&existing[..start]);
            next.push_str(&existing[end..]);
            return next;
        }
    }

    if let Some((start, end)) = legacy_project_section_range(existing, project) {
        let mut next = String::new();
        next.push_str(&existing[..start]);
        next.push_str(&existing[end..]);
        return next;
    }

    existing.to_string()
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

`knit run up` lifts EVERY bundle repo with a compose file into an isolated instance: bundle worktrees substituted for source paths, stable per-bundle host ports allocated, references to sibling stacks' published ports rewired to the bundle instances. Repeated `up` calls reuse the recorded ports, including while the bundle is already running. One stack runs as compose project `knit-run-{bundle}`; several run as `knit-run-{bundle}--<repo>` each. A compose file named `docker-compose.knit.yml` or referencing `${{KNIT_*}}` variables is instead run as-is with Knit's environment contract injected. Run state lands in `.knit/runtime-runs/{bundle}/state.json` after a successful start; `knit run down` cleans up containers by compose project label even when an `up` failed partway, while `knit run down --purge` also removes bundle-owned volumes and local build images. Landing/archive cleanup purges automatically when it disposes the worktrees. Use `knit run status` for the live URLs.

If `knit run up` fails or the lifted stack misbehaves, run `knit run eject`: it writes the lift as an editable `docker-compose.knit.yml` in the stack repo checkout, parameterized over the `KNIT_*` contract (documented in the file's header). Fix that file — it is ordinary docker compose with `${{VAR:-default}}` interpolations — and commit it with the bundle; `knit run up` then runs it as-is, and the automatic lift no longer applies to that repo. Do not work around a bad lift by hand-running docker compose.

"#,
        stack_repo = stack_repo,
        bundle = active.bundle.id,
    )
}

/// Guidance shown in worktree AGENTS.md when the project requires checks for
/// landing: agents should refresh them after the last commit.
fn worktree_checks_section(active: &ActiveBundle) -> String {
    let Some(project) = load_bundle_project(active) else {
        return String::new();
    };
    let names = project
        .landing
        .map(|landing| landing.require_checks)
        .unwrap_or_default();
    if names.is_empty() {
        return String::new();
    }
    let joined = names.join("`, `knit check run `");
    format!(
        r#"
This project requires green checks before landing: after your final commit, run `knit check run {joined}` and confirm `knit check status` reports every required check green and fresh. Verdicts are pinned to the current commits, so re-run after any new commit. `knit land apply` refuses to execute while a required check is missing, red, or stale.

"#,
        joined = joined
    )
}

fn load_bundle_project(active: &ActiveBundle) -> Option<KnitProject> {
    let project_id = active.bundle.project_id.as_deref()?;
    let path = active
        .root
        .join(".knit/projects")
        .join(format!("{project_id}.project.json"));
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

- Runs `{compose_file}` from the `{stack_repo}` checkout as isolated compose project `knit-run-<bundle>`; run state is recorded in `.knit/runtime-runs/<bundle>/state.json` after a successful start (`knit run down` cleans up containers by project label even without it; `knit run down --purge` also removes bundle-owned volumes and local build images, and landing/archive cleanup purges automatically when disposing worktrees)
- A plain compose file is lifted automatically: the shape the repos run on `main`, with paths into tracked repos remapped to bundle worktrees and published host ports reallocated; a compose file named `docker-compose.knit.yml` or referencing `${{KNIT_*}}` variables is instead run as-is with Knit's environment contract injected (`KNIT_CHECKOUT_<repo>`, `KNIT_REV_<repo>`, `KNIT_PORT_<service>`, `KNIT_DB_*`); `runtime.mode` in the project config forces a mode
- Builds the stack from bundle worktrees, not the source checkout on `main`
- Allocates stable per-bundle host ports, reusing recorded ports on repeated `up` calls (contract mode pools: {port_pools}, step `{step}`)
- A project command configured as `up`, `down`, or `status` takes precedence over these runtime verbs
- Database (contract mode): {database_detail}
- Opens `{profile_path}` on the frontend port after `knit run status`
- If the automatic lift mishandles a stack, `knit run eject` materializes it as an editable `docker-compose.knit.yml` in the stack repo (contract mode from then on); edit that file and commit it instead of working around the runtime

Shared database mode attaches bundle stacks to an existing dev database. Bundle database mode activates the compose file's `bundle-db` profile so a dedicated database container starts per runtime with its own empty database.

"#,
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

fn project_agents_section(project: &KnitProject, include_teamwork: bool) -> String {
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
    let teamwork_section = if include_teamwork {
        agent_teamwork_section("### Agent Teamwork")
    } else {
        String::new()
    };

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

Use `--pull` when you want to refresh the local history ledger from the sync remote first. The command joins Git's file history with Knit history; Git remains the source of truth for file diffs, and Knit supplies the bundle/commit-group context.
{teamwork_section}{runtime_section}{landing_section}
{end}
"#,
        begin = begin,
        end = end,
        project_id = project.id,
        default_repos = default_repos,
        observed_section = observed_section,
        teamwork_section = teamwork_section,
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
    let require_checks_line = if landing.require_checks.is_empty() {
        String::new()
    } else {
        format!(
            "\nRequired bundle checks before landing: {} — refresh with `knit check run <name>` after the final commit; `knit land apply` refuses while any is missing, red, or stale.\n",
            landing
                .require_checks
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
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
                let timeout = if mode == crate::model::DeployMode::Command {
                    format!(
                        " (timeout: {}s)",
                        deployment
                            .timeout_seconds
                            .unwrap_or(crate::commands::land::DEFAULT_COMMAND_TIMEOUT_SECONDS)
                    )
                } else {
                    String::new()
                };
                format!(
                    "- `{id}`{repo} uses `{mode}`{checkout}{command}{timeout}",
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
{require_checks_line}
Configured deployment steps:

{deployments}

Do not use `gh pr merge` for Knit-owned bundles. Use `knit land`, then `knit land apply` after reviewing the generated plan. A successful apply archives the bundle and removes generated worktrees unless `--keep-worktrees` is passed.
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
