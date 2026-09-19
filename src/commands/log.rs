use crate::checkout::checkout_dir;
use crate::cli::{HistoryGroupArg, HistoryRepoMatchArg, LogArgs};
use crate::git::display_color_args;
use crate::history_query::{
    query_bundle_history, query_project_history, HistoryEntry, HistoryGrouping, HistoryQuery,
    RepoMatch,
};
use crate::ids::short_sha;
use crate::model::{BundleNode, CommitGroup, CommitRef, Movement, RepoChange};
use crate::output as out;
use crate::selectors::{is_loggable_node, resolve_log_node};
use crate::store::{bundle_path, load_active_bundle, read_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn show_log(args: &LogArgs, global_bundle: Option<&str>) -> Result<()> {
    if args.all && global_bundle.is_some() {
        bail!("Global --bundle selects one bundle and cannot be combined with `log --all`.");
    }
    let limit = resolve_limit(args.limit, args.shorthand_limit.as_deref())?;

    if args.all {
        let (root, project_id) =
            crate::commands::history::query::resolve_query_project(args.project.as_deref())?;
        let view_repos = resolve_view(&root, &project_id, args.view.as_deref())?;
        let repos =
            crate::commands::history::query::intersect_repo_filters(&args.repos, view_repos);
        let query = build_query(args, &root, repos, limit)?;
        let entries = query_project_history(&root, &project_id, &query)?;
        return print_entries(&entries, args, true);
    }

    let active = load_active_bundle()?;
    let view_repos = match args.view.as_deref() {
        Some(name) => {
            let project = active
                .bundle
                .project_id
                .as_deref()
                .context("--view requires a project bundle.")?;
            resolve_view(&active.root, project, Some(name))?
        }
        None => None,
    };
    let repos = crate::commands::history::query::intersect_repo_filters(&args.repos, view_repos);
    let query = build_query(args, &active.root, repos, limit)?;
    let entries = query_bundle_history(&active.root, &active.bundle, &query)?;
    if !args.json {
        crate::commands::handoff::print_location(&active.bundle);
    }
    print_bundle_entries(&entries, args, &active)
}

fn resolve_view(root: &Path, project_id: &str, name: Option<&str>) -> Result<Option<Vec<String>>> {
    name.map(|name| {
        let project = crate::commands::project::load_project_by_id(root, project_id)
            .with_context(|| format!("No local project artifact found for `{project_id}`."))?;
        crate::commands::history::query::view_repo_ids(root, project_id, &project, name)
    })
    .transpose()
}

fn build_query(
    args: &LogArgs,
    root: &Path,
    repos: Option<Vec<String>>,
    limit: Option<usize>,
) -> Result<HistoryQuery> {
    Ok(HistoryQuery {
        repos,
        repo_match: match args.repo_match {
            HistoryRepoMatchArg::Any => RepoMatch::Any,
            HistoryRepoMatchArg::All => RepoMatch::All,
        },
        kinds: args.kinds.clone(),
        since: args
            .since
            .as_deref()
            .map(|value| crate::commands::history::query::parse_history_date(root, value, true))
            .transpose()?,
        until: args
            .until
            .as_deref()
            .map(|value| crate::commands::history::query::parse_history_date(root, value, false))
            .transpose()?,
        grep: args.grep.clone(),
        all_match: args.all_match,
        ignore_case: args.ignore_case,
        fixed_strings: args.fixed_strings,
        extended_regexp: args.extended_regexp,
        grouping: match args.group {
            HistoryGroupArg::Event => HistoryGrouping::Event,
            HistoryGroupArg::Commit => HistoryGrouping::Commit,
            HistoryGroupArg::Bundle => HistoryGrouping::Bundle,
        },
        full_context: args.full_context,
        reverse: args.reverse,
        limit,
        skip: args.skip,
        ..HistoryQuery::default()
    })
}

fn print_bundle_entries(
    entries: &[HistoryEntry],
    args: &LogArgs,
    active: &ActiveBundle,
) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("{}", out::muted("No log entries matched."));
        return Ok(());
    }

    let can_reuse_nodes = !args.oneline
        && args.repos.is_empty()
        && args.view.is_none()
        && args.kinds.is_empty()
        && args.grep.is_empty()
        && args.since.is_none()
        && args.until.is_none()
        && !args.full_context
        && matches!(args.group, HistoryGroupArg::Commit);
    for entry in entries {
        if can_reuse_nodes {
            if let Some(node) = active
                .bundle
                .nodes
                .iter()
                .find(|node| node.id == entry.id && is_loggable_node(node))
            {
                print_node(node);
                continue;
            }
        }
        print_entry(entry, args.oneline, false);
    }
    Ok(())
}

