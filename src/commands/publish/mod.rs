use crate::advice;
use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, RepoEntry};
use crate::output as out;
use crate::providers::{self, pr_number_from_url, publication_for_repo, PrTarget, PullRequest};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, save_active_bundle, ActiveBundle,
};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;

mod pr_body;
use pr_body::{initial_pr_body, render_knit_pr_block, upsert_knit_pr_block};

pub fn create_publications(
    selectors: &[String],
    all: bool,
    draft: bool,
    bases: &[String],
    sync: bool,
    set_upstream: bool,
    remote: &[String],
    no_remote: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_publish_repo_indexes(&active, selectors, all)?;
    let base_overrides = BaseOverrides::parse(bases)?;
    base_overrides.validate_tracked_repos(&active.bundle)?;
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = active.bundle.repos[index].clone();
        let base_branch =
            base_overrides.branch_for(&repo, publication_for_repo(&active.bundle, &repo.id));
        match create_or_reuse_pr(&mut active, &repo, &base_branch, draft, set_upstream) {
            Ok(()) => save_active_bundle(&active)?,
            Err(error) => {
                println!(
                    "{}: {}",
                    out::repo(&repo.id),
                    out::danger("PR create failed")
                );
                failures.push(format!("{}: {error:#}", repo.id));
                save_active_bundle(&active)?;
            }
        }
    }

    if failures.is_empty() && sync {
        failures.extend(sync_publications_for_indexes(&mut active, &indexes)?);
    } else if !sync {
        println!(
            "{}",
            out::warn(
                "Skipped PR body sync. Run `knit publish sync` to add cross-links later."
            )
        );
    }

    // Sync the bundle artifact to the configured KnitHub remote alongside the
    // host review objects (default on; see `knit config set push-sync`).
    crate::commands::remote::maybe_sync_bundle_to_remote(remote, no_remote)?;

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}",
            failures.join("\n")
        );
    }

    Ok(())
}

pub fn create_publications_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    selectors: &[String],
    all: bool,
    draft: bool,
    bases: &[String],
    sync: bool,
    push: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: ChangeGroup = crate::store::read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if push {
        bail!("Artifact publish does not support git push. Re-run with --no-push.");
    }

    let indexes = resolve_publish_repo_indexes_for_bundle(&bundle, selectors, all)?;
    let base_overrides = BaseOverrides::parse(bases)?;
    base_overrides.validate_tracked_repos(&bundle)?;
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = bundle.repos[index].clone();
        let base_branch = base_overrides.branch_for(&repo, publication_for_repo(&bundle, &repo.id));
        match create_or_reuse_pr_from_artifact(&cwd, &mut bundle, &repo, &base_branch, draft) {
            Ok(()) => {}
            Err(error) => {
                println!(
                    "{}: {}",
                    out::repo(&repo.id),
                    out::danger("PR create failed")
                );
                failures.push(format!("{}: {error:#}", repo.id));
            }
        }
    }

    if failures.is_empty() && sync {
        failures.extend(sync_publications_for_indexes_from_artifact(
            &cwd,
            &mut bundle,
            &indexes,
        )?);
    } else if !sync {
        println!(
            "{}",
            out::warn(
                "Skipped PR body sync. Run `knit publish sync` to add cross-links later."
            )
        );
    }

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}",
            failures.join("\n")
        );
    }

    write_bundle_artifact_output(&bundle, out_path)?;
    Ok(())
}

pub fn sync_publications(selectors: &[String], all: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_publish_repo_indexes(&active, selectors, all)?;
    let failures = sync_publications_for_indexes(&mut active, &indexes)?;
    if !failures.is_empty() {
        bail!("PR sync completed with failures:\n{}", failures.join("\n"));
    }

    Ok(())
}

pub fn sync_publications_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    selectors: &[String],
    all: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: ChangeGroup = crate::store::read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    let indexes = resolve_publish_repo_indexes_for_bundle(&bundle, selectors, all)?;
    let failures = sync_publications_for_indexes_from_artifact(&cwd, &mut bundle, &indexes)?;
    if !failures.is_empty() {
        bail!("PR sync completed with failures:\n{}", failures.join("\n"));
    }
    write_bundle_artifact_output(&bundle, out_path)?;
    Ok(())
}

