use crate::git::{
    branch_exists, current_branch, git_output, git_output_optional, git_root, infer_base_branch,
    is_git_worktree, resolve_base_ref,
};
use crate::ids::{commit_group_id, short_sha, slugify, unique_repo_id};
use crate::model::{ChangeGroup, CommitGroup, CommitRef, KnitConfig, RepoEntry};
use crate::paths::same_path;
use crate::status::{has_staged_changes, status_label};
use crate::store::{
    find_knit_root, load_active_bundle, save_active_bundle, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

pub fn init_bundle(title: &str, force: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let existing_root = find_knit_root(&cwd);

    if existing_root.is_some() && !force {
        bail!(
            "An active Knit bundle already exists here. Use --force to replace the active bundle."
        );
    }

    let root = existing_root.unwrap_or(cwd);
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let bundle_path = bundle_dir.join(format!("{bundle_id}.bundle.json"));

    if bundle_path.exists() && !force {
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            bundle_path.display()
        );
    }

    fs::create_dir_all(&bundle_dir).context("failed to create .knit/bundles")?;
    fs::create_dir_all(&worktree_dir).context("failed to create .knit/worktrees")?;

    let bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now_iso());
    write_json(&bundle_path, &bundle)?;

    let config = KnitConfig::new(bundle_id);
    write_json(&knit_dir.join("config.json"), &config)?;

    println!("Active bundle: {}", bundle_path.display());
    Ok(())
}

pub fn add_repo(repo_path: &Path, base_override: Option<&str>) -> Result<()> {
    let mut active = load_active_bundle()?;
    let repo_root = git_root(repo_path)?;
    let repo_name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .context("repo path has no valid final component")?
        .to_string();
    let repo_path = repo_root.to_string_lossy().to_string();
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };

    if let Some(index) = active
        .bundle
        .repos
        .iter()
        .position(|repo| same_path(&repo.path, &repo_path))
    {
        let existing = &mut active.bundle.repos[index];
        existing.remote = remote;
        existing.base_branch = base_branch;
        let repo_id = existing.id.clone();
        let path = existing.path.clone();
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
        println!("Updated repo {} ({})", repo_id, path);
        return Ok(());
    }

    let desired_id = slugify(&repo_name);
    let repo_id = unique_repo_id(&active.bundle, &desired_id);
    active.bundle.repos.push(RepoEntry {
        id: repo_id.clone(),
        path: repo_path,
        remote,
        base_branch,
        feature_branch: None,
        worktree_path: None,
    });
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!("Added repo {repo_id}");
    Ok(())
}