fn print_entries(entries: &[HistoryEntry], args: &LogArgs, bundle_context: bool) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("{}", out::muted("No history entries matched."));
        return Ok(());
    }
    for entry in entries {
        print_entry(entry, args.oneline, bundle_context);
    }
    Ok(())
}

fn print_entry(entry: &HistoryEntry, oneline: bool, bundle_context: bool) {
    let bundle = entry.bundle_id.as_deref().unwrap_or("-");
    if bundle_context {
        println!(
            "{}  {}  {}",
            out::node(&entry.id),
            out::repo(bundle),
            entry.message.lines().next().unwrap_or_default()
        );
    } else {
        println!(
            "{}  {}",
            out::node(&entry.id),
            entry.message.lines().next().unwrap_or_default()
        );
    }
    if oneline {
        return;
    }
    for event in &entry.events {
        let repo = event.repo_id.as_deref().unwrap_or("-");
        let detail = event
            .commit
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| event.kind.clone());
        println!("  {} {}", out::repo_field(repo, 10), detail);
    }
}

fn resolve_limit(limit: Option<usize>, shorthand_limit: Option<&str>) -> Result<Option<usize>> {
    let Some(shorthand_limit) = shorthand_limit else {
        return Ok(limit);
    };
    if limit.is_some() {
        bail!("Use either -n/--limit or -<count>, not both.");
    }

    let raw = shorthand_limit.trim();
    let count = raw.strip_prefix('-').unwrap_or(raw);
    if count.is_empty() || !count.chars().all(|char| char.is_ascii_digit()) {
        bail!("Expected a log count like `-2`.");
    }

    Ok(Some(count.parse()?))
}

