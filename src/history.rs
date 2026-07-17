use crate::ids::short_sha;
use crate::model::{
    ChangeGroup, HistoryEvent, RepoChange, RepoEntry, HISTORY_EVENT_SCHEMA_VERSION,
};
use crate::store::{acquire_named_lock, history_path, load_config, project_path};
use crate::time::now_iso;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub fn record_bundle_history(root: &Path, bundle: &ChangeGroup) -> Result<usize> {
    let Some(project_id) = history_project_id(root, bundle)? else {
        return Ok(0);
    };

    let events = events_for_bundle(&project_id, bundle);
    append_history_events(root, &project_id, &events)
}

pub fn refresh_project_history(root: &Path, project_id: &str) -> Result<usize> {
    let bundle_dir = root.join(".knit/bundles");
    if !bundle_dir.exists() {
        return Ok(0);
    }

    let mut appended = 0;
    for entry in fs::read_dir(&bundle_dir)
        .with_context(|| format!("failed to read {}", bundle_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bundle: ChangeGroup = crate::store::read_json(&path)
            .with_context(|| format!("failed to read bundle {}", path.display()))?;
        if history_project_id(root, &bundle)?.as_deref() != Some(project_id) {
            continue;
        }
        appended +=
            append_history_events(root, project_id, &events_for_bundle(project_id, &bundle))?;
    }
    Ok(appended)
}

pub fn load_history_events(root: &Path, project_id: &str) -> Result<Vec<HistoryEvent>> {
    let path = history_path(root, project_id);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        events.push(serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse history event at {}:{}",
                path.display(),
                index + 1
            )
        })?);
    }
    Ok(events)
}

pub fn append_history_events(
    root: &Path,
    project_id: &str,
    events: &[HistoryEvent],
) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }

    let _lock = acquire_named_lock(root, &format!("history-{project_id}"))?;
    let path = history_path(root, project_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut seen = load_history_events(root, project_id)?
        .into_iter()
        .map(|event| event.event_id)
        .collect::<BTreeSet<_>>();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;

    let mut appended = 0;
    for event in events {
        if !seen.insert(event.event_id.clone()) {
            continue;
        }
        let encoded = serde_json::to_string(event).context("failed to encode history event")?;
        writeln!(file, "{encoded}")
            .with_context(|| format!("failed to append {}", path.display()))?;
        appended += 1;
    }

    Ok(appended)
}

pub fn format_history_event(event: &HistoryEvent) -> String {
    let repo = event.repo_id.as_deref().unwrap_or("-");
    let sha = event
        .commit
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let bundle = event.bundle_id.as_deref().unwrap_or("-");
    let when = event.occurred_at.as_deref().unwrap_or(&event.recorded_at);
    let message = event
        .message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(&event.kind);
    format!("{when}  {repo:<18} {sha:<8} {bundle:<18} {message}")
}

fn history_project_id(root: &Path, bundle: &ChangeGroup) -> Result<Option<String>> {
    let project_id = bundle.project_id.clone().or_else(|| {
        load_config(root)
            .ok()
            .and_then(|config| config.active_project)
    });
    let Some(project_id) = project_id else {
        return Ok(None);
    };
    if project_path(root, &project_id).exists() {
        Ok(Some(project_id))
    } else {
        Ok(None)
    }
}