pub fn show_publication_status(selectors: &[String], all: bool, live: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
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

fn resolve_publish_repo_indexes(
    active: &ActiveBundle,
    selectors: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !selectors.is_empty() {
        return resolve_repo_indexes(active, selectors, all);
    }

    let repo_ids = publish_scope_repo_ids(&active.bundle);
    if repo_ids.is_empty() {
        bail!(
            "No repos in bundle `{}` have recorded commits, repo changes, or publications. Pass repo selectors or --all to publish tracked repos anyway.",
            active.bundle.id
        );
    }

    let indexes = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| repo_ids.contains(&repo.id).then_some(index))
        .collect::<Vec<_>>();

    if indexes.is_empty() {
        bail!(
            "Bundle `{}` has recorded work, but none of it matches the tracked repos.",
            active.bundle.id
        );
    }

    Ok(indexes)
}

fn publish_scope_repo_ids(bundle: &ChangeGroup) -> BTreeSet<String> {
    let mut repo_ids = recorded_work_repo_ids(bundle);
    repo_ids.extend(
        bundle
            .publications
            .iter()
            .filter(|publication| providers::is_review_kind(&publication.kind))
            .map(|publication| publication.repo_id.clone()),
    );
    repo_ids
}

fn recorded_work_repo_ids(bundle: &ChangeGroup) -> BTreeSet<String> {
    let mut repo_ids = BTreeSet::new();

    for group in &bundle.commit_groups {
        repo_ids.extend(group.commits.iter().map(|commit| commit.repo_id.clone()));
    }

    for node in &bundle.nodes {
        repo_ids.extend(node.commits.iter().map(|commit| commit.repo_id.clone()));
        repo_ids.extend(
            node.repo_changes
                .iter()
                .map(|repo_change| repo_change.repo_id.clone()),
        );
    }

    repo_ids
}

fn resolve_publish_repo_indexes_for_bundle(
    bundle: &ChangeGroup,
    selectors: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !selectors.is_empty() {
        // Best-effort: reuse selector logic only when we have an ActiveBundle.
        // For artifact-only publish, require --all or omit selectors.
        if !selectors.is_empty() {
            bail!("Artifact-only publish does not support repo selectors yet. Use --all or omit selectors.");
        }
    }

    let repo_ids = publish_scope_repo_ids(bundle);
    let indexes = bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| repo_ids.contains(&repo.id).then_some(index))
        .collect::<Vec<_>>();

    if indexes.is_empty() {
        bail!(
            "Bundle `{}` has no repos eligible for publishing. Pass --all to force publishing every repo.",
            bundle.id
        );
    }

    Ok(indexes)
}

fn create_or_reuse_pr(
    active: &mut ActiveBundle,
    repo: &RepoEntry,
    base_branch: &str,
    draft: bool,
    set_upstream: bool,
) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    ensure_feature_branch(repo, branch, &cwd)?;
    ensure_origin(repo, &cwd)?;
    let forge = providers::for_repo(repo)?;
    let target = PrTarget::checkout(&cwd);

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    run_push(&cwd, branch, set_upstream)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;
    println!(
        "{}: {} {} {}",
        out::repo(&repo.id),
        out::movement("pushed"),
        out::branch(format!("origin/{branch}")),
        out::sha(short_sha(&sha))
    );

    if let Some(existing) = publication_for_repo(&active.bundle, &repo.id) {
        if existing.base_branch != base_branch {
            bail!(
                "{}: review object already recorded against {}. Knit records one review object per repo in a bundle; create a new bundle or publish before changing the base.",
                repo.id,
                out::branch(&existing.base_branch)
            );
        }
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            existing.url
        );
        return Ok(());
    }

    if let Some(existing) = forge.find_existing(&target, branch, base_branch)? {
        providers::upsert_publication(&mut active.bundle, repo, forge.as_ref(), &existing);
        let pr =
            publication_for_repo(&active.bundle, &repo.id).expect("publication was just inserted");
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            pr.url
        );
        return Ok(());
    }

    let title = format!("{} ({})", active.bundle.title, repo.id);
    let initial_body = initial_pr_body(&active.bundle, &repo.id);
    let url = forge.create(&target, base_branch, branch, &title, &initial_body, draft)?;
    let summary = forge.view(&target, &url).unwrap_or_else(|_| PullRequest {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(base_branch.to_string()),
        head_ref_name: Some(branch.to_string()),
        body: None,
        is_draft: None,
        head_ref_oid: None,
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
    });
    providers::upsert_publication(&mut active.bundle, repo, forge.as_ref(), &summary);

    let pr = publication_for_repo(&active.bundle, &repo.id).expect("publication was just inserted");
    println!(
        "{}: {} #{} {}",
        out::repo(&repo.id),
        out::movement("created"),
        pr.number,
        pr.url
    );
    Ok(())
}

