use crate::git::{git_output, git_output_optional};
use crate::history::{format_history_event, load_history_events, refresh_project_history};
use crate::ids::{short_sha, slugify};
use crate::model::{HistoryEvent, KnitProject, ProjectRepoEntry};
use crate::output as out;
use crate::store::{find_knit_root, load_config, project_path, read_json};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

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

pub fn show_related_history(
    project: Option<&str>,
    repo: Option<&str>,
    paths: &[PathBuf],
    limit: usize,
    commit_limit: usize,
    pull: bool,
    remote: &str,
) -> Result<()> {
    if paths.is_empty() {
        bail!("Pass at least one path to inspect.");
    }
    if limit == 0 {
        bail!("--limit must be greater than zero.");
    }
    if commit_limit == 0 {
        bail!("--commit-limit must be greater than zero.");
    }

    let (root, project_id) = resolve_project(project)?;
    if pull {
        crate::commands::remote::pull_history_from_remote(Some(&project_id), remote)?;
    }

    let project = load_project(&root, &project_id)?;
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target = resolve_related_target(&root, &project, repo, paths, &cwd)?;
    let git_commits = git_commits_for_paths(&target.checkout, &target.paths, commit_limit)?;
    let commit_set = git_commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect::<BTreeSet<_>>();

    let appended = refresh_project_history(&root, &project_id)?;
    if appended > 0 {
        println!(
            "{} {} new event(s)",
            out::heading("History refreshed:"),
            appended
        );
    }

    println!(
        "{} {} {}",
        out::heading("Query:"),
        out::repo(&target.repo_id),
        target.paths.join(" ")
    );
    println!(
        "{} {} {}",
        out::heading("Git commits:"),
        git_commits.len(),
        out::muted(format!("inspected up to {commit_limit}"))
    );

    if git_commits.is_empty() {
        println!("{}", out::muted("No Git commits touched those paths."));
        return Ok(());
    }

    let events = load_history_events(&root, &project_id)?;
    let mut instances = related_instances(&events, &target.repo_id, &commit_set);
    if instances.is_empty() {
        println!(
            "{}",
            out::muted("No Knit history events matched those Git commits.")
        );
        println!(
            "{}",
            out::muted("Those commits may be ordinary Git history outside Knit, or local history may need `knit history pull`.")
        );
        return Ok(());
    }

    instances.sort_by(|left, right| {
        related_instance_time(right)
            .cmp(&related_instance_time(left))
            .then(left.bundle_id.cmp(&right.bundle_id))
            .then(left.scope_label().cmp(&right.scope_label()))
    });

    let repo_paths = related_repo_paths(&project, &target);
    let total = instances.len();
    for instance in instances.iter().take(limit) {
        print_related_instance(instance, &repo_paths);
    }
    if total > limit {
        println!(
            "{}",
            out::muted(format!(
                "{} more related instance(s) hidden; rerun with --limit {total}",
                total - limit
            ))
        );
    }

    Ok(())
}

#[derive(Debug)]
struct RelatedTarget {
    repo_id: String,
    checkout: PathBuf,
    paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct GitCommit {
    sha: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct RelationKey {
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
struct RelatedInstance {
    key: RelationKey,
    bundle_id: Option<String>,
    bundle_title: Option<String>,
    matched: Vec<HistoryEvent>,
    related: Vec<HistoryEvent>,
    other_bundle: Vec<HistoryEvent>,
}

impl RelatedInstance {
    fn scope_label(&self) -> String {
        match &self.key.scope {
            RelationScope::CommitGroup(id) => format!("commit group {id}"),
            RelationScope::Node(id) => format!("node {id}"),
            RelationScope::Bundle(id) => format!("bundle {id}"),
            RelationScope::Event(id) => format!("event {id}"),
        }
    }
}

fn load_project(root: &Path, project_id: &str) -> Result<KnitProject> {
    let path = project_path(root, project_id);
    read_json(&path).with_context(|| format!("failed to read project {}", path.display()))
}

fn resolve_related_target(
    root: &Path,
    project: &KnitProject,
    explicit_repo: Option<&str>,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<RelatedTarget> {
    let repo = match explicit_repo {
        Some(repo_id) => find_project_repo(project, repo_id)
            .with_context(|| format!("No project repo found for `{repo_id}`."))?,
        None => infer_related_repo(root, project, paths, cwd)?,
    };
    let checkout = checkout_for_related_repo(root, repo, cwd);
    let paths = paths
        .iter()
        .map(|path| repo_relative_path(root, repo, &checkout, cwd, path))
        .collect::<Result<Vec<_>>>()?;

    Ok(RelatedTarget {
        repo_id: repo.id.clone(),
        checkout,
        paths,
    })
}

fn find_project_repo<'a>(project: &'a KnitProject, repo_id: &str) -> Option<&'a ProjectRepoEntry> {
    project
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .or_else(|| {
            let slug = slugify(repo_id);
            project.repos.iter().find(|repo| repo.id == slug)
        })
}

fn infer_related_repo<'a>(
    root: &Path,
    project: &'a KnitProject,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<&'a ProjectRepoEntry> {
    let prefixed = paths
        .iter()
        .filter_map(|path| repo_prefix(project, path).map(|(repo, _)| repo.id.clone()))
        .collect::<BTreeSet<_>>();
    if prefixed.len() == 1 {
        let repo_id = prefixed.iter().next().expect("checked length");
        return find_project_repo(project, repo_id)
            .with_context(|| format!("No project repo found for `{repo_id}`."));
    }
    if prefixed.len() > 1 {
        bail!("Related history queries can inspect one repo at a time. Pass paths for one repo or use --repo.");
    }

    if let Some(repo) = repo_from_cwd(root, project, cwd) {
        return Ok(repo);
    }

    bail!("Could not infer the repo to query. Pass --repo <repo-id> or prefix the path with a project repo id.");
}

fn repo_prefix<'a>(
    project: &'a KnitProject,
    path: &Path,
) -> Option<(&'a ProjectRepoEntry, PathBuf)> {
    if path.is_absolute() {
        return None;
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    let first = first.to_string_lossy();
    let repo = find_project_repo(project, &first)?;
    let rest = components.collect::<PathBuf>();
    Some((repo, rest))
}

fn repo_from_cwd<'a>(
    root: &Path,
    project: &'a KnitProject,
    cwd: &Path,
) -> Option<&'a ProjectRepoEntry> {
    let cwd = crate::paths::canonicalize(cwd).ok()?;
    for repo in &project.repos {
        let repo_path = absolute_path(root, &repo.path);
        if cwd.starts_with(&repo_path) {
            return Some(repo);
        }
    }

