//! Knit's persisted JSON data model, split by domain. Everything is re-exported
//! here so call sites keep using `crate::model::<Type>`.
//!
//! - [`config`] workspace config + folder context map
//! - [`project`] reusable project templates (repos, runtime, landing)
//! - [`view`] per-user named views (bundle shapes) over a project
//! - [`bundle`] the bundle (`ChangeGroup`) and its contents
//! - [`history`] project-wide metadata events pointing at Git commits

mod bundle;
mod config;
mod history;
mod project;
mod view;

pub use bundle::*;
pub use config::*;
pub use history::*;
pub use project::*;
pub use view::*;

pub const SCHEMA_VERSION: &str = "0.1";

/// How a landed PR is merged into its base. Serialized lowercase to match
/// project templates and editable land plans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    #[default]
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        }
    }
}

impl std::fmt::Display for MergeMethod {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// How a deploy step deploys: run a command, or push a branch. Shared by
/// project landing templates and land plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    Command,
    Push,
}

impl DeployMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DeployMode::Command => "command",
            DeployMode::Push => "push",
        }
    }
}

impl std::fmt::Display for DeployMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// How a deploy checkout is refreshed before deploying.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployCheckoutUpdate {
    #[default]
    Fetch,
    Pull,
    None,
}

impl DeployCheckoutUpdate {
    pub fn as_str(self) -> &'static str {
        match self {
            DeployCheckoutUpdate::Fetch => "fetch",
            DeployCheckoutUpdate::Pull => "pull",
            DeployCheckoutUpdate::None => "none",
        }
    }
}

impl std::fmt::Display for DeployCheckoutUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// How a tracked repo is checked out for a bundle: a generated worktree under
/// `.knit/worktrees/`, or in place in the source checkout. Serialized as
/// `worktree` / `inPlace` to match existing artifacts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CheckoutMode {
    #[default]
    #[serde(rename = "worktree")]
    Worktree,
    #[serde(rename = "inPlace")]
    InPlace,
}

impl CheckoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckoutMode::Worktree => "worktree",
            CheckoutMode::InPlace => "inPlace",
        }
    }
}

impl std::fmt::Display for CheckoutMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}