pub fn create_worktrees() -> Result<()> {
    let mut active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit add <repo-path>` first.");
    }

    let bundle_id = active.bundle.id.clone();
    fs::create_dir_all(active.root.join(".knit/worktrees").join(&bundle_id))
        .context("failed to create bundle worktree directory")?;

    for repo in &mut active.bundle.repos {
        let repo_root = PathBuf::from(&repo.path);
        let feature_branch = format!("knit/{bundle_id}");
        let worktree_path = format!(".knit/worktrees/{}/{}", bundle_id, repo.id);
        let worktree_abs = active.root.join(&worktree_path);

        repo.feature_branch = Some(feature_branch.clone());
        repo.worktree_path = Some(worktree_path.clone());

        if worktree_abs.exists() {
            if is_git_worktree(&worktree_abs) {
                println!("{}: worktree already present at {}", repo.id, worktree_path);
                continue;
            }
            bail!(
                "{}: {} exists but is not a git worktree",
                repo.id,
                worktree_abs.display()
            );
        }

        if let Some(parent) = worktree_abs.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create worktree parent {}", parent.display())
            })?;
        }

        if branch_exists(&repo_root, &feature_branch) {
            git_output(
                &repo_root,
                [
                    OsString::from("worktree"),
                    OsString::from("add"),
                    worktree_abs.as_os_str().to_os_string(),
                    OsString::from(&feature_branch),
                ],
            )
            .with_context(|| format!("failed to add worktree for {}", repo.id))?;
            println!("{}: created worktree from existing branch", repo.id);
        } else {
            let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
            git_output(
                &repo_root,
                [
                    OsString::from("worktree"),
                    OsString::from("add"),
                    OsString::from("-b"),
                    OsString::from(&feature_branch),
                    worktree_abs.as_os_str().to_os_string(),
                    OsString::from(base_ref),
                ],
            )
            .with_context(|| format!("failed to create branch/worktree for {}", repo.id))?;
            println!("{}: created {}", repo.id, feature_branch);
        }
    }

    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}

pub fn show_status() -> Result<()> {
    let active = load_active_bundle()?;
    println!("Bundle: {}\n", active.bundle.id);
    println!("{:<14} {:<26} {:<48} status", "repo", "branch", "worktree");

    for repo in &active.bundle.repos {
        let branch = repo.feature_branch.as_deref().unwrap_or("(not created)");
        let worktree = repo.worktree_path.as_deref().unwrap_or("-");
        let status_dir = repo
            .worktree_path
            .as_ref()
            .map(|path| active.root.join(path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(&repo.path));
        let short_status = git_output(&status_dir, ["status", "--short"])?;
        println!(
            "{:<14} {:<26} {:<48} {}",
            repo.id,
            branch,
            worktree,
            status_label(&short_status)
        );
    }

    Ok(())
}

pub fn commit_staged(message: &str) -> Result<()> {
    let mut active = load_active_bundle()?;
    let repos_to_commit = repos_with_staged_changes(&active)?;

    if repos_to_commit.is_empty() {
        bail!("No staged changes found in bundle worktrees.");
    }

    let group_id = commit_group_id();
    let created_at = now_iso();
    let commit_message = format!(
        "{message}\n\nKnit-Group: {group_id}\nKnit-Bundle: {}",
        active.bundle.id
    );
    let mut commits = Vec::new();

    for (repo_id, worktree_abs) in repos_to_commit {
        git_output(
            &worktree_abs,
            [
                OsString::from("commit"),
                OsString::from("-m"),
                OsString::from(&commit_message),
            ],
        )
        .with_context(|| format!("{repo_id}: git commit failed"))?;
        let sha = git_output(&worktree_abs, ["rev-parse", "HEAD"])
            .with_context(|| format!("{repo_id}: failed to read commit sha"))?;
        let short = short_sha(&sha);
        println!("{repo_id}: committed {short}");
        commits.push(CommitRef {
            repo_id,
            sha: sha.trim().to_string(),
        });
    }

    active.bundle.commit_groups.push(CommitGroup {
        id: group_id.clone(),
        message: message.to_string(),
        created_at,
        commits,
    });
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!("Recorded commit group {group_id}");
    Ok(())
}

pub fn show_log() -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.commit_groups.is_empty() {
        println!("No commit groups recorded yet.");
        return Ok(());
    }

    for group in &active.bundle.commit_groups {
        println!("{}  {}", group.id, group.message);
        for commit in &group.commits {
            println!("  {:<10} {}", commit.repo_id, short_sha(&commit.sha));
        }
    }

    Ok(())
}

pub fn show_group(commit_group_id: &str) -> Result<()> {
    let active = load_active_bundle()?;
    let group = active
        .bundle
        .commit_groups
        .iter()
        .find(|group| group.id == commit_group_id)
        .with_context(|| format!("No commit group found for {commit_group_id}"))?;

    println!("{}  {}\n", group.id, group.message);
    for commit in &group.commits {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == commit.repo_id)
            .with_context(|| format!("No repo found for {}", commit.repo_id))?;
        let repo_dir = repo
            .worktree_path
            .as_ref()
            .map(|path| active.root.join(path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(&repo.path));
        println!("== {} {} ==", commit.repo_id, short_sha(&commit.sha));
        let output = git_output(
            &repo_dir,
            [
                OsString::from("show"),
                OsString::from("--stat"),
                OsString::from("--oneline"),
                OsString::from(&commit.sha),
            ],
        )?;
        println!("{output}");
    }

    Ok(())
}

fn repos_with_staged_changes(active: &ActiveBundle) -> Result<Vec<(String, PathBuf)>> {
    let mut repos_to_commit = Vec::new();

    for repo in &active.bundle.repos {
        let Some(worktree_path) = &repo.worktree_path else {
            continue;
        };
        let worktree_abs = active.root.join(worktree_path);
        if !worktree_abs.exists() {
            continue;
        }
        let short_status = git_output(&worktree_abs, ["status", "--short"])?;
        if has_staged_changes(&short_status) {
            repos_to_commit.push((repo.id.clone(), worktree_abs));
        }
    }

    Ok(repos_to_commit)
}
