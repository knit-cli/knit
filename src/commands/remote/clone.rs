//! `knit clone` — import a remote project export into a fresh local workspace:
//! clone its repositories, write the project and bundle artifacts, and
//! optionally materialize the active bundle.

use super::client::{
    configured_sync_remote_names, fast_forward_feature_checkouts, fetch_project_export,
    localize_bundle, normalize_base_url, prepare_feature_branches, resolve_export_bundle_payload,
    token_from_env,
};
use super::credentials::NO_ACCESS_HINT;
use super::{
    print_json_error_envelope, RemoteErrorKind, RemoteExportRepository, RemoteProjectExport,
    RemoteViews,
};
use crate::commands::agents::{
    print_bundle_worktree_agents_summary, write_bundle_worktree_agents_md,
};
use crate::commands::worktree::materialize_repos;
use crate::git::{current_branch, git_output, is_git_worktree, ref_exists};
use crate::ids::slugify;
use crate::model::{
    ChangeGroup, CheckoutMode, KnitConfig, KnitProject, KnitProjectViews, KnitRemote,
    ProjectRepoEntry, ProjectView, ViewBase, SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    bundle_path, find_knit_root, project_path, read_json, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Machine-readable `knit clone --json` result document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloneDocument {
    project: CloneDocumentProject,
    target_path: String,
    repos: Vec<CloneDocumentRepo>,
    cloned_repo_count: usize,
    failed_repo_count: usize,
    omitted_repository_count: u64,
    /// The saved view this workspace is scoped to (`--view`/`--repo`); absent
    /// for a whole-project clone.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_view: Option<String>,
    /// Repos the export offered that the scope left out on purpose. They are
    /// not failures: `knit view include <scope> <repo>` + `knit pull` adds one.
    repos_out_of_scope: Vec<String>,
    bundles: CloneDocumentBundles,
    active_bundle: Option<String>,
    worktrees_materialized: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentProject {
    id: String,
    /// Username or org slug half of the `owner/slug` clone reference. Null when
    /// the clone used a bare slug and the export carried no organization.
    owner: Option<String>,
    slug: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentRepo {
    id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentBundles {
    restored: Vec<String>,
    dropped: Vec<DroppedBundle>,
    /// Bundles left out only because they touch repos outside the scope.
    /// Distinct from `dropped`, which records clone failures and withheld
    /// repos; these are expected for a scoped workspace.
    out_of_scope: Vec<DroppedBundle>,
}

/// A bundle the export carried but the clone could not restore because one or
/// more of its repos were not cloned (failed or withheld by the server).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DroppedBundle {
    pub(super) id: String,
    pub(super) missing_repos: Vec<String>,
}

/// Name of the absolute view `knit clone --repo` saves so a hand-picked
/// scope can be extended like any other view (`knit view include scope <repo>`).
pub const CLONE_SCOPE_VIEW: &str = "scope";

/// What the caller asked `knit clone` to limit the workspace to: a saved view
/// (`--view`), an explicit repo list (`--repo`), or nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct CloneScopeRequest<'a> {
    pub view: Option<&'a str>,
    pub repos: &'a [String],
}

impl CloneScopeRequest<'_> {
    fn is_scoped(&self) -> bool {
        self.view.is_some() || !self.repos.is_empty()
    }
}

/// A clone scope resolved against the export: the view the workspace records
/// plus the repo ids it resolves to. `save_view` marks a `--repo` scope whose
/// view exists nowhere yet and must be written (and pushed) by the clone.
struct ResolvedCloneScope {
    view_name: String,
    view: ProjectView,
    repo_ids: BTreeSet<String>,
    save_view: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn clone_project_from_remote(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
    active_bundle: Option<&str>,
    materialize: bool,
    prefer_https: bool,
    scope: CloneScopeRequest<'_>,
    json: bool,
) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    match clone_project_classified(
        project_identifier,
        target,
        remote_name,
        url,
        token,
        active_bundle,
        materialize,
        prefer_https,
        scope,
    ) {
        Ok(document) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document)
                        .context("failed to serialize clone result document")?
                );
            }
            Ok(())
        }
        Err((kind, error)) => {
            if json {
                print_json_error_envelope(kind, &error);
            }
            Err(error)
        }
    }
}

