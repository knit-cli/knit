use crate::checkout::is_in_place;
use crate::git::{branch_exists, current_branch, git_output};
use crate::model::{
    BundleNode, ChangeGroup, BUNDLE_STATE_ARCHIVED, BUNDLE_STATE_CLOSED, BUNDLE_STATE_DELETED,
    BUNDLE_STATE_OPEN, CHANGE_GROUP_KIND, CHECKOUT_MODE_IN_PLACE, CHECKOUT_MODE_WORKTREE,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    bundle_exists, bundle_path as stored_bundle_path, find_knit_root, load_active_bundle,
    load_config, read_json, save_config, set_folder_active_bundle, set_workspace_active_bundle,
    write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;

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
            if state == BUNDLE_STATE_ARCHIVED && !archived {
                continue;
            }
            if state == BUNDLE_STATE_DELETED && !deleted {
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

pub fn switch_bundle(bundle_id: &str, workspace: bool, here: bool) -> Result<()> {
    if workspace && here {
        bail!("Use either --workspace or --here, not both.");
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let bundle_id = crate::ids::slugify(bundle_id);
    if !bundle_exists(&root, &bundle_id) {
        bail!("No Knit bundle named `{bundle_id}` found.");
    }
    let bundle: ChangeGroup = read_json(&stored_bundle_path(&root, &bundle_id))?;
    if bundle_state(&bundle) == BUNDLE_STATE_ARCHIVED {
        bail!("Bundle `{bundle_id}` is archived. Run `knit bundle restore {bundle_id}` first.");
    }

    if here {
        set_folder_active_bundle(&root, &cwd, &bundle_id)?;
        println!(
            "{} {} {}",
            out::heading("Folder bundle:"),
            out::node(&bundle_id),
            out::path(crate::store::relative_path_for_storage(&root, &cwd))
        );
    } else if workspace {
        set_workspace_active_bundle(&root, &bundle_id)?;
        println!(
            "{} {}",
            out::heading("Active bundle:"),
            out::node(&bundle_id)
        );
    } else if cwd == root {
        bail!(
            "Refusing to switch the shared workspace fallback without --workspace. Use `knit switch {bundle_id} --workspace`, `knit switch {bundle_id} --here`, run from the target worktree, or pass `--bundle {bundle_id}` to a single command."
        );
    } else {
        set_folder_active_bundle(&root, &cwd, &bundle_id)?;
        println!(
            "{} {} {}",
            out::heading("Folder bundle:"),
            out::node(&bundle_id),
            out::path(crate::store::relative_path_for_storage(&root, &cwd))
        );
    }

    Ok(())
}

pub fn archive_bundle(bundle_id: &str) -> Result<()> {
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let mut bundle = load_existing_bundle(&path, &bundle_id)?;
    let now = now_iso();
    bundle.state = Some(BUNDLE_STATE_ARCHIVED.to_string());
    bundle.archived_at = Some(now.clone());
    bundle.updated_at = now;
    write_json(&path, &bundle)?;
    clear_active_if_matches(&root, &bundle_id)?;
    println!(
        "{} {}",
        out::heading("Archived bundle:"),
        out::node(&bundle_id)
    );
    Ok(())
}

pub fn restore_bundle(bundle_id: &str) -> Result<()> {
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let mut bundle = load_existing_bundle(&path, &bundle_id)?;
    if bundle_state(&bundle) != BUNDLE_STATE_ARCHIVED {
        bail!("Bundle `{bundle_id}` is not archived.");
    }
    let restored_state = if has_closed_node(&bundle) {
        BUNDLE_STATE_CLOSED
    } else {
        BUNDLE_STATE_OPEN
    };
    bundle.state = Some(restored_state.to_string());
    bundle.archived_at = None;
    bundle.updated_at = now_iso();
    write_json(&path, &bundle)?;
    println!(
        "{} {} ({})",
        out::heading("Restored bundle:"),
        out::node(&bundle_id),
        restored_state
    );
    Ok(())
}

pub fn delete_bundle(
    bundle_id: &str,
    force: bool,
    worktrees: bool,
    branches: bool,
    force_branches: bool,
) -> Result<()> {
    if !force {
        bail!("Deleting a bundle requires --force.");
    }
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let mut bundle = load_existing_bundle(&path, &bundle_id)?;
    if force_branches && !branches {
        bail!("Use --branches with --force-branches.");
    }
    if branches && !worktrees {
        bail!("Deleting local branches requires --worktrees so generated checkouts are removed first.");
    }
    if worktrees {
        let mut active = ActiveBundle::unlocked(root.clone(), path.clone(), bundle);
        crate::commands::clean::clean_worktrees_for_bundle(&mut active, force)?;
        bundle = active.bundle;
    }
    if branches {
        delete_local_feature_branches(&bundle, force_branches)?;
    }
    let now = now_iso();
    bundle.state = Some(BUNDLE_STATE_DELETED.to_string());
    bundle.deleted_at = Some(now.clone());
    bundle.updated_at = now;
    let deleted_dir = root.join(".knit/deleted/bundles");
    fs::create_dir_all(&deleted_dir)
        .with_context(|| format!("failed to create {}", deleted_dir.display()))?;
    let deleted_path = deleted_dir.join(format!("{bundle_id}.bundle.json"));
    write_json(&deleted_path, &bundle)?;
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    clear_active_if_matches(&root, &bundle_id)?;
    println!(
        "{} {} {}",
        out::heading("Deleted bundle:"),
        out::node(&bundle_id),
        out::path(deleted_path.display())
    );
    Ok(())
}

fn delete_local_feature_branches(bundle: &ChangeGroup, force: bool) -> Result<()> {
    let mut failures = Vec::new();
    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::muted("no feature branch recorded")
            );
            continue;
        };
        let repo_root = PathBuf::from(&repo.path);
        if !repo_root.exists() {
            failures.push(format!(
                "{}: original repo path is missing, cannot delete {}",
                repo.id, branch
            ));
            continue;
        }
        if is_in_place(repo) && current_branch(&repo_root)?.as_deref() == Some(branch) {
            failures.push(format!(
                "{}: {} is checked out in the source repo; switch branches before deleting it",
                repo.id, branch
            ));
            continue;
        }
        if !branch_exists(&repo_root, branch) {
            println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::muted("branch already missing"),
                out::branch(branch)
            );
            continue;
        }
        let delete_flag = if force { "-D" } else { "-d" };
        let args = vec![
            OsString::from("branch"),
            OsString::from(delete_flag),
            OsString::from(branch),
        ];
        match git_output(&repo_root, args) {
            Ok(_) => println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::movement("removed"),
                out::branch(branch)
            ),
            Err(error) => failures.push(format!("{}: {error:#}", repo.id)),
        }
    }
    if !failures.is_empty() {
        bail!(
            "failed to delete feature branches:\n{}",
            failures.join("\n")
        );
    }
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

