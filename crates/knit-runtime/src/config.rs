//! Runtime configuration: the `runtime` block of a knit project. These types
//! are the crate's public config surface; the knit CLI re-exports them from
//! its model so project JSON (de)serialization is shared.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Repo whose checkout hosts the runtime compose file. When set, the
    /// runtime is that single stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_repo: Option<String>,
    /// Repos whose compose stacks `knit run up` lifts, each as its own
    /// isolated per-bundle compose project. Empty (and no `stackRepo`) means
    /// every bundle repo with a compose file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stacks: Vec<String>,
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
            stacks: Vec::new(),
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
    /// Shared mode, transform stacks: the compose service that IS the
    /// database. The service is stripped from the lifted stack and env
    /// references to it are rewired to `host:port`, so the bundle runs its
    /// code against the shared dev database instead of a fresh empty one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Container-side port of the stripped service (default 5432), used to
    /// rewrite `<service>:<containerPort>` and bare port references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_port: Option<u16>,
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
            service: None,
            container_port: None,
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
