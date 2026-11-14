//! `knit remote` and sync-remote transport. This root holds the shared remote response
//! DTOs; the work is split across submodules:
//!
//! - [`client`] HTTP transport, remote/token resolution, and bundle localization
//! - [`push`] remote config CRUD plus pushing projects/bundles and sync-on-push
//! - [`sync_auth`] `knit auth remote` token setup/verification and `remote auth-status`
//! - [`clone`] cloning a remote project export into a fresh workspace
//! - [`pull`] pulling/fetching recorded bundle state and remote bundle cleanup
//! - [`history`] project history event sync

mod client;
mod clone;
mod credentials;
mod facade;
pub(crate) mod handoff;
mod helpers;
mod history;
pub(crate) mod landing;
mod projects;
mod pull;
mod push;
mod sync_auth;
mod views;

pub use client::configured_sync_remote_names;
pub(crate) use client::{resolve_remote, resolve_token};
pub use clone::{clone_project_from_remote, CloneScopeRequest};
pub(crate) use credentials::{normalize_git_target, request_forge_credential, VendAttempt};
pub use facade::{sync_pull, sync_push, SyncTargets};
pub use history::pull_history_from_remote;
pub use projects::list_remote_projects;
pub use pull::{
    archive_deleted_bundle_on_remotes, archive_remote_bundle_by_id, delete_bundle_from_remote,
    delete_remote_bundle_by_id, fetch_bundles_from_remote, fetch_remote_bundle_payload,
    list_remote_bundles, prepare_remote_pull, pull_bundle_by_slug, pull_bundle_remote_state,
    pull_remote_state, pull_views_from_remote, remote_bundle_lifecycle, RemoteBundleOutcome,
    RemoteBundleRecord, RemotePullContext,
};
pub use push::sync_remote_helpers_command;
pub use push::{
    add_remote, list_remotes, maybe_sync_bundle_to_remote,
    maybe_sync_bundle_to_remote_with_history, planned_sync_remote_names,
    push_all_bundles_to_remote, push_bundle_to_remote, push_project_to_remote,
    push_views_to_remote, remove_remote, set_remote_token, show_remote,
    sync_active_bundle_to_remote_if_enabled, sync_bundle_to_remote_after_push,
    sync_bundle_to_remote_if_enabled,
};
pub(crate) use push::{push_active_bundle_to_remote, push_handoff_bundle_to_remote};
pub use sync_auth::{auth_remote, remote_auth_status};
pub use views::list_remote_views;

use crate::model::{HistoryEvent, KnitProject, ProjectView};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The GET `/projects/:id/view` response body: the user's personal document
/// plus the project's admin-managed shared templates. An older server sends
/// no `templates` at all; the field defaults to empty so such servers keep
/// working unchanged.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteViews {
    #[serde(default)]
    default_view: Option<String>,
    #[serde(default)]
    views: BTreeMap<String, ProjectView>,
    #[serde(default)]
    templates: BTreeMap<String, ProjectView>,
}

impl RemoteViews {
    /// Resolve a view name the way consumers should: the personal view, else
    /// the shared template of the same name.
    fn effective_view(&self, name: &str) -> Option<&ProjectView> {
        self.views.get(name).or_else(|| self.templates.get(name))
    }
}

// Shared HTTP/response DTOs. Kept in the module root so the sibling submodules
// (descendants) can read their fields without a wider `pub`.

