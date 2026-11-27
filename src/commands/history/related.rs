//! The `knit related` join: git commits that touched a path, matched to
//! Knit history events to recover bundle scope and the companion commits in
//! other repos, plus the rendering of the result.

use super::target::RelatedTarget;
use crate::git::git_output;
use crate::ids::short_sha;
use crate::model::{HistoryEvent, KnitProject};
use crate::output as out;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(super) struct GitCommit {
    pub(super) sha: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct RelationKey {
    bundle_id: Option<String>,
    scope: RelationScope,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum RelationScope {
    CommitGroup(String),
    Node(String),
    Bundle(String),
    Event(String),
}

#[derive(Debug, Clone)]
pub(super) struct RelatedInstance {
    pub(super) key: RelationKey,
    pub(super) bundle_id: Option<String>,
    pub(super) bundle_title: Option<String>,
    pub(super) matched: Vec<HistoryEvent>,
    pub(super) related: Vec<HistoryEvent>,
    pub(super) other_bundle: Vec<HistoryEvent>,
}

impl RelatedInstance {
    pub(super) fn scope_label(&self) -> String {
        match &self.key.scope {
            RelationScope::CommitGroup(id) => format!("commit group {id}"),
            RelationScope::Node(id) => format!("node {id}"),
            RelationScope::Bundle(id) => format!("bundle {id}"),
            RelationScope::Event(id) => format!("event {id}"),
        }
    }
}

pub(super) fn git_commits_for_paths(
    repo: &Path,
    paths: &[String],
    commit_limit: usize,
) -> Result<Vec<GitCommit>> {
    let mut args = vec![
        OsString::from("log"),
        OsString::from(format!("-n{commit_limit}")),
        OsString::from("--format=%H"),
        OsString::from("--"),
    ];
    args.extend(paths.iter().map(OsString::from));
    let output = git_output(repo, args)?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|sha| GitCommit {
            sha: sha.to_string(),
        })
        .collect())
}

pub(super) fn related_instances(
    events: &[HistoryEvent],
    repo_id: &str,
    commits: &BTreeSet<String>,
) -> Vec<RelatedInstance> {
    let mut matches_by_key = BTreeMap::<RelationKey, Vec<HistoryEvent>>::new();
    for event in events {
        if event.repo_id.as_deref() == Some(repo_id)
            && event
                .commit
                .as_deref()
                .is_some_and(|commit| commits.contains(commit))
        {
            matches_by_key
                .entry(relation_key(event))
                .or_default()
                .push(event.clone());
        }
    }

    matches_by_key
        .into_iter()
        .map(|(key, matched)| {
            let matched_ids = matched
                .iter()
                .map(|event| event.event_id.clone())
                .collect::<BTreeSet<_>>();
            let scoped_events = events
                .iter()
                .filter(|event| relation_matches(&key, event))
                .cloned()
                .collect::<Vec<_>>();
            let scoped_ids = scoped_events
                .iter()
                .map(|event| event.event_id.clone())
                .collect::<BTreeSet<_>>();
            let related = scoped_events
                .into_iter()
                .filter(|event| !matched_ids.contains(&event.event_id))
                .collect::<Vec<_>>();
            let other_bundle = match &key.bundle_id {
                Some(bundle_id) => events
                    .iter()
                    .filter(|event| event.bundle_id.as_deref() == Some(bundle_id))
                    .filter(|event| !matched_ids.contains(&event.event_id))
                    .filter(|event| !scoped_ids.contains(&event.event_id))
                    .cloned()
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            let representative = matched.first().cloned();

            RelatedInstance {
                key,
                bundle_id: representative
                    .as_ref()
                    .and_then(|event| event.bundle_id.clone()),
                bundle_title: representative
                    .as_ref()
                    .and_then(|event| event.bundle_title.clone()),
                matched,
                related,
                other_bundle,
            }
        })
        .collect()
}

fn relation_key(event: &HistoryEvent) -> RelationKey {
    let scope = if let Some(group) = &event.commit_group_id {
        RelationScope::CommitGroup(group.clone())
    } else if let Some(node) = &event.node_id {
        RelationScope::Node(node.clone())
    } else if let Some(bundle) = &event.bundle_id {
        RelationScope::Bundle(bundle.clone())
    } else {
        RelationScope::Event(event.event_id.clone())
    };

    RelationKey {
        bundle_id: event.bundle_id.clone(),
        scope,
    }
}

fn relation_matches(key: &RelationKey, event: &HistoryEvent) -> bool {
    if event.bundle_id != key.bundle_id {
        return false;
    }

    match &key.scope {
        RelationScope::CommitGroup(id) => event.commit_group_id.as_deref() == Some(id),
        RelationScope::Node(id) => event.node_id.as_deref() == Some(id),
        RelationScope::Bundle(id) => event.bundle_id.as_deref() == Some(id),
        RelationScope::Event(id) => event.event_id == *id,
    }
}

/// When the work in this scope happened: the latest of the matched and
/// same-scope events, which for a commit group is when its commits were made
/// and for a base commit is its commit time. The rest of the bundle is
/// context, not this scope: counting it let a later archive or tag stand in
/// for the commit time.
pub(super) fn related_instance_time(instance: &RelatedInstance) -> String {
    instance
        .matched
        .iter()
        .chain(instance.related.iter())
        .map(event_time)
        .max()
        .unwrap_or_default()
}

/// The event's time in UTC, so times recorded with a local offset (older
/// base-commit entries) compare and print like every other ledger time.
fn event_time(event: &HistoryEvent) -> String {
    let raw = event.occurred_at.as_deref().unwrap_or(&event.recorded_at);
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|time| {
            time.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        })
        .unwrap_or_else(|_| raw.to_string())
}

