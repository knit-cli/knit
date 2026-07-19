//! Remote sync for project history events.

use super::client::{
    effective_workspace_config, load_project_if_present, request_json, resolve_project_id,
    resolve_remote, resolve_token, with_first_available_remote,
};
use crate::history::{append_history_events, load_history_events, refresh_project_history};
use crate::model::{HistoryEvent, KnitRemote};
use crate::output as out;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteHistoryPush {
    inserted_count: usize,
    skipped_count: usize,
}

pub fn push_history_to_remote(project: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, project)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let local_project = load_project_if_present(&root, &project_id)?;
    let remote_project = super::push::upsert_project_for_history(
        remote,
        &token,
        &project_id,
        local_project.as_ref(),
    )?;
    if let Some(project) = local_project.as_ref() {
        super::push::push_repositories_for_history(
            remote,
            &token,
            &remote_project.slug,
            &project.repos,
        )?;
    }
    let pushed =
        push_project_history_events(remote, &token, &remote_project.slug, &root, &project_id)?;
    println!(
        "{} {} {}",
        out::movement("pushed history"),
        out::repo(&project_id),
        out::muted(format!("{pushed} event(s)"))
    );
    Ok(())
}

pub fn pull_history_from_remote(project: Option<&str>, remote_name: Option<&str>) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, project)?;
    let events = with_first_available_remote(&config, remote_name, |_, remote, token| {
        fetch_project_history_events(remote, token, &project_id)
    })?;
    let appended = append_history_events(&root, &project_id, &events)?;
    println!(
        "{} {} {}",
        out::movement("pulled history"),
        out::repo(&project_id),
        out::muted(format!("{appended} new event(s)"))
    );
    Ok(())
}

pub(super) fn push_project_history_events(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    root: &Path,
    project_id: &str,
) -> Result<usize> {
    refresh_project_history(root, project_id)?;
    let events = load_history_events(root, project_id)?;
    if events.is_empty() {
        return Ok(0);
    }
    let payload = json!({ "events": events });
    let response: RemoteHistoryPush = request_json(
        remote,
        token,
        "POST",
        &format!("/projects/{project_slug}/history-events"),
        Some(&payload),
    )?;
    Ok(response.inserted_count + response.skipped_count)
}

pub(super) fn fetch_project_history_events(
    remote: &KnitRemote,
    token: &str,
    project_identifier: &str,
) -> Result<Vec<HistoryEvent>> {
    let raw: Vec<serde_json::Value> = request_json(
        remote,
        token,
        "GET",
        &format!("/projects/{project_identifier}/history-events"),
        None,
    )
    .with_context(|| format!("failed to fetch history for project `{project_identifier}`"))?;
    Ok(super::decode_history_events(&raw, project_identifier))
}
