//! `knit remote` and sync-remote transport. This root holds the shared remote response
//! DTOs; the work is split across submodules:
//!
//! - [`client`] HTTP transport, remote/token resolution, and bundle localization
//! - [`push`] remote config CRUD plus pushing projects/bundles and sync-on-push
//! - [`clone`] cloning a remote project export into a fresh workspace
//! - [`pull`] pulling/fetching recorded bundle state and remote bundle cleanup
//! - [`history`] project history event sync

mod client;
mod clone;
mod credentials;
mod facade;
mod helpers;
mod history;
mod projects;
mod pull;
mod push;

pub use client::configured_sync_remote_names;
pub(crate) use client::{resolve_remote, resolve_token};
pub use clone::clone_project_from_remote;
pub(crate) use credentials::{normalize_git_target, request_forge_credential, VendAttempt};
pub use facade::{sync_pull, sync_push, SyncTargets};
pub use history::pull_history_from_remote;
pub use projects::list_remote_projects;
pub use pull::{
    archive_remote_bundle_by_id, delete_bundle_from_remote, delete_remote_bundle_by_id,
    fetch_bundles_from_remote, list_remote_bundles, prepare_remote_pull, pull_bundle_by_slug,
    pull_bundle_remote_state, pull_remote_state, pull_views_from_remote, remote_bundle_lifecycle,
    RemoteBundleOutcome, RemoteBundleRecord, RemotePullContext,
};
pub use push::sync_remote_helpers_command;
pub use push::{
    add_remote, list_remotes, maybe_sync_bundle_to_remote, push_all_bundles_to_remote,
    push_bundle_to_remote, push_project_to_remote, push_views_to_remote, remote_auth_status,
    remove_remote, set_remote_token, show_remote, sync_active_bundle_to_remote_if_enabled,
    sync_bundle_to_remote_if_enabled,
};

use crate::model::{HistoryEvent, KnitProject, ProjectView};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteViews {
    #[serde(default)]
    default_view: Option<String>,
    #[serde(default)]
    views: BTreeMap<String, ProjectView>,
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

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportArtifact {
    artifact_hash: String,
    payload: Value,
}

#[cfg(test)]
mod tests {
    use super::decode_history_events;
    use serde_json::json;

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
