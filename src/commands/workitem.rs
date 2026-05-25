use crate::ids::slugify;
use crate::model::{
    ChangeGroup, KnitWorkItem, WORK_ITEM_EXECUTION_CLAIMED, WORK_ITEM_PLANNING_APPROVED,
};
use crate::output as out;
use crate::store::{
    acquire_named_lock, bundle_path, find_knit_root, load_config, project_path, read_json,
    work_item_path, write_json,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

pub fn add_work_item(
    title: &str,
    item_kind: &str,
    description: Option<&str>,
    project: Option<&str>,
    org: Option<&str>,
    repo_hints: &[String],
    depends_on: &[String],
    labels: &[String],
    acceptance_criteria: &[String],
    priority: Option<&str>,
) -> Result<()> {
    let root = current_root_or_cwd()?;
    let config = load_config(&root).ok();
    fs::create_dir_all(root.join(".knit/work-items"))
        .context("failed to create .knit/work-items")?;
    let id = unique_work_item_id(&root, &slugify(title));
    let now = now_iso();
    let mut item = KnitWorkItem::new(id.clone(), title.to_string(), now);
    item.item_kind = normalize_item_kind(item_kind)?;
    item.description = description.unwrap_or_default().to_string();
    item.project_id = project.map(slugify).or_else(|| {
        config
            .as_ref()
            .and_then(|config| config.active_project.clone())
    });
    item.org_id = org.map(slugify).or_else(|| {
        item.project_id.as_ref().and_then(|project_id| {
            let path = project_path(&root, project_id);
            read_json::<crate::model::KnitProject>(&path)
                .ok()
                .and_then(|project| project.org_id)
        })
    });
    item.repo_hints = repo_hints.iter().map(|repo| slugify(repo)).collect();
    item.depends_on = depends_on.iter().map(|id| slugify(id)).collect();
    item.labels = labels.iter().map(|label| slugify(label)).collect();
    item.acceptance_criteria = acceptance_criteria.to_vec();
    item.priority = priority.map(ToOwned::to_owned);

    let path = work_item_path(&root, &id);
    write_json(&path, &item)?;
    println!("{} {}", out::heading("Work item:"), out::repo(&id));
    println!("{} {}", out::heading("Path:"), out::path(path.display()));
    Ok(())
}

pub fn list_work_items(project: Option<&str>, all: bool) -> Result<()> {
    let root = current_root()?;
    let items = load_work_items(&root)?;
    let project = project.map(slugify);
    let items = items
        .into_iter()
        .filter(|item| all || project.is_none() || item.project_id.as_deref() == project.as_deref())
        .collect::<Vec<_>>();
    if items.is_empty() {
        println!("{}", out::muted("No work items."));
        return Ok(());
    }
    for item in items {
        println!(
            "{} {:<9} {:<14} {:<18} {}",
            out::repo(&item.id),
            item.item_kind,
            item.planning_status,
            item.execution_status,
            item.title
        );
    }
    Ok(())
}

pub fn show_work_item(id: &str) -> Result<()> {
    let root = current_root()?;
    let item = load_work_item(&root, id)?;
    println!("{}", serde_json::to_string_pretty(&item)?);
    Ok(())
}

pub fn update_work_item(
    id: &str,
    title: Option<&str>,
    description: Option<&str>,
    planning_status: Option<&str>,
    execution_status: Option<&str>,
    lane: Option<&str>,
    rank: Option<u32>,
    rationale: Option<&str>,
    planner: Option<&str>,
    target: Option<&str>,
    last_outcome: Option<&str>,
    depends_on: &[String],
    repo_hints: &[String],
    bundle_ids: &[String],
) -> Result<()> {
    let root = current_root()?;
    let item_id = slugify(id);
    let _lock = acquire_named_lock(&root, &format!("work-item-{item_id}"))?;
    let path = work_item_path(&root, &item_id);
    let mut item: KnitWorkItem = read_json(&path)?;
    if let Some(title) = title {
        item.title = title.to_string();
    }
    if let Some(description) = description {
        item.description = description.to_string();
    }
    if let Some(status) = planning_status {
        item.planning_status = status.to_string();
        if status == "plotted" {
            item.plotted_at = Some(now_iso());
        }
    }
    if let Some(status) = execution_status {
        item.execution_status = status.to_string();
    }
    if let Some(lane) = lane {
        item.lane = Some(lane.to_string());
    }
    if let Some(rank) = rank {
        item.rank = Some(rank);
    }
    if let Some(rationale) = rationale {
        item.planning_rationale = Some(rationale.to_string());
    }
    if let Some(planner) = planner {
        item.planner = Some(planner.to_string());
    }
    if let Some(target) = target {
        item.target = Some(target.to_string());
    }
    if let Some(outcome) = last_outcome {
        item.last_outcome = Some(outcome.to_string());
    }
    if !depends_on.is_empty() {
        item.depends_on = depends_on.iter().map(|id| slugify(id)).collect();
    }
    if !repo_hints.is_empty() {
        item.repo_hints = repo_hints.iter().map(|repo| slugify(repo)).collect();
    }
    if !bundle_ids.is_empty() {
        for bundle_id in bundle_ids {
            push_unique(&mut item.bundle_ids, slugify(bundle_id));
        }
    }
    item.updated_at = now_iso();
    write_json(&path, &item)?;
    println!(
        "{} {}",
        out::heading("Updated work item:"),
        out::repo(item.id)
    );
    Ok(())
}

pub fn approve_work_item(id: &str) -> Result<()> {
    let root = current_root()?;
    let item_id = slugify(id);
    let _lock = acquire_named_lock(&root, &format!("work-item-{item_id}"))?;
    let path = work_item_path(&root, &item_id);
    let mut item: KnitWorkItem = read_json(&path)?;
    let now = now_iso();
    item.planning_status = WORK_ITEM_PLANNING_APPROVED.to_string();
    item.approved_at = Some(now.clone());
    item.updated_at = now;
    write_json(&path, &item)?;
    println!(
        "{} {}",
        out::heading("Approved work item:"),
        out::repo(item.id)
    );
    Ok(())
}

pub fn export_work_items(project: Option<&str>, all: bool) -> Result<()> {
    let root = current_root()?;
    let project = project.map(slugify);
    let items = load_work_items(&root)?
        .into_iter()
        .filter(|item| all || project.is_none() || item.project_id.as_deref() == project.as_deref())
        .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
}

pub fn start_work_item(id: &str, target: Option<&str>) -> Result<()> {
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
        .context("Work item has no projectId; pass a project when creating it.")?;
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
        true,
        false,
        false,
        true,
    )?;
    let bundle_id = slugify(&title);
    let bundle_path = bundle_path(&root, &bundle_id);
    let mut bundle: ChangeGroup = read_json(&bundle_path)?;
    push_unique(&mut bundle.work_item_ids, item.id.clone());
    write_json(&bundle_path, &bundle)?;
    push_unique(&mut item.bundle_ids, bundle_id.clone());
    item.execution_status = WORK_ITEM_EXECUTION_CLAIMED.to_string();
    item.target = target
        .map(ToOwned::to_owned)
        .or_else(|| Some("manual".to_string()));
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

pub fn load_work_items(root: &Path) -> Result<Vec<KnitWorkItem>> {
    let dir = root.join(".knit/work-items");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().map(|path| read_json(&path)).collect()
}

fn load_work_item(root: &Path, id: &str) -> Result<KnitWorkItem> {
    read_json(&work_item_path(root, &slugify(id)))
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

fn current_root_or_cwd() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Ok(find_knit_root(&cwd).unwrap_or(cwd))
}

fn unique_work_item_id(root: &Path, desired_id: &str) -> String {
    if !work_item_path(root, desired_id).exists() {
        return desired_id.to_string();
    }
    for index in 2.. {
        let candidate = format!("{desired_id}-{index}");
        if !work_item_path(root, &candidate).exists() {
            return candidate;
        }
    }
    unreachable!("unbounded iterator should always find a work item id")
}

fn normalize_item_kind(kind: &str) -> Result<String> {
    let normalized = slugify(kind);
    match normalized.as_str() {
        "feature" | "bug" | "chore" | "investigation" => Ok(normalized),
        _ => bail!("Work item kind must be feature, bug, chore, or investigation."),
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}
