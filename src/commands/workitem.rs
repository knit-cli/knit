//! Consume work items into bundles. Work items are planned, triaged, and
//! approved in KnitHub, which materializes them into `.knit/work-items/` and
//! syncs local changes back. The CLI's only verb is consumption:
//! `knit bundle --workitem <id>` claims an item, creates (or reuses) its
//! bundle, and writes the agent prompt.

use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitWorkItem, WORK_ITEM_EXECUTION_CLAIMED};
use crate::output as out;
use crate::store::{
    acquire_named_lock, bundle_path, find_knit_root, read_json, work_item_path, write_json,
};
use crate::time::now_iso;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn start_work_item(id: &str) -> Result<()> {
    let root = current_root()?;
    let item_id = slugify(id);
    let _lock = acquire_named_lock(&root, &format!("work-item-{item_id}"))?;
    let path = work_item_path(&root, &item_id);
    let mut item: KnitWorkItem = read_json(&path)?;
    if let Some(existing) = item.bundle_ids.first() {
        let bundle_path = bundle_path(&root, existing);
        if bundle_path.exists() {
            write_work_item_prompt(&root, existing, &item)?;
            println!("{} {}", out::heading("Bundle:"), out::repo(existing));
            println!(
                "{} {}",
                out::heading("Worktree:"),
                out::path(root.join(".knit/worktrees").join(existing).display())
            );
            return Ok(());
        }
    }
    let project_id = item
        .project_id
        .clone()
        .context("Work item has no projectId; set one on the KnitHub work item.")?;
    let mut title = item.title.clone();
    if bundle_path(&root, &slugify(&title)).exists() {
        title = format!("{} {}", item.title, item.id);
    }
    let repos = item.repo_hints.clone();
    crate::commands::init::start_bundle(
        &title,
        Some(&project_id),
        &repos,
        false,
        None,
        &[],
        &[],
        true,
        false,
        false,
        true,
        None,
    )?;
    let bundle_id = slugify(&title);
    let bundle_path = bundle_path(&root, &bundle_id);
    let mut bundle: ChangeGroup = read_json(&bundle_path)?;
    push_unique(&mut bundle.work_item_ids, item.id.clone());
    write_json(&bundle_path, &bundle)?;
    push_unique(&mut item.bundle_ids, bundle_id.clone());
    item.execution_status = WORK_ITEM_EXECUTION_CLAIMED.to_string();
    if item.target.is_none() {
        item.target = Some("manual".to_string());
    }
    item.updated_at = now_iso();
    write_json(&path, &item)?;
    write_work_item_prompt(&root, &bundle_id, &item)?;
    println!("{} {}", out::heading("Bundle:"), out::repo(&bundle_id));
    println!(
        "{} {}",
        out::heading("Worktree:"),
        out::path(root.join(".knit/worktrees").join(&bundle_id).display())
    );
    Ok(())
}

fn write_work_item_prompt(root: &Path, bundle_id: &str, item: &KnitWorkItem) -> Result<()> {
    let text = work_item_prompt(item);
    let prompt_path = root
        .join(".knit/work-items")
        .join(format!("{}.prompt.md", item.id));
    fs::write(&prompt_path, &text)
        .with_context(|| format!("failed to write {}", prompt_path.display()))?;
    let worktree_path = root.join(".knit/worktrees").join(bundle_id);
    if worktree_path.exists() {
        let worktree_prompt = worktree_path.join("WORK_ITEM.md");
        fs::write(&worktree_prompt, text)
            .with_context(|| format!("failed to write {}", worktree_prompt.display()))?;
    }
    Ok(())
}

fn work_item_prompt(item: &KnitWorkItem) -> String {
    let mut text = format!("# {}\n\n", item.title);
    text.push_str(&format!("Work item: `{}`\n", item.id));
    text.push_str(&format!("Kind: `{}`\n", item.item_kind));
    if let Some(project_id) = &item.project_id {
        text.push_str(&format!("Project: `{project_id}`\n"));
    }
    if !item.repo_hints.is_empty() {
        text.push_str(&format!("Repo hints: `{}`\n", item.repo_hints.join("`, `")));
    }
    text.push('\n');
    if !item.description.trim().is_empty() {
        text.push_str(&item.description);
        text.push_str("\n\n");
    }
    if !item.acceptance_criteria.is_empty() {
        text.push_str("## Acceptance Criteria\n\n");
        for criterion in &item.acceptance_criteria {
            text.push_str(&format!("- {criterion}\n"));
        }
        text.push('\n');
    }
    if let Some(rationale) = &item.planning_rationale {
        text.push_str("## Planning Notes\n\n");
        text.push_str(rationale);
        text.push('\n');
    }
    text
}

fn current_root() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_knit_root(&cwd).context("No Knit workspace found.")
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}
