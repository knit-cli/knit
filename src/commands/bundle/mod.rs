//! `knit bundle` lifecycle and inspection: show/list/validate, switch, and
//! shared bundle-state helpers. Archive/restore/delete live in [`lifecycle`],
//! artifact validation in [`validate`], and the dead-work prune subsystem in
//! [`prune`].

mod lifecycle;
mod prune;
mod validate;

pub(crate) use lifecycle::delete_repo_feature_branch;
pub use lifecycle::{archive_bundle, delete_bundle, restore_bundle};
pub use prune::prune_merged_bundles;

use crate::model::{BundleState, ChangeGroup};
use crate::output as out;
use crate::store::{
    bundle_exists, bundle_path as stored_bundle_path, find_knit_root, load_active_bundle,
    read_json, set_workspace_active_bundle,
};
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use validate::validate_change_group;

pub fn show_current_bundle() -> Result<()> {
    let active = load_active_bundle()?;
    println!(
        "{} {}",
        out::heading("Bundle:"),
        out::node(&active.bundle.id)
    );
    println!(
        "{} {}",
        out::heading("Resolved from:"),
        active.resolution_source.label()
    );
    println!("{} {}", out::heading("Title:"), active.bundle.title);
    if let Some(project_id) = &active.bundle.project_id {
        println!("{} {}", out::heading("Project:"), out::repo(project_id));
    }
    println!(
        "{} {}",
        out::heading("Path:"),
        out::path(active.bundle_path.display())
    );
    println!(
        "{} {} repo(s)",
        out::heading("Repos:"),
        active.bundle.repos.len()
    );
    Ok(())
}

pub fn bundle_path() -> Result<()> {
    let active = load_active_bundle()?;
    println!("{}", active.bundle_path.display());
    Ok(())
}

pub fn print_bundle() -> Result<()> {
    let active = load_active_bundle()?;
    let text =
        serde_json::to_string_pretty(&active.bundle).context("failed to serialize bundle")?;
    println!("{text}");
    Ok(())
}

pub fn validate_bundle() -> Result<()> {
    let active = load_active_bundle()?;
    let errors = validate_change_group(&active.bundle);
    if errors.is_empty() {
        println!(
            "{} {}",
            out::ok("Bundle valid:"),
            out::path(active.bundle_path.display())
        );
        return Ok(());
    }

    println!(
        "{} {}",
        out::danger("Bundle invalid:"),
        out::path(active.bundle_path.display())
    );
    for error in &errors {
        println!("  - {error}");
    }
    bail!("bundle validation failed with {} error(s)", errors.len());
}

pub fn list_bundles(all: bool, archived: bool, deleted: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let dir = root.join(".knit/bundles");
    let deleted_dir = root.join(".knit/deleted/bundles");
    if !dir.exists() && !deleted_dir.exists() {
        println!("{}", out::muted("No bundles."));
        return Ok(());
    }

    let active_id = load_active_bundle().ok().map(|active| active.bundle.id);
    let mut entries = Vec::new();
    if dir.exists() {
        entries.extend(bundle_json_paths(&dir)?);
    }
    if all || deleted {
        if deleted_dir.exists() {
            entries.extend(bundle_json_paths(&deleted_dir)?);
        }
    }
    entries.sort();

    for path in entries {
        let bundle: ChangeGroup = read_json(&path)?;
        let state = bundle_state(&bundle);
        if !all {
            if state == BundleStatus::Archived && !archived {
                continue;
            }
            if state == BundleStatus::Deleted && !deleted {
                continue;
            }
        }
        let marker = if active_id.as_deref() == Some(bundle.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{} {} {:<8} {} repo(s)",
            marker,
            out::node(&bundle.id),
            state,
            bundle.repos.len()
        );
    }
    Ok(())
}

fn bundle_json_paths(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    Ok(fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect())
}

/// Sorted ids of every open bundle in the workspace. Unreadable bundle files
/// are skipped rather than aborting, so a single bad artifact does not block a
/// workspace-wide pull.
pub fn list_open_bundle_ids(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for path in bundle_json_paths(&dir)? {
        let Ok(bundle) = read_json::<ChangeGroup>(&path) else {
            continue;
        };
        if bundle_state(&bundle) == BundleStatus::Open {
            ids.push(bundle.id);
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn switch_bundle(bundle_id: &str, workspace: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let bundle_id = crate::ids::slugify(bundle_id);
    if !bundle_exists(&root, &bundle_id) {
        bail!("No Knit bundle named `{bundle_id}` found.");
    }
    let bundle: ChangeGroup = read_json(&stored_bundle_path(&root, &bundle_id))?;
    if bundle_state(&bundle) == BundleStatus::Archived {
        bail!("Bundle `{bundle_id}` is archived. Run `knit bundle restore {bundle_id}` first.");
    }

    if !workspace {
        bail!(
            "Refusing to switch the shared workspace fallback without --workspace. Use `knit switch {bundle_id} --workspace`, run from the target worktree, or pass `--bundle {bundle_id}` to a single command."
        );
    }

    set_workspace_active_bundle(&root, &bundle_id)?;
    println!(
        "{} {}",
        out::heading("Active bundle:"),
        out::node(&bundle_id)
    );

    Ok(())
}

fn current_root() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_knit_root(&cwd).context("No Knit workspace found.")
}

fn load_existing_bundle(path: &std::path::Path, bundle_id: &str) -> Result<ChangeGroup> {
    if !path.exists() {
        bail!("No Knit bundle named `{bundle_id}` found.");
    }
    read_json(path)
}

/// Bundle state as presented to users: the persisted [`BundleState`] plus the
/// derived `Landed`, which is inferred from the ledger and never written to
/// the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleStatus {
    Open,
    Closed,
    Archived,
    Deleted,
    Landed,
}

impl BundleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BundleStatus::Open => "open",
            BundleStatus::Closed => "closed",
            BundleStatus::Archived => "archived",
            BundleStatus::Deleted => "deleted",
            BundleStatus::Landed => "landed",
        }
    }
}

impl std::fmt::Display for BundleStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

pub fn bundle_state(bundle: &ChangeGroup) -> BundleStatus {
    match bundle.state {
        Some(BundleState::Archived) => return BundleStatus::Archived,
        Some(BundleState::Deleted) => return BundleStatus::Deleted,
        Some(BundleState::Closed) => return BundleStatus::Closed,
        _ => {}
    }
    // An explicit `open` state outranks node inference so restoring an archived
    // bundle that carries a legacy `feature.closed` node actually reopens it.
    let explicitly_open = bundle.state == Some(BundleState::Open);
    if !explicitly_open && has_closed_node(bundle) {
        BundleStatus::Closed
    } else if has_landed_node(bundle) {
        // "feature.landed" is a ledger marker; if any recorded publication is still open we
        // should not present the bundle as landed.
        if has_open_publications(bundle) {
            BundleStatus::Open
        } else {
            BundleStatus::Landed
        }
    } else {
        BundleStatus::Open
    }
}

fn has_landed_node(bundle: &ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.landed")
}

fn has_closed_node(bundle: &ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.closed")
}

fn has_open_publications(bundle: &ChangeGroup) -> bool {
    bundle
        .publications
        .iter()
        .any(|publication| !publication_state_is_final(&publication.state))
}

fn publication_state_is_final(state: &str) -> bool {
    state.eq_ignore_ascii_case("merged") || state.eq_ignore_ascii_case("closed")
}
