//! `knit remote` and KnitHub sync. This root holds the shared remote response
//! DTOs; the work is split across submodules:
//!
//! - [`client`] HTTP transport, remote/token resolution, and bundle localization
//! - [`push`] remote config CRUD plus pushing projects/bundles and sync-on-push
//! - [`clone`] cloning a KnitHub project export into a fresh workspace
//! - [`pull`] pulling/fetching recorded bundle state and remote bundle cleanup
//! - [`history`] project history event sync

mod client;
mod clone;
mod facade;
mod history;
mod pull;
mod push;

pub use client::configured_sync_remote_names;
pub use clone::clone_project_from_remote;
pub use facade::{sync_pull, sync_push, SyncTargets};
pub use history::pull_history_from_remote;
pub use pull::{
    delete_bundle_from_remote, delete_remote_bundle_by_id, fetch_bundles_from_remote,
    list_remote_bundles, prepare_remote_pull, pull_bundle_remote_state, pull_remote_state,
    pull_views_from_remote, RemoteBundleOutcome, RemoteBundleRecord, RemotePullContext,
};
pub use push::{
    add_remote, list_remotes, maybe_sync_bundle_to_remote, push_bundle_to_remote,
    push_project_to_remote, push_views_to_remote, remove_remote, set_remote_token, show_remote,
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
    #[serde(default)]
    history_events: Vec<HistoryEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportProject {
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportRepository {
    local_id: Option<String>,
    name: String,
    default_branch: Option<String>,
    remote_url: Option<String>,
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
