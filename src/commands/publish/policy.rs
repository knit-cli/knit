//! Resolve publishing policy once, before any branch, forge or artifact writes.
use crate::model::*;
use crate::providers::publication_for_repo;
use crate::store::ActiveBundle;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PublishOptions {
    pub ready: Vec<String>,
    pub draft_repo: Vec<String>,
    pub title: Vec<String>,
    pub body_file: Vec<String>,
    pub dry_run: bool,
    pub allow_foreign_author: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedPublish {
    pub draft: bool,
    pub draft_reason: String,
    pub title: String,
    pub body: String,
    pub body_source: String,
    pub blocked_on: BTreeSet<String>,
}
impl ResolvedPublish {
    pub fn body(&self, bundle: &ChangeGroup, repo: &RepoEntry, provider: &str) -> String {
        let mut body = self.body.clone();
        // Blockers belong to authored content so a normal managed-block sync
        // cannot erase them. Fresh library links are resolved at create time.
        for library in &self.blocked_on {
            let blocker = publication_for_repo(bundle, library)
                .map(|p| p.url.as_str())
                .unwrap_or(library);
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("Blocked on {blocker}"));
        }
        let block = super::pr_body::initial_pr_body(bundle, &repo.id, provider);
        if body.is_empty() {
            block
        } else {
            format!("{body}\n\n{block}")
        }
    }
}

/// Future language detectors plug into the same consumer -> library edge map.
pub(super) trait DependencyDetector {
    fn detect(
        &self,
        bundle: &ChangeGroup,
        checkouts: &BTreeMap<String, PathBuf>,
        selected: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>>;
}
pub(super) struct CargoDetector;
impl DependencyDetector for CargoDetector {
    fn detect(
        &self,
        bundle: &ChangeGroup,
        checkouts: &BTreeMap<String, PathBuf>,
        selected: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (consumer, root) in checkouts {
            if !selected.contains(consumer) {
                continue;
            }
            let mut manifests = Vec::new();
            cargo_manifests(root, &mut manifests)?;
            for path in manifests {
                let text = std::fs::read_to_string(&path)?;
                let document: toml::Value = toml::from_str(&text)
                    .with_context(|| format!("invalid Cargo manifest {}", path.display()))?;
                let mut references = Vec::new();
                cargo_references(&document, &mut references);
                for (url, branch) in references {
                    for library in &bundle.repos {
                        if library.id == *consumer
                            || library.feature_branch.as_deref() != Some(branch.as_str())
                        {
                            continue;
                        }
                        let matches = [
                            library.source_remote.as_deref(),
                            library.target_remote.as_deref(),
                            library.remote.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .any(|remote| {
                            crate::contribution::same_repository(&url, remote).unwrap_or(false)
                        });
                        let push_matches = checkouts
                            .get(&library.id)
                            .and_then(|cwd| {
                                crate::git::git_output_optional(
                                    cwd,
                                    ["remote", "get-url", "--push", "origin"],
                                )
                                .ok()
                                .flatten()
                            })
                            .is_some_and(|remote| {
                                crate::contribution::same_repository(&url, remote.trim())
                                    .unwrap_or(false)
                            });
                        if matches || push_matches {
                            edges
                                .entry(consumer.clone())
                                .or_default()
                                .insert(library.id.clone());
                        }
                    }
                }
            }
        }
        Ok(edges)
    }
}
fn cargo_manifests(root: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if kind.is_dir()
            && !matches!(
                name.to_str(),
                Some(".git" | ".knit" | "target" | "node_modules")
            )
        {
            cargo_manifests(&entry.path(), manifests)?;
        } else if kind.is_file() && name == "Cargo.toml" {
            manifests.push(entry.path());
        }
    }
    Ok(())
}
fn cargo_references(value: &toml::Value, refs: &mut Vec<(String, String)>) {
    fn dependencies(value: &toml::Value, refs: &mut Vec<(String, String)>) {
        if let Some(table) = value.as_table() {
            if let (Some(url), Some(branch)) = (
                table.get("git").and_then(toml::Value::as_str),
                table.get("branch").and_then(toml::Value::as_str),
            ) {
                refs.push((url.into(), branch.into()));
            }
            for value in table.values() {
                dependencies(value, refs);
            }
        }
    }
    for key in [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "patch",
    ] {
        if let Some(section) = value.get(key) {
            dependencies(section, refs);
        }
    }
    if let Some(workspace) = value.get("workspace") {
        cargo_references(workspace, refs);
    }
    if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            cargo_references(target, refs);
        }
    }
}

