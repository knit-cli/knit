//! Portable contribution identities; Git still owns remotes and transport.
use crate::model::{ChangeGroup, RepoEntry};
use crate::providers::{Forge, PrTarget};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn configured(repo: &RepoEntry) -> bool {
    repo.source_remote.is_some() || repo.target_remote.is_some()
}

pub fn source(repo: &RepoEntry) -> Option<&str> {
    repo.source_remote.as_deref().or(repo.remote.as_deref())
}

pub fn destination(repo: &RepoEntry) -> Option<&str> {
    repo.target_remote.as_deref().or_else(|| source(repo))
}

pub fn github_name(remote: &str) -> Result<String> {
    let (host, name) = crate::auth::remote_target(remote)?;
    if host != "github.com" || name.split('/').count() != 2 {
        bail!("explicit contribution publishing currently supports github.com only");
    }
    Ok(name)
}

/// Compare portable identities, independently of transport spelling.
pub fn same_repository(a: &str, b: &str) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    let a = crate::auth::remote_target(a)?;
    let b = crate::auth::remote_target(b)?;
    Ok(a.0.eq_ignore_ascii_case(&b.0) && a.1.eq_ignore_ascii_case(&b.1))
}

pub fn cross_repository(repo: &RepoEntry) -> Result<bool> {
    if !configured(repo) {
        return Ok(false);
    }
    let source = source(repo).context("missing sourceRemote")?;
    let target = destination(repo).context("missing targetRemote")?;
    if source.trim().is_empty() || target.trim().is_empty() {
        bail!("empty contribution remote");
    }
    Ok(!same_repository(source, target)?)
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub source: String,
    pub target: String,
    pub branch: String,
    pub sha: String,
    pub base: String,
}

pub fn identity(repo: &RepoEntry, base: &str) -> Result<Option<Identity>> {
    if !configured(repo) {
        return Ok(None);
    }
    if !cross_repository(repo)? {
        return Ok(None);
    }
    let source = github_name(source(repo).context("missing sourceRemote")?)?;
    let target = github_name(destination(repo).context("missing targetRemote")?)?;
    let branch = repo
        .feature_branch
        .clone()
        .context("contribution requires featureBranch")?;
    let sha = repo
        .head_sha
        .clone()
        .context("contribution requires recorded headSha; run knit sync")?;
    if branch.is_empty()
        || !matches!(sha.len(), 40 | 64)
        || !sha.bytes().all(|c| c.is_ascii_hexdigit())
    {
        bail!("contribution requires a nonempty feature branch and full headSha");
    }
    Ok(Some(Identity {
        source,
        target,
        branch,
        sha,
        base: base.into(),
    }))
}

pub fn target(
    cwd: &Path,
    repo: &RepoEntry,
    forge: &dyn Forge,
    base: &str,
    artifact: bool,
) -> Result<PrTarget> {
    if let Some(identity) = identity(repo, base)? {
        if base.trim().is_empty() {
            bail!("contribution requires an effective review base branch");
        }
        let mut target = PrTarget::explicit(cwd, &identity.target);
        target.repo_remote = destination(repo).map(str::to_owned);
        target.contribution = Some(identity);
        Ok(target)
    } else if artifact || configured(repo) {
        let name = destination(repo)
            .and_then(|r| forge.repo_full_name(r))
            .context("artifact has no recognized repository remote")?;
        let mut target = PrTarget::explicit(cwd, name);
        if configured(repo) {
            target.repo_remote = destination(repo).map(str::to_owned);
        }
        Ok(target)
    } else {
        Ok(PrTarget::checkout(cwd))
    }
}

pub fn head(repo: &RepoEntry, branch: &str) -> Result<String> {
    if let Some(id) = identity(repo, &repo.base_branch)? {
        Ok(format!("{}:{branch}", id.source.split('/').next().unwrap()))
    } else {
        Ok(branch.into())
    }
}

pub fn publication_url(id: &Identity, value: &str) -> Result<()> {
    let url = url::Url::parse(value).context("contribution needs a full PR URL")?;
    let prefix = format!("/{}/pull/", id.target);
    let valid = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url
            .path()
            .to_ascii_lowercase()
            .strip_prefix(&prefix.to_ascii_lowercase())
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        bail!("publication URL contradicts targetRemote");
    }
    Ok(())
}

pub fn validate_bundle(bundle: &ChangeGroup) -> Result<()> {
    for repo in &bundle.repos {
        if let Some(id) = identity(repo, &repo.base_branch)? {
            for p in bundle
                .publications
                .iter()
                .filter(|p| p.repo_id == repo.id && crate::providers::is_review_kind(&p.kind))
            {
                publication_url(&id, &p.url)?;
            }
        }
    }
    Ok(())
}

