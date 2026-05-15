use crate::checkout::{checkout_dir, is_in_place};
use crate::git::git_output;
use crate::model::{KnitProject, RepoEntry};
use crate::output as out;
use crate::store::ActiveBundle;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const KNIT_AGENTS_BEGIN: &str = "<!-- BEGIN KNIT AGENTS -->";
const KNIT_AGENTS_END: &str = "<!-- END KNIT AGENTS -->";

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

pub(crate) fn print_worktree_agents_summary(paths: &[PathBuf]) {
    if !paths.is_empty() {
        println!(
            "{} {} repo worktree(s)",
            out::heading("Worktree AGENTS.md:"),
            paths.len()
        );
    }
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
knit commit --stage -m "Describe the feature change"
knit push --set-upstream
```

Sibling worktrees for this bundle:

{repo_worktrees}

Do not edit the original source checkout for feature work unless the bundle was created with `--in-place`.
<!-- END KNIT AGENTS -->
"#,
        bundle = active.bundle.id,
        repo = repo.id,
        checkout = checkout_display,
        repo_worktrees = if repo_worktrees.is_empty() {
            "(none)".to_string()
        } else {
            repo_worktrees
        }
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
knit bundle start "feature title" --project {project_id}
```

That command adds these default repos from the project data:

{default_repos}
{observed_section}
For narrower or unusual work, inspect the project first and then choose repo ids deliberately:

```sh
knit project show {project_id}
knit bundle start "feature title" --project {project_id} --repo <repo-id>
```
{end}
"#,
        begin = begin,
        end = end,
        project_id = project.id,
        default_repos = default_repos,
        observed_section = observed_section
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
