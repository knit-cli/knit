use crate::model::{
    BundleNode, ChangeGroup, CHANGE_GROUP_KIND, CHECKOUT_MODE_IN_PLACE, CHECKOUT_MODE_WORKTREE,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    bundle_exists, find_knit_root, load_active_bundle, read_json, set_folder_active_bundle,
    set_workspace_active_bundle,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;

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

pub fn list_bundles(_all: bool, _archived: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        println!("{}", out::muted("No bundles."));
        return Ok(());
    }

    let active_id = load_active_bundle().ok().map(|active| active.bundle.id);
    let mut entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        let bundle: ChangeGroup = read_json(&path)?;
        let marker = if active_id.as_deref() == Some(bundle.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{} {} {:<8} {} repo(s)",
            marker,
            out::node(&bundle.id),
            bundle_state(&bundle),
            bundle.repos.len()
        );
    }
    Ok(())
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

    if here {
        set_folder_active_bundle(&root, &cwd, &bundle_id)?;
        println!(
            "{} {} {}",
            out::heading("Folder bundle:"),
            out::node(&bundle_id),
            out::path(crate::store::relative_path_for_storage(&root, &cwd))
        );
    } else if workspace || cwd == root {
        set_workspace_active_bundle(&root, &bundle_id)?;
        println!(
            "{} {}",
            out::heading("Active bundle:"),
            out::node(&bundle_id)
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

fn bundle_state(bundle: &ChangeGroup) -> &'static str {
    if bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.closed")
    {
        "closed"
    } else if bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.landed")
    {
        "landed"
    } else {
        "open"
    }
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
