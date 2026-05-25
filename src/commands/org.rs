use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::slugify;
use crate::model::{KnitOrg, OrgRepoEntry};
use crate::output as out;
use crate::store::{acquire_named_lock, find_knit_root, org_path, read_json, write_json};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

pub fn init_org(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).unwrap_or(cwd);
    let org_id = slugify(name);
    let path = org_path(&root, &org_id);
    if path.exists() {
        bail!("Org {} already exists.", out::path(path.display()));
    }
    fs::create_dir_all(root.join(".knit/orgs")).context("failed to create .knit/orgs")?;
    let org = KnitOrg::new(org_id.clone(), name.to_string(), now_iso());
    write_json(&path, &org)?;
    println!("{} {}", out::heading("Org:"), out::repo(org_id));
    println!("{} {}", out::heading("Path:"), out::path(path.display()));
    Ok(())
}

pub fn list_orgs() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let dir = root.join(".knit/orgs");
    if !dir.exists() {
        println!("{}", out::muted("No orgs."));
        return Ok(());
    }
    let mut entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let org: KnitOrg = read_json(&path)?;
        println!("{} {} repo(s)", out::repo(&org.id), org.repos.len());
    }
    Ok(())
}

pub fn show_org(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let org: KnitOrg = read_json(&org_path(&root, &slugify(name)))?;
    println!("{}", serde_json::to_string_pretty(&org)?);
    Ok(())
}

pub fn add_org_repo(
    org_name: &str,
    repo_id: &str,
    repo_path: &Path,
    base: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let org_id = slugify(org_name);
    let _lock = acquire_named_lock(&root, &format!("org-{org_id}"))?;
    let path = org_path(&root, &org_id);
    let mut org: KnitOrg = read_json(&path)?;
    let repo = resolve_org_repo(repo_id, repo_path, base)?;
    if let Some(existing) = org.repos.iter_mut().find(|existing| existing.id == repo.id) {
        *existing = repo.clone();
        println!("{} {}", out::movement("updated"), out::repo(&repo.id));
    } else {
        println!("{} {}", out::movement("added"), out::repo(&repo.id));
        org.repos.push(repo);
    }
    org.updated_at = now_iso();
    write_json(&path, &org)
}

fn resolve_org_repo(
    repo_id: &str,
    repo_path: &Path,
    base_override: Option<&str>,
) -> Result<OrgRepoEntry> {
    let repo_root = git_root(repo_path)?;
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };
    Ok(OrgRepoEntry {
        id: slugify(repo_id),
        path: repo_root.to_string_lossy().to_string(),
        remote,
        base_branch,
        metadata: Default::default(),
    })
}
