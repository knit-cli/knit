use crate::commands::bundle::bundle_state;
use crate::model::{
    ChangeGroup, KnitConfig, KnitContexts, KnitProject, BUNDLE_STATE_ARCHIVED, BUNDLE_STATE_CLOSED,
    BUNDLE_STATE_OPEN,
};
use crate::output as out;
use crate::store::{find_knit_root, read_json, write_json};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub fn doctor_workspace() -> Result<()> {
    let root = current_root()?;
    let mut issues = Vec::new();
    let config_path = root.join(".knit/config.json");
    match read_json::<KnitConfig>(&config_path) {
        Ok(config) => inspect_config(&root, &config, &mut issues),
        Err(error) => issues.push(format!("config: {error:#}")),
    }
    inspect_json_dir::<ChangeGroup>(&root.join(".knit/bundles"), "bundle", &mut issues);
    inspect_json_dir::<KnitProject>(&root.join(".knit/projects"), "project", &mut issues);
    inspect_optional_json::<KnitContexts>(
        &root.join(".knit/contexts.json"),
        "contexts",
        &mut issues,
    );
    inspect_operational_json_dir(&root.join(".knit/merge-runs"), "merge run", &mut issues);
    inspect_operational_json_dir(&root.join(".knit/land-runs"), "land run", &mut issues);
    inspect_locks(&root, &mut issues);
    inspect_bundle_paths(&root, &mut issues);

    if issues.is_empty() {
        println!("{}", out::ok("Knit doctor: ok"));
        return Ok(());
    }

    println!("{}", out::danger("Knit doctor found issues:"));
    for issue in &issues {
        println!("  - {issue}");
    }
    bail!("doctor found {} issue(s)", issues.len())
}

pub fn migrate_workspace(check: bool) -> Result<()> {
    let root = current_root()?;
    let mut changed = Vec::new();
    migrate_one::<KnitConfig>(&root.join(".knit/config.json"), check, &mut changed)?;
    migrate_bundles(&root.join(".knit/bundles"), check, &mut changed)?;
    migrate_dir::<KnitProject>(&root.join(".knit/projects"), check, &mut changed)?;
    migrate_optional::<KnitContexts>(&root.join(".knit/contexts.json"), check, &mut changed)?;

    if changed.is_empty() {
        println!("{}", out::ok("No migrations needed."));
        return Ok(());
    }
    for path in &changed {
        println!(
            "{} {}",
            if check {
                out::warn("would update")
            } else {
                out::movement("updated")
            },
            out::path(path.display())
        );
    }
    if check {
        bail!("{} file(s) need migration", changed.len());
    }
    Ok(())
}

fn current_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_knit_root(&cwd).context("No Knit workspace found.")
}

fn inspect_config(root: &Path, config: &KnitConfig, issues: &mut Vec<String>) {
    if let Some(bundle_id) = &config.active_bundle {
        let path = root
            .join(".knit/bundles")
            .join(format!("{bundle_id}.bundle.json"));
        match read_json::<ChangeGroup>(&path) {
            Ok(bundle) if bundle_state(&bundle) == BUNDLE_STATE_ARCHIVED => {
                issues.push(format!("active bundle `{bundle_id}` is archived"))
            }
            Ok(_) => {}
            Err(_) => issues.push(format!("active bundle `{bundle_id}` does not exist")),
        }
    }
    if let Some(project_id) = &config.active_project {
        let path = root
            .join(".knit/projects")
            .join(format!("{project_id}.project.json"));
        if !path.exists() {
            issues.push(format!("active project `{project_id}` does not exist"));
        }
    }
}

fn inspect_json_dir<T>(dir: &Path, label: &str, issues: &mut Vec<String>)
where
    T: serde::de::DeserializeOwned,
{
    if !dir.exists() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        issues.push(format!("failed to read {}", dir.display()));
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if let Err(error) = read_json::<T>(&path) {
            issues.push(format!("{label} {}: {error:#}", path.display()));
        }
    }
}

fn inspect_optional_json<T>(path: &Path, label: &str, issues: &mut Vec<String>)
where
    T: serde::de::DeserializeOwned,
{
    if path.exists() {
        if let Err(error) = read_json::<T>(path) {
            issues.push(format!("{label} {}: {error:#}", path.display()));
        }
    }
}

fn inspect_operational_json_dir(dir: &Path, label: &str, issues: &mut Vec<String>) {
    if !dir.exists() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        issues.push(format!("failed to read {}", dir.display()));
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        match read_json::<Value>(&path) {
            Ok(value) => {
                if value["schemaVersion"].as_str().is_none() || value["kind"].as_str().is_none() {
                    issues.push(format!(
                        "{label} {} is missing schemaVersion or kind",
                        path.display()
                    ));
                }
            }
            Err(error) => issues.push(format!("{label} {}: {error:#}", path.display())),
        }
    }
}

fn inspect_locks(root: &Path, issues: &mut Vec<String>) {
    let dir = root.join(".knit/locks");
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("lock")
            {
                issues.push(format!("stale lock? {}", entry.path().display()));
            }
        }
    }
}

fn inspect_bundle_paths(root: &Path, issues: &mut Vec<String>) {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return;
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(bundle) = read_json::<ChangeGroup>(&path) else {
            continue;
        };
        for repo in &bundle.repos {
            let repo_path = PathBuf::from(&repo.path);
            if !repo_path.exists() {
                issues.push(format!(
                    "{}:{} repo path missing: {}",
                    bundle.id, repo.id, repo.path
                ));
            }
            if let Some(worktree_path) = &repo.worktree_path {
                let path = resolve_path(root, worktree_path);
                if !path.exists() {
                    issues.push(format!(
                        "{}:{} worktree missing: {}",
                        bundle.id, repo.id, worktree_path
                    ));
                }
            }
        }
    }
}

fn migrate_dir<T>(dir: &Path, check: bool, changed: &mut Vec<PathBuf>) -> Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            migrate_one::<T>(&path, check, changed)?;
        }
    }
    Ok(())
}

fn migrate_bundles(dir: &Path, check: bool, changed: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let before = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut bundle: ChangeGroup = serde_json::from_str(&before)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if bundle.state.is_none() {
            if let Some(close_node) = bundle
                .nodes
                .iter()
                .rev()
                .find(|node| node.node_type == "feature.closed")
            {
                bundle.state = Some(BUNDLE_STATE_CLOSED.to_string());
                bundle.closed_at = Some(close_node.created_at.clone());
            } else {
                bundle.state = Some(BUNDLE_STATE_OPEN.to_string());
            }
        }
        let after = format!("{}\n", serde_json::to_string_pretty(&bundle)?);
        if before != after {
            changed.push(path.clone());
            if !check {
                write_json(&path, &bundle)?;
            }
        }
    }
    Ok(())
}

fn migrate_optional<T>(path: &Path, check: bool, changed: &mut Vec<PathBuf>) -> Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    if path.exists() {
        migrate_one::<T>(path, check, changed)?;
    }
    Ok(())
}

fn migrate_one<T>(path: &Path, check: bool, changed: &mut Vec<PathBuf>) -> Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    if !path.exists() {
        return Ok(());
    }
    let before =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: T = serde_json::from_str(&before)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let after = format!("{}\n", serde_json::to_string_pretty(&value)?);
    if before != after {
        changed.push(path.to_path_buf());
        if !check {
            write_json(path, &value)?;
        }
    }
    Ok(())
}

fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}
