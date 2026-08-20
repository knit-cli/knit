//! `knit view`: manage a user's saved per-project views (named bundle shapes).
//!
//! A view is an include/exclude delta over a project's `includeByDefault` repo
//! set. Views are stored per user at `.knit/views/<project>.views.json` and never
//! touch the shared project artifact. The resolution logic itself lives in
//! [`crate::commands::init`] so `bundle start` and view apply share one code path.

use crate::commands::init::{resolve_active_view, resolve_view_repos};
use crate::commands::project::load_project_by_id;
use crate::ids::{expand_repo_selectors, slugify};
use crate::model::{KnitProjectViews, ProjectView, ViewBase};
use crate::output as out;
use crate::store::{
    acquire_named_lock, find_knit_root, load_active_bundle, load_config, load_views, project_path,
    save_views, views_path,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn list_views(project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let views = load_views(&root, &project_id)?;
    if views.views.is_empty() {
        println!("{}", out::muted("No saved views."));
        return Ok(());
    }
    for (name, view) in &views.views {
        let marker = if views.default_view.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{} {} {}",
            marker,
            out::repo(name),
            out::muted(summary(view))
        );
    }
    Ok(())
}

pub fn show_view(name: Option<&str>, project: Option<&str>, repos: bool) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;

    if repos {
        let project_artifact = load_project_by_id(&root, &project_id)?;
        let view = resolve_active_view(&root, &project_id, name)?;
        let resolved = resolve_view_repos(&project_artifact, &[], false, view.as_ref(), &[], &[])?;
        if resolved.is_empty() {
            println!("{}", out::muted("(no repos)"));
        }
        for repo in resolved {
            println!("{}", out::repo(&repo.id));
        }
        return Ok(());
    }

    let views = load_views(&root, &project_id)?;
    match name {
        Some(name) => {
            let name = slugify(name);
            let view = views
                .views
                .get(&name)
                .with_context(|| missing_view(&project_id, &name))?;
            println!(
                "{}",
                serde_json::to_string_pretty(view).context("failed to serialize view")?
            );
        }
        None => println!(
            "{}",
            serde_json::to_string_pretty(&views).context("failed to serialize views")?
        ),
    }
    Ok(())
}

pub fn save_view(
    name: &str,
    base: Option<&str>,
    from: Option<&str>,
    include: &[String],
    exclude: &[String],
    from_bundle: bool,
    project: Option<&str>,
) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let project_artifact = load_project_by_id(&root, &project_id)?;
    let name = slugify(name);
    let base = parse_base(base)?;

    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;

    // Seed from an existing view, the current bundle, or empty; the flags
    // then apply on top of the seed.
    let mut view = if let Some(from_name) = from {
        let from_name = slugify(from_name);
        views
            .views
            .get(&from_name)
            .cloned()
            .with_context(|| missing_view(&project_id, &from_name))?
    } else if from_bundle {
        if !include.is_empty() || !exclude.is_empty() {
            bail!("Use --from-bundle on its own, not with --include/--exclude.");
        }
        derive_from_bundle(&project_id, &project_artifact, base.unwrap_or_default())?
    } else {
        ProjectView::default()
    };
    if let Some(base) = base {
        view.base = base;
    }
    // A repo belongs to exactly one list; a flag flips the membership.
    for id in normalize_ids(&project_artifact, include)? {
        view.exclude.retain(|existing| existing != &id);
        if !view.include.contains(&id) {
            view.include.push(id);
        }
    }
    for id in normalize_ids(&project_artifact, exclude)? {
        view.include.retain(|existing| existing != &id);
        if !view.exclude.contains(&id) {
            view.exclude.push(id);
        }
    }
    if view.base == ViewBase::None && !view.exclude.is_empty() {
        bail!(
            "An absolute view (--base none) lists its repos with --include; \
             --exclude has nothing to remove. Use `knit view freeze` to convert \
             a delta view into an absolute repo list."
        );
    }

    views.views.insert(name.clone(), view);
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    println!("{} {}", out::movement("saved view"), out::repo(&name));
    Ok(())
}