/// Run the clone, tagging every failure with its machine-readable error kind so
/// the `--json` wrapper can emit the contract error envelope.
#[allow(clippy::too_many_arguments)] // Mirrors the public clone command's CLI options.
fn clone_project_classified(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
    active_bundle: Option<&str>,
    materialize: bool,
    prefer_https: bool,
    scope: CloneScopeRequest<'_>,
) -> std::result::Result<CloneDocument, (RemoteErrorKind, anyhow::Error)> {
    let reference = parse_clone_reference(project_identifier, url)
        .map_err(|error| (RemoteErrorKind::NoRemote, error))?;
    let (remote_name, remote, stored_token, token) =
        resolve_remote_for_clone_classified(remote_name, reference.remote_url.as_deref(), token)?;
    let mut export = fetch_project_export(&remote, token.as_deref(), &reference.project_identifier)
        .map_err(|error| (RemoteErrorKind::Http, error))?;
    if prefer_https {
        if let Some(token) = token.as_deref() {
            let hosts = super::helpers::connected_forge_hosts(&remote, token).unwrap_or_default();
            let cwd = std::env::current_dir().map_err(|e| (RemoteErrorKind::Other, e.into()))?;
            for repository in &mut export.repositories {
                if let Some(url) = repository.remote_url.clone() {
                    if let Some(https) = super::handoff::prefer_https_url(&url, &hosts) {
                        if super::handoff::reachable(&cwd, &url, &remote_name, &hosts).is_err()
                            && super::handoff::reachable(&cwd, &https, &remote_name, &hosts).is_ok()
                        {
                            repository.remote_url = Some(https);
                        }
                    }
                }
            }
        }
    }
    if scope.view.is_some() && token.is_none() {
        return Err((
            RemoteErrorKind::NoToken,
            anyhow::anyhow!(
                "`--view` needs a remote token: saved views are your own configuration on the remote. Set KNIT_REMOTE_<NAME>_TOKEN or KNIT_REMOTE_TOKEN, or run `knit remote token <name> <token>`."
            ),
        ));
    }
    clone_fetched_export(
        &reference.project_identifier,
        target,
        remote_name,
        remote,
        stored_token,
        token,
        export,
        active_bundle,
        materialize,
        scope,
    )
    .map_err(|error| (RemoteErrorKind::Other, error))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn clone_fetched_export(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: String,
    remote: KnitRemote,
    stored_token: Option<String>,
    token: Option<String>,
    export: RemoteProjectExport,
    active_bundle: Option<&str>,
    materialize: bool,
    scope: CloneScopeRequest<'_>,
) -> Result<CloneDocument> {
    // Views are fetched before any repo is cloned: `--view` resolves against
    // them, and a whole-project clone restores them as before. A failure is
    // fatal only when the scope depends on the answer.
    let project_id = export_project_id(&export);
    let mut views_unavailable: Option<String> = None;
    let remote_views = match token.as_deref() {
        Some(token) => match super::pull::fetch_remote_views(&remote, token, &project_id) {
            Ok(views) => Some(views),
            Err(error) if scope.view.is_some() => {
                return Err(error.context("failed to fetch your saved views for `--view`"))
            }
            Err(error) => {
                views_unavailable = Some(format!("{error:#}"));
                None
            }
        },
        None => {
            views_unavailable = Some("no remote token configured".to_string());
            None
        }
    };
    let resolved_scope = resolve_clone_scope(&export, scope, remote_views.as_ref())?;
    let (scoped_repositories, repos_out_of_scope, repos_unavailable) =
        partition_export_repositories(&export, resolved_scope.as_ref());

    let target_root = resolve_clone_target(target, project_identifier)?;
    prepare_clone_target(&target_root)?;

    fs::create_dir_all(target_root.join(".knit/projects")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/projects").display()
        )
    })?;
    fs::create_dir_all(target_root.join(".knit/bundles")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/bundles").display()
        )
    })?;
    fs::create_dir_all(target_root.join(".knit/worktrees")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/worktrees").display()
        )
    })?;

    super::helpers::ensure_helpers_for_git(&remote_name);
    let (repo_paths, mut failed_repos) =
        clone_export_repositories_collecting(&target_root, &scoped_repositories);
    // A scope repo the export carries no record for (withheld by the server,
    // or never registered) cannot be cloned; say so instead of dropping it.
    for repo_id in repos_unavailable {
        failed_repos.push((
            repo_id,
            "not in the remote export (withheld from your token, or not registered on the remote)"
                .to_string(),
        ));
    }
    if repo_paths.is_empty() {
        bail!(
            "Failed to clone any repository for project `{}`:\n{}",
            export.project.slug,
            format_repo_failures(&failed_repos)
        );
    }
    let project = local_project_from_export(&export, &repo_paths)?;
    write_json(&project_path(&target_root, &project.id), &project)?;

    let (bundles, dropped_bundles) =
        localized_export_bundles(&export, &project, &remote, &remote_name, token.as_deref())?;
    let out_of_scope: BTreeSet<&str> = repos_out_of_scope.iter().map(String::as_str).collect();
    let (out_of_scope_bundles, dropped_bundles): (Vec<DroppedBundle>, Vec<DroppedBundle>) =
        dropped_bundles.into_iter().partition(|dropped| {
            dropped
                .missing_repos
                .iter()
                .all(|repo| out_of_scope.contains(repo.as_str()))
        });
    for bundle in &bundles {
        write_json(&bundle_path(&target_root, &bundle.id), bundle)?;
    }
    let history_count = crate::history::append_history_events(
        &target_root,
        &project.id,
        &export.decoded_history_events(&project.id),
    )?;

    let selected_bundle_id = select_active_bundle(&bundles, &out_of_scope_bundles, active_bundle)?;
    let mut remotes = BTreeMap::new();
    remotes.insert(
        remote_name.clone(),
        KnitRemote {
            url: remote.url.clone(),
            token: stored_token,
        },
    );
    let config = KnitConfig {
        schema_version: SCHEMA_VERSION.to_string(),
        active_bundle: selected_bundle_id.clone(),
        active_project: Some(project.id.clone()),
        sync_remote: Some(remote_name.clone()),
        sync_remotes: vec![remote_name.clone()],
        advice: true,
        stealth: None,
        auto_tag: None,
        push_sync: true,
        scope_view: resolved_scope.as_ref().map(|scope| scope.view_name.clone()),
        remotes,
    };
    crate::store::save_config(&target_root, &config)?;

    // Restore the cloning user's saved views, adding the `--repo` scope view
    // when the clone made one so the scope can be extended like any view.
    let mut views = match remote_views {
        Some(remote_views) => super::pull::views_from_remote(&project.id, remote_views),
        None => KnitProjectViews::new(project.id.clone(), now_iso()),
    };
    let mut push_views = false;
    if let Some(scope) = resolved_scope.as_ref().filter(|scope| scope.save_view) {
        views
            .views
            .insert(scope.view_name.clone(), scope.view.clone());
        views.updated_at = now_iso();
        push_views = true;
    }
    if !views.views.is_empty() || views.default_view.is_some() {
        crate::store::save_views(&target_root, &views)?;
        crate::human!("{} {} view(s)", out::heading("Views:"), views.views.len());
    }
    // The scope view has to outlive the next `knit sync pull --views`, which
    // replaces local views with the remote's, so push it right away — but
    // only when the remote's document was actually read: uploading otherwise
    // would replace every view the user has on the remote with this one.
    if push_views {
        match (token.as_deref(), views_unavailable.as_deref()) {
            (Some(token), None) => {
                if let Err(error) =
                    super::push::upload_views(&remote, token, &target_root, &project.id)
                {
                    crate::human!(
                        "{} {error:#} (run `knit sync push --views` later)",
                        out::warn("scope view not pushed:")
                    );
                }
            }
            (_, reason) => crate::human!(
                "{} {}; the view `{}` exists only in this workspace until `knit sync push --views` succeeds, and a `knit sync pull --views` before that drops it.",
                out::warn("scope view not pushed:"),
                reason.unwrap_or("remote views unavailable"),
                CLONE_SCOPE_VIEW
            ),
        }
    }

    let mut worktrees_materialized = false;
    if materialize {
        if let Some(bundle_id) = selected_bundle_id.as_deref() {
            materialize_imported_bundle(&target_root, bundle_id)?;
            worktrees_materialized = true;
        }
    }

    crate::human!(
        "{} {} {}",
        out::movement("cloned"),
        out::repo(&project.id),
        out::path(target_root.display())
    );
    crate::human!(
        "{} {} repo(s), {} bundle(s)",
        out::heading("Imported:"),
        project.repos.len(),
        bundles.len()
    );
    if history_count > 0 {
        crate::human!("{} {} event(s)", out::heading("History:"), history_count);
    }
    if !failed_repos.is_empty() {
        crate::human!(
            "{} {} repo(s) could not be cloned and were left out of the workspace:",
            out::heading("Skipped:"),
            failed_repos.len()
        );
        for (local_id, error) in &failed_repos {
            crate::human!("  {}: {}", out::repo(local_id), out::muted(error));
        }
    }
    if let Some(omitted) = export.omitted_repository_count.filter(|count| *count > 0) {
        crate::human!(
            "{} the remote omitted {omitted} private repo(s) from this export; the cloned project is incomplete. Ask a project maintainer for access.",
            out::warn("Not exported:")
        );
    }
    for dropped in &dropped_bundles {
        crate::human!(
            "{} dropped bundle {}: repo {} not cloned",
            out::warn("Dropped:"),
            out::repo(&dropped.id),
            dropped.missing_repos.join(", ")
        );
    }
    if let Some(scope) = resolved_scope.as_ref() {
        crate::human!(
            "{} view {} — {} repo(s) left out: {}",
            out::heading("Scope:"),
            out::repo(&scope.view_name),
            repos_out_of_scope.len(),
            if repos_out_of_scope.is_empty() {
                "none".to_string()
            } else {
                repos_out_of_scope.join(", ")
            }
        );
        for bundle in &out_of_scope_bundles {
            crate::human!(
                "{} bundle {} touches {}",
                out::muted("Outside scope:"),
                out::repo(&bundle.id),
                bundle.missing_repos.join(", ")
            );
        }
        crate::human!(
            "{}",
            out::muted(format!(
                "Extend the scope with `knit view include {} <repo>` followed by `knit pull`.",
                scope.view_name
            ))
        );
    }

    Ok(clone_document(
        project_identifier,
        &export,
        &project,
        &target_root,
        &repo_paths,
        &failed_repos,
        &bundles,
        dropped_bundles,
        CloneScopeOutcome {
            view: resolved_scope.map(|scope| scope.view_name),
            repos_out_of_scope,
            bundles: out_of_scope_bundles,
        },
        selected_bundle_id,
        worktrees_materialized,
    ))
}

