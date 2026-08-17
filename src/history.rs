use crate::ids::short_sha;
use crate::model::{
    BundleNode, ChangeGroup, CommitDetail, HistoryEvent, RepoChange, RepoEntry,
    HISTORY_EVENT_SCHEMA_VERSION,
};
use crate::store::{acquire_named_lock, history_path, load_config, project_path};
use crate::time::now_iso;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn record_bundle_history(root: &Path, bundle: &ChangeGroup) -> Result<usize> {
    let Some(project_id) = history_project_id(root, bundle)? else {
        return Ok(0);
    };

    let recorded = recorded_event_ids(root, &project_id)?;
    let mut lookup = CommitLookup::new(root);
    let events = events_for_bundle(&project_id, bundle, &mut lookup, Some(&recorded));
    append_history_events(root, &project_id, &events)
}

pub fn refresh_project_history(root: &Path, project_id: &str) -> Result<usize> {
    let recorded = recorded_event_ids(root, project_id)?;
    let mut lookup = CommitLookup::new(root);
    let mut appended = 0;
    for (_, bundle) in project_bundles(root, project_id)? {
        let events = events_for_bundle(project_id, &bundle, &mut lookup, Some(&recorded));
        appended += append_history_events(root, project_id, &events)?;
    }
    Ok(appended)
}

/// Outcome of a rebuild: how many recorded events were rewritten with fresher
/// detail, how many are new, and how many were kept because no bundle artifact
/// generates them any more.
#[derive(Debug, Clone, Copy, Default)]
pub struct RebuildSummary {
    pub replaced: usize,
    pub added: usize,
    pub preserved: usize,
}

/// Regenerate the whole project ledger from the bundle artifacts on disk,
/// replacing recorded events with their freshly enriched form. Event ids are
/// derived from the bundle/node/commit identity alone, so a regenerated event
/// lands on top of the one it supersedes and messages and times improve in
/// place. Events no artifact generates any more — a deleted bundle's — are
/// preserved, so a rebuild never loses recorded history.
pub fn rebuild_project_history(root: &Path, project_id: &str) -> Result<RebuildSummary> {
    let bundles = project_bundles(root, project_id)?;
    let mut lookup = CommitLookup::new(root);
    let mut generated = Vec::new();
    for (_, bundle) in &bundles {
        generated.extend(events_for_bundle(project_id, bundle, &mut lookup, None));
    }

    let _lock = acquire_named_lock(root, &format!("history-{project_id}"))?;
    let existing = load_history_events(root, project_id)?;
    let mut by_id = generated
        .into_iter()
        .map(|event| (event.event_id.clone(), event))
        .collect::<BTreeMap<_, _>>();

    let mut summary = RebuildSummary::default();
    let mut ordered = Vec::with_capacity(existing.len());
    for event in existing {
        match by_id.remove(&event.event_id) {
            Some(mut fresh) => {
                // The original record time is part of the ledger's account of
                // itself; only the projected detail is rewritten.
                fresh.recorded_at = event.recorded_at.clone();
                fresh.recorded_by = event.recorded_by.clone();
                summary.replaced += 1;
                ordered.push(fresh);
            }
            None => {
                summary.preserved += 1;
                ordered.push(event);
            }
        }
    }
    summary.added = by_id.len();
    ordered.extend(by_id.into_values());

    write_history_events(root, project_id, &ordered)?;
    Ok(summary)
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
    // One event is one line: a commit's body belongs to `git show`, not to a
    // history listing that repeats the same message once per repo.
    let message = message.lines().next().unwrap_or_default().trim_end();
    format!("{when}  {repo:<18} {sha:<8} {bundle:<18} {message}")
}

/// Rewrite the ledger in one atomic step. A rebuild replaces recorded lines
/// rather than appending, so a crash mid-write must not truncate the file.
fn write_history_events(root: &Path, project_id: &str, events: &[HistoryEvent]) -> Result<()> {
    let path = history_path(root, project_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut encoded = String::new();
    for event in events {
        encoded.push_str(&serde_json::to_string(event).context("failed to encode history event")?);
        encoded.push('\n');
    }

    let temp = path.with_extension("jsonl.tmp");
    fs::write(&temp, encoded).with_context(|| format!("failed to write {}", temp.display()))?;
    match fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error).with_context(|| format!("failed to replace {}", path.display()))
        }
    }
}