pub(super) fn project(active: &ActiveBundle) -> Result<Option<KnitProject>> {
    let config = crate::store::load_config(&active.root)?;
    let id = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref());
    if let Some(id) = id {
        let path = crate::store::project_path(&active.root, id);
        if path.exists() {
            return crate::store::read_json(&path).map(Some);
        }
        eprintln!("Project configuration is unavailable; using bundle publishing overrides and built-in defaults.");
    }
    Ok(None)
}
fn assignments(
    values: &[String],
    selected: &BTreeSet<String>,
    flag: &str,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for value in values {
        let (id, text) = value
            .split_once('=')
            .with_context(|| format!("{flag} expects <repo>=<value>"))?;
        if !selected.contains(id) || text.trim().is_empty() {
            bail!("{flag}: unknown/unselected repo or empty value: {id}");
        }
        if result.insert(id.into(), text.into()).is_some() {
            bail!("{flag}: duplicate value for {id}");
        }
    }
    Ok(result)
}
fn draft_value(policy: &PublishPolicy, id: &str, dependent: bool) -> Option<bool> {
    policy.repos.get(id).and_then(|p| p.draft).or_else(|| {
        policy.draft.as_ref().map(|p| match p {
            PublishDraft::Mode(PublishDraftMode::None) => false,
            PublishDraft::Mode(PublishDraftMode::All) => true,
            PublishDraft::Mode(PublishDraftMode::Dependents) => dependent,
            PublishDraft::Repos(ids) => ids.iter().any(|r| r == id),
        })
    })
}

fn effective_draft(
    bundle: &ChangeGroup,
    project: Option<&KnitProject>,
    id: &str,
    draft_all: bool,
    options: &PublishOptions,
    dependent: bool,
) -> (bool, &'static str) {
    if options.ready.iter().any(|r| r == id) {
        return (false, "CLI --ready");
    }
    if draft_all || options.draft_repo.iter().any(|r| r == id) {
        return (true, "CLI draft override");
    }
    let legacy = project
        .and_then(|p| p.repos.iter().find(|r| r.id == id))
        .and_then(|r| r.publish.as_ref())
        .map(|p| p.draft);
    let project_policy = project.and_then(|p| p.publish.as_ref());
    let configured = bundle
        .publish
        .as_ref()
        .and_then(|p| draft_value(p, id, dependent))
        .or_else(|| {
            project_policy
                .and_then(|p| p.repos.get(id))
                .and_then(|p| p.draft)
        })
        .or(legacy)
        .or_else(|| project_policy.and_then(|p| draft_value(p, id, dependent)));
    (
        configured.unwrap_or(false),
        if configured.is_some() {
            "publishing policy"
        } else {
            "built-in ready default"
        },
    )
}