/// Resolve a configured Git remote, including its pushurl, without rewriting it.
pub fn remote_url(cwd: &Path, name: &str, push: bool) -> Result<String> {
    let key = format!("remote.{name}.{}", if push { "pushurl" } else { "url" });
    let mut urls = crate::git::git_output_optional(cwd, ["config", "--get-all", &key])?;
    if push && urls.is_none() {
        urls = crate::git::git_output_optional(
            cwd,
            ["config", "--get-all", &format!("remote.{name}.url")],
        )?;
    }
    let urls = urls.context("Git remote has no configured URL")?;
    let urls: Vec<_> = urls.lines().filter(|s| !s.is_empty()).collect();
    if urls.len() != 1 {
        bail!("contribution requires exactly one Git remote URL");
    }
    crate::auth::remote_target(urls[0])?;
    Ok(urls[0].into())
}

pub fn configure(
    cwd: &Path,
    repo: &mut RepoEntry,
    source_name: Option<&str>,
    target_name: Option<&str>,
) -> Result<()> {
    if source_name.is_none() && target_name.is_none() {
        return Ok(());
    }
    let selected = crate::auth_git::default_remote(cwd, true);
    let source_name = source_name.unwrap_or(&selected);
    repo.source_remote = Some(if crate::auth::remote_target(source_name).is_ok() {
        source_name.into()
    } else {
        remote_url(cwd, source_name, true)?
    });
    repo.target_remote = Some(remote_url(cwd, target_name.unwrap_or("origin"), false)?);
    identity(repo, &repo.base_branch)?;
    Ok(())
}

pub fn push_remote(cwd: &Path, repo: &RepoEntry) -> Result<String> {
    if !configured(repo) {
        return Ok("origin".into());
    }
    let expected = source(repo).context("missing source remote")?;
    let names = crate::git::git_output(cwd, ["remote"])?;
    for name in names.lines() {
        if let Ok(url) = remote_url(cwd, name, true) {
            if same_repository(&url, expected)? {
                return Ok(name.into());
            }
        }
    }
    bail!("{}: no Git remote with a single push URL matching sourceRemote; configure one before pushing", repo.id)
}

/// Fetch explicit roles by URL so a split fetch/push remote cannot cross them.
pub fn fetch_ref(cwd: &Path, repo: &RepoEntry, branch: &str, feature: bool) -> Result<String> {
    let url = if feature {
        source(repo)
    } else {
        destination(repo)
    }
    .context("missing contribution remote")?;
    let reference = role_ref(repo, branch, feature)?;
    crate::git::git_output(
        cwd,
        [
            "fetch",
            "--no-tags",
            url,
            &format!("+refs/heads/{branch}:{reference}"),
        ],
    )?;
    if feature {
        let sha = crate::git::rev_parse(cwd, &reference)?;
        record_source_observation(cwd, url, branch, &sha)?;
    }
    Ok(reference)
}

/// Save the exact fork tip fetched by Knit, not a local merge/rebase result.
pub(crate) fn record_source_observation(
    cwd: &Path,
    url: &str,
    branch: &str,
    sha: &str,
) -> Result<()> {
    let reference = tracking_ref(url, branch);
    crate::git::git_output(cwd, ["update-ref", &reference, sha])?;
    let names = crate::git::git_output(cwd, ["remote"])?;
    for name in names.lines() {
        let Ok(fetch_url) = remote_url(cwd, name, false) else {
            continue;
        };
        if same_repository(&fetch_url, url)? {
            if let Some(native_ref) = native_tracking_ref(cwd, name, branch)? {
                crate::git::git_output(cwd, ["update-ref", &native_ref, sha])?;
            }
        }
    }
    Ok(())
}

pub fn role_ref(repo: &RepoEntry, branch: &str, feature: bool) -> Result<String> {
    let url = if feature {
        source(repo)
    } else {
        destination(repo)
    }
    .context("missing contribution remote")?;
    Ok(tracking_ref(url, branch))
}

fn tracking_ref(url: &str, branch: &str) -> String {
    // Match same_repository: transport spelling and case do not identify a
    // different repository. Keep local/non-forge URLs compatible with old refs.
    let identity = crate::auth::remote_target(url)
        .map(|(host, path)| {
            format!(
                "{}/{}",
                host.to_ascii_lowercase(),
                path.to_ascii_lowercase()
            )
        })
        .unwrap_or_else(|_| url.to_owned());
    legacy_tracking_ref(&identity, branch)
}

fn legacy_tracking_ref(url: &str, branch: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "refs/knit/contributions/{:x}",
        Sha256::digest(format!("{url}\0{branch}").as_bytes())
    )
}