fn print_node(node: &BundleNode) {
    match node.node_type.as_str() {
        "commit.group" => {
            println!(
                "{}  {}",
                out::node(&node.id),
                node.message.as_deref().unwrap_or("Commit group")
            );
            for commit in &node.commits {
                println!(
                    "  {} {}",
                    out::repo_field(&commit.repo_id, 10),
                    out::sha(short_sha(&commit.sha))
                );
            }
        }
        "revert.group" => {
            let target = node.target_node_id.as_deref().unwrap_or("unknown");
            println!(
                "{}  {} {}  {}",
                out::node(&node.id),
                out::danger("revert"),
                out::node(target),
                node.message.as_deref().unwrap_or("Revert")
            );
            for commit in &node.commits {
                println!(
                    "  {} {}",
                    out::repo_field(&commit.repo_id, 10),
                    out::sha(short_sha(&commit.sha))
                );
            }
        }
        "git.observed" | "land.update" => {
            let heading = if node.node_type == "land.update" {
                "updated from base"
            } else {
                "observed git changes"
            };
            println!("{}  {}", out::node(&node.id), out::heading(heading));
            for change in &node.repo_changes {
                match change.movement {
                    Movement::Advanced => {
                        if change.commits.is_empty() {
                            println!(
                                "  {} {} {}",
                                out::repo_field(&change.repo_id, 10),
                                out::movement("advanced"),
                                out::sha(short_sha(&change.after_sha))
                            );
                        } else {
                            for sha in &change.commits {
                                println!(
                                    "  {} {} {}",
                                    out::repo_field(&change.repo_id, 10),
                                    out::movement("advanced"),
                                    out::sha(short_sha(sha))
                                );
                            }
                        }
                    }
                    Movement::Rewound => {
                        println!(
                            "  {} {}  {} -> {}",
                            out::repo_field(&change.repo_id, 10),
                            out::movement("rewound"),
                            change
                                .before_sha
                                .as_deref()
                                .map(short_sha)
                                .map(out::sha)
                                .unwrap_or_else(|| out::muted("-")),
                            out::sha(short_sha(&change.after_sha))
                        );
                        for sha in &change.dropped_commits {
                            println!(
                                "  {} {}  {}",
                                out::repo_field("", 10),
                                out::movement("dropped"),
                                out::sha(short_sha(sha))
                            );
                        }
                    }
                    Movement::Diverged => {
                        println!(
                            "  {} {} {} -> {}",
                            out::repo_field(&change.repo_id, 10),
                            out::movement("diverged"),
                            change
                                .before_sha
                                .as_deref()
                                .map(short_sha)
                                .map(out::sha)
                                .unwrap_or_else(|| out::muted("-")),
                            out::sha(short_sha(&change.after_sha))
                        );
                        for sha in &change.commits {
                            println!(
                                "  {} {}    {}",
                                out::repo_field("", 10),
                                out::movement("added"),
                                out::sha(short_sha(sha))
                            );
                        }
                        for sha in &change.dropped_commits {
                            println!(
                                "  {} {}  {}",
                                out::repo_field("", 10),
                                out::movement("dropped"),
                                out::sha(short_sha(sha))
                            );
                        }
                    }
                }
            }
        }
        "checkpoint" => {
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::heading("checkpoint"),
                node.message.as_deref().unwrap_or("")
            );
        }
        "check.recorded" => {
            let name = node.title.as_deref().unwrap_or("check");
            let message = node.message.as_deref().unwrap_or("");
            let verdict = if message.starts_with("pass") {
                out::ok("pass")
            } else {
                out::danger("fail")
            };
            println!(
                "{}  {} {}  {}",
                out::node(&node.id),
                out::heading(format!("check {name}")),
                verdict,
                out::muted(message)
            );
            for pin in &node.commits {
                println!(
                    "  {} {}",
                    out::repo(&pin.repo_id),
                    out::sha(short_sha(&pin.sha))
                );
            }
        }
        "tag.created" => {
            let name = node.title.as_deref().unwrap_or("tag");
            let subject = node
                .message
                .as_deref()
                .and_then(|message| message.lines().next())
                .unwrap_or("");
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::heading(format!("tag knit/{name}")),
                out::muted(subject)
            );
            for pin in &node.commits {
                println!(
                    "  {} {}",
                    out::repo(&pin.repo_id),
                    out::sha(short_sha(&pin.sha))
                );
            }
        }
        "feature.closed" => {
            let reason = node.message.as_deref().unwrap_or("closed");
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::danger("closed"),
                reason
            );
        }
        "feature.landed" => {
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::ok("landed"),
                node.provider.as_deref().unwrap_or("provider")
            );
            if let Some(repo_ids) = &node.repo_ids {
                for repo_id in repo_ids {
                    println!("  {}", out::repo(repo_id));
                }
            }
        }
        "pr.revert" => {
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::movement("pr revert"),
                node.provider.as_deref().unwrap_or("provider")
            );
            if let Some(repo_ids) = &node.repo_ids {
                for repo_id in repo_ids {
                    println!("  {}", out::repo(repo_id));
                }
            }
        }
        "repo.removed" => {
            println!("{}  {}", out::node(&node.id), out::danger("removed repos"));
            if let Some(repo_ids) = &node.repo_ids {
                for repo_id in repo_ids {
                    println!("  {}", out::repo(repo_id));
                }
            }
        }
        _ => {
            println!(
                "{}  {}  {}",
                out::node(&node.id),
                out::heading(&node.node_type),
                node.message
                    .as_deref()
                    .or(node.title.as_deref())
                    .unwrap_or("")
            );
        }
    }
}

pub fn show_target(
    target: &str,
    all: bool,
    project: Option<&str>,
    json: bool,
    global_bundle: Option<&str>,
) -> Result<()> {
    if all {
        if global_bundle.is_some() {
            bail!("Global --bundle selects one bundle and cannot be combined with `show --all`.");
        }
        if target == "HEAD" || target.starts_with("HEAD~") {
            bail!("HEAD selectors are bundle-relative; omit --all and select a bundle.");
        }
        return show_project_target(target, project, json);
    }

    let active = load_active_bundle()?;
    // HEAD is deliberately bundle-relative, including old commit_groups-only artifacts.
    let resolved_target = if target == "HEAD" || target.starts_with("HEAD~") {
        let timeline = crate::history::bundle_history_nodes(&active.bundle);
        resolve_log_node(&timeline, target)?.id.clone()
    } else {
        target.to_string()
    };
    let entries =
        selector_entries(|query| query_bundle_history(&active.root, &active.bundle, query))?;
    let entry = resolve_history_entry(&entries, &resolved_target)?;
    if json {
        println!("{}", serde_json::to_string_pretty(entry)?);
        return Ok(());
    }
    show_selected_entry(&active, entry)
}