fn create_or_reuse_pr_from_artifact(
    cwd: &Path,
    bundle: &mut ChangeGroup,
    repo: &RepoEntry,
    base_branch: &str,
    draft: bool,
) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded in the bundle artifact.",
            repo.id
        )
    })?;
    let remote = repo
        .remote
        .as_deref()
        .with_context(|| format!("{}: no git remote recorded in the bundle artifact.", repo.id))?;
    let forge = providers::for_repo(repo)?;
    let repo_full_name = forge
        .repo_full_name(remote)
        .with_context(|| format!("{}: invalid {} remote {remote}", repo.id, forge.id()))?;
    let target = PrTarget::explicit(cwd, repo_full_name);

    if let Some(existing) = publication_for_repo(bundle, &repo.id) {
        if existing.base_branch != base_branch {
            bail!(
                "{}: review object already recorded against {}. Knit records one review object per repo in a bundle; create a new bundle or publish before changing the base.",
                repo.id,
                out::branch(&existing.base_branch)
            );
        }
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            existing.url
        );
        return Ok(());
    }

    if let Some(existing) = forge.find_existing(&target, branch, base_branch)? {
        providers::upsert_publication(bundle, repo, forge.as_ref(), &existing);
        let pr = publication_for_repo(bundle, &repo.id).expect("publication was just inserted");
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            pr.url
        );
        return Ok(());
    }

    let title = format!("{} ({})", bundle.title, repo.id);
    let initial_body = initial_pr_body(bundle, &repo.id);
    let url = forge.create(&target, base_branch, branch, &title, &initial_body, draft)?;
    let summary = forge.view(&target, &url).unwrap_or_else(|_| PullRequest {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(base_branch.to_string()),
        head_ref_name: Some(branch.to_string()),
        body: None,
        is_draft: None,
        head_ref_oid: None,
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
    });
    providers::upsert_publication(bundle, repo, forge.as_ref(), &summary);

    let pr = publication_for_repo(bundle, &repo.id).expect("publication was just inserted");
    println!(
        "{}: {} #{} {}",
        out::repo(&repo.id),
        out::movement("created"),
        pr.number,
        pr.url
    );
    Ok(())
}

#[derive(Debug, Default)]
struct BaseOverrides {
    default: Option<String>,
    per_repo: BTreeMap<String, String>,
}

impl BaseOverrides {
    fn parse(values: &[String]) -> Result<Self> {
        let mut overrides = Self::default();
        for value in values {
            let value = value.trim();
            if value.is_empty() {
                bail!("--base cannot be empty.");
            }
            if let Some((repo_id, branch)) = value.split_once('=') {
                let repo_id = repo_id.trim();
                let branch = branch.trim();
                if repo_id.is_empty() || branch.is_empty() {
                    bail!("Use --base REPO=BRANCH with both sides present.");
                }
                overrides
                    .per_repo
                    .insert(crate::ids::slugify(repo_id), branch.to_string());
            } else if overrides.default.replace(value.to_string()).is_some() {
                bail!("Pass only one default --base value, or use repeated --base REPO=BRANCH overrides.");
            }
        }
        Ok(overrides)
    }

    fn branch_for(
        &self,
        repo: &RepoEntry,
        existing: Option<&crate::model::PublicationEntry>,
    ) -> String {
        self.per_repo
            .get(&repo.id)
            .or(self.default.as_ref())
            .cloned()
            .or_else(|| existing.map(|publication| publication.base_branch.clone()))
            .unwrap_or_else(|| repo.base_branch.clone())
    }

    fn validate_tracked_repos(&self, bundle: &ChangeGroup) -> Result<()> {
        for repo_id in self.per_repo.keys() {
            if !bundle.repos.iter().any(|repo| &repo.id == repo_id) {
                bail!("--base references unknown repo `{repo_id}`.");
            }
        }
        Ok(())
    }
}

