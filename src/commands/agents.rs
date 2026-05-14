use crate::checkout::{checkout_dir, is_in_place};
use crate::git::git_output;
use crate::model::RepoEntry;
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

pub(crate) fn upsert_managed_section(existing: &str, section: &str) -> String {
    if let Some(start) = existing.find(KNIT_AGENTS_BEGIN) {
        if let Some(end_offset) = existing[start..].find(KNIT_AGENTS_END) {
            let end = start + end_offset + KNIT_AGENTS_END.len();
            let mut next = String::new();
            next.push_str(&existing[..start]);
            next.push_str(section.trim_end());
            next.push_str(&existing[end..]);
            return ensure_trailing_newline(next);
        }
    }

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