/// Rewrite a delta view in place as an absolute repo list: resolve it against
/// the current default set and pin the result as `base: none`.
pub fn freeze_view(name: &str, project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let project_artifact = load_project_by_id(&root, &project_id)?;
    let name = slugify(name);

    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;
    let view = views
        .views
        .get(&name)
        .with_context(|| missing_view(&project_id, &name))?;
    if view.base == ViewBase::None {
        println!(
            "{} {}",
            out::repo(&name),
            out::muted("is already an absolute view.")
        );
        return Ok(());
    }
    let include: Vec<String> =
        resolve_view_repos(&project_artifact, &[], false, Some(view), &[], &[])?
            .into_iter()
            .map(|repo| repo.id)
            .collect();
    views.views.insert(
        name.clone(),
        ProjectView {
            base: ViewBase::None,
            include,
            exclude: Vec::new(),
        },
    );
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    println!(
        "{} {} {}",
        out::movement("froze view"),
        out::repo(&name),
        out::muted("(absolute repo list)")
    );
    Ok(())
}

fn parse_base(base: Option<&str>) -> Result<Option<ViewBase>> {
    match base {
        None => Ok(None),
        Some("default") => Ok(Some(ViewBase::Default)),
        Some("none") => Ok(Some(ViewBase::None)),
        Some(other) => bail!("Unknown view base `{other}`. Use `default` or `none`."),
    }
}

pub fn view_include(name: &str, repos: &[String], project: Option<&str>) -> Result<()> {
    mutate_view_list(name, repos, project, ListKind::Include)
}

pub fn view_exclude(name: &str, repos: &[String], project: Option<&str>) -> Result<()> {
    mutate_view_list(name, repos, project, ListKind::Exclude)
}

pub fn view_unset(name: &str, repos: &[String], project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let project_artifact = load_project_by_id(&root, &project_id)?;
    let name = slugify(name);
    let ids = normalize_ids(&project_artifact, repos)?;

    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;
    let view = views
        .views
        .get_mut(&name)
        .with_context(|| missing_view(&project_id, &name))?;
    view.include.retain(|id| !ids.contains(id));
    view.exclude.retain(|id| !ids.contains(id));
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    println!("{} {}", out::movement("updated view"), out::repo(&name));
    Ok(())
}

pub fn set_default_view(name: Option<&str>, clear: bool, project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;

    if clear {
        views.default_view = None;
        println!("{}", out::movement("cleared default view"));
    } else {
        let name = slugify(name.context("Pass a view name or use --clear.")?);
        if !views.views.contains_key(&name) {
            bail!(missing_view(&project_id, &name));
        }
        views.default_view = Some(name.clone());
        println!("{} {}", out::movement("default view"), out::repo(&name));
    }
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    Ok(())
}

pub fn remove_view(name: &str, project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let name = slugify(name);
    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;
    if views.views.remove(&name).is_none() {
        bail!(missing_view(&project_id, &name));
    }
    if views.default_view.as_deref() == Some(name.as_str()) {
        views.default_view = None;
    }
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    println!("{} {}", out::movement("removed view"), out::repo(&name));
    Ok(())
}

/// Views are maintained state, not a rotting cache: when a repo leaves its
/// project, every saved view's references to it are pruned at the removal
/// point so resolution never has to reconcile stale ids from local edits.
/// (Tolerant resolution stays as the safety net for views synced in from
/// other machines, which can skew from local membership.) Returns the repo
/// ids that were actually present in at least one view.
pub fn prune_removed_repos_from_views(
    root: &Path,
    project_id: &str,
    repo_ids: &[String],
) -> Result<Vec<String>> {
    let ids: Vec<String> = repo_ids.iter().map(|id| slugify(id)).collect();
    let _lock = acquire_named_lock(root, &format!("views-{project_id}"))?;
    let mut views = load_views(root, project_id)?;
    let mut pruned: Vec<String> = Vec::new();
    for view in views.views.values_mut() {
        for id in &ids {
            if list_remove(&mut view.include, id) | list_remove(&mut view.exclude, id)
                && !pruned.contains(id)
            {
                pruned.push(id.clone());
            }
        }
    }
    if !pruned.is_empty() {
        views.updated_at = now_iso();
        save_views(root, &views)?;
    }
    Ok(pruned)
}

fn list_remove(list: &mut Vec<String>, id: &str) -> bool {
    let before = list.len();
    list.retain(|existing| existing != id);
    before != list.len()
}

pub fn edit_views(project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let path = views_path(&root, &project_id);
    if !path.exists() {
        // Materialize an empty document so the editor opens a real file.
        let views = KnitProjectViews::new(project_id.clone(), now_iso());
        save_views(&root, &views)?;
    }
    let editor = std::env::var("EDITOR")
        .unwrap_or_else(|_| if cfg!(windows) { "notepad" } else { "vi" }.to_string());
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor exited with {status}");
    }
    // Validate the edited document parses as a views artifact.
    let _: KnitProjectViews = crate::store::read_json(&path)?;
    println!("{} {}", out::heading("Views:"), out::path(path.display()));
    Ok(())
}