struct HttpResponse {
    status: u16,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProject {
    id: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteBundle {
    id: String,
    slug: String,
    /// Canonical browser URL of the bundle on the host, derived server side.
    /// Absent when an older server does not report one.
    #[serde(default)]
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteArtifact {
    id: String,
    artifact_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectExport {
    project: RemoteExportProject,
    knit_project: Option<KnitProject>,
    repositories: Vec<RemoteExportRepository>,
    bundles: Vec<RemoteExportBundle>,
    /// Raw history events, decoded leniently via `decoded_history_events` —
    /// servers have shipped events without `projectId`, and one malformed
    /// record must never make the whole export unreadable.
    #[serde(default)]
    history_events: Vec<Value>,
    /// Repos the server withheld from this export (private repos the caller
    /// cannot see). Lets clone/pull say "the export is incomplete for you"
    /// instead of silently presenting a partial project as the whole thing.
    #[serde(default)]
    omitted_repository_count: Option<u64>,
}

impl RemoteProjectExport {
    /// A hosted project can hold several repository records that clone to the
    /// same local id, such as a contributor's fork and its upstream. A local
    /// id names one checkout, so keep one record per id: the one whose remote
    /// the shared membership records for that id, else the first listed.
    fn keep_one_record_per_local_id(&mut self) {
        let membership_remote = |id: &str| {
            self.knit_project
                .as_ref()
                .and_then(|project| project.repos.iter().find(|repo| repo.id == id))
                .and_then(|repo| repo.remote.as_deref())
        };
        let mut kept: Vec<(String, RemoteExportRepository)> = Vec::new();
        for record in std::mem::take(&mut self.repositories) {
            let local_id = clone::export_repo_local_id(&record);
            let names_member = |candidate: &RemoteExportRepository| match (
                membership_remote(&local_id),
                candidate.remote_url.as_deref(),
            ) {
                (Some(member), Some(url)) => handoff::same_repository_url(member, url),
                _ => false,
            };
            match kept.iter_mut().find(|(id, _)| *id == local_id) {
                None => kept.push((local_id, record)),
                Some((_, existing)) => {
                    if !names_member(existing) && names_member(&record) {
                        *existing = record;
                    }
                }
            }
        }
        self.repositories = kept.into_iter().map(|(_, record)| record).collect();
    }

    fn decoded_history_events(&self, project_id: &str) -> Vec<HistoryEvent> {
        decode_history_events(&self.history_events, project_id)
    }
}

/// Decode remote history events, tolerating server-side shape drift: a
/// missing `projectId` is filled from the project context (every history
/// fetch is project-scoped), and an event that still fails to decode is
/// skipped with a warning instead of failing the caller.
fn decode_history_events(raw: &[Value], project_id: &str) -> Vec<HistoryEvent> {
    let mut events = Vec::with_capacity(raw.len());
    let mut skipped = 0usize;
    for value in raw {
        let mut value = value.clone();
        if let Value::Object(fields) = &mut value {
            fields
                .entry("projectId")
                .or_insert_with(|| Value::String(project_id.to_string()));
        }
        match serde_json::from_value::<HistoryEvent>(value) {
            Ok(event) => events.push(event),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        crate::human!(
            "{} skipped {skipped} unreadable remote history event(s)",
            crate::output::heading("history:")
        );
    }
    events
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportProject {
    /// The server's immutable project id (a UUID). Slugs are ambiguous across
    /// owners, so follow-up calls keyed by project — like the views endpoint —
    /// should use this when present.
    #[serde(default)]
    id: Option<String>,
    slug: String,
    /// Present in exports of organization-owned projects; carries the org slug
    /// used as the `owner` half of an `owner/slug` clone reference.
    #[serde(default)]
    organization: Option<RemoteOwnerSummary>,
}

/// The owner/organization summary maps the API attaches to projects. Only the
/// namespace handle matters to the CLI: it is the `owner` in `owner/slug`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteOwnerSummary {
    #[serde(default)]
    slug: Option<String>,
}

/// Stable machine-readable error categories for `--json` commands, mirrored by
/// consumers such as ivaldi. `noRemote`/`noToken` are configuration problems,
/// `http` is a transport or server failure, `other` is everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteErrorKind {
    NoRemote,
    NoToken,
    Http,
    NotFound,
    Other,
}

impl RemoteErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            RemoteErrorKind::NoRemote => "noRemote",
            RemoteErrorKind::NoToken => "noToken",
            RemoteErrorKind::Http => "http",
            RemoteErrorKind::NotFound => "notFound",
            RemoteErrorKind::Other => "other",
        }
    }
}

fn json_error_envelope(kind: RemoteErrorKind, error: &anyhow::Error) -> Value {
    serde_json::json!({
        "error": {
            "kind": kind.as_str(),
            "message": format!("{error:#}"),
        }
    })
}

/// Print the `--json` error envelope to stdout. The caller still returns the
/// error so the process exits non-zero and stderr keeps the human message.
fn print_json_error_envelope(kind: RemoteErrorKind, error: &anyhow::Error) {
    println!("{}", json_error_envelope(kind, error));
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportRepository {
    local_id: Option<String>,
    name: String,
    default_branch: Option<String>,
    remote_url: Option<String>,
    /// `public`/`internal`/`private`; absent on older exports (treated as
    /// non-public so a failed clone still attempts the credential vend).
    #[serde(default)]
    visibility: Option<String>,
    metadata: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportBundle {
    id: String,
    slug: String,
    lifecycle_state: String,
    current_artifact: Option<RemoteExportArtifact>,
    /// Canonical browser URL of the bundle on the host, derived server side.
    /// Absent when an older server does not report one.
    #[serde(default)]
    web_url: Option<String>,
}

/// The artifact metadata a project export carries for a bundle. The default
/// export is slim: it identifies the current artifact (hash and friends) but
/// carries no `payload`, so the client fetches the payloads it actually needs
/// one bundle at a time. An older server that ignores the slim request still
/// inlines `payload`, and that copy is used as-is.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportArtifact {
    artifact_hash: String,
    #[serde(default)]
    payload: Option<Value>,
}

/// One bundle fetched on its own (`GET /bundles/:id?include=artifact`) — the
/// incremental door that replaces reading every payload out of one export.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteBundleDetail {
    #[serde(default)]
    current_artifact: Option<RemoteExportArtifact>,
}

/// A rewrite must not be presented solely as work to union back into the ledger.
fn divergence_message(local: &crate::model::ChangeGroup) -> &'static str {
    if has_local_rewrite(local) {
        "local and remote ledgers have diverged after a local rewrite; kept local. To publish the rewrite, run `knit sync push --force-with-lease`; use `knit pull --merge` only to combine both histories"
    } else {
        "local and remote ledgers have diverged; kept local. If local ledger nodes were intentionally removed in a local rewrite, run `knit sync push --force-with-lease`; otherwise run `knit pull --merge` to combine them"
    }
}

