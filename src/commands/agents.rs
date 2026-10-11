use crate::checkout::checkout_display_path;
use crate::model::{KnitProject, ProjectLandingDeployment};
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

{sentinel}, the main agent is the architect of a change:

- Work out which repositories must change and define the cross-repo contract yourself: names, payload shapes, formats, validation, edge cases, and which repository owns what.
- Delegate each repository's implementation to its own subagent with the contract, its worktree path, the relevant files, and how to test. Subagents work only inside their own checkout; the main agent does not write the code.
- Review every diff against the contract and against the other repositories: producers and consumers must agree on names, types, formats and edge cases, and repositories that implement the same contract must behave the same. Do not accept work because a subagent reports it done; send anything wrong, inconsistent or missing back with a precise description, and review again.
- Commit and publish only when the whole change is correct and the tests pass.
- Without subagents, work as a single agent with the same contract and review discipline.

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
knit publish create
```

`knit publish create` pushes every tracked feature branch and opens one review
(PR/MR) per repo, so committing and publishing is the whole path to review. Use
`knit push --set-upstream` only when you want the branches on `origin` without
reviews. `knit publish create --dry-run` shows each review's title, body and draft
choice; `PR-<repo>.md` or `PR.md` in this directory becomes the review body.

{teamwork_section}
Before editing a path that may have cross-repo coupling, ask Knit which prior bundle work touched it:

```sh
knit related --repo <repo-id> path/inside/repo
knit related --repo <repo-id> path/inside/repo --pull
```

Knit uses Git history to find commits for the path, then expands matching Knit history into the related bundle, commit group, and companion repo commits. Inspect the printed `git show --stat` commands before changing risky areas.

## Useful Commands

- `knit squash [-m <message>]` rewrites bundle work as one commit per repo and one group; publish with `knit push --force-with-lease`.
- `knit rebase [--squash] [--offline]` replays work onto upstream bases; use `--continue` after resolving conflicts or `--abort` to restore original heads.

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

`knit run up` lifts EVERY bundle repo with a compose file into an isolated instance: bundle worktrees substituted for source paths, stable per-bundle host ports allocated, references to sibling stacks' published ports rewired to the bundle instances. Repeated `up` calls reuse the recorded ports, including while the bundle is already running. Use `runtime.bindings` in the committed project config to identify a consumer's provider by repo/service when source ports overlap; Knit fills in the allocated port for each bundle. Shared database attachment respects database identity or explicit `database.repos`, and unrelated same-named database services remain isolated. All stacks launch before bounded startup verification; a crashed or unhealthy service makes `up` fail before success is recorded. One stack runs as compose project `knit-run-{bundle}`; several run as `knit-run-{bundle}--<repo>` each. A compose file named `docker-compose.knit.yml` or referencing `${{KNIT_*}}` variables is instead run as-is with Knit's environment contract injected. Run state lands in `.knit/runtime-runs/{bundle}/state.json` after a successful start; `knit run down` cleans up containers by compose project label even when an `up` failed partway, while `knit run down --purge` also removes bundle-owned volumes and local build images. Landing/archive cleanup purges automatically when it disposes the worktrees. Use `knit run status` for the live URLs.

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

From a bundle worktree that includes `{stack_repo}`, `knit run up`, `knit run status` and `knit run down` run
`{compose_file}` from that checkout as compose project `knit-run-<bundle>`, built from the bundle worktrees on
the bundle's own host ports (pools: {port_pools}, step `{step}`). Database: {database_detail}. `knit run status`
opens `{profile_path}` on the frontend port. The runtime config lives in `{config_file}` inside `{stack_repo}`
(`knit project pull --repo {stack_repo}` syncs it). If the automatic compose lift mishandles the stack, run
`knit run eject` and commit the generated `docker-compose.knit.yml` instead of working around it.

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
            "\nObserved repos (add one with `--repo <id>` or `knit bundle add <id>`):\n\n{}\n",
            observed_repos.join("\n")
        )
    };
    let draft_repos = project
        .repos
        .iter()
        .filter(|repo| repo.publishes_as_draft())
        .map(|repo| format!("- `{}`", repo.id))
        .collect::<Vec<_>>();
    let draft_section = if draft_repos.is_empty() {
        String::new()
    } else {
        format!(
            "\n`knit publish create` opens these repos' reviews as drafts (`knit project set-draft <repo> false` changes it):\n\n{}\n",
            draft_repos.join("\n")
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

`knit bundle "feature title"` starts from these default repos of project `{project_id}`:

{default_repos}
{observed_section}{draft_section}
`knit project show {project_id}` lists paths and base branches. `knit related <repo-id>/<path>` shows earlier
bundles that touched a file, with their companion commits in other repos.
{teamwork_section}{runtime_section}{landing_section}
{end}
"#,
        begin = begin,
        end = end,
        project_id = project.id,
        default_repos = default_repos,
        observed_section = observed_section,
        draft_section = draft_section,
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
            .map(landing_deployment_agents_line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let target_deployments = if landing.targets.is_empty() {
        "- (none)".to_string()
    } else {
        landing
            .targets
            .iter()
            .map(|(branch, target)| {
                let deployments = if target.deployments.is_empty() {
                    "  - (none)".to_string()
                } else {
                    target
                        .deployments
                        .iter()
                        .map(|deployment| {
                            format!("  {}", landing_deployment_agents_line(deployment))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                format!("- `{branch}`:\n{deployments}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"
### Landing

This project defines a default landing template. `knit land` writes the plan to `.knit/land-plans/<bundle>.land.json`;
review it, then run `knit land apply`. A failed apply stops; `knit land resume --run <path>` continues it (recovery:
`knit land --help`).

Configured landing merge order:

{merge_order}

Merge defaults: method `{merge_method}`, required checks only `{required_checks}`.
{require_checks_line}
Configured deployment steps:

{deployments}

Configured branch-target deployment steps (`landing.targets`, used by `knit land --target <branch>`):

{target_deployments}

Do not use `gh pr merge` for Knit-owned bundles.
"#
    )
}

fn landing_deployment_agents_line(deployment: &ProjectLandingDeployment) -> String {
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
    // What makes the step run at all. Reading only "repo `x`" invites the
    // assumption that every configured deployment runs on every landing.
    let watches = match &deployment.when_changed {
        Some(watched) if watched.iter().any(|repo_id| repo_id == "*") => {
            ", runs on every landing".to_string()
        }
        Some(watched) => format!(", when `{}` changed", watched.join("`, `")),
        None => deployment
            .repo_id
            .as_deref()
            .map(|repo| format!(", when `{repo}` changed"))
            .unwrap_or_else(|| ", runs on every landing".to_string()),
    };
    format!(
        "- `{id}`{repo} uses `{mode}`{checkout}{command}{timeout}{watches}",
        id = deployment.id
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