fn sync_publications_for_indexes(
    active: &mut ActiveBundle,
    indexes: &[usize],
) -> Result<Vec<String>> {
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = active.bundle.repos[index].clone();
        match sync_one_pr(active, &repo) {
            Ok(()) => save_active_bundle(active)?,
            Err(error) => {
                println!("{}: {}", out::repo(&repo.id), out::danger("PR sync failed"));
                failures.push(format!("{}: {error:#}", repo.id));
                save_active_bundle(active)?;
            }
        }
    }

    Ok(failures)
}

fn sync_publications_for_indexes_from_artifact(
    cwd: &Path,
    bundle: &mut ChangeGroup,
    indexes: &[usize],
) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    for index in indexes.iter().copied() {
        let repo = bundle.repos[index].clone();
        match sync_one_pr_from_artifact(cwd, bundle, &repo) {
            Ok(()) => {}
            Err(error) => {
                println!("{}: {}", out::repo(&repo.id), out::danger("PR sync failed"));
                failures.push(format!("{}: {error:#}", repo.id));
            }
        }
    }
    Ok(failures)
}

fn sync_one_pr(active: &mut ActiveBundle, repo: &RepoEntry) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let forge = providers::for_repo(repo)?;
    let target = PrTarget::checkout(&cwd);

    let summary = if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(&target, branch, &repo.base_branch)? {
        existing
    } else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("no review object recorded")
        );
        return Ok(());
    };

    providers::upsert_publication(&mut active.bundle, repo, forge.as_ref(), &summary);
    let current_body = summary.body.unwrap_or_default();
    let block = render_knit_pr_block(&active.bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body != current_body {
        let pr =
            publication_for_repo(&active.bundle, &repo.id).expect("publication was just inserted");
        forge.edit_body(&target, &pr.url, &next_body)?;
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("synced"),
            pr.url
        );
    } else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("PR body already synced")
        );
    }

    Ok(())
}

fn sync_one_pr_from_artifact(cwd: &Path, bundle: &mut ChangeGroup, repo: &RepoEntry) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded in the bundle artifact.",
            repo.id
        )
    })?;
    let remote = repo
        .remote
        .as_deref()
        .with_context(|| format!("{}: no git remote recorded in the bundle artifact.", repo.id))?;
    let forge = providers::for_repo(repo)?;
    let repo_full_name = forge
        .repo_full_name(remote)
        .with_context(|| format!("{}: invalid {} remote {remote}", repo.id, forge.id()))?;
    let target = PrTarget::explicit(cwd, repo_full_name);

    let summary = if let Some(pr) = publication_for_repo(bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(&target, branch, &repo.base_branch)? {
        existing
    } else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("no review object recorded")
        );
        return Ok(());
    };

    providers::upsert_publication(bundle, repo, forge.as_ref(), &summary);
    let current_body = summary.body.unwrap_or_default();
    let block = render_knit_pr_block(bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body != current_body {
        let pr = publication_for_repo(bundle, &repo.id).expect("publication was just inserted");
        forge.edit_body(&target, &pr.url, &next_body)?;
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("synced"),
            pr.url
        );
    } else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("PR body already synced")
        );
    }

    Ok(())
}

fn ensure_feature_branch(repo: &RepoEntry, expected: &str, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: PR publishing expected feature branch `{expected}`, found `{actual}` in {}.",
            repo.id,
            cwd.display()
        );
    }

    Ok(())
}

fn ensure_origin(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    git_output_optional(cwd, ["remote", "get-url", "origin"])?.with_context(|| {
        format!(
            "{}: no `origin` remote configured in {}",
            repo.id,
            cwd.display()
        )
    })?;
    Ok(())
}

fn run_push(cwd: &Path, branch: &str, set_upstream: bool) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(branch));

    git_output(cwd, args)?;
    Ok(())
}

