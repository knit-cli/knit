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
use std::io;
use std::io::IsTerminal;
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
    credentials: &[String],
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
        credentials,
        json,
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
    credentials: &[String],
    json: bool,
) -> std::result::Result<CloneDocument, (RemoteErrorKind, anyhow::Error)> {
    // Validate the explicit credential selection before anything changes:
    // unknown names, ambiguous same-host pairs, and unusable tokens fail
    // before the export is fetched or the target directory is touched. With
    // no --credential, the private credential registry is not read at all.
    // The selection activates (exact targets, Drop guard) only inside
    // clone_fetched_export, after the scope is resolved.
    let credential_selection = crate::auth::validate_clone_credentials(credentials)
        .map_err(|error| (RemoteErrorKind::Other, error))?;
    let reference = parse_clone_reference(project_identifier, url)
        .map_err(|error| (RemoteErrorKind::NoRemote, error))?;
    let (remote_name, remote, stored_token, token) =
        resolve_remote_for_clone_classified(remote_name, reference.remote_url.as_deref(), token)?;
    let export = fetch_project_export(&remote, token.as_deref(), &reference.project_identifier)
        .map_err(|error| (RemoteErrorKind::Http, error))?;
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
        prefer_https,
        scope,
        &credential_selection,
        json,
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
    mut export: RemoteProjectExport,
    active_bundle: Option<&str>,
    materialize: bool,
    prefer_https: bool,
    scope: CloneScopeRequest<'_>,
    credential_selection: &[(String, String)],
    json: bool,
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
    let (mut scoped_repositories, repos_out_of_scope, repos_unavailable) =
        partition_export_repositories(&export, resolved_scope.as_ref());

    let target_root = resolve_clone_target(target, project_identifier)?;
    prepare_clone_target(&target_root, &scoped_repositories)?;

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

    // Portable auth requirements ride the exported knitProject. Validate them
    // against the export's full membership so a scoped clone keeps groups
    // referencing repos outside its scope.
    let auth_requirements = export
        .knit_project
        .as_ref()
        .and_then(|project| project.auth.clone())
        .filter(|auth| !auth.groups.is_empty());
    if let Some(auth) = &auth_requirements {
        let mut membership = export.knit_project.clone().unwrap_or_else(|| {
            let mut synthetic = membership_project_from_export(&export);
            synthetic.auth = None;
            synthetic
        });
        membership.auth = Some(auth.clone());
        crate::auth::validate_project_auth(&membership)
            .context("Remote project carries invalid auth requirements")?;
    }

    // Bootstrap the workspace before anything touches a forge: clones, the
    // `--prefer-https` probes, all of it. Credential resolution needs the
    // project context in place at the target, and a clone whose private repos
    // fail must leave a valid, recoverable workspace behind — project (with
    // auth), config carrying the chosen sync remote, and the pending map of
    // membership the selection left out.
    let bootstrap = bootstrap_clone_workspace(
        &target_root,
        &export,
        &scoped_repositories,
        &remote_name,
        &remote,
        stored_token.clone(),
        resolved_scope.as_ref(),
    )?;

    // The views artifact — including the `--repo` scope view — is saved
    // locally before any repository is fetched and before the grouped
    // prompt runs: a clone that fails entirely (every repo private, or a
    // canceled prompt) must leave the scope recorded, or the recovery pull
    // cannot resolve the workspace's scope and skips its retries. Pushing
    // the scope view to the remote still waits for a successful clone.
    let mut views = match remote_views {
        Some(remote_views) => super::pull::views_from_remote(&project_id, remote_views),
        None => KnitProjectViews::new(project_id.clone(), now_iso()),
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
    }

    if auth_requirements.is_some()
        && credential_selection.is_empty()
        && !json
        && io::stdin().is_terminal()
    {
        // Interactive grouped setup before the first private git fetch needs a
        // credential: one direct token prompt per group, reusing a unique
        // compatible saved credential without asking. An explicit
        // `--credential` selection is the automation override and skips the
        // prompt entirely. Groups are projected to the selected repos, so an
        // out-of-scope group never requests a token; the saved project keeps
        // the full mappings. A setup error fails the clone — with
        // requirements declared there is no implicit ambient-credential
        // fallback.
        crate::human!(
            "{} {}",
            out::heading("Setting up project credentials before cloning:"),
            out::path(target_root.display())
        );
        crate::commands::auth::clone_group_setup(&target_root, &bootstrap.setup_project())?;
    } else if let Some(auth) = &auth_requirements {
        let selection = if credential_selection.is_empty() {
            None
        } else {
            credential_selection
                .iter()
                .map(|(name, host)| format!("{name} ({host})"))
                .collect::<Vec<_>>()
                .join(", ")
                .into()
        };
        crate::human!(
            "{} this project defines {} credential group(s){}; private repositories need one before they can be cloned:",
            out::heading("Auth requirements:"),
            auth.groups.len(),
            selection.map(|s| format!(" overridden by the --credential selection: {s}")).unwrap_or_default()
        );
        for group in &auth.groups {
            crate::human!(
                "  {} ({} @ {}) for {} — token type(s): {}{}",
                out::repo(&group.id),
                group.provider,
                group.host,
                group.repos.join(", "),
                group.token_types.join(", "),
                group
                    .token_url
                    .as_deref()
                    .map(|url| format!(" — create at {url}"))
                    .unwrap_or_default()
            );
        }
        // Every group's host already has a default credential (or deliberate
        // links): the clone authenticates through it with no flags and no
        // setup step; the instructions below are only for what is missing.
        // Coverage is per group, not per distinct host — two groups sharing a
        // host are both covered by that host's default.
        let defaults_cover_groups = !auth.groups.is_empty()
            && crate::auth::load()
                .map(|store| {
                    let covers = |host: &str| {
                        store
                            .default_for_host(host)
                            .and_then(|(name, _)| {
                                store
                                    .credentials
                                    .get(&name)
                                    .map(|spec| spec.host.eq_ignore_ascii_case(host))
                            })
                            .unwrap_or(false)
                    };
                    auth.groups.iter().all(|group| covers(&group.host))
                })
                .unwrap_or(false);
        if defaults_cover_groups {
            let hosts: BTreeSet<&str> = auth
                .groups
                .iter()
                .map(|group| group.host.as_str())
                .collect();
            crate::human!(
                "{} host default credential(s) cover every group ({})",
                out::heading("Auth requirements:"),
                hosts.into_iter().collect::<Vec<_>>().join(", ")
            );
        } else {
            crate::human!(
                "{}",
                out::muted(format!(
                    "Run `knit auth setup` inside {} to link credentials interactively (or `knit auth add` + `knit auth use` noninteractively), then `knit pull --bundles` to clone any repository that failed for missing access.",
                    target_root.display()
                ))
            );
        }
    }

    // Repositories no declared group covers and nothing binds keep their
    // first-fetch ambient access: their exact remote is recorded as an
    // allowance so the workspace's strict assigned-credentials gate — active
    // once the guided setup bound the grouped repositories — cannot lock a
    // public HTTPS or SSH-working repository out of the access it already
    // has. Grouped repositories and deliberate bindings are never touched,
    // and nothing is recorded while the project has no bindings at all (the
    // gate is inert then, and no personal state is needed).
    record_ungrouped_ambient_access(
        &target_root,
        &bootstrap.project,
        &scoped_repositories,
        credential_selection,
        auth_requirements.as_ref(),
    );

    // Credential isolation starts before any Git runs — the guided setup's
    // reuses, the `--prefer-https` probes, and the clones themselves. An
    // explicit selection activates as exact (host, path) targets of the
    // scoped export repositories; every clone (selection or not) pins its
    // assignment fallbacks to the clone target root, so the surrounding
    // workspace's credentials are unreachable for the whole guarded section.
    // The guard restores the previous state on every result path, including
    // materialize fetches below. A target the selection does not cover keeps
    // the clone workspace's grouped assignments instead of being forced to
    // ambient credentials.
    let coverage = clone_credential_coverage(&scoped_repositories, credential_selection);
    let _credential_guard = if credential_selection.is_empty() {
        crate::auth::activate_clone_root(target_root.clone())
    } else {
        crate::auth::activate_clone_credentials_at(target_root.clone(), coverage.targets.clone())
    };
    // Whether every scoped forge repository will be cloned with a Knit-known
    // credential: the explicit selection's exact targets, or the guided
    // setup's grouped assignments. Such a clone never needs the hosted
    // connected-forge lookup (which restricted sync tokens cannot read) or
    // the hosted helper install.
    let selection_covers_all = !credential_selection.is_empty() && coverage.uncovered.is_empty();
    let locals_cover_all = !selection_covers_all
        && local_assignments_cover(&target_root, &bootstrap.project, &scoped_repositories);

    // `--prefer-https` probes run only after the bootstrap (and interactive
    // setup) so a private repository's probe resolves the credential the user
    // just linked, exactly like the clone that follows. Hosts the explicit
    // `--credential` selection covers are never probed — probing them would
    // authenticate with ambient credentials before the selection's own exact
    // rewrite applies — and a selection or grouped-assignment set covering
    // every scoped forge repository skips the hosted connected-forge lookup
    // entirely.
    let mut rewrote_urls = false;
    if prefer_https && !selection_covers_all && !locals_cover_all && token.is_some() {
        let hosts = super::helpers::connected_forge_hosts(&remote, token.as_deref().unwrap())
            .unwrap_or_default();
        for repository in &mut export.repositories {
            if !prefer_https_probes_repository(repository, credential_selection) {
                continue;
            }
            if let Some(url) = repository.remote_url.clone() {
                if let Some(https) = super::handoff::prefer_https_url(&url, &hosts) {
                    if super::handoff::reachable(&target_root, &url, &remote_name, &hosts).is_err()
                        && super::handoff::reachable(&target_root, &https, &remote_name, &hosts)
                            .is_ok()
                    {
                        repository.remote_url = Some(https);
                        rewrote_urls = true;
                    }
                }
            }
        }
    }
    if rewrote_urls {
        // The scoped selection was cloned from the export before the rewrite;
        // everything downstream — the clones, the persisted project entries,
        // the pending map — must carry the rewritten URLs, not the SSH forms
        // the probe replaced.
        refresh_scoped_repository_urls(&mut scoped_repositories, &export);
        let mut refreshed = bootstrap.project.clone();
        for entry in &mut refreshed.repos {
            if let Some(selected) = scoped_repositories
                .iter()
                .find(|repository| export_repo_local_id(repository) == entry.id)
            {
                entry.remote = selected.remote_url.clone();
            }
        }
        write_json(&project_path(&target_root, &refreshed.id), &refreshed)?;
        refresh_clone_pending(&target_root, &export, &refreshed)?;
    }

    if selection_covers_all || locals_cover_all {
        crate::human!(
            "{} {}",
            out::heading("Credential helper:"),
            out::muted(if selection_covers_all {
                "skipped; the selected credential(s) cover every forge repository"
            } else {
                "skipped; linked project credentials cover every forge repository"
            })
        );
    } else {
        super::helpers::ensure_helpers_for_git(&remote_name);
    }
    if !credential_selection.is_empty() {
        report_clone_credential_coverage(credential_selection, &coverage);
    }
    let (mut repo_paths, mut failed_repos) =
        clone_export_repositories_collecting(&target_root, &scoped_repositories);
    // Guided fallback for authentication-shaped failures, in a terminal: raw
    // Git credential prompts are disabled for Knit's clone children, so a
    // repository without working access fails fast instead of hanging. The
    // failure enters setup right here and the affected repositories are
    // retried once. Declared groups get the same repair path pull recovery
    // uses (a rejected link is rotated, a missing link is created — never
    // overwriting deliberate bindings); repositories no group covers get one
    // inferred group per host. SSH/helper access that already worked, and
    // public repositories, are never touched. An explicit `--credential`
    // selection is an automation override — its failures name the selected
    // credential and are never second-guessed — and noninteractive clones
    // keep the actionable recoverable-workspace error below.
    if credential_selection.is_empty() && !json && io::stdin().is_terminal() {
        let failing: Vec<(String, String)> = failed_repos
            .iter()
            .filter(|(_, error)| is_auth_shaped_failure(error))
            .filter_map(|(repo_id, _)| {
                scoped_repositories
                    .iter()
                    .find(|repository| export_repo_local_id(repository) == *repo_id)
                    .and_then(|repository| repository.remote_url.clone())
                    .map(|remote| (repo_id.clone(), remote))
            })
            .collect();
        if !failing.is_empty() {
            crate::human!(
                "{} {} repo(s) need a forge token (no working access); setting that up now:",
                out::heading("Private repositories:"),
                failing.len()
            );
            let failing_ids: BTreeSet<&str> = failing.iter().map(|(id, _)| id.as_str()).collect();
            let declared_covered: BTreeSet<String> = auth_requirements
                .iter()
                .flat_map(|auth| auth.groups.iter().flat_map(|g| g.repos.clone()))
                .filter(|repo| failing_ids.contains(repo.as_str()))
                .collect();
            let mut linked: Vec<String> = Vec::new();
            if !declared_covered.is_empty() {
                // The declared groups projected onto the failing repositories,
                // repaired through the same setup pull recovery uses.
                let mut setup_project = bootstrap.project.clone();
                if let Some(auth) = setup_project.auth.as_mut() {
                    for group in &mut auth.groups {
                        group
                            .repos
                            .retain(|repo| failing_ids.contains(repo.as_str()));
                    }
                    auth.groups.retain(|group| !group.repos.is_empty());
                }
                let repair: BTreeSet<String> =
                    failing.iter().map(|(repo, _)| repo.clone()).collect();
                crate::commands::auth::recovery_group_setup(
                    &target_root,
                    &setup_project,
                    &repair,
                )
                .with_context(|| {
                    format!(
                        "guided credential repair failed; a recoverable workspace was left at {} — fix credentials there, then run `knit pull --bundles`",
                        target_root.display()
                    )
                })?;
                // A repair creates a fresh local credential for the rejected
                // repositories, or a missing group's first token becomes the
                // host's default with no per-repository links: retry
                // everything that has a binding or a host default now.
                let key = crate::auth::project_key(&target_root, &bootstrap.project.id)
                    .with_context(|| {
                        format!(
                            "guided credential repair failed; a recoverable workspace was left at {} — fix credentials there, then run `knit pull --bundles`",
                            target_root.display()
                        )
                    })?;
                let store = crate::auth::load()?;
                linked.extend(
                    store
                        .projects
                        .get(&key)
                        .into_iter()
                        .flat_map(|bindings| bindings.keys().cloned())
                        .filter(|repo| failing_ids.contains(repo.as_str())),
                );
                linked.extend(
                    failing
                        .iter()
                        .filter(|(_, remote)| {
                            crate::auth::remote_target(remote)
                                .ok()
                                .and_then(|(host, _)| store.default_for_host(&host).map(|_| ()))
                                .is_some()
                        })
                        .map(|(repo, _)| repo.clone()),
                );
            }
            let uncovered: Vec<(String, String)> = failing
                .iter()
                .filter(|(repo, _)| !declared_covered.contains(repo))
                .cloned()
                .collect();
            if !uncovered.is_empty() {
                linked.extend(crate::commands::auth::inferred_fallback_setup(
                    &target_root,
                    &bootstrap.project,
                    &uncovered,
                )
                .with_context(|| {
                    format!(
                        "guided credential setup failed; a recoverable workspace was left at {} — fix credentials there, then run `knit pull --bundles`",
                        target_root.display()
                    )
                })?);
            }
            let retry: Vec<RemoteExportRepository> = scoped_repositories
                .iter()
                .filter(|repository| linked.contains(&export_repo_local_id(repository)))
                .cloned()
                .collect();
            if !retry.is_empty() {
                let (retried_paths, retried_failures) =
                    clone_export_repositories_collecting(&target_root, &retry);
                let recovered_ids: Vec<String> = retried_paths.keys().cloned().collect();
                let retried_ids: BTreeSet<String> =
                    retry.iter().map(export_repo_local_id).collect();
                failed_repos.retain(|(repo_id, _)| !retried_ids.contains(repo_id));
                failed_repos.extend(retried_failures);
                repo_paths.extend(retried_paths);
                if !recovered_ids.is_empty() {
                    crate::human!(
                        "{} {} repo(s)",
                        out::heading("Recovered after setup:"),
                        recovered_ids.join(", ")
                    );
                }
            }
        }
    }
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
            "Failed to clone any repository for project `{}`:\n{}\n\n\
             A recoverable workspace was created at {} with the project, its auth requirements, and the {} sync remote. \
             Run `knit auth setup` there to link credentials (re-entering `knit clone` into an existing workspace is refused), then `knit pull --bundles` to clone the remaining repositories.",
            export.project.slug,
            format_repo_failures(&failed_repos),
            target_root.display(),
            remote_name
        );
    }
    let project =
        local_project_from_export(&export, &scoped_repositories, &repo_paths, &target_root)?;
    write_json(&project_path(&target_root, &project.id), &project)?;
    refresh_clone_pending(&target_root, &export, &project)?;
    // Record which personal credential cloned each repository so later Knit
    // operations in the new workspace use it without setup. Personal state
    // only: nothing is written to project artifacts or sync remotes. A
    // repository the grouped setup already linked keeps that mapping — the
    // explicit selection never overwrites grouped assignments.
    if !credential_selection.is_empty() {
        persist_clone_credential_assignments(
            &target_root,
            &project,
            &repo_paths,
            credential_selection,
        );
    }
    // Repositories that cloned with working ambient access (public HTTPS, or
    // an SSH key) keep it — recorded after the explicit assignments above,
    // so a partially selected clone (the selection covers one host, public
    // repositories sit on another) leaves the public repositories allowed
    // through the now-active strict gate. Bound repositories — grouped or
    // explicitly assigned — are never overwritten. Best effort: a recording
    // failure warns and never fails a finished clone.
    record_ambient_clone_access(&target_root, &project, &repo_paths, credential_selection);

    // Bundles localize against repos with real checkouts only: the project
    // now keeps entries for in-scope repos whose clone failed (they are the
    // recovery contract), but a bundle touching one still cannot be restored
    // here and is reported as dropped instead.
    let mut localization_project = project.clone();
    localization_project
        .repos
        .retain(|repo| repo_paths.contains_key(&repo.id));
    let (bundles, dropped_bundles) = localized_export_bundles(
        &export,
        &localization_project,
        &remote,
        &remote_name,
        token.as_deref(),
    )?;
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

    // The views artifact itself was saved before any repository was fetched
    // (see the early block); a successful clone only reports it.
    if !views.views.is_empty() || views.default_view.is_some() {
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

/// The workspace state written into the clone target before any repository is
/// fetched: the project artifact (full auth groups, entries for every selected
/// repo) and a config carrying the chosen sync remote, so credential
/// resolution has a root, a failed private clone is recoverable in place, and
/// `knit pull` can reach the remote afterwards.
struct CloneBootstrap {
    project: KnitProject,
    /// Repo ids the selection clones (groups are projected onto these for the
    /// interactive prompt).
    selected_ids: BTreeSet<String>,
}

impl CloneBootstrap {
    /// The project handed to grouped setup: same id and repos, but with each
    /// group's repositories limited to the selection — the prompt never
    /// requests a credential for an out-of-scope group or repo. The saved
    /// project keeps the full group mappings untouched.
    fn setup_project(&self) -> KnitProject {
        let mut setup = self.project.clone();
        if let Some(auth) = setup.auth.as_mut() {
            for group in &mut auth.groups {
                group.repos.retain(|repo| self.selected_ids.contains(repo));
            }
            auth.groups.retain(|group| !group.repos.is_empty());
        }
        setup
    }
}

/// Write the bootstrap project, config, and pending known-repos map into the
/// clone target. In-scope repos are project entries from the start — failed
/// clone and all — because the entry is what grouped setup maps a credential
/// onto and what pull retries; the pending map records only the membership a
/// scope deliberately left out. Both files are overwritten with finished
/// versions when the clone succeeds, and remain valid as-is when it does not.
fn bootstrap_clone_workspace(
    target_root: &Path,
    export: &RemoteProjectExport,
    scoped_repositories: &[RemoteExportRepository],
    remote_name: &str,
    remote: &KnitRemote,
    stored_token: Option<String>,
    scope: Option<&ResolvedCloneScope>,
) -> Result<CloneBootstrap> {
    let mut project = export
        .knit_project
        .clone()
        .unwrap_or_else(|| KnitProject::new(export_project_id(export), now_iso()));
    project.id = slugify(&project.id);
    let selected_ids: BTreeSet<String> = scoped_repositories
        .iter()
        .map(export_repo_local_id)
        .collect();
    project.repos = selected_ids
        .iter()
        .map(|local_id| {
            let repository = scoped_repositories
                .iter()
                .find(|repository| &export_repo_local_id(repository) == local_id)
                .expect("selected ids come from the scoped repositories");
            project_repo_entry_from_export(repository, &target_root.join(local_id))
        })
        .collect();
    project.updated_at = now_iso();
    write_json(&project_path(target_root, &project.id), &project)?;

    let mut remotes = BTreeMap::new();
    remotes.insert(
        remote_name.to_string(),
        KnitRemote {
            url: remote.url.clone(),
            token: stored_token,
        },
    );
    let config = KnitConfig {
        schema_version: SCHEMA_VERSION.to_string(),
        active_bundle: None,
        active_project: Some(project.id.clone()),
        sync_remote: Some(remote_name.to_string()),
        sync_remotes: vec![remote_name.to_string()],
        advice: true,
        stealth: None,
        auto_tag: None,
        push_sync: true,
        scope_view: scope.map(|scope| scope.view_name.clone()),
        remotes,
    };
    crate::store::save_config(target_root, &config)?;

    let pending = clone_pending_repos(export, &project);
    crate::auth::save_known_pending_repos(target_root, &project.id, &pending)?;
    Ok(CloneBootstrap {
        project,
        selected_ids,
    })
}

/// Recompute and persist the pending known-repos map against the project as
/// it now stands (after clones succeeded or failed, and after any URL
/// rewrite `--prefer-https` made), so it names exactly the membership this
/// workspace does not carry locally.
fn refresh_clone_pending(
    target_root: &Path,
    export: &RemoteProjectExport,
    project: &KnitProject,
) -> Result<()> {
    let pending = clone_pending_repos(export, project);
    crate::auth::save_known_pending_repos(target_root, &project.id, &pending)
}

/// Carry `--prefer-https` rewrites from the export's repository records onto
/// the scoped selection cloned from them, so the clones and every persisted
/// artifact (project entries, pending map) use the URL the probe chose.
fn refresh_scoped_repository_urls(
    scoped: &mut [RemoteExportRepository],
    export: &RemoteProjectExport,
) {
    for selected in scoped.iter_mut() {
        let Some(rewritten) = export
            .repositories
            .iter()
            .find(|repository| export_repo_local_id(repository) == export_repo_local_id(selected))
            .and_then(|repository| repository.remote_url.clone())
        else {
            continue;
        };
        selected.remote_url = Some(rewritten);
    }
}

/// Repo id → remote URL for the export's membership that no local project
/// entry covers. Inventory records carry the clone URL; membership entries
/// are the fallback for repos the export inventory did not include.
fn clone_pending_repos(
    export: &RemoteProjectExport,
    project: &KnitProject,
) -> BTreeMap<String, String> {
    let local: BTreeSet<&str> = project.repos.iter().map(|repo| repo.id.as_str()).collect();
    let mut pending = BTreeMap::new();
    for repository in &export.repositories {
        let local_id = export_repo_local_id(repository);
        if local.contains(local_id.as_str()) {
            continue;
        }
        if let Some(url) = repository
            .remote_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        {
            pending.insert(local_id, url.to_string());
        }
    }
    if let Some(membership) = export.knit_project.as_ref() {
        for entry in &membership.repos {
            if local.contains(entry.id.as_str()) || pending.contains_key(&entry.id) {
                continue;
            }
            if let Some(url) = entry.remote.as_deref().filter(|url| !url.trim().is_empty()) {
                pending.insert(entry.id.clone(), url.to_string());
            }
        }
    }
    pending
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

/// Validate the clone target, resuming an interrupted earlier clone when the
/// directory holds exactly that shape: empty `.knit` scaffolding (`projects`,
/// `bundles`, `worktrees`, no config, no project artifacts — an older Knit
/// that persisted its workspace only after collecting repositories) plus any
/// number of legitimate git checkouts of this export's repositories. A
/// checkout is legitimate only when the directory name is the repository's
/// own export id, it is a real git worktree *root* (not an ordinary folder
/// that merely sits inside some parent repository), and its origin matches
/// exactly that repository's remote (SSH and HTTPS forms are equivalent).
/// Adopted checkouts keep their working tree, branch, and dirty state
/// untouched. Symlinks — the target itself, `.knit`, its scaffolding, and
/// every adopted checkout — are refused, so a resume can never write through
/// them into somewhere else. Anything else — unrelated files, a checkout of
/// some other repository, an already-configured workspace — is refused
/// before anything is written.
fn prepare_clone_target(target: &Path, repositories: &[RemoteExportRepository]) -> Result<()> {
    if !target.exists() {
        fs::create_dir_all(target)
            .with_context(|| format!("failed to create clone target {}", target.display()))?;
        return Ok(());
    }
    if fs::symlink_metadata(target)
        .with_context(|| format!("failed to inspect clone target {}", target.display()))?
        .file_type()
        .is_symlink()
    {
        bail!(
            "Clone target {} is a symbolic link; refusing to write through it.",
            target.display()
        );
    }
    if target.join(".knit/config.json").exists() {
        bail!(
            "{} is already a Knit workspace; run `knit pull --bundles` inside it to recover missing repositories instead of cloning into it again.",
            target.display()
        );
    }

    let mut checkouts: Vec<(String, PathBuf)> = Vec::new();
    let mut foreign: Vec<String> = Vec::new();
    let mut entries = fs::read_dir(target)
        .with_context(|| format!("failed to read clone target {}", target.display()))?;
    while let Some(entry) = entries.next().transpose()? {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".knit" {
            if !path.is_dir() || fs::symlink_metadata(&path)?.file_type().is_symlink() {
                foreign.push(name);
            }
            continue;
        }
        if is_git_worktree(&path) {
            checkouts.push((name, path));
        } else {
            foreign.push(name);
        }
    }
    drop(entries);
    if !foreign.is_empty() {
        bail!(
            "Clone target {} holds unrelated entries ({}); move them aside or choose a different target.",
            target.display(),
            foreign.join(", ")
        );
    }
    // The `.knit` directory may only be empty scaffolding from the same
    // interrupted shape: its known subdirectories — real directories, not
    // symlinks — and nothing else. A project artifact, an unexplained entry,
    // or a symlink that would redirect writes elsewhere means a real (or
    // foreign) workspace.
    let knit = target.join(".knit");
    if knit.exists() {
        let mut knit_entries =
            fs::read_dir(&knit).with_context(|| format!("failed to read {}", knit.display()))?;
        let mut knit_foreign: Vec<String> = Vec::new();
        while let Some(entry) = knit_entries.next().transpose()? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let allowed_empty_dir = ["projects", "bundles", "worktrees"].contains(&name.as_str())
                && fs::symlink_metadata(&path)
                    .map(|meta| meta.file_type().is_dir())
                    .unwrap_or(false)
                && fs::read_dir(&path)?.next().is_none();
            if !allowed_empty_dir {
                knit_foreign.push(name);
            }
        }
        drop(knit_entries);
        if !knit_foreign.is_empty() {
            bail!(
                "Clone target {} holds Knit state ({}) that is not an interrupted clone; refusing to overwrite it.",
                target.display(),
                knit_foreign.join(", ")
            );
        }
    }
    for (name, checkout) in &checkouts {
        if fs::symlink_metadata(checkout)?.file_type().is_symlink() {
            bail!(
                "{} is a symbolic link; refusing to adopt it as a checkout.",
                checkout.display()
            );
        }
        // The directory name must be one of this export's repository ids, and
        // the origin must belong to exactly that repository — not merely to
        // some repository in the export.
        let Some(repository) = repositories
            .iter()
            .find(|repository| export_repo_local_id(repository) == *name)
        else {
            bail!(
                "{} holds `{name}`, which is not one of this project's repositories; refusing to resume the clone there.",
                checkout.display()
            );
        };
        // A real checkout *root*: an ordinary folder inside some parent Git
        // repository answers `--is-inside-work-tree` too, but its top level
        // is the parent's, and adopting it would clobber foreign work.
        let top = git_output(checkout, ["rev-parse", "--show-toplevel"]).with_context(|| {
            format!("failed to inspect existing checkout {}", checkout.display())
        })?;
        let top = fs::canonicalize(top.trim())
            .with_context(|| format!("failed to resolve {}", checkout.display()))?;
        let here = fs::canonicalize(checkout)
            .with_context(|| format!("failed to resolve {}", checkout.display()))?;
        if top != here {
            bail!(
                "{} is inside another Git repository ({}), not a checkout of its own; refusing to resume the clone there.",
                checkout.display(),
                top.display()
            );
        }
        let origin = git_output(checkout, ["remote", "get-url", "origin"]).with_context(|| {
            format!("failed to inspect existing checkout {}", checkout.display())
        })?;
        let origin = origin.trim();
        let matches_this_repo = repository
            .remote_url
            .as_deref()
            .is_some_and(|url| super::handoff::same_repository_url(origin, url));
        if !matches_this_repo {
            let expected = repository.remote_url.as_deref().unwrap_or_default();
            bail!(
                "{} holds a checkout of `{origin}`, but repository `{name}` lives at `{expected}`; refusing to resume the clone there.",
                checkout.display()
            );
        }
    }
    if !checkouts.is_empty() {
        crate::human!(
            "{} {} existing checkout(s) of this project; resuming the interrupted clone in {} (existing work is kept as-is)",
            out::heading("Resuming:"),
            checkouts.len(),
            out::path(target.display())
        );
    }
    Ok(())
}

/// How the explicit clone credential selection relates to the repositories
/// about to be cloned: repo ids on hosts a selected credential covers, the
/// uncovered hosts with the repo ids left to ambient Git authentication, and
/// the exact (host, path) targets the selection will authenticate.
#[derive(Default)]
struct CloneCredentialCoverage {
    covered: BTreeMap<String, Vec<String>>,
    uncovered: BTreeMap<String, Vec<String>>,
    targets: Vec<crate::auth::CloneCredentialTarget>,
}

/// Split the repositories' forge remotes by whether the clone's explicit
/// credential selection covers their host, and collect the exact
/// (host, path) targets the covered repositories contribute. Local remotes
/// and repos without a clone URL need no credential; unparseable URLs are
/// skipped (their clone fails on its own, so hosted helper setup is not
/// decided by them).
fn clone_credential_coverage(
    repositories: &[RemoteExportRepository],
    selection: &[(String, String)],
) -> CloneCredentialCoverage {
    let mut coverage = CloneCredentialCoverage::default();
    for repository in repositories {
        let Some(url) = repository
            .remote_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        else {
            continue;
        };
        if crate::auth::is_local_remote(url) {
            continue;
        }
        let Ok((host, path)) = crate::auth::remote_target(url) else {
            continue;
        };
        let Some((name, _)) = selection
            .iter()
            .find(|(_, selected)| selected.eq_ignore_ascii_case(&host))
        else {
            coverage
                .uncovered
                .entry(host)
                .or_default()
                .push(export_repo_local_id(repository));
            continue;
        };
        coverage
            .covered
            .entry(host.clone())
            .or_default()
            .push(export_repo_local_id(repository));
        coverage.targets.push(crate::auth::CloneCredentialTarget {
            name: name.clone(),
            host,
            path,
        });
    }
    coverage
}

/// Whether the hosted prefer-HTTPS reachability probe may touch this
/// repository: it must carry a parseable forge remote and sit on a host the
/// explicit selection does not cover — covered repositories authenticate
/// through the selection's own exact HTTPS rewrite once it activates, and
/// probing them first would use ambient credentials. Out-of-scope
/// repositories stay probeable: their pending-map entries are tomorrow's
/// credential-backed recovery clones, so they should carry the rewritten
/// HTTPS form too.
fn prefer_https_probes_repository(
    repository: &RemoteExportRepository,
    selection: &[(String, String)],
) -> bool {
    repository
        .remote_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .is_some_and(|url| {
            !crate::auth::is_local_remote(url)
                && crate::auth::remote_target(url).is_ok_and(|(host, _)| {
                    !selection
                        .iter()
                        .any(|(_, selected)| selected.eq_ignore_ascii_case(&host))
                })
        })
}

/// Tell the user exactly what the explicit selection does and does not cover.
fn report_clone_credential_coverage(
    selection: &[(String, String)],
    coverage: &CloneCredentialCoverage,
) {
    for (name, host) in selection {
        let count = coverage.covered.get(host).map(Vec::len).unwrap_or(0);
        crate::human!(
            "{} {} {}",
            out::heading("Credential:"),
            out::repo(name),
            out::muted(format!(
                "({host}) authenticates {count} repo(s) for this clone"
            ))
        );
    }
    for (host, repos) in &coverage.uncovered {
        crate::human!(
            "{} {} {}",
            out::warn("No selected credential for"),
            out::repo(host),
            out::muted(format!(
                "({}); {} existing Git credentials",
                repos.join(", "),
                if repos.len() == 1 {
                    "it uses"
                } else {
                    "they use"
                }
            ))
        );
    }
}

/// Whether every scoped forge repository will be cloned with a Knit-known
/// credential or a recorded ambient allowance: the explicit selection's
/// exact targets, the guided setup's grouped assignments, or the exact-repo
/// ambient allowances recorded for repositories no group covers. Such a
/// clone never needs the hosted connected-forge lookup (which restricted
/// sync tokens cannot read) or the hosted helper install. Repositories
/// without a forge remote need nothing.
fn local_assignments_cover(
    target_root: &Path,
    project: &KnitProject,
    repositories: &[RemoteExportRepository],
) -> bool {
    let Ok(store) = crate::auth::load() else {
        return false;
    };
    // Bindings may not exist at all — a defaults-only workspace (the
    // one-token-per-forge flow) can still cover everything, and must not be
    // sent to the hosted helper lookup.
    let Ok(key) = crate::auth::project_key(target_root, &project.id) else {
        return false;
    };
    let bindings = store.projects.get(&key);
    let ambient = store.ambient.get(&key);
    let mut any = false;
    for repository in repositories {
        let Some(url) = repository
            .remote_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        else {
            continue;
        };
        if crate::auth::is_local_remote(url) {
            continue;
        }
        let Ok((host, path)) = crate::auth::remote_target(url) else {
            continue;
        };
        any = true;
        let bound = bindings
            .and_then(|map| map.get(&export_repo_local_id(repository)))
            .and_then(|name| store.credentials.get(name))
            .is_some_and(|spec| spec.host.eq_ignore_ascii_case(&host));
        let ambient_ok = ambient
            .and_then(|map| map.get(&export_repo_local_id(repository)))
            .is_some_and(|recorded| *recorded == format!("{host}/{path}"));
        // The host's default credential covers repositories with neither a
        // binding nor an ambient allowance.
        let default_ok = store.default_for_host(&host).is_some();
        if !bound && !ambient_ok && !default_ok {
            return false;
        }
    }
    any
}

/// Record the exact-remote ambient allowance for every scoped repository no
/// declared group covers and nothing binds: they keep the public HTTPS or
/// SSH access they clone with, instead of being locked out by the strict
/// assigned-credentials gate once the grouped repositories get bindings.
/// Hosts the explicit selection covers are skipped (the selection
/// authenticates them). No personal state is written while the project has
/// no bindings at all — the gate is inert then. Best effort: failures warn.
fn record_ungrouped_ambient_access(
    target_root: &Path,
    project: &KnitProject,
    repositories: &[RemoteExportRepository],
    selection: &[(String, String)],
    auth: Option<&crate::model::ProjectAuth>,
) {
    let result = (|| -> Result<()> {
        let store = crate::auth::load()?;
        let key = crate::auth::project_key(target_root, &project.id)?;
        let Some(bindings) = store.projects.get(&key).filter(|b| !b.is_empty()) else {
            return Ok(());
        };
        for repository in repositories {
            let local_id = export_repo_local_id(repository);
            let Some(url) = repository
                .remote_url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
            else {
                continue;
            };
            if crate::auth::is_local_remote(url) {
                continue;
            }
            if bindings.contains_key(&local_id) {
                continue;
            }
            if auth.is_some_and(|auth| {
                auth.groups
                    .iter()
                    .any(|group| group.repos.contains(&local_id))
            }) {
                continue;
            }
            if let Ok((host, _)) = crate::auth::remote_target(url) {
                if selection
                    .iter()
                    .any(|(_, selected)| selected.eq_ignore_ascii_case(&host))
                {
                    continue;
                }
                // A host default serves this repository on later fetches;
                // ambient access is not what it will use.
                if store.default_for_host(&host).is_some() {
                    continue;
                }
            }
            crate::auth::record_ambient_access(target_root, &project.id, &local_id, url)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        crate::human!(
            "{} {error:#}; repositories without group coverage may need `knit auth setup` before their next fetch",
            out::warn("ambient access not recorded:")
        );
    }
}

/// Record ambient (credential-less) access for every successfully cloned
/// repository that neither carries an assignment nor was covered by the
/// explicit selection: public HTTPS and SSH-key clones stay working once the
/// project gains assignments. Best effort — failures warn, never fail.
fn record_ambient_clone_access(
    target_root: &Path,
    project: &KnitProject,
    repo_paths: &BTreeMap<String, PathBuf>,
    selection: &[(String, String)],
) {
    let result = (|| -> Result<()> {
        let store = crate::auth::load()?;
        let key = crate::auth::project_key(target_root, &project.id)?;
        let bindings = store.projects.get(&key);
        // The strict gate only exists once the project has assignments; with
        // none, ambient repositories resolve fine untouched and no personal
        // state needs to be written at all.
        if bindings.is_none_or(|bindings| bindings.is_empty()) {
            return Ok(());
        }
        for repo in &project.repos {
            let Some(remote) = repo.remote.as_deref() else {
                continue;
            };
            if !repo_paths.contains_key(&repo.id) {
                continue;
            }
            if bindings.is_some_and(|bindings| bindings.contains_key(&repo.id)) {
                continue;
            }
            if let Ok((host, _)) = crate::auth::remote_target(remote) {
                if selection
                    .iter()
                    .any(|(_, selected)| selected.eq_ignore_ascii_case(&host))
                {
                    continue;
                }
                // A host default serves this repository on later fetches;
                // ambient access is not what it will use.
                if store.default_for_host(&host).is_some() {
                    continue;
                }
            }
            crate::auth::record_ambient_access(target_root, &project.id, &repo.id, remote)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        crate::human!(
            "{} {error:#}; ambient-access repositories may need `knit auth setup` before their next fetch",
            out::warn("ambient access not recorded:")
        );
    }
}

/// After a successful clone, bind each cloned repository to the selected
/// credential covering its host in the user's personal assignment store.
/// Only repositories actually cloned are bound; failures warn without
/// failing the finished clone. Never touches project artifacts or remotes.
/// A repository the grouped setup already linked keeps its mapping: the
/// explicit selection must not overwrite grouped assignments.
fn persist_clone_credential_assignments(
    target_root: &Path,
    project: &KnitProject,
    repo_paths: &BTreeMap<String, PathBuf>,
    selection: &[(String, String)],
) {
    let result = (|| -> Result<Vec<String>> {
        // repo id -> credential name, for the repositories this clone created.
        let mut assigned: Vec<(String, String)> = Vec::new();
        for repo in &project.repos {
            let Some(remote) = repo.remote.as_deref() else {
                continue;
            };
            if !repo_paths.contains_key(&repo.id) {
                continue;
            }
            let Ok((host, _)) = crate::auth::remote_target(remote) else {
                continue;
            };
            if let Some((name, _)) = selection
                .iter()
                .find(|(_, selected)| selected.eq_ignore_ascii_case(&host))
            {
                assigned.push((repo.id.clone(), name.clone()));
            }
        }
        if assigned.is_empty() {
            return Ok(Vec::new());
        }
        let key = crate::auth::project_key(target_root, &project.id)?;
        let _lock = crate::auth::lock()?;
        let mut store = crate::auth::load()?;
        let bindings = store.projects.entry(key).or_default();
        for (repo_id, name) in &assigned {
            if bindings.contains_key(repo_id) {
                // Grouped setup already mapped this repository deliberately.
                continue;
            }
            bindings.insert(repo_id.clone(), name.clone());
        }
        crate::auth::save(&store)?;
        Ok(assigned
            .into_iter()
            .map(|(repo_id, name)| format!("{} → {}", out::repo(&repo_id), name))
            .collect())
    })();
    match result {
        Ok(assigned) if !assigned.is_empty() => crate::human!(
            "{} {}",
            out::heading("Assigned credential:"),
            out::muted(assigned.join(", "))
        ),
        Ok(_) => {}
        Err(error) => crate::human!(
            "{} {error:#}; run `knit auth use <credential> --project {} --repo <repo>` later",
            out::warn("credential assignments not saved:"),
            project.id
        ),
    }
}

/// How to update a selected credential, matched to how it stores its token:
/// an environment-backed credential is updated through its named variable
/// (never the value), a file-backed one through the complete `knit auth add`
/// replacement command. Name, provider, and host are charset-validated at
/// save time; only the username (an email address) can carry shell-sensitive
/// characters, so it is POSIX-quoted.
fn credential_update_advice(name: &str, spec: &crate::auth::CredentialSpec) -> String {
    if let Some(variable) = &spec.token_env {
        return format!("update its token in the `{variable}` environment variable");
    }
    format!(
        "update it with `knit auth add {name} --provider {} --host {} --replace{}`, which prompts for the new token",
        spec.provider,
        spec.host,
        spec.username
            .as_deref()
            .map(|username| format!(" --username {}", super::helpers::shell_quote(username)))
            .unwrap_or_default()
    )
}

/// Whether a Git failure looks like missing credentials rather than a
/// transport problem: denied authentication, a disabled terminal prompt
/// (Knit disables raw prompts for its children, so this is how a private
/// repository without working access surfaces), an HTTP 401/403, or a
/// bound credential whose token is unavailable locally — the exact
/// `auth::credential` error strings (an unset environment reference, a
/// secret file without the entry), never a broad network catch.
pub(super) fn is_auth_shaped_failure(failure: &str) -> bool {
    let failure = failure.to_ascii_lowercase();
    [
        "authentication failed",
        "access denied",
        "could not read username",
        "terminal prompts disabled",
        "invalid username or password",
        "http 401",
        "http 403",
        "returned error: 401",
        "returned error: 403",
        "requires environment variable",
        "has no saved token",
    ]
    .iter()
    .any(|marker| failure.contains(marker))
}

/// When the active clone selection covers this exact repository, the failure
/// names that credential so the user knows which token was in play. Only an
/// authentication-shaped failure says access was denied; a transport error
/// (DNS, timeout, TLS) must not be reported as a token rejection. The update
/// advice follows the credential's own token storage, or points at the help
/// text instead of inventing a command.
fn selected_credential_failure_hint(remote_url: &str, failure: &str) -> Option<String> {
    let (host, path) = crate::auth::remote_target(remote_url).ok()?;
    let name = crate::auth::clone_credential_for(&host, &path)?;
    let denied = is_auth_shaped_failure(failure);
    let update = match crate::auth::load()
        .ok()
        .and_then(|registry| registry.credentials.get(&name).cloned())
    {
        Some(spec) => credential_update_advice(&name, &spec),
        None => "update it as described in `knit auth add --help`".to_string(),
    };
    Some(if denied {
        format!(
            "the selected credential `{name}` was used and access was denied; check that its token can read this repository, or {update}"
        )
    } else {
        format!(
            "this clone used the selected credential `{name}`; if the failure is an access denial, {update}"
        )
    })
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
        // problem, a repository covered by the explicit selection names that
        // credential (distinguishing an access denial from a transport
        // failure), and only the genuinely ambiguous case earns the access
        // hint.
        let failure_text = format!("{error:#}");
        let error = if export_repo_forge_missing(repository) {
            anyhow::anyhow!(
                "{error:#}; the sync remote marked this repository missing on its forge — it does not exist (or was deleted/renamed)"
            )
        } else if let Some(hint) = selected_credential_failure_hint(remote_url, &failure_text) {
            anyhow::anyhow!("{error:#}; {hint}")
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
    repositories: &[RemoteExportRepository],
    repo_paths: &BTreeMap<String, PathBuf>,
    target_root: &Path,
) -> Result<KnitProject> {
    let mut project = export
        .knit_project
        .clone()
        .unwrap_or_else(|| KnitProject::new(export.project.slug.clone(), now_iso()));
    project.id = slugify(&project.id);
    project.repos.clear();

    for repository in repositories {
        let local_id = export_repo_local_id(repository);
        // Repos whose clone failed keep their entry at the projected path
        // (absent on disk): the entry is what grouped setup maps a credential
        // onto and what a later pull retries. Dropping it would leave a
        // failed private clone with no in-project handle, and no recovery.
        let repo_path = repo_paths
            .get(&local_id)
            .cloned()
            .unwrap_or_else(|| target_root.join(&local_id));
        project
            .repos
            .push(project_repo_entry_from_export(repository, &repo_path));
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
        // Archived/closed bundles remain available as history, but their
        // feature branches may have been deleted. A project with no open
        // bundle clones successfully without materializing one.
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
    fn missing_feature_branch_is_not_reported_as_an_authentication_failure() {
        // Status-like digits in a checkout path or branch are not an HTTP
        // authentication response.
        assert!(!is_auth_shaped_failure(
            "git fetch failed in /tmp/run-40393: couldn't find remote ref knit/task-401"
        ));
        assert!(is_auth_shaped_failure(
            "fatal: unable to access remote: The requested URL returned error: 403"
        ));
        let root = temp_dir("missing-feature");
        let source = root.join("source");
        init_source_repo(&source);
        let checkout = root.join("checkout");
        assert!(Command::new("git")
            .arg("clone")
            .arg(&source)
            .arg(&checkout)
            .output()
            .unwrap()
            .status
            .success());
        let mut bundle = ChangeGroup::new(
            "missing".into(),
            "Missing".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.repos.push(serde_json::from_value(serde_json::json!({
            "id": "app", "path": checkout, "baseBranch": "main", "featureBranch": "knit/deleted-feature"
        })).unwrap());
        let error = super::super::client::prepare_feature_branches(&bundle).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("couldn't find remote ref"), "{message}");
        assert!(!message.contains(NO_ACCESS_HINT), "{message}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clone_does_not_activate_finished_bundles_when_no_open_bundle_exists() {
        let mut archived = ChangeGroup::new(
            "finished".into(),
            "Finished".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        archived.state = Some(crate::model::BundleState::Archived);
        let mut closed = archived.clone();
        closed.id = "closed".into();
        closed.state = Some(crate::model::BundleState::Closed);
        let mut bundles = vec![archived, closed];
        assert_eq!(select_active_bundle(&bundles, &[], None).unwrap(), None);

        let mut open = ChangeGroup::new(
            "current".into(),
            "Current".into(),
            "2026-01-02T00:00:00Z".into(),
        );
        bundles.push(open.clone());
        assert_eq!(
            select_active_bundle(&bundles, &[], None)
                .unwrap()
                .as_deref(),
            Some("current")
        );
        // Older artifacts without lifecycle metadata remain open.
        open.state = None;
        assert_eq!(
            select_active_bundle(&[open], &[], None).unwrap().as_deref(),
            Some("current")
        );
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

    #[test]
    fn prefer_https_probe_skips_covered_and_local_repositories() {
        let github_selection = vec![("work".to_string(), "github.com".to_string())];
        // Uncovered forge host: the hosted probe may touch it. Out-of-scope
        // repositories are probeable too — their pending-map entries are
        // tomorrow's credential-backed recovery clones.
        assert!(prefer_https_probes_repository(
            &export_repo("cloud", "git@bitbucket.org:team/cloud.git"),
            &github_selection
        ));
        // Covered host: the selection's own exact rewrite applies instead.
        assert!(!prefer_https_probes_repository(
            &export_repo("api", "git@github.com:acme/api.git"),
            &github_selection
        ));
        // Local remotes and missing URLs never reach the hosted probe.
        assert!(!prefer_https_probes_repository(
            &export_repo("local", "/repos/local.git"),
            &github_selection
        ));
        assert!(!prefer_https_probes_repository(
            &export_repo("bare", ""),
            &github_selection
        ));
        // Without a selection every forge repository is probed, preserving
        // the pre-selection behavior.
        assert!(prefer_https_probes_repository(
            &export_repo("api", "git@github.com:acme/api.git"),
            &[]
        ));
    }

    #[test]
    fn credential_update_advice_follows_token_storage_and_quotes_username() {
        let env_backed = crate::auth::CredentialSpec {
            provider: "github".to_string(),
            host: "github.com".to_string(),
            username: None,
            token_type: None,
            token_env: Some("WORK_GITHUB_TOKEN".to_string()),
        };
        let advice = credential_update_advice("work", &env_backed);
        assert_eq!(
            advice,
            "update its token in the `WORK_GITHUB_TOKEN` environment variable"
        );
        assert!(!advice.contains("knit auth add"));

        let file_backed = crate::auth::CredentialSpec {
            provider: "bitbucket".to_string(),
            host: "bitbucket.org".to_string(),
            username: Some("dev+work@example.com".to_string()),
            token_type: None,
            token_env: None,
        };
        let advice = credential_update_advice("cloud", &file_backed);
        assert!(
            advice.contains(
                "knit auth add cloud --provider bitbucket --host bitbucket.org --replace --username 'dev+work@example.com'"
            ),
            "{advice}"
        );
        assert!(advice.contains("prompts for the new token"));
    }
}
