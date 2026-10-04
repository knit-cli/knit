use crate::checkout::{checkout_dir, checkout_display_path};
use crate::git::{current_branch, display_color_args, git_output, resolve_base_ref};
use crate::ids::short_sha;
use crate::model::RepoEntry;
use crate::output as out;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn show_diff(selectors: &[String], stat: bool, published: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    println!(
        "{} {} ({})\n",
        out::heading("Bundle:"),
        out::node(&active.bundle.id),
        active.resolution_source.label()
    );

    let repos = resolve_repos(&active, selectors)?;
    if published {
        let mut shown = 0usize;
        for repo in repos {
            match show_published_diff(&active, repo, stat)
                .with_context(|| format!("{}: published diff failed", repo.id))?
            {
                Some(()) => shown += 1,
                None if selectors.is_empty() => println!(
                    "{}",
                    out::muted(format!("{}: not published (no recorded PR/MR)", repo.id))
                ),
                None => bail!("{}: published diff failed: no recorded PR/MR", repo.id),
            }
        }
        if shown == 0 {
            println!(
                "No published reviews recorded in bundle {}",
                out::node(&active.bundle.id)
            );
        }
        return Ok(());
    }
    let mut shown = 0usize;

    for repo in repos {
        let Some(checkout) = checkout_dir(&active, repo) else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::danger("checkout missing")
            );
            continue;
        };
        let base = diff_base(repo, &checkout)?;
        let output = run_diff(&checkout, &base, stat)
            .with_context(|| format!("{}: failed to diff against {base}", repo.id))?;

        if output.trim().is_empty() {
            if selectors.is_empty() {
                continue;
            }
            println!("{}: {}", out::repo(&repo.id), out::muted("no diff"));
            print_no_diff_context(repo, &checkout)?;
            continue;
        }

        shown += 1;
        println!(
            "== {} {} {} ==",
            out::repo(&repo.id),
            out::muted("against"),
            out::sha(short_sha(&base))
        );
        println!(
            "{} {}",
            out::muted("checkout:"),
            out::path(checkout_display_path(repo))
        );
        println!("{output}");
    }

    if shown == 0 && selectors.is_empty() {
        println!(
            "{} {}",
            out::ok("No diffs found in bundle"),
            out::node(&active.bundle.id)
        );
    }

    Ok(())
}

