//! Recoverable, bundle-wide Git history rewrites.
use crate::checkout::checkout_dir;
use crate::contribution;
use crate::git::{
    current_branch, git_output, git_output_with_env, is_ancestor, ref_commit_sha, rev_parse,
};
use crate::model::ChangeGroup;
use crate::rewrite::{record_rewrite, RepoRewrite, RewriteKind};
use crate::store::{load_active_bundle_for_update, save_active_bundle, write_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct RewriteState {
    bundle_id: String,
    original_bundle: ChangeGroup,
    squash: bool,
    message: String,
    repos: Vec<RepoState>,
    // Persist the exact ledger result before saving it, making a failed save retryable.
    final_bundle: Option<ChangeGroup>,
    groups: Vec<String>,
    #[serde(default)]
    aborting: bool,
}

#[derive(Serialize, Deserialize)]
struct RepoState {
    repo_id: String,
    path: PathBuf,
    branch: String,
    old_base: String,
    old_head: String,
    new_base: String,
    new_head: Option<String>,
    status: String,
    phase: String,
    needs_squash: bool,
    #[serde(default)]
    rebase_head: Option<String>,
    #[serde(default)]
    repin: Option<Repin>,
}

#[derive(Serialize, Deserialize)]
struct Repin {
    head: String,
    files: Vec<LockEdit>,
    tree: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LockEdit {
    path: String,
    before: String,
    after: String,
}

// Only tracked lockfiles are rewrite inputs; never regenerate or serialize TOML.
fn lockfiles(path: &Path) -> Result<Vec<String>> {
    Ok(git_output(path, ["ls-files", "--stage", "-z"])?
        .split('\0')
        .filter_map(|entry| entry.split_once('\t'))
        .filter(|(mode, name)| {
            mode.starts_with("100")
                && Path::new(name)
                    .file_name()
                    .is_some_and(|n| n == "Cargo.lock")
        })
        .map(|(_, name)| name.to_owned())
        .collect())
}

fn lock_source(line: &str) -> Option<(&str, String, std::ops::Range<usize>)> {
    let (key, value) = line.split_once('=')?;
    if key.trim() != "source" {
        return None;
    }
    let value = value.trim_start().strip_prefix('"')?;
    let end = value.find('"')?;
    let source = value[..end].strip_prefix("git+")?;
    let (remote, _) = source.split_once('?')?;
    let parsed = url::Url::parse(source).ok()?;
    let pairs: Vec<_> = parsed.query_pairs().collect();
    if pairs.len() != 1 || pairs[0].0 != "branch" {
        return None;
    }
    let sha = parsed.fragment()?;
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let start = line.len() - value.len() + end - sha.len();
    Some((remote, pairs[0].1.to_string(), start..start + sha.len()))
}

fn library_urls(state: &RewriteState) -> BTreeMap<String, Vec<String>> {
    state
        .original_bundle
        .repos
        .iter()
        .map(|repo| {
            let mut urls: Vec<String> = [
                repo.remote.clone(),
                repo.source_remote.clone(),
                repo.target_remote.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            if let Some(saved) = state.repos.iter().find(|r| r.repo_id == repo.id) {
                if let Ok(url) = git_output(&saved.path, ["remote", "get-url", "--push", "origin"])
                {
                    urls.push(url);
                }
            }
            (repo.id.clone(), urls)
        })
        .collect()
}

fn matching_library<'a>(
    state: &'a RewriteState,
    urls: &BTreeMap<String, Vec<String>>,
    consumer: &str,
    remote: &str,
    branch: &str,
) -> Result<Option<&'a RepoState>> {
    if branch != format!("knit/{}", state.bundle_id) {
        return Ok(None);
    }
    let matches: Vec<_> = state
        .repos
        .iter()
        .filter(|repo| {
            repo.repo_id != consumer
                && repo.branch == branch
                && urls[&repo.repo_id]
                    .iter()
                    .any(|url| contribution::same_repository(remote, url).unwrap_or(false))
        })
        .collect();
    if matches.len() > 1 {
        bail!("Ambiguous Cargo lockfile library for {consumer}");
    }
    Ok(matches.into_iter().next())
}

fn order_libraries_first(state: &mut RewriteState) -> Result<()> {
    let urls = library_urls(state);
    let mut dependencies = BTreeMap::<String, Vec<String>>::new();
    for repo in &state.repos {
        let deps = dependencies.entry(repo.repo_id.clone()).or_default();
        for file in lockfiles(&repo.path)? {
            for line in fs::read_to_string(repo.path.join(file))?.lines() {
                if let Some((remote, branch, _)) = lock_source(line) {
                    if let Some(library) =
                        matching_library(state, &urls, &repo.repo_id, remote, &branch)?
                    {
                        deps.push(library.repo_id.clone());
                    }
                }
            }
        }
    }
    let mut ordered = Vec::new();
    while !state.repos.is_empty() {
        let Some(i) = state.repos.iter().position(|repo| {
            dependencies[&repo.repo_id]
                .iter()
                .all(|id| ordered.iter().any(|r: &RepoState| &r.repo_id == id))
        }) else {
            bail!("Cyclic Cargo lockfile dependencies prevent a library-first rewrite");
        };
        ordered.push(state.repos.remove(i));
    }
    state.repos = ordered;
    Ok(())
}

fn plan_repin(state: &RewriteState, i: usize) -> Result<Repin> {
    let repo = &state.repos[i];
    let urls = library_urls(state);
    let mut files = Vec::new();
    for path in lockfiles(&repo.path)? {
        let before = fs::read_to_string(repo.path.join(&path))?;
        let mut after = String::new();
        for line in before.split_inclusive('\n') {
            let mut replacement = None;
            if let Some((remote, branch, range)) = lock_source(line) {
                if let Some(library) =
                    matching_library(state, &urls, &repo.repo_id, remote, &branch)?
                {
                    let head = library
                        .new_head
                        .as_deref()
                        .context("Cargo library rewrite is not complete")?;
                    if head != library.old_head {
                        replacement = Some((range, head));
                    }
                }
            }
            if let Some((range, head)) = replacement {
                after.push_str(&line[..range.start]);
                after.push_str(head);
                after.push_str(&line[range.end..]);
            } else {
                after.push_str(line);
            }
        }
        if before != after {
            files.push(LockEdit {
                path,
                before,
                after,
            });
        }
    }
    let head = rev_parse(&repo.path, "HEAD")?;
    if !files.is_empty() && head == repo.new_base {
        bail!(
            "{}: cannot fold Cargo lockfile repinning into a missing feature commit",
            repo.repo_id
        );
    }
    Ok(Repin {
        head,
        files,
        tree: None,
    })
}

fn execute_repin(state: &mut RewriteState, i: usize, path: &Path) -> Result<()> {
    let repo = &state.repos[i];
    let repin = repo
        .repin
        .as_ref()
        .context("Missing Cargo repin checkpoint")?;
    let head = rev_parse(&repo.path, "HEAD")?;
    if head != repin.head {
        // The amend may have succeeded before the durable checkpoint.
        return verify_repin(repo, repin);
    }
    if repin.files.is_empty() {
        clean(&repo.path)?;
        return Ok(());
    }
    // Retry only the saved edits, never absorb unrelated staged or working changes.
    let changed = format!(
        "{}\0{}",
        git_output(&repo.path, ["diff", "--name-only", "-z"])?,
        git_output(&repo.path, ["diff", "--cached", "--name-only", "-z"])?
    );
    for changed in changed.split('\0').filter(|s| !s.is_empty()) {
        if !repin.files.iter().any(|edit| edit.path == changed) {
            bail!("Unrelated changes during Cargo repin recovery: {changed}");
        }
    }
    for edit in &repin.files {
        let current = fs::read_to_string(repo.path.join(&edit.path))?;
        let indexed = git_output(&repo.path, ["show", &format!(":{}", edit.path)])?;
        if (current != edit.before && current != edit.after)
            || (indexed != edit.before.trim_end() && indexed != edit.after.trim_end())
        {
            bail!(
                "{}: lockfile changed during Cargo repin recovery",
                edit.path
            );
        }
    }
    for edit in &repin.files {
        fs::write(repo.path.join(&edit.path), &edit.after)?;
        git_output(&repo.path, ["add", "--", &edit.path])?;
    }
    let tree = git_output(&repo.path, ["write-tree"])?;
    state.repos[i].repin.as_mut().unwrap().tree = Some(tree);
    persist(path, state)?;
    git_output(&state.repos[i].path, ["commit", "--amend", "--no-edit"])?;
    let repo = &state.repos[i];
    verify_repin(repo, repo.repin.as_ref().unwrap())
}

fn verify_repin(repo: &RepoState, repin: &Repin) -> Result<()> {
    clean(&repo.path)?;
    if repin.tree.as_deref() != Some(rev_parse(&repo.path, "HEAD^{tree}")?.as_str())
        || git_output(&repo.path, ["show", "-s", "--format=%P", "HEAD"])?
            != git_output(&repo.path, ["show", "-s", "--format=%P", &repin.head])?
    {
        bail!("Unexpected HEAD during Cargo repin recovery");
    }
    Ok(())
}

pub fn squash(message: Option<&str>) -> Result<()> {
    start(true, message, true, false)
}

pub fn rebase(
    squash: bool,
    message: Option<&str>,
    offline: bool,
    continue_rebase: bool,
    abort: bool,
) -> Result<()> {
    if continue_rebase || abort {
        let mut active = load_active_bundle_for_update()?;
        let path = state_path(&active)?;
        let mut state: RewriteState = serde_json::from_slice(
            &fs::read(&path)
                .context("No pending rewrite; start with knit rebase or knit squash")?,
        )?;
        validate_state(&active, &state)?;
        if abort {
            abort_rewrite(&active, &mut state, &path)
        } else {
            if state.aborting {
                bail!("Abort is in progress; retry knit rebase --abort");
            }
            execute(&mut active, &mut state, &path)
        }
    } else {
        start(squash, message, offline, true)
    }
}

fn state_path(active: &ActiveBundle) -> Result<PathBuf> {
    let id = &active.bundle.id;
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        bail!("Invalid bundle id for rewrite recovery");
    }
    Ok(active.root.join(".knit/rebase").join(format!("{id}.json")))
}