fn has_local_rewrite(local: &crate::model::ChangeGroup) -> bool {
    local.nodes.iter().any(|node| {
        node.node_type == "git.observed"
            && node.repo_changes.iter().any(|change| {
                matches!(
                    change.movement,
                    crate::model::Movement::Rewound | crate::model::Movement::Diverged
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_history_events, RemoteProjectExport, RemoteViews};
    use serde_json::json;

    fn export_with(knit_project: serde_json::Value, urls: &[&str]) -> RemoteProjectExport {
        let repositories: Vec<_> = urls
            .iter()
            .map(|url| json!({"localId": "app", "name": "app", "remoteUrl": url, "metadata": {}}))
            .collect();
        serde_json::from_value(json!({
            "project": {"slug": "demo"},
            "knitProject": knit_project,
            "repositories": repositories,
            "bundles": [],
        }))
        .unwrap()
    }

    fn kept_urls(export: &RemoteProjectExport) -> Vec<&str> {
        export
            .repositories
            .iter()
            .map(|record| record.remote_url.as_deref().unwrap())
            .collect()
    }

    #[test]
    fn export_keeps_the_record_the_membership_names_for_a_repeated_local_id() {
        let mut export = export_with(
            json!({
                "schemaVersion": "0.1", "kind": "KnitProject", "id": "demo",
                "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
                "repos": [{"id": "app", "path": "/x/app", "remote": "git@github.com:org/app.git",
                           "baseBranch": "main"}],
            }),
            &[
                "https://github.com/contributor/app.git",
                "https://github.com/org/app.git",
            ],
        );
        export.keep_one_record_per_local_id();
        assert_eq!(kept_urls(&export), ["https://github.com/org/app.git"]);
    }

    #[test]
    fn export_without_membership_keeps_the_first_record_for_a_repeated_local_id() {
        let mut export = export_with(
            serde_json::Value::Null,
            &[
                "https://github.com/contributor/app.git",
                "https://github.com/org/app.git",
            ],
        );
        export.keep_one_record_per_local_id();
        assert_eq!(
            kept_urls(&export),
            ["https://github.com/contributor/app.git"]
        );
    }

    #[test]
    fn remote_views_decode_tolerates_a_server_without_templates() {
        let older: RemoteViews =
            serde_json::from_value(json!({"defaultView": "backend", "views": {}})).unwrap();
        assert!(older.templates.is_empty());

        let newer: RemoteViews = serde_json::from_value(json!({
            "defaultView": "backend",
            "views": {"backend": {"exclude": ["frontend"]}},
            "templates": {"all-frontend": {"base": "none", "include": ["frontend"]}},
        }))
        .unwrap();
        assert_eq!(newer.views.len(), 1);
        assert_eq!(newer.templates.len(), 1);
        assert!(newer.templates["all-frontend"]
            .include
            .contains(&"frontend".to_string()));
    }

    #[test]
    fn decode_history_events_fills_missing_project_id_and_skips_garbage() {
        let raw = vec![
            json!({
                "schemaVersion": "knit.history.event.v1",
                "eventId": "evt-complete",
                "projectId": "demo",
                "kind": "bundle.created",
                "recordedAt": "2026-07-18T15:00:00Z",
                "recordedBy": "cli",
            }),
            // A native server-side event shipped without projectId.
            json!({
                "schemaVersion": "knit.history.event.v1",
                "eventId": "review-decision:abc",
                "kind": "review.approved",
                "bundleId": "some-bundle",
                "recordedAt": "2026-07-18T15:53:42Z",
                "recordedBy": "native",
            }),
            // Unreadable however repaired: skipped, never an error.
            json!({"eventId": 42}),
            json!("not an object"),
        ];

        let events = decode_history_events(&raw, "demo");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, "evt-complete");
        assert_eq!(events[1].event_id, "review-decision:abc");
        assert_eq!(events[1].project_id, "demo");
    }
}

#[cfg(test)]
mod rewrite_diagnostic_tests {
    use super::divergence_message;
    use crate::model::{BundleNode, ChangeGroup};

    #[test]
    fn observed_rewinds_and_divergence_name_local_rewrite() {
        for movement in ["rewound", "diverged"] {
            let mut bundle = ChangeGroup::new("work".into(), "Work".into(), "now".into());
            let node: BundleNode = serde_json::from_value(serde_json::json!({
                "id": "rewrite", "type": "git.observed", "createdAt": "now",
                "repoChanges": [{"repoId": "repo", "movement": movement,
                    "beforeSha": "old", "afterSha": "new", "commits": []}]
            }))
            .unwrap();
            bundle.nodes.push(node);
            let message = divergence_message(&bundle);
            assert!(message.contains("after a local rewrite"));
            assert!(message.contains("knit sync push --force-with-lease"));
        }
    }

    #[test]
    fn missing_nodes_do_not_claim_a_confirmed_rewrite() {
        let bundle = ChangeGroup::new("work".into(), "Work".into(), "now".into());
        let message = divergence_message(&bundle);
        assert!(message.contains("If local ledger nodes were intentionally removed"));
        assert!(message.contains("knit sync push --force-with-lease"));
        assert!(message.contains("knit pull --merge"));
    }
}
