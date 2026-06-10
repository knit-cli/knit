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
pub const DEFAULT_LANDING_MERGE_METHOD: &str = "merge";
pub const CHECKOUT_MODE_WORKTREE: &str = "worktree";
pub const CHECKOUT_MODE_IN_PLACE: &str = "inPlace";

/// Default `checkoutMode` shared by project and bundle repo entries.
fn default_checkout_mode() -> String {
    CHECKOUT_MODE_WORKTREE.to_string()
}