fn clear_active_if_matches(root: &std::path::Path, bundle_id: &str) -> Result<()> {
    let mut config = load_config(root)?;
    if config.active_bundle.as_deref() == Some(bundle_id) {
        config.active_bundle = None;
        save_config(root, &config)?;
    }
    Ok(())
}

pub fn bundle_state(bundle: &ChangeGroup) -> &'static str {
    match bundle.state.as_deref() {
        Some(BUNDLE_STATE_ARCHIVED) => return BUNDLE_STATE_ARCHIVED,
        Some(BUNDLE_STATE_DELETED) => return BUNDLE_STATE_DELETED,
        Some(BUNDLE_STATE_CLOSED) => return BUNDLE_STATE_CLOSED,
        _ => {}
    }
    if has_closed_node(bundle) {
        BUNDLE_STATE_CLOSED
    } else if bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.landed")
    {
        "landed"
    } else {
        BUNDLE_STATE_OPEN
    }
}

fn has_closed_node(bundle: &ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.closed")
}

fn validate_change_group(bundle: &ChangeGroup) -> Vec<String> {
    let mut errors = Vec::new();

    if bundle.schema_version != SCHEMA_VERSION {
        errors.push(format!(
            "schemaVersion must be `{SCHEMA_VERSION}`, found `{}`",
            bundle.schema_version
        ));
    }
    if bundle.kind != CHANGE_GROUP_KIND {
        errors.push(format!(
            "kind must be `{CHANGE_GROUP_KIND}`, found `{}`",
            bundle.kind
        ));
    }
    if bundle.id.trim().is_empty() {
        errors.push("id must not be empty".to_string());
    }
    if bundle.title.trim().is_empty() {
        errors.push("title must not be empty".to_string());
    }
    if bundle.created_at.trim().is_empty() {
        errors.push("createdAt must not be empty".to_string());
    }
    if bundle.updated_at.trim().is_empty() {
        errors.push("updatedAt must not be empty".to_string());
    }

    validate_repos(bundle, &mut errors);
    validate_commit_groups(bundle, &mut errors);
    validate_nodes(bundle, &mut errors);

    errors
}