fn persist(path: &Path, state: &RewriteState) -> Result<()> {
    write_json(path, state)?;
    // Windows requires write access for FlushFileBuffers, used by sync_all.
    fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("Missing recovery directory")?)?.sync_all()?;
    Ok(())
}

fn clean(path: &Path) -> Result<()> {
    if !git_output(path, ["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        bail!(
            "{}: tracked staged/unstaged changes must be committed or stashed first",
            path.display()
        );
    }
    Ok(())
}

fn expected_branch(path: &Path, branch: &str) -> Result<()> {
    if current_branch(path)?.as_deref() != Some(branch) {
        bail!("{}: expected feature branch {branch}", path.display());
    }
    Ok(())
}

fn git_path_exists(path: &Path, name: &str) -> Result<bool> {
    let value = git_output(path, ["rev-parse", "--git-path", name])?;
    Ok(path.join(value).exists())
}

fn rebasing(path: &Path) -> Result<bool> {
    Ok(git_path_exists(path, "rebase-merge")? || git_path_exists(path, "rebase-apply")?)
}

fn start(squash: bool, message: Option<&str>, offline: bool, do_rebase: bool) -> Result<()> {
    start_active(
        load_active_bundle_for_update()?,
        squash,
        message,
        offline,
        do_rebase,
    )
}