fn recorded_event_ids(root: &Path, project_id: &str) -> Result<BTreeSet<String>> {
    Ok(load_history_events(root, project_id)?
        .into_iter()
        .map(|event| event.event_id)
        .collect())
}

fn project_bundles(root: &Path, project_id: &str) -> Result<Vec<(PathBuf, ChangeGroup)>> {
    let bundle_dir = root.join(".knit/bundles");
    if !bundle_dir.exists() {
        return Ok(Vec::new());
    }

    let mut bundles = Vec::new();
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
        bundles.push((path, bundle));
    }
    bundles.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(bundles)
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

/// Names and times commits for history events. The ledger's own
/// `commitDetails` answer first; anything older falls back to the repo's
/// checkout, batched and cached so a sweep costs at most one git call per repo.
struct CommitLookup {
    root: PathBuf,
    checkouts: BTreeMap<String, Option<PathBuf>>,
    details: BTreeMap<(String, String), Option<CommitDetail>>,
}

impl CommitLookup {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            checkouts: BTreeMap::new(),
            details: BTreeMap::new(),
        }
    }

    /// Where this repo's commits can still be read: its bundle worktree while
    /// the bundle is live, otherwise the source checkout, whose feature
    /// branches outlive the generated worktree.
    fn checkout(&mut self, repo: &RepoEntry) -> Option<PathBuf> {
        if let Some(cached) = self.checkouts.get(&repo.id) {
            return cached.clone();
        }
        let resolved = repo
            .worktree_path
            .as_ref()
            .map(|path| {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    self.root.join(path)
                }
            })
            .filter(|path| path.exists())
            .or_else(|| Some(PathBuf::from(&repo.path)).filter(|path| path.exists()));
        self.checkouts.insert(repo.id.clone(), resolved.clone());
        resolved
    }

    fn prefetch(&mut self, repo: Option<&RepoEntry>, shas: &BTreeSet<String>) {
        let Some(repo) = repo else {
            return;
        };
        let wanted = shas
            .iter()
            .filter(|sha| {
                !self
                    .details
                    .contains_key(&(repo.id.clone(), (*sha).clone()))
            })
            .cloned()
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            return;
        }
        let found = match self.checkout(repo) {
            Some(checkout) => crate::git::commit_details(&checkout, &wanted),
            None => BTreeMap::new(),
        };
        for sha in wanted {
            let detail = found.get(&sha).cloned();
            self.details.insert((repo.id.clone(), sha), detail);
        }
    }

    fn detail(&self, repo_id: &str, sha: &str) -> Option<&CommitDetail> {
        self.details
            .get(&(repo_id.to_string(), sha.to_string()))
            .and_then(Option::as_ref)
    }
}

/// A commit-bearing history event before its detail is resolved.
struct CommitEvent<'a> {
    kind: String,
    event_id: String,
    repo_id: &'a str,
    sha: &'a str,
    change: Option<&'a RepoChange>,
    /// Ledger detail recorded when the commit was observed, if any.
    recorded: Option<&'a CommitDetail>,
    /// Whether the node's own message is the commit message and must win over
    /// the resolved subject (commit and revert groups).
    node_message_wins: bool,
    /// Whether the event needs no resolution at all: already in the ledger, or
    /// a node whose commits are pins rather than authored work.
    resolved: bool,
}