/// Only mirror the standard, sole branch mapping. Custom or negative refspecs
/// stay on the canonical source ref rather than guessing a tracking destination.
fn native_tracking_ref(cwd: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    let mapping = crate::git::git_output_optional(
        cwd,
        ["config", "--get-all", &format!("remote.{remote}.fetch")],
    )?;
    let standard = format!("refs/heads/*:refs/remotes/{remote}/*");
    Ok(mapping
        .filter(|value| value.strip_prefix('+').unwrap_or(value) == standard)
        .map(|_| format!("refs/remotes/{remote}/{branch}")))
}

pub(crate) struct PushTracking {
    pub reference: String,
    pub expected: Option<String>,
    pub explicit_lease: bool,
}

/// Read the fork observation without fetching. Dedicated source remotes use
/// tracking refs shared by native Git and Knit; split remotes use the role ref.
pub(crate) fn push_tracking(
    cwd: &Path,
    remote: &str,
    branch: &str,
    recorded_source: Option<&str>,
) -> Result<Option<PushTracking>> {
    let Ok(push_url) = remote_url(cwd, remote, true) else {
        return Ok(None);
    };
    let fetch_url = remote_url(cwd, remote, false)?;
    let split = !same_repository(&push_url, &fetch_url)?;
    let reference = tracking_ref(&push_url, branch);
    let mut candidates = Vec::new();
    if !split {
        if let Some(native_ref) = native_tracking_ref(cwd, remote, branch)? {
            candidates.push(native_ref);
        }
    }
    candidates.push(reference.clone());
    // Older Knit versions keyed observations by raw URL. Only consider legacy
    // keys for this source, never the target repository's observation.
    if let Some(source) = recorded_source {
        if !same_repository(source, &push_url)? {
            bail!("push remote contradicts recorded sourceRemote");
        }
        candidates.push(legacy_tracking_ref(source, branch));
    }
    candidates.push(legacy_tracking_ref(&push_url, branch));
    let mut expected = None;
    for candidate in candidates {
        expected = crate::git::ref_commit_sha(cwd, &candidate)?;
        if expected.is_some() {
            break;
        }
    }
    Ok(Some(PushTracking {
        reference,
        expected,
        explicit_lease: split || recorded_source.is_some(),
    }))
}

pub fn track_source(cwd: &Path, repo: &RepoEntry, branch: &str) -> Result<()> {
    if !configured(repo) {
        return Ok(());
    }
    let url = source(repo).context("missing sourceRemote")?;
    crate::git::git_output(cwd, ["config", &format!("branch.{branch}.remote"), url])?;
    crate::git::git_output(
        cwd,
        [
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(())
}

/// Union merges must not silently select between two contribution intents.
pub fn compatible(local: &ChangeGroup, remote: &ChangeGroup) -> Result<()> {
    for repo in &local.repos {
        if let Some(other) = remote.repos.iter().find(|r| r.id == repo.id) {
            if configured(repo) && configured(other) {
                for (a, b) in [
                    (source(repo), source(other)),
                    (destination(repo), destination(other)),
                ] {
                    if !same_repository(
                        a.context("missing contribution identity")?,
                        b.context("missing contribution identity")?,
                    )? {
                        bail!("{}: conflicting sourceRemote/targetRemote; reconcile identities before merging ledgers", repo.id);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Discover a split origin pushurl or Git push preference; never infer a fork parent.
pub fn discover(cwd: &Path, repo: &mut RepoEntry) -> Result<bool> {
    if configured(repo) {
        return Ok(false);
    }
    let selected = crate::auth_git::default_remote(cwd, true);
    if crate::auth::remote_target(&selected).is_err() {
        let urls = crate::git::git_output_optional(
            cwd,
            ["config", "--get-all", &format!("remote.{selected}.pushurl")],
        )?
        .or(crate::git::git_output_optional(
            cwd,
            ["config", "--get-all", &format!("remote.{selected}.url")],
        )?);
        if urls.as_deref().is_some_and(|urls| urls.lines().count() > 1) {
            bail!("ambiguous Git push destination: configure one source remote before publishing");
        }
    }
    let push = if crate::auth::remote_target(&selected).is_ok() {
        Some(selected.clone())
    } else {
        remote_url(cwd, &selected, true).ok()
    };
    let base = remote_url(cwd, "origin", false).ok();
    if let (Some(push), Some(base)) = (push, base) {
        if !same_repository(&push, &base)? {
            repo.source_remote = Some(push);
            repo.target_remote = Some(base);
            identity(repo, &repo.base_branch)?;
            return Ok(true);
        }
    }
    Ok(false)
}