enum ListKind {
    Include,
    Exclude,
}

fn mutate_view_list(
    name: &str,
    repos: &[String],
    project: Option<&str>,
    kind: ListKind,
) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let project_artifact = load_project_by_id(&root, &project_id)?;
    let name = slugify(name);
    let ids = normalize_ids(&project_artifact, repos)?;

    let _lock = acquire_named_lock(&root, &format!("views-{project_id}"))?;
    let mut views = load_views(&root, &project_id)?;
    let view = views.views.entry(name.clone()).or_default();
    if matches!(kind, ListKind::Exclude) && view.base == ViewBase::None {
        bail!(
            "View {} is absolute (base none); drop repos from it with `knit view unset` instead.",
            out::repo(&name)
        );
    }
    // A repo belongs to exactly one list; moving it flips the membership.
    for id in &ids {
        view.include.retain(|existing| existing != id);
        view.exclude.retain(|existing| existing != id);
    }
    let target = match kind {
        ListKind::Include => &mut view.include,
        ListKind::Exclude => &mut view.exclude,
    };
    for id in ids {
        target.push(id);
    }
    views.updated_at = now_iso();
    save_views(&root, &views)?;
    println!("{} {}", out::movement("updated view"), out::repo(&name));
    Ok(())
}

/// Build a view that reproduces the current bundle shape: as deltas against
/// the project's default set, or — for `base: none` — as the absolute list of
/// the bundle's project repos.
fn derive_from_bundle(
    project_id: &str,
    project_artifact: &crate::model::KnitProject,
    base: ViewBase,
) -> Result<ProjectView> {
    let active = load_active_bundle()?;
    if active.bundle.project_id.as_deref() != Some(project_id) {
        bail!(
            "The active bundle is not based on project {}.",
            out::repo(project_id)
        );
    }
    let bundle_ids: Vec<String> = active
        .bundle
        .repos
        .iter()
        .filter(|repo| project_artifact.repos.iter().any(|p| p.id == repo.id))
        .map(|repo| repo.id.clone())
        .collect();

    if base == ViewBase::None {
        return Ok(ProjectView {
            base,
            include: bundle_ids,
            exclude: Vec::new(),
        });
    }
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for repo in &project_artifact.repos {
        let in_bundle = bundle_ids.contains(&repo.id);
        if in_bundle && !repo.include_by_default {
            include.push(repo.id.clone());
        } else if !in_bundle && repo.include_by_default {
            exclude.push(repo.id.clone());
        }
    }
    Ok(ProjectView {
        base,
        include,
        exclude,
    })
}

/// Slugify, validate against the project, and de-duplicate a list of repo ids.
fn normalize_ids(project: &crate::model::KnitProject, repos: &[String]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for repo in expand_repo_selectors(repos) {
        let id = slugify(&repo);
        if !project.repos.iter().any(|repo| repo.id == id) {
            bail!(
                "Project {} has no repo named {}.",
                out::repo(&project.id),
                out::repo(&id)
            );
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn summary(view: &ProjectView) -> String {
    if view.base == ViewBase::None {
        return if view.include.is_empty() {
            "(empty absolute view)".to_string()
        } else {
            format!("={}", view.include.join(","))
        };
    }
    let mut parts = Vec::new();
    if !view.include.is_empty() {
        parts.push(format!("+{}", view.include.join(",")));
    }
    if !view.exclude.is_empty() {
        parts.push(format!("-{}", view.exclude.join(",")));
    }
    if parts.is_empty() {
        "(no deltas)".to_string()
    } else {
        parts.join(" ")
    }
}

fn missing_view(project_id: &str, name: &str) -> String {
    format!(
        "Project {} has no saved view named {}.",
        out::repo(project_id),
        out::repo(name)
    )
}

/// Resolve the workspace root and the project id to operate on.
pub(crate) fn resolve_project(project: Option<&str>) -> Result<(PathBuf, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = match project {
        Some(name) => slugify(name),
        None => load_config(&root)?
            .active_project
            .context("No active Knit project. Pass --project or run `knit init <name>`.")?,
    };
    if !project_path(&root, &project_id).exists() {
        bail!("Project {} does not exist.", out::repo(&project_id));
    }
    Ok((root, project_id))
}
