//! Shared land plan/run data model: the editable plan file shape, the run
//! record with per-step status, and step outcome carriers used by execution.

use crate::model::{DeployCheckoutUpdate, DeployMode, RepoEntry};
use crate::providers::PullRequest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const LAND_PLAN_KIND: &str = "KnitLandPlan";
pub(super) const LAND_RUN_KIND: &str = "KnitLandRun";
pub(super) const DEFAULT_LAND_PROVIDER: &str = "github";

/// Kind of a land plan step. Serialized snake_case to match the editable plan
/// files (`merge_pr`, `wait_checks`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LandStepKind {
    MergePr,
    WaitChecks,
    Run,
    Deploy,
}

impl LandStepKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            LandStepKind::MergePr => "merge_pr",
            LandStepKind::WaitChecks => "wait_checks",
            LandStepKind::Run => "run",
            LandStepKind::Deploy => "deploy",
        }
    }
}

impl std::fmt::Display for LandStepKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// Status of a land run or one of its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LandStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl LandStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            LandStatus::Pending => "pending",
            LandStatus::Running => "running",
            LandStatus::Succeeded => "succeeded",
            LandStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for LandStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LandPlan {
    pub(super) schema_version: String,
    pub(super) kind: String,
    pub(super) id: String,
    pub(super) provider: String,
    pub(super) bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source_project_id: Option<String>,
    pub(super) created_at: String,
    /// What `knit land apply` does when a step fails: stop and wait for
    /// `knit land resume` (default), or create revert PRs for the merge steps
    /// that already landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) on_failure: Option<crate::model::LandOnFailure>,
    pub(super) steps: Vec<LandStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LandStep {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) step_type: LandStepKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) method: Option<crate::model::MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) wait_for_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) required_checks_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delete_branch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) required_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) deployment_mode: Option<DeployMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) checkout: Option<LandCheckout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LandCheckout {
    pub(super) branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) update: Option<DeployCheckoutUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LandRun {
    pub(super) schema_version: String,
    pub(super) kind: String,
    pub(super) id: String,
    pub(super) plan_id: String,
    pub(super) bundle_id: String,
    pub(super) provider: String,
    pub(super) plan_path: String,
    pub(super) status: LandStatus,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    /// Set when `knit land rollback` (or `onFailure: rollback`) created revert
    /// PRs for this run's merged steps. A rolled-back run cannot be resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) rolled_back_at: Option<String>,
    pub(super) steps: Vec<LandRunStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LandRunStep {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) step_type: LandStepKind,
    pub(super) status: LandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) publication_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) exit_code: Option<i32>,
}

#[derive(Debug)]
pub(super) struct StepOutcome {
    pub(super) success: bool,
    pub(super) detail: String,
    pub(super) publication_url: Option<String>,
    pub(super) stdout: Option<String>,
    pub(super) stderr: Option<String>,
    pub(super) exit_code: Option<i32>,
    pub(super) publication_update: Option<PublicationUpdate>,
}

#[derive(Debug)]
pub(super) struct PublicationUpdate {
    pub(super) repo: RepoEntry,
    pub(super) pr: PullRequest,
}