fn validate_repos(bundle: &ChangeGroup, errors: &mut Vec<String>) {
    let mut repo_ids = BTreeSet::new();
    for repo in &bundle.repos {
        if repo.id.trim().is_empty() {
            errors.push("repo id must not be empty".to_string());
        } else if !repo_ids.insert(repo.id.as_str()) {
            errors.push(format!("repo id `{}` is duplicated", repo.id));
        }
        if repo.path.trim().is_empty() {
            errors.push(format!("repo `{}` path must not be empty", repo.id));
        }
        if repo.base_branch.trim().is_empty() {
            errors.push(format!("repo `{}` baseBranch must not be empty", repo.id));
        }
        if !matches!(
            repo.checkout_mode.as_str(),
            CHECKOUT_MODE_WORKTREE | CHECKOUT_MODE_IN_PLACE
        ) {
            errors.push(format!(
                "repo `{}` checkoutMode must be `{CHECKOUT_MODE_WORKTREE}` or `{CHECKOUT_MODE_IN_PLACE}`",
                repo.id
            ));
        }
        if repo.checkout_mode == CHECKOUT_MODE_IN_PLACE
            && repo.worktree_path.as_deref() != Some(repo.path.as_str())
        {
            errors.push(format!(
                "repo `{}` in-place checkout must have worktreePath equal to path",
                repo.id
            ));
        }
    }
}

fn validate_commit_groups(bundle: &ChangeGroup, errors: &mut Vec<String>) {
    let mut group_ids = BTreeSet::new();
    for group in &bundle.commit_groups {
        if group.id.trim().is_empty() {
            errors.push("commit group id must not be empty".to_string());
        } else if !group_ids.insert(group.id.as_str()) {
            errors.push(format!("commit group id `{}` is duplicated", group.id));
        }
        if group.message.trim().is_empty() {
            errors.push(format!(
                "commit group `{}` message must not be empty",
                group.id
            ));
        }
        if group.created_at.trim().is_empty() {
            errors.push(format!(
                "commit group `{}` createdAt must not be empty",
                group.id
            ));
        }
        if group.commits.is_empty() {
            errors.push(format!("commit group `{}` must record commits", group.id));
        }
        for commit in &group.commits {
            validate_commit_ref(
                "commit group",
                &group.id,
                &commit.repo_id,
                &commit.sha,
                errors,
            );
        }
    }
}

