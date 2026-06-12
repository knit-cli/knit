//! The `knit related` join: git commits that touched a path, matched to
//! Knit history events to recover bundle scope and the companion commits in
//! other repos, plus the rendering of the result.

use super::target::RelatedTarget;
use crate::git::{git_output, git_output_optional};
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

pub(super) fn related_instance_time(instance: &RelatedInstance) -> String {
    instance
        .matched
        .iter()
        .chain(instance.related.iter())
        .chain(instance.other_bundle.iter())
        .map(event_time)
        .max()
        .unwrap_or_default()
}

fn event_time(event: &HistoryEvent) -> String {
    event
        .occurred_at
        .as_deref()
        .unwrap_or(&event.recorded_at)
        .to_string()
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

pub(super) fn print_related_instance(
    instance: &RelatedInstance,
    repo_paths: &BTreeMap<String, PathBuf>,
) {
    println!();
    let bundle = instance.bundle_id.as_deref().unwrap_or("-");
    let title = instance
        .bundle_title
        .as_deref()
        .unwrap_or("untitled bundle");
    println!(
        "{} {} {}",
        out::heading("Bundle:"),
        out::repo(bundle),
        title
    );
    println!("{} {}", out::heading("Scope:"), instance.scope_label());
    println!(
        "{} {}",
        out::heading("When:"),
        related_instance_time(instance)
    );

    println!("{}", out::heading("Touched path:"));
    print_event_list(&instance.matched, repo_paths);

    if !instance.related.is_empty() {
        println!("{}", out::heading("Related in same scope:"));
        print_event_list(&instance.related, repo_paths);
    }

    if !instance.other_bundle.is_empty() {
        println!("{}", out::heading("Other commits in bundle:"));
        print_event_list(&instance.other_bundle, repo_paths);
    }

    print_inspect_commands(instance, repo_paths);
}

pub(super) fn print_event_list(events: &[HistoryEvent], repo_paths: &BTreeMap<String, PathBuf>) {
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
        let message = event_message(&event, repo_paths);
        println!(
            "  {} {} {}",
            out::repo_field(repo, 18),
            out::sha(format!("{sha:<8}")),
            message
        );
    }
}

fn event_message(event: &HistoryEvent, repo_paths: &BTreeMap<String, PathBuf>) -> String {
    if let (Some(repo_id), Some(commit)) = (&event.repo_id, &event.commit) {
        if let Some(repo_path) = repo_paths.get(repo_id) {
            if let Ok(Some(subject)) =
                git_output_optional(repo_path, ["show", "-s", "--format=%s", commit.as_str()])
            {
                if !subject.trim().is_empty() {
                    return subject;
                }
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
            shell_quote(&repo_path.to_string_lossy()),
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

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
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