fn start_active(
    mut active: ActiveBundle,
    squash: bool,
    message: Option<&str>,
    offline: bool,
    do_rebase: bool,
) -> Result<()> {
    let path = state_path(&active)?;
    if path.exists() {
        bail!("A rewrite is already pending. Use knit rebase --continue or knit rebase --abort");
    }
    let message = message.map(str::to_owned).unwrap_or_else(|| {
        if active.bundle.commit_groups.len() == 1 {
            active.bundle.commit_groups[0].message.clone()
        } else {
            active.bundle.title.clone()
        }
    });
    if squash && message.trim().is_empty() {
        bail!("Squash message must not be empty");
    }
    let mut repos = Vec::new();
    // Complete local preflight for every repo before even fetching a base.
    for repo in &active.bundle.repos {
        let checkout = checkout_dir(&active, repo)
            .with_context(|| format!("{}: checkout is not materialized", repo.id))?;
        let branch = repo
            .feature_branch
            .as_ref()
            .context("Missing feature branch")?;
        expected_branch(&checkout, branch)?;
        clean(&checkout)?;
        if rebasing(&checkout)?
            || git_path_exists(&checkout, "MERGE_HEAD")?
            || git_path_exists(&checkout, "CHERRY_PICK_HEAD")?
            || git_path_exists(&checkout, "REVERT_HEAD")?
        {
            bail!("{}: finish the existing Git operation first", repo.id);
        }
        let old_head = rev_parse(&checkout, "HEAD")?;
        let old_base = rev_parse(
            &checkout,
            repo.base_sha
                .as_deref()
                .context("Missing recorded base SHA")?,
        )?;
        if squash && !is_ancestor(&checkout, &old_base, &old_head) {
            bail!(
                "{} ({}): recorded base {} is not an ancestor of HEAD; refusing to squash onto obsolete history. Run knit sync to reconcile manual Git changes, then verify the recorded base before retrying",
                repo.id, checkout.display(), old_base
            );
        }
        let count: usize = git_output(
            &checkout,
            ["rev-list", "--count", &format!("{old_base}..{old_head}")],
        )?
        .parse()?;
        let needs_squash = squash
            && (crate::author::needs_reauthor(&checkout, &old_base, &old_head)?
                || count > 1
                || (count == 1
                    && git_output(&checkout, ["show", "-s", "--format=%B", "HEAD"])?.trim_end()
                        != message.trim_end()));
        repos.push(RepoState {
            repo_id: repo.id.clone(),
            path: checkout,
            branch: branch.clone(),
            old_base: old_base.clone(),
            old_head,
            new_base: old_base,
            new_head: None,
            status: "pending".into(),
            phase: "pending".into(),
            needs_squash,
            rebase_head: None,
            repin: None,
        });
    }
    if repos.is_empty() {
        bail!("No tracked repositories to rewrite");
    }
    if do_rebase {
        for (repo, state) in active.bundle.repos.iter().zip(&mut repos) {
            let reference = if contribution::configured(repo) {
                if offline {
                    contribution::role_ref(repo, &repo.base_branch, false)?
                } else {
                    contribution::fetch_ref(&state.path, repo, &repo.base_branch, false)?
                }
            } else {
                let reference = format!("refs/remotes/origin/{}", repo.base_branch);
                if !offline {
                    git_output(
                        &state.path,
                        [
                            "fetch",
                            "--no-tags",
                            "origin",
                            &format!("+refs/heads/{}:{reference}", repo.base_branch),
                        ],
                    )?;
                }
                reference
            };
            state.new_base = match ref_commit_sha(&state.path, &reference)? {
                Some(sha) => sha,
                None if offline => {
                    rev_parse(&state.path, &format!("refs/heads/{}", repo.base_branch))?
                }
                None => bail!("{}: fetched base is missing", repo.id),
            };
        }
    }
    let mut state = RewriteState {
        bundle_id: active.bundle.id.clone(),
        original_bundle: active.bundle.clone(),
        squash,
        message,
        repos,
        final_bundle: None,
        groups: Vec::new(),
        aborting: false,
    };
    order_libraries_first(&mut state)?;
    fs::create_dir_all(path.parent().context("Missing state directory")?)?;
    persist(&path, &state)?;
    execute(&mut active, &mut state, &path)
}

fn validate_state(active: &ActiveBundle, state: &RewriteState) -> Result<()> {
    let current = serde_json::to_value(&active.bundle)?;
    if state.bundle_id != active.bundle.id
        || (current != serde_json::to_value(&state.original_bundle)?
            && state
                .final_bundle
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?
                .as_ref()
                != Some(&current))
    {
        bail!(
            "Bundle changed during rewrite; reconcile the saved recovery state before continuing"
        );
    }
    for saved in &state.repos {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|r| r.id == saved.repo_id)
            .context("Recovery repo missing")?;
        if checkout_dir(active, repo).as_ref() != Some(&saved.path)
            || repo.feature_branch.as_ref() != Some(&saved.branch)
        {
            bail!("{}: recovery checkout or branch changed", saved.repo_id);
        }
    }
    Ok(())
}

fn execute(active: &mut ActiveBundle, state: &mut RewriteState, path: &Path) -> Result<()> {
    // Validate every checkout before resuming any of them.
    for repo in &state.repos {
        if repo.phase != "rebasing" || !rebasing(&repo.path)? {
            expected_branch(&repo.path, &repo.branch)?;
        }
        if repo.phase == "pending" || repo.status == "done" {
            clean(&repo.path)?;
            let expected = repo.new_head.as_ref().unwrap_or(&repo.old_head);
            if rev_parse(&repo.path, "HEAD")? != *expected {
                bail!("{}: HEAD changed since rewrite was saved", repo.repo_id);
            }
        }
    }
    for i in 0..state.repos.len() {
        if state.repos[i].status == "done" {
            continue;
        }
        if let Err(error) = execute_repo(state, i, path) {
            state.repos[i].status = "conflict".into();
            persist(path, state)?;
            let repo = &state.repos[i];
            bail!("{} at {}: {error:#}\nResolve conflicts and git add the resolved files, or fix the failing hook/configuration, then run knit rebase --continue. To restore original heads, run knit rebase --abort.", repo.repo_id, repo.path.display());
        }
    }
    if state.final_bundle.is_none() && logical_noop(active, state) {
        state.groups = active
            .bundle
            .commit_groups
            .iter()
            .map(|g| g.id.clone())
            .collect();
        fs::remove_file(path)?;
        print_result(state);
        return Ok(());
    }
    if state.final_bundle.is_none() {
        let rewrites: Vec<_> = state
            .repos
            .iter()
            .filter(|r| {
                r.old_head != *r.new_head.as_ref().expect("completed rewrite head")
                    || r.old_base != r.new_base
                    || (state.squash && r.old_head != r.old_base)
            })
            .map(|r| RepoRewrite {
                repo_id: r.repo_id.clone(),
                old_head: r.old_head.clone(),
                new_head: r.new_head.clone().expect("completed rewrite head"),
                old_base: r.old_base.clone(),
                new_base: r.new_base.clone(),
            })
            .collect();
        state.groups = record_rewrite(
            active,
            &rewrites,
            if state.squash {
                RewriteKind::Squash
            } else {
                RewriteKind::Rebase
            },
            state.squash.then_some(state.message.as_str()),
        )?;
        state.final_bundle = Some(active.bundle.clone());
        persist(path, state)?;
    }
    active.bundle = state
        .final_bundle
        .clone()
        .context("Missing completed ledger")?;
    save_active_bundle(active)?;
    fs::remove_file(path)?;
    print_result(state);
    Ok(())
}