fn dependency_edges(
    bundle: &ChangeGroup,
    project: Option<&KnitProject>,
    checkouts: &BTreeMap<String, PathBuf>,
    jobs: &[super::remote::PublishJob],
    draft_all: bool,
    options: &PublishOptions,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    // Only the winning dependents policy is sensitive to dependency state.
    // Higher-priority booleans, lists and CLI overrides disable detection too.
    let consumers: BTreeSet<_> = jobs
        .iter()
        .filter(|j| {
            effective_draft(bundle, project, &j.repo.id, draft_all, options, false).0
                != effective_draft(bundle, project, &j.repo.id, draft_all, options, true).0
        })
        .map(|j| j.repo.id.clone())
        .collect();
    if consumers.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut edges = CargoDetector.detect(bundle, checkouts, &consumers)?;
    if let Some(landing) = project.and_then(|p| p.landing.as_ref()) {
        for dep in &landing.dependencies {
            if !bundle.repos.iter().any(|r| r.id == dep.library) {
                continue;
            }
            for consumer in &consumers {
                let applies = match &dep.consumers {
                    ProjectLandingConsumers::Wildcard(_) => true,
                    ProjectLandingConsumers::Repositories(ids) => ids.contains(consumer),
                };
                if applies && consumer != &dep.library {
                    edges
                        .entry(consumer.clone())
                        .or_default()
                        .insert(dep.library.clone());
                }
            }
        }
    }
    Ok(edges)
}

/// Refresh only dependency reviews needed by the effective policy. The caller
/// owns an in-memory bundle snapshot: this never writes a forge or artifact.
pub(super) fn refresh_dependencies(
    bundle: &mut ChangeGroup,
    project: Option<&KnitProject>,
    root: &Path,
    checkouts: &BTreeMap<String, PathBuf>,
    jobs: &[super::remote::PublishJob],
    draft_all: bool,
    options: &PublishOptions,
) -> Result<()> {
    let libraries: BTreeSet<_> =
        dependency_edges(bundle, project, checkouts, jobs, draft_all, options)?
            .into_values()
            .flatten()
            .collect();
    for library in libraries {
        let Some(publication) = publication_for_repo(bundle, &library).cloned() else {
            continue;
        };
        let repo = bundle
            .repos
            .iter()
            .find(|r| r.id == library)
            .context("missing dependency repository")?;
        let cwd = checkouts
            .get(&library)
            .map(PathBuf::as_path)
            .unwrap_or(root);
        let forge = crate::providers::for_repo(repo)?;
        let mut target = crate::contribution::target(
            cwd,
            repo,
            forge.as_ref(),
            &publication.base_branch,
            !checkouts.contains_key(&library),
        )?;
        target.verify_head = false;
        let live = forge
            .view(&target, &publication.url)
            .with_context(|| format!("read dependency review for {library}"))?;
        let state = live
            .state
            .context("dependency review did not report a state")?;
        if let Some(stored) = bundle
            .publications
            .iter_mut()
            .find(|p| p.repo_id == library && p.url == publication.url)
        {
            stored.state = state;
        }
    }
    Ok(())
}

