//! Reusable project templates: repos, run commands, runtime, and landing plan.

use super::{CheckoutMode, SCHEMA_VERSION};
pub use knit_runtime::config::{
    DatabaseMode, ProjectRuntime, ProjectRuntimeDatabase, ProjectRuntimePorts, RuntimeMode,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROJECT_CONFIG_FILE: &str = "knit.project.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitProject {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default)]
    pub repos: Vec<ProjectRepoEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, ProjectRunCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ProjectRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<ProjectLandingPlan>,
}

impl KnitProject {
    pub fn new(id: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: "KnitProject".to_string(),
            id,
            created_at: now.clone(),
            updated_at: now,
            org_id: None,
            repos: Vec::new(),
            commands: BTreeMap::new(),
            runtime: None,
            landing: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default)]
    pub checkout_mode: CheckoutMode,
    #[serde(default = "default_include_by_default")]
    pub include_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRunCommand {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// What `knit land apply` does when a step fails: `resume` (default) stops
    /// and waits for `knit land resume`; `rollback` creates revert PRs for the
    /// merge steps that already landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<super::LandOnFailure>,
    /// Named checks (see `knit check`) that must be green and fresh at the
    /// current bundle heads before `knit land apply` will execute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_checks: Vec<String>,
    #[serde(default)]
    pub merge: ProjectLandingMergePlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<ProjectLandingDeployment>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingMergePlan {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_order: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub needs: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_unlisted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<super::MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_checks_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_branch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingDeployment {
    pub id: String,
    #[serde(default, alias = "repo", skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<super::DeployMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<ProjectLandingCheckout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingCheckout {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<super::DeployCheckoutUpdate>,
}

fn default_include_by_default() -> bool {
    true
}
