//! Project-wide workspace state that is intentionally independent of bundle context.

use crate::commands::base::inspect_base;
use crate::commands::bundle::list_open_bundle_ids;
use crate::commands::project::load_project_by_id;
use crate::ids::short_sha;
use crate::output as out;
use crate::store::{find_knit_root, load_config};
use anyhow::{Context, Result};
use std::path::Path;

pub fn show_workspace_status() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = config
        .active_project
        .context("No active Knit project. Run `knit init <name>` first.")?;
    let project = load_project_by_id(&root, &project_id)?;

    println!("{} {}", out::heading("Project:"), out::repo(&project_id));
    for repo in &project.repos {
        let state = inspect_base(Path::new(&repo.path), &repo.base_branch)
            .with_context(|| format!("{}: failed to inspect workspace state", repo.id))?;
        let local = state
            .local_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "missing".to_string());
        let remote = state
            .remote_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "missing".to_string());
        let ahead = state
            .ahead
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let behind = state
            .behind
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let cleanliness = if state.dirty { "dirty" } else { "clean" };
        println!(
            "  {} current={} base={} local={} origin={} ahead={} behind={} {}",
            out::repo(&repo.id),
            out::branch(&state.current_branch),
            out::branch(&repo.base_branch),
            out::sha(local),
            out::sha(remote),
            ahead,
            behind,
            out::muted(cleanliness)
        );
    }

    let bundles = list_open_bundle_ids(&root)?;
    if bundles.is_empty() {
        println!("{} 0", out::heading("Open bundles:"));
    } else {
        println!(
            "{} {} ({})",
            out::heading("Open bundles:"),
            bundles.len(),
            bundles.join(", ")
        );
    }
    Ok(())
}