/// What a scope left out of the clone, for the result document.
struct CloneScopeOutcome {
    view: Option<String>,
    repos_out_of_scope: Vec<String>,
    bundles: Vec<DroppedBundle>,
}

/// The local project id a clone of this export will use, before the project
/// artifact exists: the exported knit project's id, else the remote slug.
fn export_project_id(export: &RemoteProjectExport) -> String {
    slugify(
        export
            .knit_project
            .as_ref()
            .map(|project| project.id.as_str())
            .unwrap_or(export.project.slug.as_str()),
    )
}

/// The project membership a scope resolves against: the exported knit
/// project when it carries repos, else a project built from the export's
/// repository records (paths are unknown before cloning and irrelevant here).
fn membership_project_from_export(export: &RemoteProjectExport) -> KnitProject {
    if let Some(project) = export
        .knit_project
        .as_ref()
        .filter(|project| !project.repos.is_empty())
    {
        return project.clone();
    }
    let mut project = KnitProject::new(export_project_id(export), now_iso());
    project.repos = export
        .repositories
        .iter()
        .map(|repository| project_repo_entry_from_export(repository, Path::new("")))
        .collect();
    project
}

/// Resolve `--view`/`--repo` into the view the workspace will record and the
/// repo ids to clone. `--view` must name one of the user's remote views;
/// `--repo` ids must be project repos and become the absolute view `scope`.
fn resolve_clone_scope(
    export: &RemoteProjectExport,
    scope: CloneScopeRequest<'_>,
    remote_views: Option<&RemoteViews>,
) -> Result<Option<ResolvedCloneScope>> {
    if !scope.is_scoped() {
        return Ok(None);
    }
    let membership = membership_project_from_export(export);
    let known: Vec<&str> = membership
        .repos
        .iter()
        .map(|repo| repo.id.as_str())
        .collect();

    if let Some(view_name) = scope.view {
        let view_name = slugify(view_name);
        let views = remote_views.context("no saved views were returned by the remote")?;
        let Some(view) = views.views.get(&view_name).cloned() else {
            let available: Vec<&str> = views.views.keys().map(String::as_str).collect();
            bail!(
                "You have no saved view named `{view_name}` for project `{}` on the remote. {}",
                export.project.slug,
                if available.is_empty() {
                    "Clone the whole project, or pass `--repo <id>` to pick repos directly."
                        .to_string()
                } else {
                    format!("Available views: {}.", available.join(", "))
                }
            );
        };
        let repo_ids: BTreeSet<String> = crate::commands::init::resolve_view_repos(
            &membership,
            &[],
            false,
            Some((view_name.as_str(), &view)),
            &[],
            &[],
        )?
        .into_iter()
        .map(|repo| repo.id)
        .collect();
        if repo_ids.is_empty() {
            bail!(
                "View `{view_name}` resolves to no repos of project `{}`; nothing to clone.",
                export.project.slug
            );
        }
        return Ok(Some(ResolvedCloneScope {
            view_name,
            view,
            repo_ids,
            save_view: false,
        }));
    }

    let mut repo_ids = BTreeSet::new();
    for repo in scope.repos {
        let repo_id = slugify(repo);
        if !known.contains(&repo_id.as_str()) {
            bail!(
                "Project `{}` has no repo named `{repo_id}`. Available: {}.",
                export.project.slug,
                known.join(", ")
            );
        }
        repo_ids.insert(repo_id);
    }
    let view = ProjectView {
        base: ViewBase::None,
        include: repo_ids.iter().cloned().collect(),
        exclude: Vec::new(),
    };
    // The user may already keep a remote view named `scope` (from an earlier
    // `--repo` clone). Reuse it when it is the same shape; refuse to overwrite
    // a different one, since that would silently rescope the other workspace.
    let mut save_view = true;
    if let Some(existing) = remote_views.and_then(|views| views.views.get(CLONE_SCOPE_VIEW)) {
        let same_shape = existing.base == ViewBase::None
            && existing.exclude.is_empty()
            && existing.include.iter().cloned().collect::<BTreeSet<_>>() == repo_ids;
        if same_shape {
            save_view = false;
        } else {
            bail!(
                "You already have a remote view named `{CLONE_SCOPE_VIEW}` for project `{}` with a different shape ({}). Clone it with `--view {CLONE_SCOPE_VIEW}`, or rename it first (`knit view save <new> --from {CLONE_SCOPE_VIEW}` then `knit view rm {CLONE_SCOPE_VIEW}` in a workspace that has it).",
                export.project.slug,
                if existing.include.is_empty() {
                    "empty".to_string()
                } else {
                    existing.include.join(", ")
                }
            );
        }
    }
    Ok(Some(ResolvedCloneScope {
        view_name: CLONE_SCOPE_VIEW.to_string(),
        view,
        repo_ids,
        save_view,
    }))
}

