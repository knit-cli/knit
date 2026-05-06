use crate::git::git_output;
use crate::ids::short_sha;
use crate::model::BundleNode;
use crate::output as out;
use crate::store::load_active_bundle;
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

fn is_loggable_node(node: &BundleNode) -> bool {
    matches!(
        node.node_type.as_str(),
        "commit.group" | "git.observed" | "revert.group" | "repo.removed"
    )
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
        "git.observed" => {
            println!(
                "{}  {}",
                out::node(&node.id),
                out::heading("observed git changes")
            );
            for change in &node.repo_changes {
                match change.movement.as_str() {
                    "advanced" => {
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
                    "rewound" => {
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
                    "diverged" => {
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
                    movement => {
                        println!(
                            "  {} {} {}",
                            out::repo_field(&change.repo_id, 10),
                            out::movement(movement),
                            out::sha(short_sha(&change.after_sha))
                        );
                    }
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

pub fn show_group(commit_group_id: &str) -> Result<()> {
    let active = load_active_bundle()?;
    let group = active
        .bundle
        .commit_groups
        .iter()
        .find(|group| group.id == commit_group_id)
        .with_context(|| format!("No commit group found for {commit_group_id}"))?;

    println!("{}  {}\n", out::node(&group.id), group.message);
    for commit in &group.commits {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == commit.repo_id)
            .with_context(|| format!("No repo found for {}", commit.repo_id))?;
        let repo_dir = repo
            .worktree_path
            .as_ref()
            .map(|path| active.root.join(path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(&repo.path));
        println!(
            "== {} {} ==",
            out::repo(&commit.repo_id),
            out::sha(short_sha(&commit.sha))
        );
        let output = git_output(
            &repo_dir,
            [
                OsString::from("show"),
                OsString::from("--stat"),
                OsString::from("--oneline"),
                OsString::from(&commit.sha),
            ],
        )?;
        println!("{output}");
    }

    Ok(())
}