pub(super) fn resolve(
    bundle: &ChangeGroup,
    project: Option<&KnitProject>,
    root: &Path,
    checkouts: &BTreeMap<String, PathBuf>,
    jobs: &[super::remote::PublishJob],
    draft_all: bool,
    options: &PublishOptions,
) -> Result<BTreeMap<String, ResolvedPublish>> {
    let selected: BTreeSet<String> = jobs.iter().map(|j| j.repo.id.clone()).collect();
    for id in options.ready.iter().chain(&options.draft_repo) {
        if !selected.contains(id) {
            bail!("Draft override names unknown/unselected repository `{id}`");
        }
        if options.ready.contains(id) && (draft_all || options.draft_repo.contains(id)) {
            bail!("Conflicting CLI draft/ready overrides for `{id}`");
        }
    }
    let titles = assignments(&options.title, &selected, "--title")?;
    let files = assignments(&options.body_file, &selected, "--body-file")?;
    let edges = dependency_edges(bundle, project, checkouts, jobs, draft_all, options)?;
    let empty = PublishPolicy::default();
    let project_policy = project.and_then(|p| p.publish.as_ref()).unwrap_or(&empty);
    let bundle_policy = bundle.publish.as_ref().unwrap_or(&empty);
    let released = if edges.is_empty() {
        BTreeSet::new()
    } else {
        released_libraries(bundle, root)?
    };
    let mut results = BTreeMap::new();
    for job in jobs {
        let id = &job.repo.id;
        let blockers: BTreeSet<String> = edges
            .get(id)
            .into_iter()
            .flatten()
            .filter(|library| {
                !publication_for_repo(bundle, library)
                    .is_some_and(|p| p.state.eq_ignore_ascii_case("merged"))
                    || project.and_then(|p| p.landing.as_ref()).is_some_and(|l| {
                        l.dependencies.iter().any(|d| {
                            d.library == **library
                                && d.release.is_some()
                                && !released.contains(*library)
                        })
                    })
            })
            .cloned()
            .collect();
        for library in &blockers {
            if !selected.contains(library) && publication_for_repo(bundle, library).is_none() {
                bail!("{id}: dependency {library} has no review link; select it for publishing with this consumer first");
            }
        }
        let dependent = !blockers.is_empty();
        let (draft, reason) = effective_draft(bundle, project, id, draft_all, options, dependent);
        let br = bundle_policy.repos.get(id);
        let pr = project_policy.repos.get(id);
        let literal = titles.get(id).or_else(|| br.and_then(|p| p.title.as_ref()));
        let title_mode = bundle_policy.title.or(project_policy.title);
        let literal = literal.or_else(|| {
            if bundle_policy.title.is_none() {
                pr.and_then(|p| p.title.as_ref())
            } else {
                None
            }
        });
        let file = files
            .get(id)
            .or_else(|| br.and_then(|p| p.body_file.as_ref()))
            .or_else(|| bundle_policy.body.as_ref().and_then(|p| p.file.as_ref()))
            .or_else(|| pr.and_then(|p| p.body_file.as_ref()))
            .or_else(|| project_policy.body.as_ref().and_then(|p| p.file.as_ref()));
        let fallback = bundle_policy
            .body
            .as_ref()
            .and_then(|p| p.fallback)
            .or_else(|| project_policy.body.as_ref().and_then(|p| p.fallback));
        let mut body = String::new();
        let mut body_source = "knit".to_string();
        let mut file_title = None;
        if let Some(file) = file {
            let path = root.join(file.replace("{repo}", id));
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let (title, prose) = strip_title(&text);
                    file_title = title;
                    body = prose;
                    body_source = format!("file:{}", path.display());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !files.contains_key(id) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("read publish body {}", path.display()))
                }
            }
        }
        if body_source == "knit" && fallback == Some(PublishBodyFallback::UpstreamTemplate) {
            if let Some((source, text)) =
                super::template::upstream_template(&job.repo, &job.base_branch, root)?
            {
                body = text;
                body_source = source;
            }
        }
        let title = if let Some(title) = literal {
            title.clone()
        } else {
            match title_mode {
                None => format!("{} ({id})", bundle.title),
                Some(PublishTitle::BundleTitle) => bundle.title.clone(),
                Some(PublishTitle::CommitGroup) => bundle
                    .commit_groups
                    .last()
                    .map(|g| g.message.lines().next().unwrap_or_default().to_owned())
                    .unwrap_or_else(|| bundle.title.clone()),
                Some(PublishTitle::File) => file_title.with_context(|| {
                    format!("{id}: title=file requires a body file with a Title: line")
                })?,
            }
        };
        if title.trim().is_empty() {
            bail!("{id}: publish title is empty");
        }
        results.insert(
            id.clone(),
            ResolvedPublish {
                draft,
                draft_reason: if dependent && draft {
                    format!(
                        "{reason}; blocked on {}",
                        blockers.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                } else {
                    reason.into()
                },
                title,
                body,
                body_source,
                blocked_on: blockers,
            },
        );
    }
    waves(selected.into_iter(), &results)?;
    Ok(results)
}
fn strip_title(text: &str) -> (Option<String>, String) {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    match first.strip_prefix("Title:") {
        Some(title) => (Some(title.trim().to_owned()), rest.to_owned()),
        None => (None, text.to_owned()),
    }
}