/// Split the export's repositories into the ones to clone and the ids the
/// scope leaves out (export order). Unscoped clones keep everything.
fn partition_export_repositories(
    export: &RemoteProjectExport,
    scope: Option<&ResolvedCloneScope>,
) -> (Vec<RemoteExportRepository>, Vec<String>, Vec<String>) {
    let Some(scope) = scope else {
        return (export.repositories.clone(), Vec::new(), Vec::new());
    };
    let mut selected = Vec::new();
    let mut left_out = Vec::new();
    let mut seen = BTreeSet::new();
    for repository in &export.repositories {
        let local_id = export_repo_local_id(repository);
        if scope.repo_ids.contains(&local_id) {
            seen.insert(local_id);
            selected.push(repository.clone());
        } else {
            left_out.push(local_id);
        }
    }
    // Scope ids the membership knows but the export carries no record for:
    // the caller reports them as failures rather than losing them.
    let unavailable = scope
        .repo_ids
        .iter()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    (selected, left_out, unavailable)
}

/// Assemble the `--json` result document from the clone's outcomes. Repos keep
/// the export's order; repos the server withheld appear only in
/// `omittedRepositoryCount` (the export never names them).
#[allow(clippy::too_many_arguments)]
fn clone_document(
    project_identifier: &str,
    export: &RemoteProjectExport,
    project: &KnitProject,
    target_root: &Path,
    repo_paths: &BTreeMap<String, PathBuf>,
    failed_repos: &[(String, String)],
    bundles: &[ChangeGroup],
    dropped_bundles: Vec<DroppedBundle>,
    scope: CloneScopeOutcome,
    active_bundle: Option<String>,
    worktrees_materialized: bool,
) -> CloneDocument {
    let out_of_scope: BTreeSet<&str> = scope
        .repos_out_of_scope
        .iter()
        .map(String::as_str)
        .collect();
    let (identifier_owner, _slug) = super::client::split_project_identifier(project_identifier);
    let owner = identifier_owner.or_else(|| {
        export
            .project
            .organization
            .as_ref()
            .and_then(|organization| organization.slug.clone())
    });
    let repos = export
        .repositories
        .iter()
        .filter(|repository| !out_of_scope.contains(export_repo_local_id(repository).as_str()))
        .map(|repository| {
            let local_id = export_repo_local_id(repository);
            if repo_paths.contains_key(&local_id) {
                CloneDocumentRepo {
                    id: local_id,
                    status: "cloned",
                    error: None,
                }
            } else {
                let error = failed_repos
                    .iter()
                    .find(|(failed_id, _)| *failed_id == local_id)
                    .map(|(_, error)| error.clone());
                CloneDocumentRepo {
                    id: local_id,
                    status: "failed",
                    error,
                }
            }
        })
        .collect::<Vec<_>>();
    let mut repos = repos;
    for (failed_id, error) in failed_repos {
        if !repos.iter().any(|repo| &repo.id == failed_id) {
            repos.push(CloneDocumentRepo {
                id: failed_id.clone(),
                status: "failed",
                error: Some(error.clone()),
            });
        }
    }
    let cloned_repo_count = repos.iter().filter(|repo| repo.status == "cloned").count();
    let failed_repo_count = repos.len() - cloned_repo_count;

    CloneDocument {
        project: CloneDocumentProject {
            id: project.id.clone(),
            owner,
            slug: export.project.slug.clone(),
        },
        target_path: target_root.display().to_string(),
        repos,
        cloned_repo_count,
        failed_repo_count,
        omitted_repository_count: export.omitted_repository_count.unwrap_or(0),
        scope_view: scope.view,
        repos_out_of_scope: scope.repos_out_of_scope,
        bundles: CloneDocumentBundles {
            restored: bundles.iter().map(|bundle| bundle.id.clone()).collect(),
            dropped: dropped_bundles,
            out_of_scope: scope.bundles,
        },
        active_bundle,
        worktrees_materialized,
    }
}

/// Resolve the remote endpoint and any available token exactly as `knit clone`
/// does. Public exports do not require credentials, so token absence is not a
/// resolution error here. Callers that require authentication must reject
/// `None` themselves with a `noToken` error.
type ResolvedCloneRemote = (String, KnitRemote, Option<String>, Option<String>);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CloneReference {
    pub(super) project_identifier: String,
    pub(super) remote_url: Option<String>,
}

