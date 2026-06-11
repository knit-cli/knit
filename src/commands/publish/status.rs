//! `knit publish status`: recorded review objects, optionally joined with
//! live landing-readiness columns from the host.

use super::scope::filter_indexes_by_provider;
use crate::advice;
use crate::model::ChangeGroup;
use crate::output as out;
use crate::providers::{self, publication_for_repo};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Result};

pub fn show_publication_status(
    selectors: &[String],
    all: bool,
    live: bool,
    provider: Option<&str>,
) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let indexes = filter_indexes_by_provider(&active.bundle.repos, indexes, provider)?;
    println!("Bundle: {}\n", out::heading(&active.bundle.id));

    if live {
        // Fetch live state from the host and report landing-readiness columns.
        println!(
            "{}  {}  {}  {}  {}  {}  {}",
            out::header_field("repo", 16),
            out::header_field("pr", 6),
            out::header_field("state", 8),
            out::header_field("mergeable", 10),
            out::header_field("checks", 9),
            out::header_field("review", 9),
            out::heading("verdict")
        );
        for index in indexes {
            let repo = &active.bundle.repos[index];
            match publication_for_repo(&active.bundle, &repo.id) {
                Some(pr) => {
                    let readiness =
                        crate::commands::land::assess_landing_readiness(&active, repo, &pr.url);
                    crate::commands::land::print_readiness_row(&readiness);
                }
                None => println!(
                    "{}  {}",
                    out::repo_field(&repo.id, 16),
                    out::muted("(no PR)")
                ),
            }
        }
        print_landing_advice(&active);
        return Ok(());
    }

    println!(
        "{}  {}  {}  {}",
        out::header_field("repo", 14),
        out::header_field("review", 10),
        out::header_field("state", 12),
        out::heading("url")
    );

    for index in indexes {
        let repo = &active.bundle.repos[index];
        if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
            println!(
                "{}  {}  {}  {}",
                out::repo_field(&repo.id, 14),
                out::sha(format!("#{}", pr.number)),
                out::status(&format!("{:<12}", pr.state.to_lowercase())),
                pr.url
            );
        } else {
            println!(
                "{}  {}  {}  {}",
                out::repo_field(&repo.id, 14),
                out::muted(format!("{:<10}", "(none)")),
                out::muted(format!("{:<12}", "not created")),
                out::muted("-")
            );
        }
    }
    print_landing_advice(&active);

    Ok(())
}

fn print_landing_advice(active: &ActiveBundle) {
    if active.bundle.publications.is_empty() || has_landed_node(&active.bundle) {
        return;
    }
    let review_count = active
        .bundle
        .publications
        .iter()
        .filter(|publication| providers::is_review_kind(&publication.kind))
        .count();
    if review_count == 0 {
        return;
    }
    println!();
    println!(
        "{} {} review object(s) recorded, not landed",
        out::heading("Landing:"),
        review_count
    );
    advice::print(
        &active.root,
        "when the user says to land/release, run `knit land` to create or show the plan, then `knit land apply` after inspection.",
    );
}

fn has_landed_node(bundle: &ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.landed")
}