fn validate_nodes(bundle: &ChangeGroup, errors: &mut Vec<String>) {
    if bundle.nodes.is_empty() {
        errors.push("nodes must not be empty".to_string());
        return;
    }

    let mut node_ids = BTreeSet::new();
    for node in &bundle.nodes {
        validate_node(node, &mut node_ids, errors);
    }

    let last_node_id = bundle.nodes.last().map(|node| node.id.as_str());
    if bundle.head_node_id.as_deref() != last_node_id {
        errors.push(format!(
            "headNodeId must point at the latest node `{}`, found `{}`",
            last_node_id.unwrap_or(""),
            bundle.head_node_id.as_deref().unwrap_or("")
        ));
    }
}

fn validate_node(node: &BundleNode, node_ids: &mut BTreeSet<String>, errors: &mut Vec<String>) {
    if node.id.trim().is_empty() {
        errors.push("node id must not be empty".to_string());
    } else if !node_ids.insert(node.id.clone()) {
        errors.push(format!("node id `{}` is duplicated", node.id));
    }
    if node.node_type.trim().is_empty() {
        errors.push(format!("node `{}` type must not be empty", node.id));
    }
    if node.created_at.trim().is_empty() {
        errors.push(format!("node `{}` createdAt must not be empty", node.id));
    }

    match node.node_type.as_str() {
        "feature.created" => {
            if node.title.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record title", node.id));
            }
        }
        "repo.added" | "repo.removed" | "worktree.materialized" => {
            if node.repo_ids.as_ref().is_none_or(Vec::is_empty) {
                errors.push(format!("node `{}` must record repoIds", node.id));
            }
        }
        "commit.group" | "revert.group" => {
            if node
                .commit_group_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                errors.push(format!("node `{}` must record commitGroupId", node.id));
            }
            if node.message.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record message", node.id));
            }
            if node.commits.is_empty() {
                errors.push(format!("node `{}` must record commits", node.id));
            }
            for commit in &node.commits {
                validate_commit_ref("node", &node.id, &commit.repo_id, &commit.sha, errors);
            }
            if node.node_type == "revert.group"
                && node
                    .target_node_id
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            {
                errors.push(format!("node `{}` must record targetNodeId", node.id));
            }
        }
        "git.observed" => {
            if node.repo_changes.is_empty() {
                errors.push(format!("node `{}` must record repoChanges", node.id));
            }
        }
        "land.update" => {
            if node.repo_changes.is_empty() {
                errors.push(format!("node `{}` must record repoChanges", node.id));
            }
            if node.provider.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record provider", node.id));
            }
        }
        "checkpoint" => {
            if node.message.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record message", node.id));
            }
        }
        "feature.landed" => {
            if node.repo_ids.as_ref().is_none_or(Vec::is_empty) {
                errors.push(format!("node `{}` must record repoIds", node.id));
            }
            if node.plan_id.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record planId", node.id));
            }
            if node.run_id.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record runId", node.id));
            }
            if node.provider.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("node `{}` must record provider", node.id));
            }
            if node.publication_urls.is_empty() {
                errors.push(format!("node `{}` must record publicationUrls", node.id));
            }
        }
        "feature.closed" => {}
        _ => {}
    }

    for change in &node.repo_changes {
        if change.repo_id.trim().is_empty() {
            errors.push(format!(
                "node `{}` repoChange repoId must not be empty",
                node.id
            ));
        }
        if change.movement.trim().is_empty() {
            errors.push(format!(
                "node `{}` repoChange movement must not be empty",
                node.id
            ));
        }
        if change.after_sha.trim().is_empty() {
            errors.push(format!(
                "node `{}` repoChange afterSha must not be empty",
                node.id
            ));
        }
    }
}

fn validate_commit_ref(
    owner_kind: &str,
    owner_id: &str,
    repo_id: &str,
    sha: &str,
    errors: &mut Vec<String>,
) {
    if repo_id.trim().is_empty() {
        errors.push(format!(
            "{owner_kind} `{owner_id}` commit repoId must not be empty"
        ));
    }
    if sha.trim().is_empty() {
        errors.push(format!(
            "{owner_kind} `{owner_id}` commit sha must not be empty"
        ));
    }
}