fn selector_entries(
    mut query: impl FnMut(&HistoryQuery) -> Result<Vec<HistoryEntry>>,
) -> Result<Vec<HistoryEntry>> {
    let mut entries = Vec::new();
    for grouping in [
        HistoryGrouping::Commit,
        HistoryGrouping::Event,
        HistoryGrouping::Bundle,
    ] {
        entries.extend(query(&HistoryQuery {
            grouping,
            full_context: true,
            ..HistoryQuery::default()
        })?);
    }
    // Single-event groups may share their event id.
    let mut seen = std::collections::BTreeSet::new();
    entries.retain(|entry| seen.insert((entry.bundle_id.clone(), entry.id.clone())));
    Ok(entries)
}

fn show_project_target(target: &str, project: Option<&str>, json: bool) -> Result<()> {
    let (root, project_id) = crate::commands::history::query::resolve_query_project(project)?;
    let entries = selector_entries(|query| query_project_history(&root, &project_id, query))?;
    let entry = resolve_history_entry(&entries, target)?;
    if json {
        println!("{}", serde_json::to_string_pretty(entry)?);
        return Ok(());
    }
    let Some(bundle_id) = entry.bundle_id.as_deref() else {
        print_entry_metadata(entry);
        println!(
            "{}",
            out::muted("Patch unavailable: this history entry has no bundle id.")
        );
        return Ok(());
    };
    let path = bundle_path(&root, bundle_id);
    if !path.exists() {
        print_entry_metadata(entry);
        println!(
            "{}",
            out::muted(
                "Patch unavailable locally: the bundle artifact was deleted or is not present."
            )
        );
        return Ok(());
    }
    let bundle = read_json(&path)?;
    show_selected_entry(&ActiveBundle::unlocked(root, path, bundle), entry)
}

fn show_selected_entry(active: &ActiveBundle, entry: &HistoryEntry) -> Result<()> {
    if let Some(node) = active.bundle.nodes.iter().find(|node| node.id == entry.id) {
        return show_node(active, node);
    }
    if let Some(group) = active
        .bundle
        .commit_groups
        .iter()
        .find(|group| group.id == entry.id)
    {
        print_commit_group_header(group);
        return show_commit_refs(active, &group.commits);
    }
    print_entry_metadata(entry);
    let mut seen = std::collections::BTreeSet::new();
    for event in &entry.events {
        if let (Some(repo), Some(sha)) = (event.repo_id.as_deref(), event.commit.as_deref()) {
            if seen.insert((repo, sha)) {
                show_repo_commit(active, repo, sha)?;
            }
        }
    }
    if seen.is_empty() {
        println!(
            "{}",
            out::muted("No local Git details recorded for this entry.")
        );
    }
    Ok(())
}

