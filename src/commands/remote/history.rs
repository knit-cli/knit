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
    // Older sync remotes report only inserted/skipped.
    #[serde(default)]
    updated_count: usize,
    #[serde(default)]
    failed_count: usize,
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
    // Batched so a project ledger of thousands of events never rides in one
    // request body; each batch upserts independently and is idempotent.
    let mut accepted = 0;
    let mut failed = 0;
    for batch in events.chunks(HISTORY_PAGE_SIZE) {
        let payload = json!({ "events": batch });
        let response: RemoteHistoryPush = request_json(
            remote,
            token,
            "POST",
            &format!("/projects/{project_slug}/history-events"),
            Some(&payload),
        )?;
        accepted += response.inserted_count + response.updated_count + response.skipped_count;
        failed += response.failed_count;
    }
    if failed > 0 {
        eprintln!(
            "{} {failed} history event(s) were rejected by the sync remote and are missing there; the next push retries them",
            out::warn("warning:")
        );
    }
    Ok(accepted)
}

/// How many history events ride in one request, both directions.
const HISTORY_PAGE_SIZE: usize = 500;

/// Fetch a project's history events page by page with the server's keyset
/// cursor (`before` + `beforeId`, taken from the last event of each page), so
/// a ledger of any size never arrives as one response. A server that ignores
/// the cursor resends the newest page; the no-new-events guard turns that
/// into "stop with what one page held" instead of a loop.
pub(super) fn fetch_project_history_events(
    remote: &KnitRemote,
    token: &str,
    project_identifier: &str,
) -> Result<Vec<HistoryEvent>> {
    let base_path = format!("/projects/{project_identifier}/history-events");
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cursor: Option<(String, String)> = None;

    loop {
        let path = match &cursor {
            Some((before, before_id)) => format!(
                "{base_path}?limit={HISTORY_PAGE_SIZE}&before={before}&beforeId={before_id}"
            ),
            None => format!("{base_path}?limit={HISTORY_PAGE_SIZE}"),
        };
        let page: Vec<serde_json::Value> = request_json(remote, token, "GET", &path, None)
            .with_context(|| {
                format!("failed to fetch history for project `{project_identifier}`")
            })?;
        let page_len = page.len();

        let mut new_events = 0;
        for event in page {
            let Some(event_id) = event.get("eventId").and_then(|value| value.as_str()) else {
                continue;
            };
            if seen.insert(event_id.to_string()) {
                all.push(event);
                new_events += 1;
            }
        }
        if page_len < HISTORY_PAGE_SIZE || new_events == 0 {
            break;
        }

        let Some(last) = all.last() else { break };
        let next = last
            .get("occurredAt")
            .and_then(|value| value.as_str())
            .zip(last.get("eventId").and_then(|value| value.as_str()))
            .map(|(before, before_id)| (before.to_string(), before_id.to_string()));
        // Without a usable cursor another request could only repeat this page.
        let Some(next) = next else { break };
        cursor = Some(next);
    }

    Ok(super::decode_history_events(&all, project_identifier))
}