/// Accept the traditional `owner/slug` selector and the absolute, GitHub-like
/// form `https://host/owner/slug`. In the absolute form the authority is the
/// remote endpoint and the two path segments are the unambiguous project
/// namespace; there is no second URL argument that can disagree with it.
pub(super) fn parse_clone_reference(reference: &str, url: Option<&str>) -> Result<CloneReference> {
    let parsed = match url::Url::parse(reference) {
        Ok(parsed) => parsed,
        Err(_) if reference.contains("://") => {
            bail!("Invalid absolute project URL `{reference}`.")
        }
        Err(_) => {
            return Ok(CloneReference {
                project_identifier: reference.to_string(),
                remote_url: url.map(ToString::to_string),
            })
        }
    };

    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "Clone project URLs must have the form `https://host/owner/project` without credentials, a query, or a fragment."
        );
    }
    if url.is_some() {
        bail!("Do not pass --url with an absolute project URL; the project URL already identifies the remote endpoint.");
    }

    let segments = parsed
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.trim().is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if segments.len() != 2 {
        bail!(
            "Clone project URLs must have exactly an owner and project path: `https://host/owner/project`."
        );
    }

    let mut endpoint = parsed;
    endpoint.set_path("");
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    Ok(CloneReference {
        project_identifier: format!("{}/{}", slugify(&segments[0]), slugify(&segments[1])),
        remote_url: Some(endpoint.as_str().trim_end_matches('/').to_string()),
    })
}

pub(super) fn resolve_remote_for_clone_classified(
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> std::result::Result<ResolvedCloneRemote, (RemoteErrorKind, anyhow::Error)> {
    let (remote_name, remote, stored_token) = resolve_clone_endpoint(remote_name, url, token)
        .map_err(|error| (RemoteErrorKind::NoRemote, error))?;
    let resolved_token = token
        .map(ToString::to_string)
        .or_else(|| token_from_env(&remote_name))
        .or_else(|| remote.token.clone());
    Ok((remote_name, remote, stored_token, resolved_token))
}

pub(super) fn resolve_clone_endpoint(
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> Result<(String, KnitRemote, Option<String>)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Inside a workspace, the effective config already merges global remotes in.
    // Outside one, fall back to the user-global config so `knit clone` works from
    // any directory, not just an existing Knit workspace.
    let config = match find_knit_root(&cwd) {
        Some(root) => crate::store::load_effective_config(&root).ok(),
        None => crate::store::load_global_config().ok(),
    };
    let requested_name = remote_name.map(slugify).filter(|name| !name.is_empty());
    let configured_name = config.as_ref().and_then(|config| {
        if let Some(url) = url {
            configured_sync_remote_names(config)
                .into_iter()
                .find(|name| {
                    config
                        .remotes
                        .get(name)
                        .is_some_and(|remote| clone_endpoints_match(&remote.url, url))
                })
        } else {
            configured_sync_remote_names(config).into_iter().next()
        }
    });
    // An absolute clone URL is self-contained. When no configured remote owns
    // that endpoint, record it as `origin`, just as Git does for a URL clone.
    let remote_name = requested_name
        .clone()
        .or(configured_name)
        .or_else(|| url.map(|_| "origin".to_string()))
        .with_context(|| {
            "No remote selected. Pass an absolute project URL, use `--remote <name>` with `--url <url>`, or configure a sync remote first."
        })?;
    let configured = config
        .as_ref()
        .and_then(|config| config.remotes.get(&remote_name).cloned());
    if requested_name.is_some() {
        if let (Some(configured), Some(url)) = (configured.as_ref(), url) {
            if !clone_endpoints_match(&configured.url, url) {
                bail!(
                    "Remote `{remote_name}` points at `{}`, but the absolute project URL points at `{}`.",
                    configured.url,
                    url
                );
            }
        }
    }
    let env_url = std::env::var("KNIT_REMOTE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let remote_url = url
        .map(ToString::to_string)
        .or(env_url)
        .or_else(|| configured.as_ref().map(|remote| remote.url.clone()))
        .with_context(|| {
            format!("No URL configured for remote `{remote_name}`. Pass --url, set KNIT_REMOTE_URL, or run `knit remote add {remote_name} <url>`.")
        })?;
    let stored_token = token
        .map(ToString::to_string)
        .or_else(|| configured.as_ref().and_then(|remote| remote.token.clone()));
    let remote = KnitRemote {
        url: normalize_base_url(&remote_url),
        token: stored_token.clone(),
    };

    Ok((remote_name, remote, stored_token))
}

/// Treat a hosted frontend and its `api.` sibling as the same configured
/// service so an absolute API clone URL can reuse the user's stored token.
fn clone_endpoints_match(left: &str, right: &str) -> bool {
    fn identity(value: &str) -> Option<(String, String, Option<u16>)> {
        let parsed = url::Url::parse(value).ok()?;
        let host = parsed
            .host_str()?
            .trim_start_matches("api.")
            .to_ascii_lowercase();
        Some((parsed.scheme().to_ascii_lowercase(), host, parsed.port()))
    }

    identity(left) == identity(right)
}

fn resolve_clone_target(target: Option<&Path>, project_identifier: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Default the directory to the project slug, dropping any `owner/` prefix.
    let (_owner, slug) = super::client::split_project_identifier(project_identifier);
    let target = target
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(slug));
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(cwd.join(target))
    }
}

fn prepare_clone_target(target: &Path) -> Result<()> {
    if target.join(".knit/config.json").exists() {
        bail!("{} is already a Knit workspace.", target.display());
    }

    if target.exists() {
        let mut entries = fs::read_dir(target)
            .with_context(|| format!("failed to read clone target {}", target.display()))?;
        if entries.next().transpose()?.is_some() {
            bail!("Clone target {} is not empty.", target.display());
        }
    } else {
        fs::create_dir_all(target)
            .with_context(|| format!("failed to create clone target {}", target.display()))?;
    }

    Ok(())
}

