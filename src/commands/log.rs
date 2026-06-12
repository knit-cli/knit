use crate::checkout::checkout_dir;
use crate::git::git_output;
use crate::ids::short_sha;
use crate::model::{BundleNode, CommitGroup, CommitRef, Movement, RepoChange};
use crate::output as out;
use crate::selectors::{is_loggable_node, resolve_log_node};
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn show_log(limit: Option<usize>, shorthand_limit: Option<&str>) -> Result<()> {
    let limit = resolve_limit(limit, shorthand_limit)?;
    let active = load_active_bundle()?;
    if active.bundle.nodes.is_empty() && active.bundle.commit_groups.is_empty() {
        println!("{}", out::muted("No bundle nodes recorded yet."));
        return Ok(());
    }

    if !active.bundle.nodes.is_empty() {
        let nodes = active
            .bundle
            .nodes
            .iter()
            .filter(|node| is_loggable_node(node))
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            println!("{}", out::muted("No log entries recorded yet."));
            return Ok(());
        }

        let start = limited_start(nodes.len(), limit);
        for node in &nodes[start..] {
            print_node(node);
        }
        return Ok(());
    }

    let start = limited_start(active.bundle.commit_groups.len(), limit);
    for group in &active.bundle.commit_groups[start..] {
        println!("{}  {}", out::node(&group.id), group.message);
        for commit in &group.commits {
            println!(
                "  {} {}",
                out::repo_field(&commit.repo_id, 10),
                out::sha(short_sha(&commit.sha))
            );
        }
    }

    Ok(())
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

fn limited_start(len: usize, limit: Option<usize>) -> usize {
    match limit {
        Some(limit) => len.saturating_sub(limit),
        None => 0,
    }
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
        _ => {}
    }
}

pub fn show_target(target: &str) -> Result<()> {
    let active = load_active_bundle()?;
    if !active.bundle.nodes.is_empty() {
        let node = resolve_log_node(&active.bundle.nodes, target)?;
        return show_node(&active, node);
    }

    show_commit_group(&active, target)
}

fn show_node(active: &ActiveBundle, node: &BundleNode) -> Result<()> {
    print_show_header(node);

    match node.node_type.as_str() {
        "commit.group" | "revert.group" => show_commit_refs(active, &node.commits),
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

    match git_output(
        &repo_dir,
        [
            OsString::from("show"),
            OsString::from("--stat"),
            OsString::from("--oneline"),
            OsString::from(sha),
        ],
    ) {
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

fn show_commit_group(active: &ActiveBundle, commit_group_id: &str) -> Result<()> {
    let group = active
        .bundle
        .commit_groups
        .iter()
        .find(|group| group.id == commit_group_id)
        .with_context(|| format!("No commit group found for {commit_group_id}"))?;

    print_commit_group_header(group);
    show_commit_refs(active, &group.commits)?;

    Ok(())
}

fn print_commit_group_header(group: &CommitGroup) {
    println!("{}  {}\n", out::node(&group.id), group.message);
}