pub(super) fn preview(
    bundle: &ChangeGroup,
    jobs: &[super::remote::PublishJob],
    resolutions: &BTreeMap<String, ResolvedPublish>,
) -> Result<()> {
    for job in jobs {
        let r = &resolutions[&job.repo.id];
        let head = crate::contribution::head(
            &job.repo,
            job.repo
                .feature_branch
                .as_deref()
                .context("missing feature branch")?,
        )?;
        println!("{}: target={} base={} head={} source={} fork={} draft={} ({})\n  title: {}\n  body source: {}\n{}",job.repo.id,crate::contribution::destination(&job.repo).unwrap_or("unknown"),job.base_branch,head,crate::contribution::source(&job.repo).unwrap_or("unknown"),crate::contribution::cross_repository(&job.repo)?,r.draft,r.draft_reason,r.title,r.body_source,r.body(bundle,&job.repo,crate::providers::for_repo(&job.repo)?.id()));
    }
    Ok(())
}

/// Stable topological waves preserve the bounded parallelism within each level.
/// Reject cycles during resolution, before any push or review mutation.
pub(super) fn waves(
    ids: impl Iterator<Item = String>,
    resolved: &BTreeMap<String, ResolvedPublish>,
) -> Result<Vec<BTreeSet<String>>> {
    let mut pending: BTreeSet<String> = ids.collect();
    let mut result = Vec::new();
    while !pending.is_empty() {
        let next: BTreeSet<_> = pending
            .iter()
            .filter(|id| resolved[*id].blocked_on.is_disjoint(&pending))
            .cloned()
            .collect();
        if next.is_empty() {
            bail!(
                "Publishing dependency cycle: {}",
                pending.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        pending.retain(|id| !next.contains(id));
        result.push(next);
    }
    Ok(result)
}

/// A recorded release acknowledgement is usable only for this bundle's exact
/// library head and review; receipts for earlier revisions cannot clear it.
fn released_libraries(bundle: &ChangeGroup, root: &Path) -> Result<BTreeSet<String>> {
    let mut released = BTreeSet::new();
    let Some(knit) = root
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == ".knit"))
    else {
        return Ok(released);
    };
    let runs = knit.join("land-runs");
    if !runs.is_dir() {
        return Ok(released);
    }
    for entry in std::fs::read_dir(runs)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let run: serde_json::Value = crate::store::read_json(&path)?;
        if run["schemaVersion"] != "0.2"
            || run["kind"] != "KnitLandRun"
            || run["bundleId"] != bundle.id
            || run["sourceBundle"]["id"] != bundle.id
        {
            continue;
        }
        for library in &bundle.repos {
            let Some(head) = crate::tracking::latest_recorded_head_sha(bundle, library) else {
                continue;
            };
            // v0.2 runs carry the immutable reviewed plan; its bundleHeads
            // pin is authoritative even when sourceBundle has legacy fields.
            let matches = run["plan"]["bundleHeads"][&library.id].as_str() == Some(head.as_str());
            let review_matches = publication_for_repo(bundle, &library.id).is_some_and(|pr| {
                run["sourceBundle"]["publications"]
                    .as_array()
                    .is_some_and(|prs| {
                        prs.iter()
                            .any(|p| p["repoId"] == library.id && p["url"] == pr.url)
                    })
            });
            if matches
                && review_matches
                && run["steps"].as_array().is_some_and(|steps| {
                    steps.iter().any(|s| {
                        s["id"] == format!("release-{}", library.id)
                            && s["repoId"] == library.id
                            && s["type"] == "manual"
                            && s["status"] == "succeeded"
                    })
                })
            {
                released.insert(library.id.clone());
            }
        }
    }
    Ok(released)
}