pub(super) fn clone_export_repositories(
    target_root: &Path,
    repositories: &[RemoteExportRepository],
) -> Result<BTreeMap<String, PathBuf>> {
    let mut paths = BTreeMap::new();

    for repository in repositories {
        let (local_id, repo_path) = clone_one_export_repository(target_root, repository)?;
        paths.insert(local_id, repo_path);
    }

    Ok(paths)
}

/// Clone (or adopt) one exported repository. Authentication comes from the
/// installed Git credential helpers; a failed non-public clone carries the
/// access hint.
pub(super) fn clone_one_export_repository(
    target_root: &Path,
    repository: &RemoteExportRepository,
) -> Result<(String, PathBuf)> {
    let local_id = export_repo_local_id(repository);
    let remote_url = repository
        .remote_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .with_context(|| format!("{local_id}: remote export has no clone URL."))?;
    let repo_path = target_root.join(&local_id);

    if repo_path.exists() {
        if !is_git_worktree(&repo_path) {
            bail!("{} exists but is not a git checkout.", repo_path.display());
        }
        let origin = git_output(&repo_path, ["remote", "get-url", "origin"])?;
        if !super::handoff::same_repository_url(origin.trim(), remote_url) {
            bail!(
                "{} belongs to a different Git origin; refusing to adopt it",
                repo_path.display()
            );
        }
        crate::human!(
            "{}: {} {}",
            out::repo(&local_id),
            out::muted("using existing checkout"),
            out::path(repo_path.display())
        );
        return Ok((local_id, repo_path));
    }

    let clone_args = [
        OsString::from("clone"),
        OsString::from(remote_url),
        repo_path.as_os_str().to_os_string(),
    ];
    if let Err(error) = git_output(target_root, clone_args) {
        // A failed clone can leave a partial target dir behind; clear it so a
        // rerun starts clean.
        if repo_path.exists() {
            let _ = fs::remove_dir_all(&repo_path);
        }
        // Blame the right thing: a repo the sync remote knows is gone from its
        // forge fails for everyone, a failed public clone is not a credential
        // problem, and only the genuinely ambiguous case earns the access hint.
        let error = if export_repo_forge_missing(repository) {
            anyhow::anyhow!(
                "{error:#}; the sync remote marked this repository missing on its forge — it does not exist (or was deleted/renamed)"
            )
        } else if repository.visibility.as_deref() == Some("public") {
            error
        } else {
            anyhow::anyhow!("{error:#}; {NO_ACCESS_HINT}")
        };
        return Err(error.context(format!("{local_id}: failed to clone {remote_url}")));
    }
    crate::human!(
        "{}: {} {}",
        out::repo(&local_id),
        out::movement("cloned"),
        out::path(repo_path.display())
    );

    checkout_export_base_branch(&repo_path, repository)?;
    Ok((local_id, repo_path))
}

/// Clone every exported repository, skipping (and recording) any that fail so an
/// inaccessible repo, such as a private GitHub repo the token cannot read, does
/// not abort the whole clone. Mirrors the per-repo resilience used by incremental
/// remote pull. Returns the cloned paths and the (local id, error) failures.
fn clone_export_repositories_collecting(
    target_root: &Path,
    repositories: &[RemoteExportRepository],
) -> (BTreeMap<String, PathBuf>, Vec<(String, String)>) {
    let mut paths = BTreeMap::new();
    let mut failed = Vec::new();
    for repository in repositories {
        match clone_one_export_repository(target_root, repository) {
            Ok((local_id, repo_path)) => {
                paths.insert(local_id, repo_path);
            }
            Err(error) => failed.push((export_repo_local_id(repository), format!("{error:#}"))),
        }
    }
    (paths, failed)
}

