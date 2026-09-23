//! Render and upsert the managed "Knit Bundle" cross-link block in a PR body.

use super::scope::publish_scope_repo_ids;
use crate::model::ChangeGroup;
use crate::providers::publication_for_repo;
use std::collections::BTreeSet;

pub(super) const KNIT_PR_BLOCK_BEGIN: &str = "<!-- BEGIN KNIT BUNDLE -->";
pub(super) const KNIT_PR_BLOCK_END: &str = "<!-- END KNIT BUNDLE -->";
/// Bitbucket renders HTML comments as visible text, so there the managed
/// block is fenced with link reference definitions instead; Markdown link
/// reference definitions do not render as content.
pub(super) const KNIT_PR_BLOCK_BEGIN_REFS: &str = "[knit-bundle-begin]: #";
pub(super) const KNIT_PR_BLOCK_END_REFS: &str = "[knit-bundle-end]: #";

/// The generic link label for a bundle's hosted page. Provider- and
/// host-agnostic on purpose: the URL says where it goes.
pub(super) const VIEW_BUNDLE_LABEL: &str = "View bundle";

pub(super) fn initial_pr_body(
    bundle: &ChangeGroup,
    current_repo_id: &str,
    provider: &str,
) -> String {
    render_knit_pr_block(bundle, Some(current_repo_id), provider)
}

pub(super) fn render_knit_pr_block(
    bundle: &ChangeGroup,
    current_repo_id: Option<&str>,
    provider: &str,
) -> String {
    let content = knit_block_content(bundle, current_repo_id);
    if provider == "bitbucket" {
        // A reference definition cannot interrupt a paragraph, so blank lines
        // keep both markers parsing as definitions instead of trailing along
        // with adjacent prose as literal text.
        format!("{KNIT_PR_BLOCK_BEGIN_REFS}\n\n{content}\n\n{KNIT_PR_BLOCK_END_REFS}")
    } else {
        format!("{KNIT_PR_BLOCK_BEGIN}\n{content}\n{KNIT_PR_BLOCK_END}")
    }
}

fn knit_block_content(bundle: &ChangeGroup, current_repo_id: Option<&str>) -> String {
    let mut lines = Vec::new();

    // The hosted bundle link leads the block so it is the first thing a
    // reader sees. Only server-reported URLs are used; the CLI never derives
    // a web URL from the API URL.
    for url in hosted_bundle_links(bundle) {
        lines.push(format!("[{VIEW_BUNDLE_LABEL}]({url})"));
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }

    lines.extend([
        "## Knit Bundle".to_string(),
        String::new(),
        format!("This PR is part of Knit bundle `{}`.", bundle.id),
        String::new(),
        "See the other review objects in this bundle:".to_string(),
    ]);

    let repo_ids = publish_scope_repo_ids(bundle);
    let scoped_repos = bundle
        .repos
        .iter()
        .filter(|repo| repo_ids.is_empty() || repo_ids.contains(&repo.id));

    for repo in scoped_repos {
        match publication_for_repo(bundle, &repo.id) {
            Some(pr) => {
                let marker = if current_repo_id == Some(repo.id.as_str()) {
                    " (this PR)"
                } else {
                    ""
                };
                lines.push(format!("- `{}`: {}{}", repo.id, pr.url, marker));
            }
            None => lines.push(format!("- `{}`: pending", repo.id)),
        }
    }

    lines.extend([
        String::new(),
        format!("Bundle id: `{}`", bundle.id),
        format!("Bundle title: {}", bundle.title),
    ]);

    lines.join("\n")
}