fn print_result(state: &RewriteState) {
    for repo in &state.repos {
        crate::human!(
            "{}: {} -> {} (base {})",
            repo.repo_id,
            crate::ids::short_sha(&repo.old_head),
            crate::ids::short_sha(repo.new_head.as_deref().unwrap()),
            crate::ids::short_sha(&repo.new_base)
        );
    }
    for group in &state.groups {
        crate::human!("Commit group: {group}");
    }
    crate::human!("Next: publish the rewrite with `knit push --force-with-lease`");
}

// Git no-ops can still require consolidating separately recorded groups.
fn logical_noop(active: &ActiveBundle, state: &RewriteState) -> bool {
    if state.repos.iter().any(|repo| {
        repo.new_head.as_ref() != Some(&repo.old_head) || repo.new_base != repo.old_base
    }) {
        return false;
    }
    if !state.squash {
        return true;
    }
    let expected: std::collections::BTreeSet<_> = state
        .repos
        .iter()
        .filter(|repo| repo.old_head != repo.old_base)
        .map(|repo| (&repo.repo_id, &repo.old_head))
        .collect();
    if expected.is_empty() {
        return active.bundle.commit_groups.is_empty();
    }
    let [group] = active.bundle.commit_groups.as_slice() else {
        return false;
    };
    group.message == state.message
        && group.commits.len() == expected.len()
        && group
            .commits
            .iter()
            .map(|commit| (&commit.repo_id, &commit.sha))
            .collect::<std::collections::BTreeSet<_>>()
            == expected
}

fn execute_repo(state: &mut RewriteState, i: usize, path: &Path) -> Result<()> {
    if state.repos[i].phase == "pending" {
        state.repos[i].phase = if state.repos[i].needs_squash {
            "squashing"
        } else {
            "squashed"
        }
        .into();
        persist(path, state)?;
    }
    if state.repos[i].phase == "squashing" {
        let repo = &state.repos[i];
        let head = rev_parse(&repo.path, "HEAD")?;
        if head == repo.old_head {
            git_output(&repo.path, ["reset", "--soft", &repo.old_base])?;
        } else if head != repo.old_base {
            // A crash after commit but before the checkpoint is recoverable.
            clean(&repo.path)?;
            if git_output(&repo.path, ["rev-parse", "HEAD^"])? != repo.old_base {
                bail!("Unexpected HEAD during squash recovery");
            }
        }
        if rev_parse(&repo.path, "HEAD")? == repo.old_base {
            // A real commit deliberately preserves hooks, signing and local configuration.
            git_output(
                &repo.path,
                ["commit", "--allow-empty", "-m", &state.message],
            )?;
        }
        state.repos[i].phase = "squashed".into();
        persist(path, state)?;
    }
    if state.repos[i].phase == "squashed" {
        clean(&state.repos[i].path)?;
        state.repos[i].rebase_head = Some(rev_parse(&state.repos[i].path, "HEAD")?);
        state.repos[i].phase = "rebasing".into();
        persist(path, state)?;
    }
    let repo = &state.repos[i];
    if repo.phase == "rebasing" && rebasing(&repo.path)? {
        git_output_with_env(
            &repo.path,
            ["rebase", "--continue"],
            &[("GIT_EDITOR", "true")],
        )?;
    } else if repo.phase == "rebasing" && repo.old_base != repo.new_base {
        let head = rev_parse(&repo.path, "HEAD")?;
        let before = repo
            .rebase_head
            .as_ref()
            .context("Missing pre-rebase recovery head")?;
        if head != *before {
            if !is_ancestor(&repo.path, &repo.new_base, &head) {
                bail!("Unexpected HEAD during rebase recovery");
            }
        } else {
            git_output(
                &repo.path,
                ["rebase", "--onto", &repo.new_base, &repo.old_base],
            )?;
        }
    }
    expected_branch(&repo.path, &repo.branch)?;
    if repo.phase == "rebasing" {
        clean(&repo.path)?;
        state.repos[i].repin = Some(plan_repin(state, i)?);
        state.repos[i].phase = "repinning".into();
        persist(path, state)?;
    }
    execute_repin(state, i, path)?;
    state.repos[i].new_head = Some(rev_parse(&state.repos[i].path, "HEAD")?);
    state.repos[i].status = "done".into();
    persist(path, state)
}