#[allow(clippy::too_many_arguments)]
fn commit_event<'a>(
    project_id: &str,
    bundle_id: &str,
    node: &'a BundleNode,
    kind: String,
    repo_id: &'a str,
    sha: &'a str,
    change: Option<&'a RepoChange>,
    pins: bool,
    node_message_wins: bool,
    recorded: Option<&BTreeSet<String>>,
) -> CommitEvent<'a> {
    let event_id = history_event_id(&[
        project_id,
        bundle_id,
        repo_id,
        &node.id,
        &node.node_type,
        &kind,
        sha,
    ]);
    let already = recorded.is_some_and(|recorded| recorded.contains(&event_id));
    CommitEvent {
        kind,
        event_id,
        repo_id,
        sha,
        change,
        recorded: change
            .filter(|_| !pins)
            .and_then(|change| change.commit_details.get(sha)),
        node_message_wins,
        resolved: pins || already,
    }
}

fn events_for_bundle(
    project_id: &str,
    bundle: &ChangeGroup,
    lookup: &mut CommitLookup,
    recorded: Option<&BTreeSet<String>>,
) -> Vec<HistoryEvent> {
    let repos = bundle
        .repos
        .iter()
        .map(|repo| (repo.id.as_str(), repo))
        .collect::<BTreeMap<_, _>>();
    let mut events = Vec::new();

    for node in &bundle.nodes {
        let pins = node_records_pins(&node.node_type);
        let node_message_wins = matches!(node.node_type.as_str(), "commit.group" | "revert.group");
        let mut candidates: Vec<CommitEvent> = Vec::new();
        let mut explicit_commits = BTreeSet::new();

        for commit in &node.commits {
            explicit_commits.insert((commit.repo_id.as_str(), commit.sha.as_str()));
            let change = node
                .repo_changes
                .iter()
                .find(|change| change.repo_id == commit.repo_id);
            candidates.push(commit_event(
                project_id,
                &bundle.id,
                node,
                event_kind_for_node(&node.node_type, false),
                &commit.repo_id,
                &commit.sha,
                change,
                pins,
                node_message_wins,
                recorded,
            ));
        }

        for change in &node.repo_changes {
            for sha in &change.commits {
                if explicit_commits.contains(&(change.repo_id.as_str(), sha.as_str())) {
                    continue;
                }
                candidates.push(commit_event(
                    project_id,
                    &bundle.id,
                    node,
                    event_kind_for_node(&node.node_type, false),
                    &change.repo_id,
                    sha,
                    Some(change),
                    pins,
                    node_message_wins,
                    recorded,
                ));
            }

            for sha in &change.dropped_commits {
                candidates.push(commit_event(
                    project_id,
                    &bundle.id,
                    node,
                    "commit.dropped".to_string(),
                    &change.repo_id,
                    sha,
                    Some(change),
                    pins,
                    node_message_wins,
                    recorded,
                ));
            }
        }

        // One git call per repo per node instead of one per commit.
        let mut wanted: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for candidate in &candidates {
            if candidate.resolved || candidate.recorded.is_some() {
                continue;
            }
            wanted
                .entry(candidate.repo_id)
                .or_default()
                .insert(candidate.sha.to_string());
        }
        for (repo_id, shas) in wanted {
            lookup.prefetch(repos.get(repo_id).copied(), &shas);
        }

        for candidate in candidates {
            let detail = candidate.recorded.cloned().or_else(|| {
                (!candidate.resolved)
                    .then(|| lookup.detail(candidate.repo_id, candidate.sha).cloned())
                    .flatten()
            });
            let subject = detail
                .as_ref()
                .map(|detail| detail.subject.trim())
                .filter(|subject| !subject.is_empty());
            let message = match (candidate.node_message_wins, subject) {
                (false, Some(subject)) => Some(subject.to_string()),
                _ => node.message.clone(),
            };
            let occurred_at = detail
                .as_ref()
                .map(|detail| detail.authored_at.clone())
                .filter(|authored_at| !authored_at.trim().is_empty())
                .unwrap_or_else(|| node.created_at.clone());

            events.push(history_event(
                project_id,
                bundle,
                repos.get(candidate.repo_id).copied(),
                candidate.event_id,
                &candidate.kind,
                Some(candidate.repo_id),
                Some(candidate.sha),
                candidate.change,
                &node.id,
                &node.node_type,
                node.commit_group_id.as_deref(),
                node.title.as_deref(),
                message.as_deref(),
                &occurred_at,
            ));
        }

        // Lifecycle: the ledger already records that a bundle was created,
        // landed or archived, but history only ever projected commits, so it
        // could not say any of it.
        let Some(kind) = lifecycle_kind(&node.node_type) else {
            continue;
        };
        let message = node
            .message
            .as_deref()
            .or(node.title.as_deref())
            .filter(|message| !message.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| lifecycle_message(kind).to_string());
        let repo_ids = node
            .repo_ids
            .clone()
            .filter(|repo_ids| kind.starts_with("repo.") && !repo_ids.is_empty())
            .unwrap_or_default();
        let targets = if repo_ids.is_empty() {
            vec![None]
        } else {
            repo_ids
                .iter()
                .map(|repo_id| Some(repo_id.clone()))
                .collect()
        };
        for repo_id in targets {
            let event_id = history_event_id(&[
                project_id,
                &bundle.id,
                repo_id.as_deref().unwrap_or(""),
                &node.id,
                &node.node_type,
                kind,
                "",
            ]);
            events.push(history_event(
                project_id,
                bundle,
                repo_id
                    .as_deref()
                    .and_then(|repo_id| repos.get(repo_id).copied()),
                event_id,
                kind,
                repo_id.as_deref(),
                None,
                None,
                &node.id,
                &node.node_type,
                node.commit_group_id.as_deref(),
                node.title.as_deref(),
                Some(&message),
                &node.created_at,
            ));
        }
    }

    events
}