fn resolve_history_entry<'a>(
    entries: &'a [HistoryEntry],
    target: &str,
) -> Result<&'a HistoryEntry> {
    let ids = entries
        .iter()
        .filter(|entry| entry.id == target)
        .collect::<Vec<_>>();
    let is_commit_projection = |entry: &&HistoryEntry| {
        entry.events.iter().any(|event| {
            event.node_id.as_deref() == Some(entry.id.as_str())
                || event.commit_group_id.as_deref() == Some(entry.id.as_str())
                || (event.node_id.is_none()
                    && event.commit_group_id.is_none()
                    && event.event_id == entry.id)
        })
    };
    let aliases = entries
        .iter()
        .filter(is_commit_projection)
        .filter(|entry| history_entry_exact(entry, target))
        .collect::<Vec<_>>();
    let id_prefixes = entries
        .iter()
        .filter(|entry| entry.id.starts_with(target))
        .collect::<Vec<_>>();
    let aliases_prefixes = entries
        .iter()
        .filter(is_commit_projection)
        .filter(|entry| history_entry_matches(entry, target))
        .collect::<Vec<_>>();
    let candidates = if !ids.is_empty() {
        ids
    } else if !aliases.is_empty() {
        aliases
    } else if !id_prefixes.is_empty() {
        id_prefixes
    } else {
        aliases_prefixes
    };
    match candidates.as_slice() {
        [] => bail!("No project history entry matches `{target}`."),
        [entry] => Ok(*entry),
        many => bail!(
            "History selector `{target}` is ambiguous across {} entries: {}.",
            many.len(),
            many.iter()
                .map(|entry| format!("{}/{}", entry.bundle_id.as_deref().unwrap_or("-"), entry.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn history_entry_matches(entry: &HistoryEntry, target: &str) -> bool {
    entry.id.starts_with(target)
        || entry.events.iter().any(|event| {
            event.event_id.starts_with(target)
                || event
                    .node_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(target))
                || event
                    .commit_group_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(target))
                || event
                    .commit
                    .as_deref()
                    .is_some_and(|sha| sha.starts_with(target))
        })
}

fn history_entry_exact(entry: &HistoryEntry, target: &str) -> bool {
    entry.id == target
        || entry.events.iter().any(|event| {
            event.event_id == target
                || event.node_id.as_deref() == Some(target)
                || event.commit_group_id.as_deref() == Some(target)
                || event.commit.as_deref() == Some(target)
        })
}

fn print_entry_metadata(entry: &HistoryEntry) {
    println!("{} {}", out::heading("Entry:"), out::node(&entry.id));
    if let Some(bundle_id) = &entry.bundle_id {
        println!("{} {}", out::heading("Bundle:"), out::repo(bundle_id));
    }
    if let Some(title) = &entry.bundle_title {
        println!("{} {}", out::heading("Title:"), title);
    }
    println!("{} {}", out::heading("Occurred:"), entry.occurred_at);
    println!("{} {}", out::heading("Message:"), entry.message);
    for event in &entry.events {
        println!("  {}", crate::history::format_history_event(event));
    }
}

fn show_node(active: &ActiveBundle, node: &BundleNode) -> Result<()> {
    print_show_header(node);

    match node.node_type.as_str() {
        "commit.group" | "revert.group" | "tag.created" => show_commit_refs(active, &node.commits),
        "git.observed" | "land.update" => show_observed_node(active, node),
        "repo.removed" => {
            if let Some(repo_ids) = &node.repo_ids {
                for repo_id in repo_ids {
                    println!("  {}", out::repo(repo_id));
                }
            } else {
                println!("{}", out::muted("No repo ids recorded."));
            }
            Ok(())
        }
        "feature.landed" | "pr.revert" => {
            if let Some(target_node_id) = &node.target_node_id {
                println!("{} {}", out::heading("Reverts:"), out::node(target_node_id));
            }
            if let Some(plan_id) = &node.plan_id {
                println!("{} {}", out::heading("Plan:"), out::node(plan_id));
            }
            if let Some(run_id) = &node.run_id {
                println!("{} {}", out::heading("Run:"), out::node(run_id));
            }
            if let Some(provider) = &node.provider {
                println!("{} {}", out::heading("Provider:"), provider);
            }
            for url in &node.publication_urls {
                println!("  {url}");
            }
            Ok(())
        }
        node_type => {
            println!(
                "{}",
                out::muted(format!("No git details for {node_type} nodes."))
            );
            Ok(())
        }
    }
}

fn print_show_header(node: &BundleNode) {
    println!("{} {}", out::heading("Node:"), out::node(&node.id));
    println!("{} {}", out::heading("Type:"), node.node_type);
    if let Some(group_id) = &node.commit_group_id {
        println!("{} {}", out::heading("Group:"), out::node(group_id));
    }
    if let Some(target_node_id) = &node.target_node_id {
        println!("{} {}", out::heading("Target:"), out::node(target_node_id));
    }
    if let Some(title) = &node.title {
        println!("{} {}", out::heading("Title:"), title);
    }
    if let Some(message) = &node.message {
        println!("{} {}", out::heading("Message:"), message);
    } else if node.node_type == "git.observed" {
        println!("{} observed git changes", out::heading("Message:"));
    } else if node.node_type == "land.update" {
        println!(
            "{} updated feature branches from base",
            out::heading("Message:")
        );
    }
    if let Some(session_id) = &node.session_id {
        println!("{} {}", out::heading("Session:"), out::muted(session_id));
    }
    if let Some(actor) = &node.actor {
        let mut who = actor.label.clone().unwrap_or_else(|| actor.session.clone());
        if let Some(email) = &actor.email {
            who = format!("{who} <{email}>");
        }
        println!("{} {}", out::heading("Actor:"), out::muted(&who));
    }
    println!();
}

fn show_commit_refs(active: &ActiveBundle, commits: &[CommitRef]) -> Result<()> {
    if commits.is_empty() {
        println!("{}", out::muted("No commits recorded on this node."));
        return Ok(());
    }

    for commit in commits {
        show_repo_commit(active, &commit.repo_id, &commit.sha)?;
    }

    Ok(())
}

fn show_observed_node(active: &ActiveBundle, node: &BundleNode) -> Result<()> {
    if node.repo_changes.is_empty() {
        println!("{}", out::muted("No repo changes recorded on this node."));
        return Ok(());
    }

    for change in &node.repo_changes {
        print_change_summary(change);

        match change.movement {
            Movement::Advanced => {
                if change.commits.is_empty() {
                    show_repo_commit(active, &change.repo_id, &change.after_sha)?;
                } else {
                    for sha in &change.commits {
                        show_repo_commit(active, &change.repo_id, sha)?;
                    }
                }
            }
            Movement::Rewound => {
                for sha in &change.dropped_commits {
                    show_repo_commit(active, &change.repo_id, sha)?;
                }
            }
            Movement::Diverged => {
                for sha in &change.commits {
                    show_repo_commit(active, &change.repo_id, sha)?;
                }
                for sha in &change.dropped_commits {
                    show_repo_commit(active, &change.repo_id, sha)?;
                }
            }
        }
    }

    Ok(())
}

fn print_change_summary(change: &RepoChange) {
    let before = change
        .before_sha
        .as_deref()
        .map(short_sha)
        .map(out::sha)
        .unwrap_or_else(|| out::muted("-"));
    println!(
        "{} {} {} -> {}",
        out::repo(&change.repo_id),
        out::movement(change.movement.as_str()),
        before,
        out::sha(short_sha(&change.after_sha))
    );
}

fn show_repo_commit(active: &ActiveBundle, repo_id: &str, sha: &str) -> Result<()> {
    println!("== {} {} ==", out::repo(repo_id), out::sha(short_sha(sha)));

    let Some(repo_dir) = repo_dir_for_show(active, repo_id) else {
        println!(
            "{}",
            out::muted("  repo is no longer tracked and no worktree was found")
        );
        return Ok(());
    };

    let mut show_args = vec![OsString::from("show")];
    show_args.extend(display_color_args());
    show_args.extend([
        OsString::from("--stat"),
        OsString::from("--oneline"),
        OsString::from(sha),
    ]);
    let output = std::process::Command::new("git")
        .args(show_args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(&repo_dir)
        .output()
        .context("failed to run local git show")
        .and_then(|output| {
            if !output.status.success() {
                bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        });
    match output {
        Ok(output) if output.trim().is_empty() => {
            println!("{}", out::muted("  git show returned no output"));
        }
        Ok(output) => {
            println!("{output}");
        }
        Err(error) => {
            println!(
                "{}",
                out::danger(format!("  commit unavailable locally: {error}"))
            );
        }
    }

    Ok(())
}

fn repo_dir_for_show(active: &ActiveBundle, repo_id: &str) -> Option<PathBuf> {
    if let Some(repo) = active.bundle.repos.iter().find(|repo| repo.id == repo_id) {
        return checkout_dir(active, repo).or_else(|| {
            let path = PathBuf::from(&repo.path);
            path.exists().then_some(path)
        });
    }

    let worktree = active
        .root
        .join(".knit/worktrees")
        .join(&active.bundle.id)
        .join(repo_id);
    worktree.exists().then_some(worktree)
}

fn print_commit_group_header(group: &CommitGroup) {
    println!("{}  {}\n", out::node(&group.id), group.message);
}
