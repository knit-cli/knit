use crate::commands::bundle::bundle_state;
use crate::model::{BundleState, ChangeGroup, KnitConfig, KnitProject};
use crate::output as out;
use crate::store::{
    find_knit_root, global_config_path, load_effective_config, read_json, write_json,
};
use crate::tracking::ledger_recorded_head_sha;
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
    if let Ok(global_path) = global_config_path() {
        if global_path.exists() {
            if let Err(error) = read_json::<KnitConfig>(&global_path) {
                issues.push(format!("global config: {error:#}"));
            }
        }
    }
    if let Ok(effective) = load_effective_config(&root) {
        inspect_effective_remotes(&effective, &mut issues);
    }
    inspect_json_dir::<ChangeGroup>(&root.join(".knit/bundles"), "bundle", &mut issues);
    inspect_json_dir::<KnitProject>(&root.join(".knit/projects"), "project", &mut issues);
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
    let mut removed = Vec::new();
    migrate_remove_legacy(&root.join(".knit/contexts.json"), check, &mut removed)?;

    if changed.is_empty() && removed.is_empty() {
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
    for path in &removed {
        println!(
            "{} {}",
            if check {
                out::warn("would remove")
            } else {
                out::movement("removed")
            },
            out::path(path.display())
        );
    }
    if check {
        bail!("{} file(s) need migration", changed.len() + removed.len());
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
            Ok(bundle)
                if bundle_state(&bundle) == crate::commands::bundle::BundleStatus::Archived =>
            {
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

fn inspect_effective_remotes(config: &KnitConfig, issues: &mut Vec<String>) {
    use crate::commands::remote::configured_sync_remote_names;
    for remote_name in configured_sync_remote_names(config) {
        if !config.remotes.contains_key(&remote_name) {
            issues.push(format!(
                "sync remote `{remote_name}` is configured but no matching remote entry exists"
            ));
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
        // Archived bundles are expected to have no checkouts: archiving removes
        // generated worktrees, so a recorded-but-missing worktree is normal and
        // must not be reported as an issue.
        let archived = bundle.state == Some(BundleState::Archived);
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
                if !archived && !path.exists() {
                    issues.push(format!(
                        "{}:{} worktree missing: {}",
                        bundle.id, repo.id, worktree_path
                    ));
                }
            }
            if let Some(ledger_head) = ledger_recorded_head_sha(&bundle, repo) {
                if repo.head_sha.as_deref() != Some(ledger_head.as_str()) {
                    issues.push(format!(
                        "{}:{} headSha projection differs from ledger: {} != {}",
                        bundle.id,
                        repo.id,
                        repo.head_sha.as_deref().unwrap_or("(none)"),
                        ledger_head
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
    let run_dir = dir
        .parent()
        .context("bundle directory has no parent")?
        .join("land-runs");
    let mut runs = Vec::new();
    if run_dir.exists() {
        for entry in fs::read_dir(&run_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(run) = read_json::<Value>(&path) {
                    runs.push(run);
                }
            }
        }
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
            if let Some(archive_node) = bundle
                .nodes
                .iter()
                .rev()
                .find(|node| node.node_type == "feature.archived")
            {
                bundle.state = Some(BundleState::Archived);
                bundle.archived_at = Some(archive_node.created_at.clone());
            } else if let Some(close_node) = bundle
                .nodes
                .iter()
                .rev()
                .find(|node| node.node_type == "feature.closed")
            {
                bundle.state = Some(BundleState::Closed);
                bundle.closed_at = Some(close_node.created_at.clone());
            } else {
                bundle.state = Some(BundleState::Open);
            }
        }
        let projected_heads = bundle
            .repos
            .iter()
            .map(|repo| (repo.id.clone(), ledger_recorded_head_sha(&bundle, repo)))
            .collect::<Vec<_>>();
        for (repo_id, head) in projected_heads {
            let Some(head) = head else {
                continue;
            };
            if let Some(repo) = bundle.repos.iter_mut().find(|repo| repo.id == repo_id) {
                if repo.head_sha.as_deref() != Some(head.as_str()) {
                    repo.head_sha = Some(head);
                }
            }
        }
        // Preserve extension metadata that older typed models do not know.
        let original: Value = serde_json::from_str(&before)?;
        let mut document =
            crate::model::preserve_bundle_extensions(&original, serde_json::to_value(&bundle)?)?;
        for warning in crate::commands::land::v2::repair_landed_nodes(&mut document, &runs) {
            eprintln!("{}: {warning}; left unchanged", path.display());
        }
        if original != document {
            changed.push(path.clone());
            if !check {
                write_json(&path, &document)?;
            }
        }
    }
    Ok(())
}

/// Remove a legacy Knit file that is no longer read or written.
fn migrate_remove_legacy(path: &Path, check: bool, removed: &mut Vec<PathBuf>) -> Result<()> {
    if path.exists() {
        removed.push(path.to_path_buf());
        if !check {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
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

#[cfg(test)]
mod landing_record_tests {
    use super::*;
    use crate::commands::land::v2::canonical_hash;
    use serde_json::json;

    #[test]
    fn landing_record_migrate_skips_formatting_only_changes() {
        let root = std::env::temp_dir().join(crate::ids::node_id("migration-format"));
        let bundles = root.join("bundles");
        fs::create_dir_all(&bundles).unwrap();
        let bundle = ChangeGroup::new(
            "sample".into(),
            "Synthetic bundle".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        let path = bundles.join("sample.bundle.json");
        let bytes = serde_json::to_vec(&bundle).unwrap();
        fs::write(&path, &bytes).unwrap();
        for check in [true, false] {
            let mut changed = Vec::new();
            migrate_bundles(&bundles, check, &mut changed).unwrap();
            assert!(changed.is_empty());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn landing_record_migrate_check_apply_and_repeat_preserve_metadata_and_run_bytes() {
        let root = std::env::temp_dir().join(crate::ids::node_id("landing-migration"));
        let bundles = root.join("bundles");
        let runs = root.join("land-runs");
        fs::create_dir_all(&bundles).unwrap();
        fs::create_dir_all(&runs).unwrap();
        for branch in [false, true] {
            let id = if branch { "branch" } else { "review" };
            let mut bundle = serde_json::to_value(ChangeGroup::new(
                id.into(),
                "Synthetic landing".into(),
                "2026-01-01T00:00:00Z".into(),
            ))
            .unwrap();
            bundle["repos"] =
                json!([{"id":"api","path":"api","baseBranch":"main","extension":{"keep":1}}]);
            bundle["extension"] = json!({"keep":2});
            bundle["nodes"].as_array_mut().unwrap().push(json!({
                "id":"land-1","type":"feature.landed","createdAt":"2026-01-01T00:00:00Z",
                "repoIds":["api"],"planId":"plan-1","runId":id,"provider":"github",
                "landing":{"terminal":!branch,"extension":{"keep":3}},
                "sessionId":"recorded-session","actor":{"session":"recorded-actor"},"extension":{"keep":4}
            }));
            bundle["headNodeId"] = json!("land-1");
            let plan = json!({"id":"plan-1","bundleId":id,"schemaVersion":"0.2","kind":"KnitLandPlan","steps":[{"id":"merge","repoId":"api","type":if branch {"merge_branch"} else {"merge_pr"}}]});
            let mut output = json!({"targetBranch":"main"});
            if !branch {
                output["publicationUrl"] = json!("https://example.invalid/api/pull/1");
            }
            let run = json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":id,"bundleId":id,"planId":"plan-1","planHash":canonical_hash(&plan),"plan":plan,"sourceBundle":bundle,"steps":[{"id":"merge","repoId":"api","type":if branch {"merge_branch"} else {"merge_pr"},"status":"succeeded","output":output}],"finalized":true});
            let bundle_path = bundles.join(format!("{id}.bundle.json"));
            let run_path = runs.join(format!("{id}.run.json"));
            write_json(&bundle_path, &bundle).unwrap();
            write_json(&run_path, &run).unwrap();
            let bundle_before = fs::read(&bundle_path).unwrap();
            let run_before = fs::read(&run_path).unwrap();
            let mut changed = Vec::new();
            migrate_bundles(&bundles, true, &mut changed).unwrap();
            assert!(changed.contains(&bundle_path));
            assert_eq!(fs::read(&bundle_path).unwrap(), bundle_before);
            migrate_bundles(&bundles, false, &mut Vec::new()).unwrap();
            let repaired: Value = read_json(&bundle_path).unwrap();
            assert_eq!(fs::read(&run_path).unwrap(), run_before);
            let mut expected = bundle;
            if branch {
                expected["nodes"][1]["landing"]["branchOnly"] = json!(true);
            } else {
                expected["nodes"][1]["publicationUrls"] =
                    json!(["https://example.invalid/api/pull/1"]);
            }
            // Existing migration also materializes known model defaults; none
            // of the opaque fields or original node attribution may disappear.
            let typed_expected = serde_json::to_value(
                serde_json::from_value::<ChangeGroup>(expected.clone()).unwrap(),
            )
            .unwrap();
            expected = crate::model::preserve_bundle_extensions(&expected, typed_expected).unwrap();
            assert_eq!(repaired, expected);
            let typed = serde_json::from_value(repaired).unwrap();
            assert!(crate::commands::bundle::validate_change_group(&typed).is_empty());
            let mut repeated = Vec::new();
            migrate_bundles(&bundles, false, &mut repeated).unwrap();
            assert!(repeated.is_empty());
            assert_eq!(fs::read(&run_path).unwrap(), run_before);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
