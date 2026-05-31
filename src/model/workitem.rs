//! Actionable work items (`KnitWorkItem`) and their planning/execution states.

use super::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const WORK_ITEM_KIND: &str = "KnitWorkItem";
pub const WORK_ITEM_PLANNING_UNPLANNED: &str = "unplanned";
pub const WORK_ITEM_PLANNING_PLOTTED: &str = "plotted";
pub const WORK_ITEM_PLANNING_APPROVED: &str = "approved";
pub const WORK_ITEM_PLANNING_STALE: &str = "stale";
pub const WORK_ITEM_EXECUTION_IDLE: &str = "idle";
pub const WORK_ITEM_EXECUTION_CLAIMED: &str = "claimed";
pub const WORK_ITEM_EXECUTION_RUNNING: &str = "running";
pub const WORK_ITEM_EXECUTION_WAITING_REVIEW: &str = "waiting_review";
pub const WORK_ITEM_EXECUTION_WAITING_LAND: &str = "waiting_land";
pub const WORK_ITEM_EXECUTION_LANDED: &str = "landed";
pub const WORK_ITEM_EXECUTION_FAILED: &str = "failed";
pub const WORK_ITEM_EXECUTION_CANCELED: &str = "canceled";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitWorkItem {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub item_kind: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub planning_status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plotted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    pub execution_status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundle_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl KnitWorkItem {
    pub fn new(id: String, title: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: WORK_ITEM_KIND.to_string(),
            id,
            item_kind: "feature".to_string(),
            title,
            description: String::new(),
            acceptance_criteria: Vec::new(),
            org_id: None,
            project_id: None,
            repo_hints: Vec::new(),
            priority: None,
            labels: Vec::new(),
            planning_status: WORK_ITEM_PLANNING_UNPLANNED.to_string(),
            depends_on: Vec::new(),
            lane: None,
            rank: None,
            planner: None,
            planning_rationale: None,
            plotted_at: None,
            approved_at: None,
            execution_status: WORK_ITEM_EXECUTION_IDLE.to_string(),
            bundle_ids: Vec::new(),
            claim_id: None,
            target: None,
            last_outcome: None,
            created_at: now.clone(),
            updated_at: now,
            metadata: BTreeMap::new(),
        }
    }
}