#[allow(clippy::too_many_arguments)]
fn history_event(
    project_id: &str,
    bundle: &ChangeGroup,
    repo: Option<&RepoEntry>,
    event_id: String,
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
    HistoryEvent {
        schema_version: HISTORY_EVENT_SCHEMA_VERSION.to_string(),
        event_id,
        project_id: project_id.to_string(),
        kind: kind.to_string(),
        bundle_id: Some(bundle.id.clone()),
        bundle_title: Some(bundle.title.clone()),
        repo_id: repo_id.map(ToString::to_string),
        repo_remote: repo.and_then(|repo| repo.remote.clone()),
        base_branch: repo.map(|repo| repo.base_branch.clone()),
        branch: repo.and_then(|repo| repo.feature_branch.clone()),
        commit: commit.map(ToString::to_string),
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
        // The node title travels with the event so consumers (hosted dashboards) can
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

/// Nodes whose `commits` are per-repo head pins rather than authored work: a
/// tag or a check verdict names a state, and its node message is the record.
/// Naming or re-timing those from the pinned commits would misreport them.
fn node_records_pins(node_type: &str) -> bool {
    matches!(node_type, "tag.created" | "check.recorded")
}

fn lifecycle_kind(node_type: &str) -> Option<&'static str> {
    match node_type {
        "feature.created" => Some("bundle.created"),
        "feature.landed" => Some("bundle.landed"),
        "feature.archived" => Some("bundle.archived"),
        "repo.added" => Some("repo.added"),
        "repo.removed" => Some("repo.removed"),
        _ => None,
    }
}

fn lifecycle_message(kind: &str) -> &'static str {
    match kind {
        "bundle.created" => "Bundle created",
        "bundle.landed" => "Bundle landed",
        "bundle.archived" => "Bundle archived",
        "repo.added" => "Repo added to bundle",
        "repo.removed" => "Repo removed from bundle",
        _ => "Bundle updated",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BundleNode, CommitRef, Movement};

    fn bundle_with(nodes: Vec<BundleNode>) -> ChangeGroup {
        let mut bundle = ChangeGroup::new(
            "venue-capacity".to_string(),
            "venue capacity".to_string(),
            "2026-08-01T10:00:00Z".to_string(),
        );
        bundle.project_id = Some("demo".to_string());
        bundle.nodes.extend(nodes);
        bundle
    }

    fn observed_change(sha: &str, detail: Option<CommitDetail>) -> RepoChange {
        RepoChange {
            repo_id: "backend".to_string(),
            movement: Movement::Advanced,
            before_sha: Some("base000".to_string()),
            after_sha: sha.to_string(),
            commits: vec![sha.to_string()],
            dropped_commits: Vec::new(),
            commit_details: detail
                .map(|detail| BTreeMap::from([(sha.to_string(), detail)]))
                .unwrap_or_default(),
        }
    }

    fn events(bundle: &ChangeGroup) -> Vec<HistoryEvent> {
        let mut lookup = CommitLookup::new(Path::new("/knit-nonexistent-root"));
        events_for_bundle("demo", bundle, &mut lookup, None)
    }

    fn find<'a>(events: &'a [HistoryEvent], kind: &str) -> Vec<&'a HistoryEvent> {
        events.iter().filter(|event| event.kind == kind).collect()
    }

    #[test]
    fn observed_commits_use_recorded_subject_and_author_date() {
        let bundle = bundle_with(vec![BundleNode::git_observed(
            "node-observed".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            vec![observed_change(
                "aaa111",
                Some(CommitDetail {
                    subject: "Tighten capacity validation".to_string(),
                    authored_at: "2026-08-11T18:22:05+02:00".to_string(),
                }),
            )],
        )]);

        let events = events(&bundle);
        let observed = find(&events, "commit.observed");
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].message.as_deref(),
            Some("Tighten capacity validation")
        );
        assert_eq!(
            observed[0].occurred_at.as_deref(),
            Some("2026-08-11T18:22:05+02:00")
        );
    }

    #[test]
    fn commits_without_detail_fall_back_to_the_node() {
        let bundle = bundle_with(vec![BundleNode::git_observed(
            "node-observed".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            vec![observed_change("aaa111", None)],
        )]);

        let events = events(&bundle);
        let observed = find(&events, "commit.observed");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].message, None);
        assert_eq!(
            observed[0].occurred_at.as_deref(),
            Some("2026-08-14T09:00:00Z")
        );
    }

    #[test]
    fn commit_groups_keep_the_node_message_but_take_the_author_date() {
        let mut change = observed_change(
            "bbb222",
            Some(CommitDetail {
                subject: "Add capacity form".to_string(),
                authored_at: "2026-08-12T07:30:00Z".to_string(),
            }),
        );
        change.before_sha = None;
        let bundle = bundle_with(vec![BundleNode::commit_group(
            "group-1".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            "Add capacity form\n\nWith a longer body.".to_string(),
            vec![CommitRef {
                repo_id: "backend".to_string(),
                sha: "bbb222".to_string(),
            }],
            vec![change],
        )]);

        let events = events(&bundle);
        let recorded = find(&events, "commit.recorded");
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].message.as_deref(),
            Some("Add capacity form\n\nWith a longer body.")
        );
        assert_eq!(
            recorded[0].occurred_at.as_deref(),
            Some("2026-08-12T07:30:00Z")
        );
    }

    #[test]
    fn dropped_commits_are_named_when_their_subject_is_known() {
        let mut change = observed_change("ccc333", None);
        change.movement = Movement::Rewound;
        change.commits = Vec::new();
        change.dropped_commits = vec!["ccc333".to_string()];
        change.commit_details = BTreeMap::from([(
            "ccc333".to_string(),
            CommitDetail {
                subject: "Typo in seat map".to_string(),
                authored_at: "2026-08-10T11:00:00Z".to_string(),
            },
        )]);
        let bundle = bundle_with(vec![BundleNode::git_observed(
            "node-rewind".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            vec![change],
        )]);

        let dropped = events(&bundle);
        let dropped = find(&dropped, "commit.dropped");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].message.as_deref(), Some("Typo in seat map"));
    }

    #[test]
    fn tag_pins_keep_their_node_message_and_time() {
        let bundle = bundle_with(vec![BundleNode::tag_created(
            "node-tag".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            "v1".to_string(),
            "Known-good release".to_string(),
            vec![CommitRef {
                repo_id: "backend".to_string(),
                sha: "ddd444".to_string(),
            }],
        )]);

        let events = events(&bundle);
        let tagged = find(&events, "commit.tagged");
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].message.as_deref(), Some("Known-good release"));
        assert_eq!(
            tagged[0].occurred_at.as_deref(),
            Some("2026-08-14T09:00:00Z")
        );
        assert_eq!(tagged[0].node_type.as_deref(), Some("tag.created"));
    }

    #[test]
    fn lifecycle_nodes_become_events_and_worktrees_do_not() {
        let bundle = bundle_with(vec![
            BundleNode::repos_added(
                "node-repos".to_string(),
                "2026-08-01T10:01:00Z".to_string(),
                vec!["backend".to_string(), "frontend".to_string()],
            ),
            BundleNode::worktrees_materialized(
                "node-worktrees".to_string(),
                "2026-08-01T10:02:00Z".to_string(),
                vec!["backend".to_string()],
            ),
            BundleNode::repos_removed(
                "node-removed".to_string(),
                "2026-08-01T10:03:00Z".to_string(),
                vec!["frontend".to_string()],
            ),
            BundleNode::feature_landed(
                "node-landed".to_string(),
                "2026-08-15T12:00:00Z".to_string(),
                "plan-1".to_string(),
                "run-1".to_string(),
                "github".to_string(),
                vec!["backend".to_string()],
                Vec::new(),
            ),
            BundleNode::feature_archived(
                "node-archived".to_string(),
                "2026-08-15T12:05:00Z".to_string(),
                Some("landed".to_string()),
            ),
        ]);

        let events = events(&bundle);
        let created = find(&events, "bundle.created");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].message.as_deref(), Some("venue capacity"));
        assert_eq!(created[0].commit, None);
        assert_eq!(created[0].repo_id, None);
        assert_eq!(
            created[0].occurred_at.as_deref(),
            Some("2026-08-01T10:00:00Z")
        );

        let added = find(&events, "repo.added");
        assert_eq!(
            added
                .iter()
                .map(|event| event.repo_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["backend", "frontend"]
        );
        assert_eq!(added[0].message.as_deref(), Some("Repo added to bundle"));

        assert_eq!(find(&events, "repo.removed").len(), 1);
        assert_eq!(find(&events, "bundle.landed").len(), 1);
        let archived = find(&events, "bundle.archived");
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].message.as_deref(), Some("landed"));

        assert!(events
            .iter()
            .all(|event| event.node_type.as_deref() != Some("worktree.materialized")));
    }

    #[test]
    fn lifecycle_and_commit_events_have_distinct_stable_ids() {
        let bundle = bundle_with(vec![BundleNode::git_observed(
            "node-observed".to_string(),
            "2026-08-14T09:00:00Z".to_string(),
            vec![observed_change("aaa111", None)],
        )]);

        let first = events(&bundle);
        let second = events(&bundle);
        let ids = first
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), first.len());
        assert_eq!(
            ids,
            second
                .iter()
                .map(|event| event.event_id.clone())
                .collect::<BTreeSet<_>>()
        );
        // The pre-existing commit event id must not move: it keys every event
        // already recorded in every workspace ledger.
        assert_eq!(
            first
                .iter()
                .find(|event| event.kind == "commit.observed")
                .unwrap()
                .event_id,
            history_event_id(&[
                "demo",
                "venue-capacity",
                "backend",
                "node-observed",
                "git.observed",
                "commit.observed",
                "aaa111",
            ])
        );
    }

    #[test]
    fn listing_prints_only_the_first_line_of_a_message() {
        let bundle = bundle_with(Vec::new());
        let mut event = events(&bundle).remove(0);
        event.message = Some("Add capacity form\n\nWith a longer body.".to_string());
        let line = format_history_event(&event);
        assert!(line.ends_with("Add capacity form"), "{line}");
        assert!(!line.contains('\n'), "{line}");
    }

    #[test]
    fn listing_shows_dashes_for_events_without_repo_or_commit() {
        let bundle = bundle_with(Vec::new());
        let line = format_history_event(&events(&bundle)[0]);
        assert!(line.contains("-        "), "{line}");
        assert!(line.ends_with("venue capacity"), "{line}");
    }
}
