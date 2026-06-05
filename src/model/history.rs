//! Project-wide Knit history events. These are metadata-only pointers to Git
//! commits and bundle ledger nodes; they never store patches or file contents.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HISTORY_EVENT_SCHEMA_VERSION: &str = "knit.history.event.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    pub schema_version: String,
    pub event_id: String,
    pub project_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub movement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    pub recorded_at: String,
    pub recorded_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}