fn format_repo_failures(failed: &[(String, String)]) -> String {
    failed
        .iter()
        .map(|(local_id, error)| format!("  {local_id}: {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn checkout_export_base_branch(
    repo_path: &Path,
    repository: &RemoteExportRepository,
) -> Result<()> {
    let Some(base_branch) = repository
        .default_branch
        .as_deref()
        .filter(|branch| !branch.trim().is_empty())
    else {
        return Ok(());
    };
    if current_branch(repo_path)?.as_deref() == Some(base_branch) {
        return Ok(());
    }

    let remote_ref = format!("origin/{base_branch}");
    if ref_exists(repo_path, &remote_ref) {
        git_output(
            repo_path,
            [
                OsString::from("checkout"),
                OsString::from("-B"),
                OsString::from(base_branch),
                OsString::from(remote_ref),
            ],
        )?;
    }
    Ok(())
}

fn local_project_from_export(
    export: &RemoteProjectExport,
    repo_paths: &BTreeMap<String, PathBuf>,
) -> Result<KnitProject> {
    let mut project = export
        .knit_project
        .clone()
        .unwrap_or_else(|| KnitProject::new(export.project.slug.clone(), now_iso()));
    project.id = slugify(&project.id);
    project.repos.clear();

    for repository in &export.repositories {
        let local_id = export_repo_local_id(repository);
        // Repos that failed to clone are absent from repo_paths; leave them out
        // of the local project rather than recording an entry with no checkout.
        let Some(repo_path) = repo_paths.get(&local_id) else {
            continue;
        };
        project
            .repos
            .push(project_repo_entry_from_export(repository, repo_path));
    }

    project.updated_at = now_iso();
    Ok(project)
}

/// Build a local project repo entry from an exported repository and its cloned
/// path. Shared with incremental remote pull so both code paths record repos the
/// same way.
pub(super) fn project_repo_entry_from_export(
    repository: &RemoteExportRepository,
    repo_path: &Path,
) -> ProjectRepoEntry {
    ProjectRepoEntry {
        id: export_repo_local_id(repository),
        path: repo_path.to_string_lossy().to_string(),
        remote: repository.remote_url.clone(),
        base_branch: repository
            .default_branch
            .clone()
            .filter(|branch| !branch.trim().is_empty())
            .unwrap_or_else(|| "main".to_string()),
        // Remote metadata is advisory; anything other than an explicit
        // `inPlace` falls back to the worktree default.
        checkout_mode: match metadata_string(&repository.metadata, "checkoutMode").as_deref() {
            Some("inPlace") => CheckoutMode::InPlace,
            _ => CheckoutMode::Worktree,
        },
        include_by_default: metadata_bool(&repository.metadata, "includeByDefault").unwrap_or(true),
    }
}

/// Localize every exportable bundle onto the local project, dropping any bundle
/// that references a repo missing from the project (because its clone failed or
/// the server withheld it). Returns the localized bundles plus a record of each
/// dropped bundle and the repo ids it was missing, so callers can surface the
/// loss instead of silently presenting a partial import.
///
/// The export carries no payloads, so each bundle's artifact is fetched on its
/// own, one after another: a clone must never ask the server to build the whole
/// project's artifacts at once.
fn localized_export_bundles(
    export: &RemoteProjectExport,
    project: &KnitProject,
    remote: &KnitRemote,
    remote_name: &str,
    token: Option<&str>,
) -> Result<(Vec<ChangeGroup>, Vec<DroppedBundle>)> {
    let available: BTreeSet<&str> = project.repos.iter().map(|repo| repo.id.as_str()).collect();
    let mut localized = Vec::new();
    let mut dropped = Vec::new();

    for bundle in export
        .bundles
        .iter()
        .filter(|bundle| bundle.lifecycle_state != "deleted")
    {
        if bundle.current_artifact.is_none() {
            continue;
        }
        let (payload, artifact_hash) = resolve_export_bundle_payload(remote, token, bundle)?;
        let missing_repos = missing_bundle_repos(&payload, &available);
        if !missing_repos.is_empty() {
            dropped.push(DroppedBundle {
                id: bundle.slug.clone(),
                missing_repos,
            });
            continue;
        }
        let mut imported = localize_bundle(payload, project)?;
        imported.record_sync_target_with_artifact(
            remote_name,
            &bundle.id,
            &remote.url,
            Some(&artifact_hash),
        );
        localized.push(imported);
    }

    Ok((localized, dropped))
}

/// Repo ids a bundle payload references that are absent from the cloned
/// project, in payload order.
pub(super) fn missing_bundle_repos(
    payload: &ChangeGroup,
    available: &BTreeSet<&str>,
) -> Vec<String> {
    payload
        .repos
        .iter()
        .filter(|repo| !available.contains(repo.id.as_str()))
        .map(|repo| repo.id.clone())
        .collect()
}

fn select_active_bundle(
    bundles: &[ChangeGroup],
    out_of_scope: &[DroppedBundle],
    requested: Option<&str>,
) -> Result<Option<String>> {
    if let Some(requested) = requested {
        let requested = slugify(requested);
        if bundles.iter().any(|bundle| bundle.id == requested) {
            return Ok(Some(requested));
        }
        if let Some(skipped) = out_of_scope.iter().find(|bundle| bundle.id == requested) {
            bail!(
                "Bundle `{requested}` touches repos outside this clone's scope ({}). Include them in the view and clone again, or clone the whole project.",
                skipped.missing_repos.join(", ")
            );
        }
        bail!("Remote export has no bundle named `{requested}`.");
    }

    Ok(bundles
        .iter()
        .find(|bundle| {
            bundle.state.unwrap_or(crate::model::BundleState::Open)
                == crate::model::BundleState::Open
        })
        .or_else(|| bundles.first())
        .map(|bundle| bundle.id.clone()))
}

pub(super) fn materialize_imported_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let bundle_path = bundle_path(root, bundle_id);
    let bundle: ChangeGroup = read_json(&bundle_path)?;
    prepare_feature_branches(&bundle)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), bundle_path, bundle);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    let bundle_agents = write_bundle_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    crate::store::save_active_bundle(&active)
}

pub(super) fn export_repo_local_id(repository: &RemoteExportRepository) -> String {
    repository
        .local_id
        .clone()
        .or_else(|| metadata_string(&repository.metadata, "localId"))
        .unwrap_or_else(|| slugify(&repository.name))
}