fn write_bundle_artifact_output(bundle: &ChangeGroup, out_path: Option<&Path>) -> Result<()> {
    match out_path {
        Some(path) => crate::store::write_json(path, bundle),
        None => {
            let json = serde_json::to_string_pretty(bundle).context("failed to encode bundle JSON")?;
            println!("{json}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::pr_body::{KNIT_PR_BLOCK_BEGIN, KNIT_PR_BLOCK_END};
    use crate::model::{
        CommitGroup, CommitRef, PublicationEntry, CHANGE_GROUP_KIND, SCHEMA_VERSION,
    };

    fn pr_publication(repo_id: &str, number: u64, url: &str) -> PublicationEntry {
        PublicationEntry {
            repo_id: repo_id.to_string(),
            provider: "github".to_string(),
            kind: providers::PULL_REQUEST_KIND.to_string(),
            number,
            url: url.to_string(),
            base_branch: "main".to_string(),
            head_branch: "knit/venue-capacity".to_string(),
            state: "OPEN".to_string(),
            title: None,
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
        }
    }

    fn repo(id: &str) -> RepoEntry {
        RepoEntry {
            id: id.to_string(),
            path: format!("/tmp/{id}"),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: "worktree".to_string(),
            base_sha: None,
            feature_branch: Some("knit/venue-capacity".to_string()),
            worktree_path: None,
            head_sha: None,
        }
    }

    #[test]
    fn managed_block_is_replaced_without_touching_user_body() {
        let previous = format!("Intro\n\n{KNIT_PR_BLOCK_BEGIN}\nold\n{KNIT_PR_BLOCK_END}\n\nTail");
        let next = upsert_knit_pr_block(&previous, "new block");
        assert_eq!(next, "Intro\n\nnew block\n\nTail");
    }

    #[test]
    fn rendered_block_lists_known_and_pending_prs() {
        let mut bundle = ChangeGroup {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id: "venue-capacity".to_string(),
            title: "venue capacity".to_string(),
            state: Some(crate::model::BUNDLE_STATE_OPEN.to_string()),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
            created_at: "2026-05-05T00:00:00.000Z".to_string(),
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            head_node_id: None,
            repos: vec![repo("backend"), repo("frontend"), repo("docs")],
            commit_groups: vec![CommitGroup {
                id: "kg_123".to_string(),
                message: "change backend and frontend".to_string(),
                created_at: "2026-05-05T00:00:00.000Z".to_string(),
                commits: vec![
                    CommitRef {
                        repo_id: "backend".to_string(),
                        sha: "abc123".to_string(),
                    },
                    CommitRef {
                        repo_id: "frontend".to_string(),
                        sha: "def456".to_string(),
                    },
                ],
            }],
            nodes: Vec::new(),
            publications: vec![pr_publication(
                "backend",
                123,
                "https://github.com/acme/backend/pull/123",
            )],
            work_item_ids: Vec::new(),
        };

        let block = render_knit_pr_block(&bundle, Some("backend"));
        assert!(block.contains("This PR is part of Knit bundle `venue-capacity`."));
        assert!(block.contains("`backend`: https://github.com/acme/backend/pull/123 (this PR)"));
        assert!(block.contains("`frontend`: pending"));
        assert!(!block.contains("`docs`: pending"));

        bundle.publications.push(pr_publication(
            "frontend",
            456,
            "https://github.com/acme/frontend/pull/456",
        ));
        let synced = render_knit_pr_block(&bundle, Some("backend"));
        assert!(synced.contains("`frontend`: https://github.com/acme/frontend/pull/456"));
        assert!(!synced.contains("`docs`: pending"));
    }

    #[test]
    fn publish_scope_excludes_tracked_repos_without_recorded_work() {
        let bundle = ChangeGroup {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id: "venue-capacity".to_string(),
            title: "venue capacity".to_string(),
            state: Some(crate::model::BUNDLE_STATE_OPEN.to_string()),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
            created_at: "2026-05-05T00:00:00.000Z".to_string(),
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            head_node_id: None,
            repos: vec![repo("backend"), repo("docs")],
            commit_groups: vec![CommitGroup {
                id: "kg_123".to_string(),
                message: "change backend".to_string(),
                created_at: "2026-05-05T00:00:00.000Z".to_string(),
                commits: vec![CommitRef {
                    repo_id: "backend".to_string(),
                    sha: "abc123".to_string(),
                }],
            }],
            nodes: Vec::new(),
            publications: Vec::new(),
            work_item_ids: Vec::new(),
        };

        let scope = publish_scope_repo_ids(&bundle);
        assert!(scope.contains("backend"));
        assert!(!scope.contains("docs"));
    }
}