fn events_for_bundle(project_id: &str, bundle: &ChangeGroup) -> Vec<HistoryEvent> {
    let repos = bundle
        .repos
        .iter()
        .map(|repo| (repo.id.as_str(), repo))
        .collect::<BTreeMap<_, _>>();
    let mut events = Vec::new();

    for node in &bundle.nodes {
        let mut explicit_commits = BTreeSet::new();
        for commit in &node.commits {
            explicit_commits.insert((commit.repo_id.clone(), commit.sha.clone()));
            let change = node
                .repo_changes
                .iter()
                .find(|change| change.repo_id == commit.repo_id);
            events.push(history_event(
                project_id,
                bundle,
                repos.get(commit.repo_id.as_str()).copied(),
                &event_kind_for_node(&node.node_type, false),
                Some(&commit.repo_id),
                Some(&commit.sha),
                change,
                &node.id,
                &node.node_type,
                node.commit_group_id.as_deref(),
                node.title.as_deref(),
                node.message.as_deref(),
                &node.created_at,
            ));
        }

        for change in &node.repo_changes {
            for sha in &change.commits {
                if explicit_commits.contains(&(change.repo_id.clone(), sha.clone())) {
                    continue;
                }
                events.push(history_event(
                    project_id,
                    bundle,
                    repos.get(change.repo_id.as_str()).copied(),
                    &event_kind_for_node(&node.node_type, false),
                    Some(&change.repo_id),
                    Some(sha),
                    Some(change),
                    &node.id,
                    &node.node_type,
                    node.commit_group_id.as_deref(),
                    node.title.as_deref(),
                    node.message.as_deref(),
                    &node.created_at,
                ));
            }

            for sha in &change.dropped_commits {
                events.push(history_event(
                    project_id,
                    bundle,
                    repos.get(change.repo_id.as_str()).copied(),
                    "commit.dropped",
                    Some(&change.repo_id),
                    Some(sha),
                    Some(change),
                    &node.id,
                    &node.node_type,
                    node.commit_group_id.as_deref(),
                    node.title.as_deref(),
                    node.message.as_deref(),
                    &node.created_at,
                ));
            }
        }
    }

    events
}

#[allow(clippy::too_many_arguments)]
fn history_event(
    project_id: &str,
    bundle: &ChangeGroup,
    repo: Option<&RepoEntry>,
    kind: &str,
    repo_id: Option<&str>,
    commit: Option<&str>,
    change: Option<&RepoChange>,
    node_id: &str,
    node_type: &str,
    commit_group_id: Option<&str>,
    title: Option<&str>,
    message: Option<&str>,
    occurred_at: &str,
) -> HistoryEvent {
    let repo_id = repo_id.map(ToString::to_string);
    let commit = commit.map(ToString::to_string);
    let event_id = history_event_id(&[
        project_id,
        &bundle.id,
        repo_id.as_deref().unwrap_or(""),
        node_id,
        node_type,
        kind,
        commit.as_deref().unwrap_or(""),
    ]);

    HistoryEvent {
        schema_version: HISTORY_EVENT_SCHEMA_VERSION.to_string(),
        event_id,
        project_id: project_id.to_string(),
        kind: kind.to_string(),
        bundle_id: Some(bundle.id.clone()),
        bundle_title: Some(bundle.title.clone()),
        repo_id,
        repo_remote: repo.and_then(|repo| repo.remote.clone()),
        base_branch: repo.map(|repo| repo.base_branch.clone()),
        branch: repo.and_then(|repo| repo.feature_branch.clone()),
        commit,
        before_sha: change.and_then(|change| change.before_sha.clone()),
        after_sha: change.map(|change| change.after_sha.clone()),
        movement: change.map(|change| change.movement),
        node_id: Some(node_id.to_string()),
        node_type: Some(node_type.to_string()),
        commit_group_id: commit_group_id.map(ToString::to_string),
        message: message.map(ToString::to_string),
        occurred_at: Some(occurred_at.to_string()),
        recorded_at: now_iso(),
        recorded_by: "knit".to_string(),
        // The node title travels with the event so consumers (KnitHub) can
        // name titled nodes — a tag's name, a check's name — without parsing
        // the message text.
        metadata: title.map(|title| serde_json::json!({ "title": title })),
    }
}

fn event_kind_for_node(node_type: &str, dropped: bool) -> String {
    if dropped {
        return "commit.dropped".to_string();
    }
    match node_type {
        "git.observed" => "commit.observed",
        "revert.group" => "commit.reverted",
        "land.update" => "commit.integrated",
        "tag.created" => "commit.tagged",
        _ => "commit.recorded",
    }
    .to_string()
}

fn history_event_id(parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("khist_{hash:016x}")
}