    let worktrees = root.join(".knit/worktrees");
    let relative = cwd.strip_prefix(worktrees).ok()?;
    let mut components = relative.components();
    components.next()?;
    let Some(Component::Normal(repo_id)) = components.next() else {
        return None;
    };
    find_project_repo(project, &repo_id.to_string_lossy())
}

fn checkout_for_related_repo(root: &Path, repo: &ProjectRepoEntry, cwd: &Path) -> PathBuf {
    let source = absolute_path(root, &repo.path);
    let Ok(cwd) = crate::paths::canonicalize(cwd) else {
        return source;
    };
    if cwd.starts_with(&source) {
        return source;
    }

    let worktrees = root.join(".knit/worktrees");
    let Ok(relative) = cwd.strip_prefix(&worktrees) else {
        return source;
    };
    let mut components = relative.components();
    let Some(bundle_id) = components.next() else {
        return source;
    };
    let Some(Component::Normal(repo_id)) = components.next() else {
        return source;
    };
    if repo_id.to_string_lossy() != repo.id {
        return source;
    }

    worktrees.join(bundle_id).join(repo_id)
}

fn repo_relative_path(
    root: &Path,
    repo: &ProjectRepoEntry,
    checkout: &Path,
    cwd: &Path,
    path: &Path,
) -> Result<String> {
    let path = repo_prefix_path(repo, path).unwrap_or_else(|| path.to_path_buf());
    let repo_path = absolute_path(root, &repo.path);

    let relative = if path.is_absolute() {
        strip_path_prefix(&path, checkout)
            .or_else(|| strip_path_prefix(&path, &repo_path))
            .with_context(|| {
                format!(
                    "{} is not inside repo `{}` ({})",
                    path.display(),
                    repo.id,
                    repo_path.display()
                )
            })?
    } else {
        let cwd = crate::paths::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        if cwd.starts_with(checkout) {
            let cwd_relative = cwd.strip_prefix(checkout).unwrap_or(Path::new(""));
            cwd_relative.join(path)
        } else if cwd.starts_with(&repo_path) {
            let cwd_relative = cwd.strip_prefix(&repo_path).unwrap_or(Path::new(""));
            cwd_relative.join(path)
        } else {
            path
        }
    };

    Ok(path_to_git_pathspec(&relative))
}

fn repo_prefix_path(repo: &ProjectRepoEntry, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    if first.to_string_lossy() != repo.id {
        return None;
    }
    Some(components.collect())
}

fn strip_path_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let path = crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let prefix = crate::paths::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    crate::paths::strip_path_prefix(&path, &prefix)
}

fn path_to_git_pathspec(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

fn absolute_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn git_commits_for_paths(
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

fn related_instances(
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

fn related_instance_time(instance: &RelatedInstance) -> String {
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

fn related_repo_paths(project: &KnitProject, target: &RelatedTarget) -> BTreeMap<String, PathBuf> {
    let mut paths = project
        .repos
        .iter()
        .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
        .collect::<BTreeMap<_, _>>();
    paths.insert(target.repo_id.clone(), target.checkout.clone());
    paths
}

fn print_related_instance(instance: &RelatedInstance, repo_paths: &BTreeMap<String, PathBuf>) {
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

fn print_event_list(events: &[HistoryEvent], repo_paths: &BTreeMap<String, PathBuf>) {
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

fn resolve_project(project: Option<&str>) -> Result<(std::path::PathBuf, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project
        .map(slugify)
        .or(config.active_project)
        .context("No active project selected. Pass --project or run `knit init <name>`.")?;
    Ok((root, project_id))
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