pub(super) fn related_repo_paths(
    project: &KnitProject,
    target: &RelatedTarget,
) -> BTreeMap<String, PathBuf> {
    let mut paths = project
        .repos
        .iter()
        .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
        .collect::<BTreeMap<_, _>>();
    paths.insert(target.repo_id.clone(), target.checkout.clone());
    paths
}

/// Cached Git subjects keyed by `(repo_id, sha)` so rendering never runs one
/// `git show` per row. Absent entries mean the subject is unknown, so
/// rendering falls back to the recorded event message.
pub(super) type CommitSubjects = BTreeMap<(String, String), String>;

/// Load Git subjects once for every distinct commit rendered across the
/// displayed instances, batched per repo via
/// [`crate::git::commit_details`]. Blank subjects are not cached so the
/// recorded-message fallback in [`event_message`] keeps the old precedence.
pub(super) fn prefetch_commit_subjects(
    instances: &[RelatedInstance],
    repo_paths: &BTreeMap<String, PathBuf>,
) -> CommitSubjects {
    let mut shas_by_repo = BTreeMap::<&str, BTreeSet<&str>>::new();
    for instance in instances {
        for event in instance
            .matched
            .iter()
            .chain(instance.related.iter())
            .chain(instance.other_bundle.iter())
        {
            if let (Some(repo_id), Some(commit)) = (&event.repo_id, &event.commit) {
                shas_by_repo.entry(repo_id).or_default().insert(commit);
            }
        }
    }

    let mut subjects = CommitSubjects::new();
    for (repo_id, shas) in shas_by_repo {
        let Some(repo_path) = repo_paths.get(repo_id) else {
            continue;
        };
        let wanted = shas
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for (sha, detail) in crate::git::commit_details(repo_path, &wanted) {
            if detail.subject.trim().is_empty() {
                continue;
            }
            subjects.insert((repo_id.to_string(), sha), detail.subject);
        }
    }
    subjects
}

pub(super) fn print_related_instance(
    instance: &RelatedInstance,
    repo_paths: &BTreeMap<String, PathBuf>,
    subjects: &CommitSubjects,
) {
    println!();
    match &instance.bundle_id {
        Some(bundle) => println!(
            "{} {} {}",
            out::heading("Bundle:"),
            out::repo(bundle),
            instance
                .bundle_title
                .as_deref()
                .unwrap_or("untitled bundle")
        ),
        None => println!("{} {}", out::heading("Bundle:"), no_bundle_label(instance)),
    }
    println!("{} {}", out::heading("Scope:"), instance.scope_label());
    println!(
        "{} {}",
        out::heading("When:"),
        related_instance_time(instance)
    );

    println!("{}", out::heading("Touched path:"));
    print_event_list(&instance.matched, subjects);

    if !instance.related.is_empty() {
        println!("{}", out::heading("Related in same scope:"));
        print_event_list(&instance.related, subjects);
    }

    if !instance.other_bundle.is_empty() {
        println!("{}", out::heading("Other activity in bundle:"));
        print_event_list(&instance.other_bundle, subjects);
    }

    print_inspect_commands(instance, repo_paths);
}