/// The bundle's hosted web links, usable and deduplicated in a deterministic
/// order. Invalid or unsafe URLs recorded by a server are omitted rather
/// than rendered.
pub(super) fn hosted_bundle_links(bundle: &ChangeGroup) -> Vec<String> {
    bundle
        .sync_targets
        .iter()
        .filter_map(|target| target.web_url.as_deref())
        .filter_map(safe_link_destination)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A URL fit for a Markdown link destination, or `None` when the recorded
/// URL is not usable and must be omitted. Parsing and validation use the
/// `url` crate: the scheme must be http(s), the authority must carry a host
/// and no credentials, and the normalized form must be free of control
/// characters. The URL is rendered in its normalized form; when it contains
/// characters that could reshape an inline destination (parentheses, or the
/// brackets of an IPv6 authority) it is wrapped in an angle-bracket
/// destination instead, which leaves balanced brackets intact.
fn safe_link_destination(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let normalized = parsed.as_str();
    if normalized.chars().any(char::is_control) {
        return None;
    }
    if normalized
        .chars()
        .any(|ch| matches!(ch, '(' | ')' | '[' | ']'))
    {
        Some(format!("<{normalized}>"))
    } else {
        Some(normalized.to_string())
    }
}

/// Whether a rendered block leads with the hosted bundle link. Only rendered
/// blocks produced by [`render_knit_pr_block`] are asked: the check looks at
/// the first non-empty content line after the block's begin marker, so a
/// heading or a stray label deeper in the block never triggers relocation.
fn block_leads_with_hosted_link(block: &str) -> bool {
    let content = block
        .strip_prefix(KNIT_PR_BLOCK_BEGIN)
        .or_else(|| block.strip_prefix(KNIT_PR_BLOCK_BEGIN_REFS))
        .unwrap_or(block);
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .is_some_and(|line| line.starts_with(&format!("[{VIEW_BUNDLE_LABEL}](")))
}

pub(super) fn upsert_knit_pr_block(existing_body: &str, block: &str) -> String {
    let hosted_link_first = block_leads_with_hosted_link(block);
    let Some((begin, end)) = managed_block_bounds(existing_body) else {
        return place_knit_pr_block(existing_body, block, hosted_link_first);
    };

    let before = existing_body[..begin].trim_end();
    let after = existing_body[end..].trim_start();
    if hosted_link_first {
        // The hosted link must be the first visible content, so the managed
        // block moves to the top; the surrounding user prose keeps its
        // original order after it.
        let mut rest = Vec::new();
        if !before.is_empty() {
            rest.push(before);
        }
        if !after.is_empty() {
            rest.push(after);
        }
        return match rest.is_empty() {
            true => block.to_string(),
            false => format!("{block}\n\n{}", rest.join("\n\n")),
        };
    }
    match (before.is_empty(), after.is_empty()) {
        (true, true) => block.to_string(),
        (true, false) => format!("{block}\n\n{after}"),
        (false, true) => format!("{before}\n\n{block}"),
        (false, false) => format!("{before}\n\n{block}\n\n{after}"),
    }
}

/// Place a block into a body that has no managed block yet. With a hosted
/// link the block leads the body; otherwise it is appended after the
/// existing prose, the placement it always had. Prepending keeps the
/// existing body verbatim — trailing whitespace included — so no user
/// formatting is lost to the move.
fn place_knit_pr_block(existing_body: &str, block: &str, hosted_link_first: bool) -> String {
    if existing_body.trim().is_empty() {
        return block.to_string();
    }
    if hosted_link_first {
        format!("{block}\n\n{existing_body}")
    } else {
        format!("{}\n\n{}", existing_body.trim_end(), block)
    }
}

/// Byte range of the first complete managed-block delimiter pair in `body`.
///
/// Both delimiter generations are recognized: the HTML comments older Knit
/// versions wrote on every host, and the reference definitions Bitbucket gets.
/// A pair must close with its own end marker — markers of different
/// generations never pair — and a body without any complete pair returns
/// `None` so the caller appends instead of eating unmatched text.
fn managed_block_bounds(body: &str) -> Option<(usize, usize)> {
    [
        (KNIT_PR_BLOCK_BEGIN, KNIT_PR_BLOCK_END),
        (KNIT_PR_BLOCK_BEGIN_REFS, KNIT_PR_BLOCK_END_REFS),
    ]
    .into_iter()
    .filter_map(|(begin_marker, end_marker)| {
        let begin = body.find(begin_marker)?;
        let end = begin + body[begin..].find(end_marker)? + end_marker.len();
        Some((begin, end))
    })
    .min_by_key(|(begin, _)| *begin)
}