fn abort_rewrite(active: &ActiveBundle, state: &mut RewriteState, path: &Path) -> Result<()> {
    if serde_json::to_value(&active.bundle)? != serde_json::to_value(&state.original_bundle)? {
        bail!("Rewrite ledger is already saved; use knit rebase --continue to finish cleanup");
    }
    for repo in &state.repos {
        if !rebasing(&repo.path)? {
            expected_branch(&repo.path, &repo.branch)?;
            let head = rev_parse(&repo.path, "HEAD")?;
            if (repo.status == "done"
                && Some(&head) != repo.new_head.as_ref()
                && head != repo.old_head)
                || (repo.phase == "pending" && head != repo.old_head)
            {
                bail!(
                    "{}: HEAD changed after the rewrite; preserve that work before aborting",
                    repo.repo_id
                );
            }
        }
    }
    state.aborting = true;
    persist(path, state)?;
    for i in (0..state.repos.len()).rev() {
        let repo = &state.repos[i];
        if rebasing(&repo.path)? {
            git_output(&repo.path, ["rebase", "--abort"])?;
        }
        expected_branch(&repo.path, &repo.branch)?;
        if repo.phase == "repinning" {
            if let Some(repin) = &repo.repin {
                if rev_parse(&repo.path, "HEAD")? == repin.head {
                    for edit in &repin.files {
                        let current = fs::read_to_string(repo.path.join(&edit.path))?;
                        let indexed = git_output(&repo.path, ["show", &format!(":{}", edit.path)])?;
                        if (current != edit.before && current != edit.after)
                            || (indexed != edit.before.trim_end()
                                && indexed != edit.after.trim_end())
                        {
                            bail!("{}: preserve changed lockfile before aborting", edit.path);
                        }
                    }
                    for edit in &repin.files {
                        git_output(
                            &repo.path,
                            [
                                "restore",
                                "--source",
                                &repin.head,
                                "--staged",
                                "--worktree",
                                "--",
                                &edit.path,
                            ],
                        )?;
                    }
                }
            }
        }
        if repo.phase != "pending" {
            git_output(&repo.path, ["reset", "--keep", &repo.old_head]).with_context(|| {
                format!(
                    "{}: restore failed; preserve local changes then retry knit rebase --abort",
                    repo.repo_id
                )
            })?;
        }
        state.repos[i].phase = "pending".into();
        state.repos[i].status = "pending".into();
        state.repos[i].new_head = None;
        persist(path, state)?;
    }
    fs::remove_file(path)?;
    crate::human!("Rewrite aborted; original heads restored and ledger unchanged.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommitGroup, CommitRef, RepoEntry};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn isolated_test(name: &str) -> bool {
        let full_name = format!("commands::rewrite::tests::{name}");
        if std::env::var("KNIT_REWRITE_TEST_CHILD").as_deref() == Ok(full_name.as_str()) {
            return false;
        }
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/rewrite-test-homes")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&home).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full_name, "--test-threads=1", "--nocapture"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("KNIT_REWRITE_TEST_CHILD", &full_name)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(home);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/rewrite-fixtures")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(root.join(".knit/rebase")).unwrap();
            Self { root }
        }
        fn repo(&self, id: &str) -> RepoEntry {
            let path = self.root.join(id);
            fs::create_dir_all(&path).unwrap();
            git_output(&path, ["init", "-b", "main"]).unwrap();
            git_output(&path, ["config", "user.name", "Test User"]).unwrap();
            git_output(&path, ["config", "user.email", "test@example.invalid"]).unwrap();
            git_output(&path, ["config", "commit.gpgsign", "false"]).unwrap();
            fs::write(path.join("file"), "base\n").unwrap();
            commit(&path, "Base");
            let base = rev_parse(&path, "HEAD").unwrap();
            git_output(&path, ["checkout", "-b", "knit/example"]).unwrap();
            serde_json::from_value(serde_json::json!({
                "id": id, "path": path, "worktreePath": path, "baseBranch": "main",
                "baseSha": base, "headSha": base, "featureBranch": "knit/example"
            }))
            .unwrap()
        }
        fn active(&self, repos: Vec<RepoEntry>) -> ActiveBundle {
            let mut bundle = ChangeGroup::new(
                "example".into(),
                "Example change".into(),
                crate::time::now_iso(),
            );
            bundle.repos = repos;
            ActiveBundle::unlocked(
                self.root.clone(),
                self.root.join(".knit/example.bundle.json"),
                bundle,
            )
        }
        fn state(&self) -> RewriteState {
            serde_json::from_slice(&fs::read(self.root.join(".knit/rebase/example.json")).unwrap())
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn commit(path: &Path, message: &str) {
        git_output(path, ["add", "-A"]).unwrap();
        git_output(path, ["commit", "-m", message]).unwrap();
    }
    fn feature(repo: &RepoEntry, filename: &str, message: &str) -> String {
        let path = Path::new(repo.worktree_path.as_ref().unwrap());
        fs::write(path.join(filename), message).unwrap();
        commit(path, message);
        rev_parse(path, "HEAD").unwrap()
    }
    fn load_saved(fixture: &Fixture) -> ChangeGroup {
        serde_json::from_slice(&fs::read(fixture.root.join(".knit/example.bundle.json")).unwrap())
            .unwrap()
    }

    fn cargo_fixture(f: &Fixture) -> (RepoEntry, RepoEntry, String) {
        let mut library = f.repo("library");
        let consumer = f.repo("consumer");
        let lib = Path::new(&library.path);
        fs::create_dir_all(lib.join("src")).unwrap();
        fs::write(
            lib.join("Cargo.toml"),
            "[workspace]\n[package]\nname = \"local-library\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(lib.join("src/lib.rs"), "pub fn value() -> u32 { 7 }\n").unwrap();
        commit(lib, "Add library");
        let old = feature(&library, "note", "Library change");
        let remote = url::Url::from_file_path(lib).unwrap().to_string();
        library.remote = Some(remote.clone());
        let root = Path::new(&consumer.path);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), format!("[workspace]\n[package]\nname = \"local-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nlocal-library = {{ git = \"{remote}\", branch = \"knit/example\" }}\n")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { assert_eq!(local_library::value(), 7); }\n",
        )
        .unwrap();
        cargo(f, &consumer, &["generate-lockfile"]);
        commit(root, "Add consumer");
        feature(&consumer, "note", "Consumer change");
        (library, consumer, old)
    }

    fn cargo(f: &Fixture, repo: &RepoEntry, args: &[&str]) {
        let cargo = Path::new(env!("CARGO"));
        let mut command = std::process::Command::new(cargo);
        command
            .args(args)
            .current_dir(&repo.path)
            .env("CARGO_HOME", f.root.join("cargo-home"))
            .env("CARGO_TARGET_DIR", f.root.join("cargo-target"))
            .env("CARGO_NET_GIT_FETCH_WITH_CLI", "true");
        let rustc = cargo.with_file_name("rustc");
        if rustc.exists() {
            command.env("RUSTC", rustc);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "cargo {args:?}: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_repinned(
        f: &Fixture,
        library: &RepoEntry,
        consumer: &RepoEntry,
        old: &str,
        squash: bool,
    ) {
        let head = rev_parse(Path::new(&library.path), "HEAD").unwrap();
        assert_ne!(head, old);
        let lock = fs::read_to_string(Path::new(&consumer.path).join("Cargo.lock")).unwrap();
        assert!(lock.contains(&format!("#{head}")));
        assert!(!lock.contains(&format!("#{old}")));
        let saved = load_saved(f);
        for repo in [library, consumer] {
            let path = Path::new(&repo.path);
            let recorded = saved.repos.iter().find(|r| r.id == repo.id).unwrap();
            let tip = rev_parse(path, "HEAD").unwrap();
            assert_eq!(recorded.head_sha.as_deref(), Some(tip.as_str()));
            if squash {
                assert_eq!(
                    git_output(
                        path,
                        [
                            "rev-list",
                            "--count",
                            &format!("{}..HEAD", recorded.base_sha.as_ref().unwrap())
                        ]
                    )
                    .unwrap(),
                    "1"
                );
                assert_eq!(saved.commit_groups.len(), 1);
                assert_eq!(
                    saved.commit_groups[0]
                        .commits
                        .iter()
                        .filter(|c| c.repo_id == repo.id && c.sha == tip)
                        .count(),
                    1
                );
            }
        }
        // Only file:// Git is used; populate the private cache, then prove locked offline use.
        cargo(f, consumer, &["build", "--locked"]);
        cargo(f, consumer, &["build", "--locked", "--offline"]);
    }

    #[test]
    fn cargo_squash_repins_in_library_order_and_one_group() {
        if isolated_test("cargo_squash_repins_in_library_order_and_one_group") {
            return;
        }
        let f = Fixture::new();
        let (library, consumer, old) = cargo_fixture(&f);
        // Consumer deliberately precedes its library in bundle order.
        start_active(
            f.active(vec![consumer.clone(), library.clone()]),
            true,
            None,
            true,
            false,
        )
        .unwrap();
        assert_repinned(&f, &library, &consumer, &old, true);
    }

    #[test]
    fn cargo_noop_rebase_preserves_older_pin_heads_and_ledger() {
        if isolated_test("cargo_noop_rebase_preserves_older_pin_heads_and_ledger") {
            return;
        }
        let f = Fixture::new();
        let mut library = f.repo("library");
        let mut consumer = f.repo("consumer");
        let old_pin = feature(&library, "first", "First library change");
        let library_head = feature(&library, "second", "Second library change");
        assert_ne!(old_pin, library_head);
        let remote = url::Url::from_file_path(&library.path).unwrap().to_string();
        library.remote = Some(remote.clone());
        let lock = format!(
            "version = 4\n\n[[package]]\nname = \"local-library\"\nversion = \"0.1.0\"\nsource = \"git+{remote}?branch=knit/example#{old_pin}\"\n"
        );
        let lock_path = Path::new(&consumer.path).join("Cargo.lock");
        fs::write(&lock_path, &lock).unwrap();
        commit(Path::new(&consumer.path), "Pin earlier library commit");
        let consumer_head = rev_parse(Path::new(&consumer.path), "HEAD").unwrap();
        library.head_sha = Some(library_head.clone());
        consumer.head_sha = Some(consumer_head.clone());
        let mut active = f.active(vec![consumer.clone(), library.clone()]);
        active.bundle.commit_groups.push(CommitGroup {
            id: "previous".into(),
            message: "Existing changes".into(),
            created_at: crate::time::now_iso(),
            commits: vec![
                CommitRef {
                    repo_id: library.id.clone(),
                    sha: library_head.clone(),
                },
                CommitRef {
                    repo_id: consumer.id.clone(),
                    sha: consumer_head.clone(),
                },
            ],
            author: None,
        });
        let ledger_path = active.bundle_path.clone();
        let ledger = serde_json::to_vec(&active.bundle).unwrap();
        fs::write(&ledger_path, &ledger).unwrap();

        start_active(active, false, None, true, true).unwrap();

        assert_eq!(fs::read(&lock_path).unwrap(), lock.as_bytes());
        assert_eq!(
            rev_parse(Path::new(&library.path), "HEAD").unwrap(),
            library_head
        );
        assert_eq!(
            rev_parse(Path::new(&consumer.path), "HEAD").unwrap(),
            consumer_head
        );
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger);
        clean(Path::new(&consumer.path)).unwrap();
        assert!(!f.root.join(".knit/rebase/example.json").exists());
    }

    #[test]
    fn cargo_rebase_amends_tip_without_extra_commit() {
        if isolated_test("cargo_rebase_amends_tip_without_extra_commit") {
            return;
        }
        let f = Fixture::new();
        let (library, consumer, old) = cargo_fixture(&f);
        for repo in [&library, &consumer] {
            let path = Path::new(&repo.path);
            git_output(path, ["checkout", "main"]).unwrap();
            fs::write(path.join("upstream"), "upstream").unwrap();
            commit(path, "Upstream");
            git_output(path, ["checkout", "knit/example"]).unwrap();
        }
        start_active(
            f.active(vec![consumer.clone(), library.clone()]),
            false,
            None,
            true,
            true,
        )
        .unwrap();
        assert_repinned(&f, &library, &consumer, &old, false);
        for repo in [&library, &consumer] {
            assert_eq!(
                git_output(Path::new(&repo.path), ["rev-list", "--count", "main..HEAD"]).unwrap(),
                "2"
            );
        }
    }

    #[test]
    fn cargo_repin_preserves_nonmatching_lines_byte_for_byte() {
        if isolated_test("cargo_repin_preserves_nonmatching_lines_byte_for_byte") {
            return;
        }
        let f = Fixture::new();
        let (library, consumer, old) = cargo_fixture(&f);
        let remote = library.remote.as_ref().unwrap();
        let other = format!("# comment #{old}\r\nsource = \"registry+https://example.invalid/index\"\r\nsource = \"git+{remote}?branch=main#{old}\"\r\nsource = \"git+{remote}?branch=knit/other#{old}\"\r\nsource = \"git+file:///other?branch=knit/example#{old}\"\r\nsource = \"git+{remote}?rev={old}#{old}\"\r\nchecksum = \"{old}\"\r\n");
        let matched =
            format!("  source = \"git+{remote}?branch=knit%2Fexample#{old}\" # retained\r\n");
        fs::write(
            Path::new(&consumer.path).join("Cargo.lock"),
            format!("{other}{matched}"),
        )
        .unwrap();
        commit(Path::new(&consumer.path), "Lock entries");
        start_active(
            f.active(vec![consumer.clone(), library.clone()]),
            true,
            None,
            true,
            false,
        )
        .unwrap();
        let new = rev_parse(Path::new(&library.path), "HEAD").unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&consumer.path).join("Cargo.lock")).unwrap(),
            format!("{other}{}", matched.replace(&old, &new))
        );
    }

    #[cfg(unix)]
    #[test]
    fn cargo_repin_hook_failure_can_continue_or_abort() {
        if isolated_test("cargo_repin_hook_failure_can_continue_or_abort") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        for recovery in ["continue", "crash", "abort"] {
            let f = Fixture::new();
            let (library, consumer, old) = cargo_fixture(&f);
            let consumer_head = rev_parse(Path::new(&consumer.path), "HEAD").unwrap();
            let old_lock = fs::read(Path::new(&consumer.path).join("Cargo.lock")).unwrap();
            let hook = Path::new(&consumer.path).join(".git/hooks/pre-commit");
            // Squash succeeds; only the repinning amend fails.
            fs::write(
                &hook,
                format!("#!/bin/sh\ngit show :Cargo.lock | grep -q '{old}'\n"),
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(start_active(
                f.active(vec![consumer.clone(), library.clone()]),
                true,
                None,
                true,
                false
            )
            .is_err());
            let mut state = f.state();
            assert_eq!(state.repos[0].repo_id, library.id);
            assert_eq!(state.repos[1].phase, "repinning");
            assert_eq!(state.repos[1].status, "conflict");
            assert!(!f.root.join(".knit/example.bundle.json").exists());
            fs::remove_file(hook).unwrap();
            let mut active = f.active(vec![consumer.clone(), library.clone()]);
            active.bundle = state.original_bundle.clone();
            let path = state_path(&active).unwrap();
            if recovery == "abort" {
                abort_rewrite(&active, &mut state, &path).unwrap();
                assert_eq!(rev_parse(Path::new(&library.path), "HEAD").unwrap(), old);
                assert_eq!(
                    rev_parse(Path::new(&consumer.path), "HEAD").unwrap(),
                    consumer_head
                );
                assert_eq!(
                    fs::read(Path::new(&consumer.path).join("Cargo.lock")).unwrap(),
                    old_lock
                );
                clean(Path::new(&consumer.path)).unwrap();
                assert!(!active.bundle_path.exists());
            } else {
                // Simulate a crash after the amend, before its completion checkpoint.
                if recovery == "continue" {
                    // A staged-only unrelated edit must not be absorbed by the amend.
                    let root = Path::new(&consumer.path);
                    fs::write(root.join("note"), "unrelated").unwrap();
                    git_output(root, ["add", "note"]).unwrap();
                    fs::write(root.join("note"), "Consumer change").unwrap();
                    let error = execute(&mut active, &mut state, &path).unwrap_err();
                    assert!(format!("{error:#}").contains("Unrelated changes"));
                    git_output(root, ["restore", "--staged", "note"]).unwrap();
                }
                if recovery == "crash" {
                    git_output(
                        Path::new(&consumer.path),
                        ["commit", "--amend", "--no-edit"],
                    )
                    .unwrap();
                }
                execute(&mut active, &mut state, &path).unwrap();
                assert_repinned(&f, &library, &consumer, &old, true);
            }
            assert!(!path.exists());
        }
    }

    #[test]
    fn squash_combines_repos_and_retains_unchanged_single_commit() {
        if isolated_test("squash_combines_repos_and_retains_unchanged_single_commit") {
            return;
        }
        let f = Fixture::new();
        let a = f.repo("api");
        let b = f.repo("web");
        feature(&a, "first", "First");
        feature(&a, "second", "Second");
        let original_b = feature(&b, "first", "Combined");
        start_active(
            f.active(vec![a.clone(), b.clone()]),
            true,
            Some("Combined"),
            true,
            false,
        )
        .unwrap();
        let saved = load_saved(&f);
        assert_eq!(saved.commit_groups.len(), 1);
        assert_eq!(saved.commit_groups[0].commits.len(), 2);
        assert_eq!(saved.commit_groups[0].message, "Combined");
        assert_eq!(rev_parse(Path::new(&b.path), "HEAD").unwrap(), original_b);
        assert_eq!(
            git_output(Path::new(&a.path), ["rev-list", "--count", "main..HEAD"]).unwrap(),
            "1"
        );
        assert!(!f.root.join(".knit/rebase/example.json").exists());
    }

    #[test]
    fn squash_consolidates_unchanged_heads_then_preserves_ledger_bytes() {
        if isolated_test("squash_consolidates_unchanged_heads_then_preserves_ledger_bytes") {
            return;
        }
        let f = Fixture::new();
        let a = f.repo("api");
        let b = f.repo("web");
        let a_head = feature(&a, "first", "Combined");
        let b_head = feature(&b, "first", "Combined");
        let mut active = f.active(vec![a.clone(), b.clone()]);
        active.bundle.commit_groups = vec![(&a, &a_head), (&b, &b_head)]
            .into_iter()
            .map(|(repo, head)| CommitGroup {
                id: format!("previous-{}", repo.id),
                message: "Combined".into(),
                created_at: crate::time::now_iso(),
                author: None,
                commits: vec![CommitRef {
                    repo_id: repo.id.clone(),
                    sha: head.clone(),
                }],
            })
            .collect();
        start_active(active, true, Some("Combined"), true, false).unwrap();
        let saved = load_saved(&f);
        assert_eq!(saved.commit_groups.len(), 1);
        assert_eq!(saved.commit_groups[0].commits.len(), 2);
        assert_eq!(rev_parse(Path::new(&a.path), "HEAD").unwrap(), a_head);
        assert_eq!(rev_parse(Path::new(&b.path), "HEAD").unwrap(), b_head);
        let bytes = serde_json::to_vec(&saved).unwrap();
        let mut active = f.active(vec![a, b]);
        active.bundle = saved;
        let path = active.bundle_path.clone();
        fs::write(&path, &bytes).unwrap();
        start_active(active, true, Some("Combined"), true, false).unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(!f.root.join(".knit/rebase/example.json").exists());
    }

    #[test]
    fn preflight_checks_every_repo_before_mutation() {
        if isolated_test("preflight_checks_every_repo_before_mutation") {
            return;
        }
        let f = Fixture::new();
        let a = f.repo("api");
        let b = f.repo("web");
        feature(&a, "first", "First");
        let head = feature(&a, "second", "Second");
        fs::write(Path::new(&b.path).join("file"), "dirty").unwrap();
        assert!(start_active(
            f.active(vec![a.clone(), b.clone()]),
            true,
            None,
            true,
            false
        )
        .is_err());
        assert_eq!(rev_parse(Path::new(&a.path), "HEAD").unwrap(), head);
        git_output(Path::new(&b.path), ["checkout", "--", "file"]).unwrap();
        git_output(Path::new(&b.path), ["checkout", "main"]).unwrap();
        assert!(start_active(f.active(vec![a.clone(), b]), true, None, true, false).is_err());
        assert_eq!(rev_parse(Path::new(&a.path), "HEAD").unwrap(), head);
    }

    #[cfg(unix)]
    #[test]
    fn hook_failure_can_continue_and_preserves_sole_group_message() {
        if isolated_test("hook_failure_can_continue_and_preserves_sole_group_message") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let a = f.repo("api");
        feature(&a, "first", "First");
        let head = feature(&a, "second", "Second");
        let hook = Path::new(&a.path).join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let mut active = f.active(vec![a.clone()]);
        active.bundle.commit_groups.push(CommitGroup {
            id: "previous".into(),
            message: "Preserved message".into(),
            created_at: crate::time::now_iso(),
            commits: vec![CommitRef {
                repo_id: a.id.clone(),
                sha: head,
            }],
            author: None,
        });
        let original = active.bundle.clone();
        assert!(start_active(active, true, None, true, false).is_err());
        assert!(!f.root.join(".knit/example.bundle.json").exists());
        let mut state = f.state();
        assert_eq!(state.repos[0].status, "conflict");
        fs::remove_file(hook).unwrap();
        let mut active = f.active(vec![a.clone()]);
        active.bundle = original;
        let state_path = state_path(&active).unwrap();
        execute(&mut active, &mut state, &state_path).unwrap();
        assert_eq!(
            git_output(Path::new(&a.path), ["show", "-s", "--format=%B", "HEAD"]).unwrap(),
            "Preserved message"
        );
        assert_eq!(load_saved(&f).commit_groups.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn hook_failure_can_abort_soft_reset_without_losing_feature_work() {
        if isolated_test("hook_failure_can_abort_soft_reset_without_losing_feature_work") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let a = f.repo("api");
        feature(&a, "first", "First");
        let head = feature(&a, "second", "Second");
        let hook = Path::new(&a.path).join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(start_active(f.active(vec![a.clone()]), true, None, true, false).is_err());
        let mut state = f.state();
        let mut active = f.active(vec![a.clone()]);
        active.bundle = state.original_bundle.clone();
        let path = state_path(&active).unwrap();
        abort_rewrite(&active, &mut state, &path).unwrap();
        assert_eq!(rev_parse(Path::new(&a.path), "HEAD").unwrap(), head);
        clean(Path::new(&a.path)).unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&a.path).join("second")).unwrap(),
            "Second"
        );
        assert!(!active.bundle_path.exists());
    }

    #[test]
    fn conflict_continue_finishes_remaining_repos_and_saves_once() {
        if isolated_test("conflict_continue_finishes_remaining_repos_and_saves_once") {
            return;
        }
        let f = Fixture::new();
        let a = f.repo("api");
        let b = f.repo("web");
        feature(&a, "file", "feature");
        feature(&b, "extra", "Feature");
        for repo in [&a, &b] {
            let path = Path::new(&repo.path);
            git_output(path, ["checkout", "main"]).unwrap();
            fs::write(path.join("file"), "upstream").unwrap();
            commit(path, "Upstream");
            git_output(path, ["checkout", "knit/example"]).unwrap();
        }
        assert!(start_active(
            f.active(vec![a.clone(), b.clone()]),
            false,
            None,
            true,
            true
        )
        .is_err());
        assert!(!f.root.join(".knit/example.bundle.json").exists());
        let mut state = f.state();
        assert_eq!(state.repos[1].status, "pending");
        let path = Path::new(&a.path);
        fs::write(path.join("file"), "resolved").unwrap();
        git_output(path, ["add", "file"]).unwrap();
        let mut active = f.active(vec![a, b.clone()]);
        active.bundle = state.original_bundle.clone();
        let state_path = state_path(&active).unwrap();
        execute(&mut active, &mut state, &state_path).unwrap();
        assert_eq!(
            load_saved(&f).repos[1].base_sha,
            Some(rev_parse(Path::new(&b.path), "main").unwrap())
        );
        assert!(!state_path.exists());
    }

    #[test]
    fn abort_restores_completed_squash_and_conflicted_repo_without_ledger_write() {
        if isolated_test("abort_restores_completed_squash_and_conflicted_repo_without_ledger_write")
        {
            return;
        }
        let f = Fixture::new();
        let a = f.repo("api");
        let b = f.repo("web");
        feature(&a, "first", "First");
        let a_head = feature(&a, "second", "Second");
        let b_head = feature(&b, "file", "feature");
        let path = Path::new(&b.path);
        git_output(path, ["checkout", "main"]).unwrap();
        fs::write(path.join("file"), "upstream").unwrap();
        commit(path, "Upstream");
        git_output(path, ["checkout", "knit/example"]).unwrap();
        assert!(
            start_active(f.active(vec![a.clone(), b.clone()]), true, None, true, true).is_err()
        );
        let mut state = f.state();
        assert_eq!(state.repos[0].status, "done");
        let mut active = f.active(vec![a.clone(), b.clone()]);
        active.bundle = state.original_bundle.clone();
        let state_path = state_path(&active).unwrap();
        abort_rewrite(&active, &mut state, &state_path).unwrap();
        assert_eq!(rev_parse(Path::new(&a.path), "HEAD").unwrap(), a_head);
        assert_eq!(rev_parse(Path::new(&b.path), "HEAD").unwrap(), b_head);
        assert!(!state_path.exists());
        assert!(!active.bundle_path.exists());
    }
}
