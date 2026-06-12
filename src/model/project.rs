//! Reusable project templates: repos, run commands, runtime, and landing plan.

use super::{CheckoutMode, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROJECT_CONFIG_FILE: &str = "knit.project.json";

/// How a bundle runtime gets its database: attached to an existing shared dev
/// database, or a dedicated per-bundle container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseMode {
    #[default]
    Shared,
    Bundle,
}

impl DatabaseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DatabaseMode::Shared => "shared",
            DatabaseMode::Bundle => "bundle",
        }
    }
}

impl std::fmt::Display for DatabaseMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// How `knit run up` executes a compose file: lift the repo's existing shape
/// into the bundle namespace, or run a `KNIT_*`-aware file with the contract
/// injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    #[default]
    Transform,
    Contract,
}

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

/// A project's bundle runtime. Knit lifts the stack repo's compose shape
/// into a per-bundle namespace (compose project name, free host ports,
/// bundle checkouts substituted for source paths). Repos can instead commit
/// a compose file written against Knit's `KNIT_*` environment contract for
/// precise control. Every field is optional: a bundle whose single repo has
/// a docker-compose file runs with zero configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRuntime {
    #[serde(default = "default_runtime_kind")]
    pub kind: String,
    /// Repo whose checkout hosts the runtime compose file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_repo: Option<String>,
    #[serde(default = "default_project_config_file")]
    pub project_config_file: String,
    /// Compose file inside the stack repo. Defaults to
    /// `docker-compose.knit.yml` when present, then the repo's own
    /// `docker-compose.yml`/`compose.yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    /// Force transform or contract mode instead of detecting it from the
    /// compose file (contract filename or `${KNIT_*}` references).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RuntimeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<ProjectRuntimeDatabase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<ProjectRuntimePorts>,
    /// Path opened on the frontend port after `knit run status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_path: Option<String>,
}

impl Default for ProjectRuntime {
    fn default() -> Self {
        Self {
            kind: default_runtime_kind(),
            stack_repo: None,
            project_config_file: default_project_config_file(),
            compose_file: None,
            mode: None,
            database: None,
            ports: None,
            profile_path: None,
        }
    }
}

fn default_runtime_kind() -> String {
    "docker-compose".to_string()
}

fn default_project_config_file() -> String {
    "knit.project.json".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRuntimeDatabase {
    #[serde(default)]
    pub mode: DatabaseMode,
    #[serde(default = "default_database_host")]
    pub host: String,
    #[serde(default = "default_database_port")]
    pub port: u16,
    #[serde(default = "default_database_name")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_base: Option<u16>,
    /// Optional command run in the stack checkout to start the shared dev
    /// database when it is unreachable (e.g. `docker compose up -d db`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<Vec<String>>,
}

fn default_database_host() -> String {
    "host.docker.internal".to_string()
}

fn default_database_port() -> u16 {
    5432
}

fn default_database_name() -> String {
    "app_dev".to_string()
}

/// Host port allocation pools for bundle runtimes. Container-side ports are
/// the compose file's business; Knit only hands out free host ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRuntimePorts {
    #[serde(default = "default_backend_port_base")]
    pub backend_base: u16,
    #[serde(default = "default_frontend_port_base")]
    pub frontend_base: u16,
    #[serde(default = "default_port_step")]
    pub step: u16,
    /// Contract mode: service name -> base host port, each exposed as
    /// `KNIT_PORT_<SERVICE>`. Empty means a backend/frontend pair from the
    /// base fields above.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, u16>,
}

impl ProjectRuntimePorts {
    /// The service port pools contract mode allocates from.
    pub fn service_bases(&self) -> BTreeMap<String, u16> {
        if self.services.is_empty() {
            BTreeMap::from([
                ("backend".to_string(), self.backend_base),
                ("frontend".to_string(), self.frontend_base),
            ])
        } else {
            self.services.clone()
        }
    }
}

fn default_backend_port_base() -> u16 {
    4001
}

fn default_frontend_port_base() -> u16 {
    5174
}

fn default_port_step() -> u16 {
    10
}

impl Default for ProjectRuntimeDatabase {
    fn default() -> Self {
        Self {
            mode: DatabaseMode::default(),
            host: default_database_host(),
            port: default_database_port(),
            name: default_database_name(),
            name_template: None,
            port_base: None,
            start_command: None,
        }
    }
}

impl Default for ProjectRuntimePorts {
    fn default() -> Self {
        Self {
            backend_base: default_backend_port_base(),
            frontend_base: default_frontend_port_base(),
            step: default_port_step(),
            services: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