/// What a scope outside every bundle is, in the `[base: <branch>]` form that
/// `knit history list` uses for base history: there is no bundle to name.
fn no_bundle_label(instance: &RelatedInstance) -> String {
    let base_branch = instance
        .matched
        .iter()
        .filter(|event| event.kind == "base.commit")
        .find_map(|event| event.branch.as_deref());
    match base_branch {
        Some(branch) => format!("none [base: {branch}]"),
        None => "none".to_string(),
    }
}

pub(super) fn print_event_list(events: &[HistoryEvent], subjects: &CommitSubjects) {
    let mut events = events.to_vec();
    events.sort_by(|left, right| {
        event_time(left)
            .cmp(&event_time(right))
            .then(left.repo_id.cmp(&right.repo_id))
            .then(left.commit.cmp(&right.commit))
    });

    for event in events {
        let repo = event.repo_id.as_deref().unwrap_or("-");
        let sha = event
            .commit
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "-".to_string());
        let message = event_message(&event, subjects);
        println!(
            "  {} {} {}",
            out::repo_field(repo, 18),
            out::sha(format!("{sha:<8}")),
            message
        );
    }
}

fn event_message(event: &HistoryEvent, subjects: &CommitSubjects) -> String {
    if let (Some(repo_id), Some(commit)) = (&event.repo_id, &event.commit) {
        if let Some(subject) = subjects.get(&(repo_id.clone(), commit.clone())) {
            if !subject.trim().is_empty() {
                return subject.clone();
            }
        }
    }

    event
        .message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(&event.kind)
        .to_string()
}