fn show_published_diff(active: &ActiveBundle, repo: &RepoEntry, stat: bool) -> Result<Option<()>> {
    let checkout = checkout_dir(active, repo).context("bundle checkout unavailable")?;
    if crate::git::git_root(&checkout).ok() != canonical(&checkout) {
        bail!("bundle checkout unavailable: not a Git checkout root");
    }
    let local_head = git_output(&checkout, ["rev-parse", "--verify", "HEAD^{commit}"])
        .context("bundle checkout has no resolvable local HEAD")?;
    let Some(publication) = crate::providers::publication_for_repo(&active.bundle, &repo.id) else {
        return Ok(None);
    };
    let forge = crate::providers::by_id(&publication.provider)
        .with_context(|| format!("unknown publication provider `{}`", publication.provider))?;
    let target = published_target(&checkout, &publication.url, forge.id())?;
    let review = forge
        .view(&target, &publication.url)
        .with_context(|| format!("cannot resolve published review {}", publication.url))?;
    let sha = review
        .head_ref_oid
        .as_deref()
        .context("published review has no head SHA")?;
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("published review has no valid full head SHA");
    }

    let remote = if crate::contribution::configured(repo) {
        match crate::contribution::push_remote(&checkout, repo) {
            Ok(name) => crate::contribution::git_remote_url(&checkout, &name, true)?,
            Err(_) => crate::contribution::source(repo)
                .context("missing source remote")?
                .to_owned(),
        }
    } else {
        let name = crate::auth_git::default_remote(&checkout, true);
        crate::contribution::git_remote_url(&checkout, &name, true)?
    };
    // A URL and an empty refmap prevent configured fetch refspecs from updating
    // tracking/lease refs. The immutable host SHA is the only requested object.
    crate::git::git_output_without_recovery(
        &checkout,
        [
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--refmap=",
            "--",
            &remote,
            sha,
        ],
    )
    .with_context(|| format!("cannot fetch published head {sha} for {}", publication.url))?;
    git_output(&checkout, ["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .context("published head is not an available commit")?;
    let mut args = vec![
        OsString::from("--no-optional-locks"),
        OsString::from("diff"),
    ];
    args.extend(display_color_args());
    args.extend(["--no-ext-diff", "--no-textconv"].map(OsString::from));
    if stat {
        args.push(OsString::from("--stat"));
    }
    args.extend([OsString::from(sha), OsString::from("--")]);
    let output = git_output(&checkout, args)?;
    println!("== {} against published ==", out::repo(&repo.id));
    println!("review: {}", publication.url);
    println!("published: {sha}");
    println!("local HEAD: {local_head}");
    println!("checkout: {}", out::path(checkout_display_path(repo)));
    println!(
        "{}",
        if output.trim().is_empty() {
            "no diff"
        } else {
            &output
        }
    );
    Ok(Some(()))
}

fn published_target(
    checkout: &Path,
    review_url: &str,
    provider: &str,
) -> Result<crate::providers::PrTarget> {
    let mut url = url::Url::parse(review_url).context("invalid recorded review URL")?;
    let marker = match provider {
        "github" => "/pull/",
        "gitlab" => "/-/merge_requests/",
        "forgejo" => "/pulls/",
        "bitbucket" => "/pull-requests/",
        _ => bail!("unsupported review provider `{provider}`"),
    };
    let (repo, number) = url
        .path()
        .rsplit_once(marker)
        .context("invalid recorded review URL path")?;
    if repo.trim_matches('/').is_empty() || number.parse::<u64>().is_err() {
        bail!("invalid recorded review URL path");
    }
    let repo = repo.trim_start_matches('/').to_owned();
    url.set_path(&format!("/{repo}"));
    url.set_query(None);
    url.set_fragment(None);
    // Resolve auth from the recorded review destination, not the checkout's
    // origin or the ledger's (possibly rewritten) contribution head.
    let mut target = crate::providers::PrTarget::explicit(checkout, repo);
    target.repo_remote = Some(url.to_string());
    Ok(target)
}

fn print_no_diff_context(repo: &RepoEntry, checkout: &Path) -> Result<()> {
    println!(
        "  {} {}",
        out::muted("bundle checkout:"),
        out::path(checkout.display())
    );

    let checkout_status = git_output(checkout, ["status", "--short"])?;
    if checkout_status.lines().any(|line| line.starts_with("??")) {
        println!(
            "  {} bundle checkout has untracked files; `knit diff` follows `git diff` and does not include them",
            out::warn("Note:")
        );
    }

    let source = PathBuf::from(&repo.path);
    if canonical(&source) == canonical(checkout) {
        return Ok(());
    }

    let source_status = git_output(&source, ["status", "--short"])?;
    let source_head = git_output(&source, ["rev-parse", "HEAD"])?;
    let checkout_head = git_output(checkout, ["rev-parse", "HEAD"])?;
    if source_status.trim().is_empty() && source_head == checkout_head {
        return Ok(());
    }

    let source_branch = current_branch(&source)?.unwrap_or_else(|| "detached".to_string());
    let checkout_branch = current_branch(checkout)?.unwrap_or_else(|| "detached".to_string());
    let source_state = if source_status.trim().is_empty() {
        "clean"
    } else {
        "modified"
    };
    println!(
        "  {} source checkout {} is {} on {} at {}; bundle checkout is on {} at {}",
        out::warn("Note:"),
        out::path(source.display()),
        source_state,
        out::branch(source_branch),
        out::sha(short_sha(&source_head)),
        out::branch(checkout_branch),
        out::sha(short_sha(&checkout_head))
    );
    Ok(())
}

fn run_diff(checkout: &Path, base: &str, stat: bool) -> Result<String> {
    let mut args = vec![OsString::from("diff")];
    args.extend(display_color_args());
    if stat {
        args.push(OsString::from("--stat"));
    }
    args.push(OsString::from(base));
    git_output(checkout, args)
}

fn diff_base(repo: &RepoEntry, checkout: &Path) -> Result<String> {
    if let Some(base_sha) = &repo.base_sha {
        return Ok(base_sha.clone());
    }

    let repo_root = PathBuf::from(&repo.path);
    let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
    git_output(checkout, ["rev-parse", &base_ref]).map(|sha| sha.trim().to_string())
}

fn resolve_repos<'a>(active: &'a ActiveBundle, selectors: &[String]) -> Result<Vec<&'a RepoEntry>> {
    if selectors.is_empty() {
        return Ok(active.bundle.repos.iter().collect());
    }

    let mut indexes = BTreeSet::new();
    for selector in selectors {
        let matches = active
            .bundle
            .repos
            .iter()
            .enumerate()
            .filter_map(|(index, repo)| repo_matches(active, repo, selector).then_some(index))
            .collect::<Vec<_>>();

        if matches.is_empty() {
            bail!("No tracked repo matched `{selector}`.");
        }

        indexes.extend(matches);
    }

    Ok(indexes
        .into_iter()
        .map(|index| &active.bundle.repos[index])
        .collect())
}

fn repo_matches(active: &ActiveBundle, repo: &RepoEntry, selector: &str) -> bool {
    if selector == repo.id || selector == repo.path {
        return true;
    }

    if repo
        .worktree_path
        .as_ref()
        .is_some_and(|worktree_path| selector == worktree_path)
    {
        return true;
    }

    let selector_path = Path::new(selector);
    if !selector_path.exists() {
        return false;
    }

    let Some(selector_abs) = canonical(selector_path) else {
        return false;
    };

    canonical(Path::new(&repo.path)).is_some_and(|path| path == selector_abs)
        || repo
            .worktree_path
            .as_ref()
            .and_then(|path| canonical(&active.root.join(path)))
            .is_some_and(|path| path == selector_abs)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    crate::paths::canonicalize(path).ok()
}
