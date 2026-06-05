use crate::history::{format_history_event, load_history_events, refresh_project_history};
use crate::ids::slugify;
use crate::output as out;
use crate::store::{find_knit_root, load_config};
use anyhow::{Context, Result};

pub fn show_history(
    project: Option<&str>,
    limit: usize,
    repo: Option<&str>,
    bundle: Option<&str>,
) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let appended = refresh_project_history(&root, &project_id)?;
    if appended > 0 {
        println!(
            "{} {} new event(s)",
            out::heading("History refreshed:"),
            appended
        );
    }

    let mut events = load_history_events(&root, &project_id)?;
    events.retain(|event| {
        repo.is_none_or(|repo| event.repo_id.as_deref() == Some(repo))
            && bundle.is_none_or(|bundle| event.bundle_id.as_deref() == Some(bundle))
    });
    events.sort_by(|a, b| {
        let a_time = a.occurred_at.as_deref().unwrap_or(&a.recorded_at);
        let b_time = b.occurred_at.as_deref().unwrap_or(&b.recorded_at);
        a_time.cmp(b_time).then(a.event_id.cmp(&b.event_id))
    });

    if events.is_empty() {
        println!("{}", out::muted("No history events recorded yet."));
        return Ok(());
    }

    let start = events.len().saturating_sub(limit);
    for event in events.into_iter().skip(start) {
        println!("{}", format_history_event(&event));
    }
    Ok(())
}

pub fn refresh_history(project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let appended = refresh_project_history(&root, &project_id)?;
    println!(
        "{} {} {}",
        out::movement("refreshed"),
        out::repo(&project_id),
        out::muted(format!("{appended} new event(s)"))
    );
    Ok(())
}

pub fn push_history(project: Option<&str>, remote: &str) -> Result<()> {
    crate::commands::remote::push_history_to_remote(project, remote)
}

pub fn pull_history(project: Option<&str>, remote: &str) -> Result<()> {
    crate::commands::remote::pull_history_from_remote(project, remote)
}

pub fn sync_history(project: Option<&str>, remote: &str) -> Result<()> {
    crate::commands::remote::sync_history_with_remote(project, remote)
}

fn resolve_project(project: Option<&str>) -> Result<(std::path::PathBuf, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project
        .map(slugify)
        .or(config.active_project)
        .context("No active project selected. Pass --project or run `knit project init <name>`.")?;
    Ok((root, project_id))
}