/// True when the sync remote marked this repository missing on its forge
/// (an owner-credentialed 404 during visibility refresh) — no credential the
/// puller could connect would make a clone succeed.
pub(super) fn export_repo_forge_missing(repository: &RemoteExportRepository) -> bool {
    metadata_string(&repository.metadata, "forgeState").as_deref() == Some("missing")
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn metadata_bool(metadata: &Value, key: &str) -> Option<bool> {
    metadata.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("knit-clone-test-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_source_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    fn export_repo(name: &str, remote_url: &str) -> RemoteExportRepository {
        RemoteExportRepository {
            local_id: Some(name.to_string()),
            name: name.to_string(),
            default_branch: None,
            remote_url: Some(remote_url.to_string()),
            visibility: None,
            metadata: Value::Null,
        }
    }

    #[test]
    fn absolute_clone_reference_owns_endpoint_and_project_namespace() {
        let reference =
            parse_clone_reference("https://svartal.com/Marc-Merino/Demoapp/", None).unwrap();

        assert_eq!(
            reference,
            CloneReference {
                project_identifier: "marc-merino/demoapp".to_string(),
                remote_url: Some("https://svartal.com".to_string()),
            }
        );
    }

    #[test]
    fn absolute_clone_reference_rejects_confusable_url_override() {
        let error = parse_clone_reference(
            "https://api.svartal.com/marc/demoapp",
            Some("https://api.knithub.dev"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("Do not pass --url"));
    }

    #[test]
    fn clone_endpoint_matches_frontend_and_api_siblings() {
        assert!(clone_endpoints_match(
            "https://svartal.com",
            "https://api.svartal.com"
        ));
        assert!(!clone_endpoints_match(
            "https://api.knithub.dev",
            "https://api.svartal.com"
        ));
    }

    #[test]
    fn clone_collecting_skips_failed_repos_and_keeps_the_good_ones() {
        let root = temp_dir("collect");
        let source = root.join("source.git");
        init_source_repo(&source);
        let target = root.join("workspace");
        fs::create_dir_all(&target).unwrap();

        // `bad` points at a path that cannot be cloned; `good` is a real repo.
        let repos = [
            export_repo("bad", &root.join("does-not-exist").to_string_lossy()),
            export_repo("good", &source.to_string_lossy()),
        ];

        let (paths, failed) = clone_export_repositories_collecting(&target, &repos);

        assert!(paths.contains_key("good"), "good repo should be cloned");
        assert!(target.join("good").join(".git").exists());
        assert!(!paths.contains_key("bad"), "bad repo should be skipped");
        assert!(!target.join("bad").exists());

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "bad");
        assert!(
            failed[0].1.contains("bad") || failed[0].1.contains("clone"),
            "failure should name the repo or the clone step: {}",
            failed[0].1
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn export_bundle_payload(id: &str, repo_ids: &[&str]) -> Value {
        let repos: Vec<Value> = repo_ids
            .iter()
            .map(|repo_id| {
                serde_json::json!({
                    "id": repo_id,
                    "path": format!("/tmp/{repo_id}"),
                    "baseBranch": "main",
                })
            })
            .collect();
        serde_json::json!({
            "schemaVersion": "1",
            "kind": "knit.bundle",
            "id": id,
            "title": id,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "repos": repos,
            "commitGroups": [],
        })
    }

    fn export_with_bundles(bundles: Value) -> RemoteProjectExport {
        serde_json::from_value(serde_json::json!({
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": bundles,
            "historyEvents": [],
        }))
        .unwrap()
    }

    fn project_with_backend() -> KnitProject {
        let mut project = KnitProject::new("demo".to_string(), now_iso());
        project.repos.push(ProjectRepoEntry {
            id: "backend".to_string(),
            path: "/tmp/backend".to_string(),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: CheckoutMode::Worktree,
            include_by_default: true,
        });
        project
    }

    #[test]
    fn localized_export_bundles_records_dropped_bundles_with_missing_repos() {
        let export = export_with_bundles(serde_json::json!([
            {
                "id": "rb-1",
                "slug": "feature-a",
                "lifecycleState": "open",
                "currentArtifact": {
                    "artifactHash": "hash-a",
                    "payload": export_bundle_payload("feature-a", &["backend"]),
                },
            },
            {
                "id": "rb-2",
                "slug": "feature-c",
                "lifecycleState": "open",
                "currentArtifact": {
                    "artifactHash": "hash-c",
                    "payload": export_bundle_payload("feature-c", &["backend", "frontend"]),
                },
            },
            // A deleted bundle and an artifact-less bundle are ignored, not dropped.
            {"id": "rb-3", "slug": "gone", "lifecycleState": "deleted", "currentArtifact": null},
            {"id": "rb-4", "slug": "empty", "lifecycleState": "open", "currentArtifact": null},
        ]));
        let project = project_with_backend();
        // Payloads inlined by an older server are used as they came: no
        // per-bundle fetch, so the unreachable URL is never contacted.
        let remote = KnitRemote {
            url: "http://127.0.0.1:1".to_string(),
            token: None,
        };

        let (localized, dropped) =
            localized_export_bundles(&export, &project, &remote, "hosted", None).unwrap();

        assert_eq!(localized.len(), 1);
        assert_eq!(localized[0].id, "feature-a");
        assert_eq!(localized[0].synced_artifact_hash("hosted"), Some("hash-a"));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, "feature-c");
        assert_eq!(dropped[0].missing_repos, vec!["frontend".to_string()]);
    }

    #[test]
    fn clone_document_serializes_to_the_contract_shape() {
        let document = CloneDocument {
            project: CloneDocumentProject {
                id: "knit-tools".to_string(),
                owner: Some("marc-merino".to_string()),
                slug: "knit-tools".to_string(),
            },
            target_path: "/abs/path/to/knit-tools".to_string(),
            repos: vec![
                CloneDocumentRepo {
                    id: "backend".to_string(),
                    status: "cloned",
                    error: None,
                },
                CloneDocumentRepo {
                    id: "frontend".to_string(),
                    status: "failed",
                    error: Some("git clone failed".to_string()),
                },
            ],
            cloned_repo_count: 1,
            failed_repo_count: 1,
            omitted_repository_count: 1,
            scope_view: Some("backend".to_string()),
            repos_out_of_scope: vec!["docs".to_string()],
            bundles: CloneDocumentBundles {
                restored: vec!["feature-a".to_string()],
                dropped: vec![DroppedBundle {
                    id: "feature-c".to_string(),
                    missing_repos: vec!["frontend".to_string()],
                }],
                out_of_scope: vec![DroppedBundle {
                    id: "feature-d".to_string(),
                    missing_repos: vec!["docs".to_string()],
                }],
            },
            active_bundle: Some("feature-a".to_string()),
            worktrees_materialized: true,
        };

        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "project": {"id": "knit-tools", "owner": "marc-merino", "slug": "knit-tools"},
                "targetPath": "/abs/path/to/knit-tools",
                "repos": [
                    {"id": "backend", "status": "cloned"},
                    {"id": "frontend", "status": "failed", "error": "git clone failed"},
                ],
                "clonedRepoCount": 1,
                "failedRepoCount": 1,
                "omittedRepositoryCount": 1,
                "scopeView": "backend",
                "reposOutOfScope": ["docs"],
                "bundles": {
                    "restored": ["feature-a"],
                    "dropped": [{"id": "feature-c", "missingRepos": ["frontend"]}],
                    "outOfScope": [{"id": "feature-d", "missingRepos": ["docs"]}],
                },
                "activeBundle": "feature-a",
                "worktreesMaterialized": true,
            })
        );
    }

    #[test]
    fn format_repo_failures_lists_each_repo() {
        let failures = vec![
            ("backend".to_string(), "Repository not found".to_string()),
            ("frontend".to_string(), "permission denied".to_string()),
        ];
        let text = format_repo_failures(&failures);
        assert!(text.contains("backend: Repository not found"));
        assert!(text.contains("frontend: permission denied"));
    }
}