fn print_inspect_commands(instance: &RelatedInstance, repo_paths: &BTreeMap<String, PathBuf>) {
    let mut commands = BTreeSet::new();
    for event in instance
        .matched
        .iter()
        .chain(instance.related.iter())
        .chain(instance.other_bundle.iter())
    {
        let (Some(repo_id), Some(commit)) = (&event.repo_id, &event.commit) else {
            continue;
        };
        let Some(repo_path) = repo_paths.get(repo_id) else {
            continue;
        };
        commands.insert(format!(
            "git -C {} show --stat {}",
            out::shell_word(&repo_path.to_string_lossy()),
            commit
        ));
    }

    if commands.is_empty() {
        return;
    }

    println!("{}", out::heading("Inspect:"));
    for command in commands {
        println!("  {command}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_instances_expand_same_scope_and_bundle_context() {
        let events = vec![
            event("touch", "knithub-frontend", "aaa111", Some("kg1"), "n1"),
            event("api", "knithub", "bbb222", Some("kg1"), "n1"),
            event("docs", "knit", "ccc333", Some("kg2"), "n2"),
        ];
        let commits = BTreeSet::from(["aaa111".to_string()]);

        let instances = related_instances(&events, "knithub-frontend", &commits);

        assert_eq!(instances.len(), 1);
        assert_eq!(
            instances[0]
                .matched
                .iter()
                .map(|event| event.repo_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["knithub-frontend"]
        );
        assert_eq!(
            instances[0]
                .related
                .iter()
                .map(|event| event.repo_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["knithub"]
        );
        assert_eq!(
            instances[0]
                .other_bundle
                .iter()
                .map(|event| event.repo_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["knit"]
        );
    }

    #[test]
    fn when_is_the_commit_time_not_a_later_bundle_event() {
        let mut archived = event("archive", "backend", "aaa111", None, "n9");
        archived.kind = "bundle.archived".to_string();
        archived.repo_id = None;
        archived.commit = None;
        archived.occurred_at = Some("2026-06-05T12:30:00.123Z".to_string());
        let events = vec![
            event("touch", "backend", "aaa111", Some("kg1"), "n1"),
            event("api", "frontend", "bbb222", Some("kg1"), "n1"),
            archived,
        ];
        let commits = BTreeSet::from(["aaa111".to_string()]);

        let instances = related_instances(&events, "backend", &commits);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].other_bundle.len(), 1);
        assert_eq!(related_instance_time(&instances[0]), "2026-06-05T10:00:00Z");
    }

    #[test]
    fn when_is_printed_in_utc_whatever_offset_was_recorded() {
        let mut base = event(
            "base",
            "backend",
            "aaa111",
            None,
            "base:backend:main:aaa111",
        );
        base.occurred_at = Some("2026-06-05T12:00:00+02:00".to_string());

        assert_eq!(event_time(&base), "2026-06-05T10:00:00Z");
    }

    #[test]
    fn base_commits_are_labelled_base_history_not_a_bundle() {
        let mut base = event(
            "base",
            "backend",
            "aaa111",
            None,
            "base:backend:main:aaa111",
        );
        base.kind = "base.commit".to_string();
        base.bundle_id = None;
        base.bundle_title = None;
        base.branch = Some("main".to_string());
        let commits = BTreeSet::from(["aaa111".to_string()]);

        let instances = related_instances(&[base], "backend", &commits);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].bundle_id, None);
        assert_eq!(no_bundle_label(&instances[0]), "none [base: main]");
    }

    #[test]
    fn event_message_prefers_cached_git_subject() {
        let event = event("e1", "backend", "aaa111", Some("kg1"), "n1");
        let mut subjects = CommitSubjects::new();
        subjects.insert(
            ("backend".to_string(), "aaa111".to_string()),
            "Git subject line".to_string(),
        );

        assert_eq!(event_message(&event, &subjects), "Git subject line");
    }

    #[test]
    fn event_message_falls_back_to_recorded_message() {
        let event = event("e1", "backend", "aaa111", Some("kg1"), "n1");

        assert_eq!(
            event_message(&event, &CommitSubjects::new()),
            "backend change"
        );
    }

    #[test]
    fn event_message_falls_back_to_kind_when_message_blank() {
        let mut event = event("e1", "backend", "aaa111", Some("kg1"), "n1");
        event.message = Some("   ".to_string());

        assert_eq!(
            event_message(&event, &CommitSubjects::new()),
            "commit.recorded"
        );
    }

    #[test]
    fn event_message_falls_back_when_cached_subject_blank() {
        let event = event("e1", "backend", "aaa111", Some("kg1"), "n1");
        let mut subjects = CommitSubjects::new();
        subjects.insert(
            ("backend".to_string(), "aaa111".to_string()),
            "  ".to_string(),
        );

        assert_eq!(event_message(&event, &subjects), "backend change");
    }

    fn event(
        event_id: &str,
        repo_id: &str,
        commit: &str,
        commit_group_id: Option<&str>,
        node_id: &str,
    ) -> HistoryEvent {
        HistoryEvent {
            schema_version: "knit.history.event.v1".to_string(),
            event_id: event_id.to_string(),
            project_id: "knit-tools".to_string(),
            kind: "commit.recorded".to_string(),
            bundle_id: Some("bundle-a".to_string()),
            bundle_title: Some("Bundle A".to_string()),
            repo_id: Some(repo_id.to_string()),
            repo_remote: None,
            base_branch: Some("main".to_string()),
            branch: Some("knit/bundle-a".to_string()),
            commit: Some(commit.to_string()),
            before_sha: None,
            after_sha: Some(commit.to_string()),
            movement: Some(crate::model::Movement::Advanced),
            node_id: Some(node_id.to_string()),
            node_type: Some("commit.group".to_string()),
            commit_group_id: commit_group_id.map(ToString::to_string),
            message: Some(format!("{repo_id} change")),
            occurred_at: Some("2026-06-05T10:00:00Z".to_string()),
            recorded_at: "2026-06-05T10:00:01Z".to_string(),
            recorded_by: "knit".to_string(),
            metadata: None,
        }
    }
}
