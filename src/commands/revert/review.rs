//! Shared, identity-checked compensation for legacy rollback and saved-plan recovery.
use crate::{
    contribution,
    git::{git_output, is_ancestor, rev_parse},
    model::RepoEntry,
    providers::{self, Forge, PrTarget},
};
use anyhow::{bail, Context, Result};

/// Return the new review and its own identity; the original contribution's
/// branch/SHA must never be reused to verify a compensation review.
pub(crate) fn create_review(
    forge: &dyn Forge,
    target: &PrTarget,
    repo: &RepoEntry,
    selector: &str,
    title: &str,
    body: &str,
) -> Result<(String, PrTarget)> {
    providers::target_credential(target, forge.id())?;
    let original = forge.view(target, selector)?;
    if original.state.as_deref() != Some("MERGED") {
        bail!("only a verified merged review can be reverted");
    }
    if target.contribution.is_none() {
        return Ok((
            forge.revert_pull_request(target, selector, title, body)?,
            target.clone(),
        ));
    }
    if forge.id() != "github" {
        bail!("cross-repository compensation requires GitHub");
    }
    let identity = target.contribution.as_ref().unwrap();
    contribution::publication_url(identity, selector)?;
    let revision = forge
        .merged_revision(target, selector)?
        .context("merged review has no immutable merge revision")?;
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid merged revision");
    }
    let cwd = &target.cwd;
    git_output(cwd, ["rev-parse", "--git-dir"])
        .context("fork compensation requires a local repository binding")?;
    let source = contribution::source(repo).context("missing source remote")?;
    let destination = contribution::destination(repo).context("missing target remote")?;
    let mut source_target = PrTarget::explicit(cwd, &identity.source);
    source_target.repo_remote = Some(source.to_owned());
    providers::target_credential(&source_target, forge.id())?;
    // Check both repository identities and fork relationship before any push.
    providers::github::preflight_contribution(target, false)?;
    let base_ref = contribution::fetch_ref(cwd, repo, &identity.base, false)?;
    let base = rev_parse(cwd, &base_ref)?;
    if git_output(cwd, ["cat-file", "-e", &format!("{revision}^{{commit}}")]).is_err() {
        git_output(cwd, ["fetch", "--no-tags", destination, &revision])?;
    }
    if !is_ancestor(cwd, &revision, &base) {
        bail!("merged revision is not contained in the recorded target base");
    }
    let parents = git_output(cwd, ["rev-list", "--parents", "-n", "1", &revision])?;
    let parents: Vec<_> = parents.split_whitespace().skip(1).collect();
    if parents.len() == 1 {
        // Squash/single-commit merges are safe only when this commit contains
        // the complete reviewed change. Never silently undo only the last
        // commit of a multi-commit rebase merge.
        if git_output(cwd, ["cat-file", "-e", &identity.sha]).is_err() {
            git_output(cwd, ["fetch", "--no-tags", source, &identity.sha])?;
        }
        let before = repo
            .base_sha
            .as_deref()
            .context("single-parent compensation requires the recorded source base revision")?;
        if !is_ancestor(cwd, before, &identity.sha) {
            bail!("recorded source base is not an ancestor of the reviewed contribution");
        }
        let reviewed = git_output(
            cwd,
            ["diff", "--binary", "--no-ext-diff", before, &identity.sha],
        )?;
        let merged = git_output(
            cwd,
            ["diff", "--binary", "--no-ext-diff", parents[0], &revision],
        )?;
        if reviewed != merged {
            bail!("merged commit does not represent the complete reviewed change; author a compensation branch explicitly for this rebase or resolved squash merge");
        }
    } else if parents.len() != 2 || parents[1] != identity.sha {
        bail!("merged commit parents do not identify the reviewed contribution");
    }
    let branch = format!(
        "knit/revert-{}-{}",
        original.number,
        crate::ids::revert_group_id()
    );
    let worktree = std::env::temp_dir().join(format!("knit-{}", crate::ids::revert_group_id()));
    git_output(
        cwd,
        [
            "worktree",
            "add",
            "--detach",
            worktree
                .to_str()
                .context("invalid compensation checkout path")?,
            &base,
        ],
    )?;
    let result = (|| {
        let mut args = vec!["revert", "--no-commit"];
        if parents.len() == 2 {
            args.extend(["-m", "1"]);
        }
        args.push(&revision);
        git_output(&worktree, args)
            .context("compensation conflicts with the target base; no branch was pushed")?;
        git_output(&worktree, ["commit", "-m", title])?;
        let sha = rev_parse(&worktree, "HEAD")?;
        let mut compensation = repo.clone();
        compensation.feature_branch = Some(branch.clone());
        compensation.head_sha = Some(sha.clone());
        compensation.base_branch = identity.base.clone();
        let new_target = contribution::target(cwd, &compensation, forge, &identity.base, true)?;
        // Require absence atomically: never overwrite a same-named remote ref.
        git_output(
            cwd,
            [
                "push",
                &format!("--force-with-lease=refs/heads/{branch}:"),
                source,
                &format!("{sha}:refs/heads/{branch}"),
            ],
        )?;
        let head = contribution::head(&compensation, &branch)?;
        let url = forge.create(&new_target, &identity.base, &head, title, body, false)?;
        forge.view(&new_target, &url)?;
        Ok((url, new_target))
    })();
    // This checkout was created solely for this attempt; the user's feature
    // checkout and branch remain untouched even if revert conflicts.
    let cleanup = git_output(
        cwd,
        ["worktree", "remove", "--force", worktree.to_str().unwrap()],
    );
    match (result, cleanup) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.context(
            "compensation review was created but its temporary checkout could not be removed",
        )),
    }
}
