use crate::checkout::checkout_dir;
use crate::model::{KnitProject, ProjectRepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    find_knit_root, load_active_bundle, load_config, project_path, read_json, ActiveBundle,
    BundleResolutionSource,
};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which `git reset` mode to run. Mirrors git's own modes.
#[derive(Clone, Copy)]
enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    fn resolve(soft: bool, hard: bool) -> Self {
        match (soft, hard) {
            (true, _) => Self::Soft,
            (_, true) => Self::Hard,
            _ => Self::Mixed,
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
        }
    }
}

/// Run `git reset <mode> <commit>` across tracked checkouts.
///
/// Scope is context-aware, matching how Knit already resolves bundles:
/// when a bundle is resolved explicitly, through `KNIT_BUNDLE`, a worktree cwd,
/// or folder context, the bundle's checkouts are reset. When the only thing
/// available is the shared workspace fallback (running from the source root),
/// the active project's source repo checkouts are reset instead.
pub fn reset_checkouts(
    soft: bool,
    hard: bool,
    commit: Option<&str>,
    repos: &[String],
    all: bool,
) -> Result<()> {
    let mode = ResetMode::resolve(soft, hard);
    let commit = commit.unwrap_or("HEAD");

    let active = load_active_bundle().ok();
    let use_bundle = active
        .as_ref()
        .is_some_and(|active| active.resolution_source != BundleResolutionSource::Config);

    if use_bundle {
        reset_bundle_checkouts(&active.expect("bundle present"), mode, commit, repos, all)
    } else {
        let root = match active {
            Some(active) => active.root,
            None => {
                let cwd = std::env::current_dir().context("failed to read current directory")?;
                find_knit_root(&cwd)
                    .context("Not inside a Knit workspace. Run this from a Knit workspace.")?
            }
        };
        reset_project_sources(&root, mode, commit, repos, all)
    }
}

fn reset_bundle_checkouts(
    active: &ActiveBundle,
    mode: ResetMode,
    commit: &str,
    repos: &[String],
    all: bool,
) -> Result<()> {
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(active, repos, all)?;
    let multiple = indexes.len() > 1;
    let mut failures = Vec::new();

    for index in indexes {
        let repo = &active.bundle.repos[index];
        let cwd = match checkout_dir(active, repo) {
            Some(cwd) => cwd,
            None => {
                failures.push(format!(
                    "{}: no active checkout. Run `knit bundle worktree` to materialize it.",
                    repo.id
                ));
                continue;
            }
        };
        run_reset(&repo.id, &cwd, mode, commit, multiple, &mut failures);
    }

    finish(&failures)
}

fn reset_project_sources(
    root: &Path,
    mode: ResetMode,
    commit: &str,
    repos: &[String],
    all: bool,
) -> Result<()> {
    let project = load_active_project(root)?;
    if project.repos.is_empty() {
        bail!(
            "Project `{}` has no repos. Run `knit project add <repo-id> <path>` first.",
            project.id
        );
    }

    let selected = select_project_repos(&project, repos, all)?;
    let multiple = selected.len() > 1;
    let mut failures = Vec::new();

    println!(
        "{}",
        out::muted(format!(
            "resetting source checkouts for project {}",
            project.id
        ))
    );

    for repo in selected {
        let cwd = PathBuf::from(&repo.path);
        if !cwd.exists() {
            failures.push(format!(
                "{}: source path {} does not exist",
                repo.id,
                cwd.display()
            ));
            continue;
        }
        run_reset(&repo.id, &cwd, mode, commit, multiple, &mut failures);
    }

    finish(&failures)
}

fn run_reset(
    repo_id: &str,
    cwd: &Path,
    mode: ResetMode,
    commit: &str,
    show_header: bool,
    failures: &mut Vec<String>,
) {
    if show_header {
        println!("== {} ({}) ==", out::repo(repo_id), out::path(cwd.display()));
    }

    let status = Command::new("git")
        .args(["reset", mode.flag(), commit])
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => failures.push(match status.code() {
            Some(code) => format!("{repo_id} exited {code}"),
            None => format!("{repo_id} terminated by signal"),
        }),
        Err(error) => failures.push(format!("{repo_id}: failed to run git reset: {error}")),
    }
}

fn finish(failures: &[String]) -> Result<()> {
    if !failures.is_empty() {
        bail!("git reset failed: {}", failures.join(", "));
    }
    Ok(())
}

fn load_active_project(root: &Path) -> Result<KnitProject> {
    let config = load_config(root)?;
    let project_id = config.active_project.context(
        "No active project to reset. Set one with `knit project set <name>` or run from a bundle worktree to reset its checkouts.",
    )?;
    read_json(&project_path(root, &project_id))
        .with_context(|| format!("failed to load project `{project_id}`"))
}

fn select_project_repos<'a>(
    project: &'a KnitProject,
    selectors: &[String],
    all: bool,
) -> Result<Vec<&'a ProjectRepoEntry>> {
    if all && !selectors.is_empty() {
        bail!("Use either --all or repo selectors, not both.");
    }
    if all || selectors.is_empty() {
        return Ok(project.repos.iter().collect());
    }

    let mut selected = Vec::new();
    for selector in selectors {
        let Some(repo) = project
            .repos
            .iter()
            .find(|repo| project_repo_matches(repo, selector))
        else {
            bail!("No project repo matched `{selector}`.");
        };
        if !selected.iter().any(|existing: &&ProjectRepoEntry| {
            std::ptr::eq(*existing as *const _, repo as *const _)
        }) {
            selected.push(repo);
        }
    }
    Ok(selected)
}

fn project_repo_matches(repo: &ProjectRepoEntry, selector: &str) -> bool {
    if selector == repo.id || selector == repo.path {
        return true;
    }
    let selector_path = Path::new(selector);
    selector_path.exists()
        && canonical(selector_path)
            .zip(canonical(Path::new(&repo.path)))
            .is_some_and(|(selector_abs, repo_abs)| selector_abs == repo_abs)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}
