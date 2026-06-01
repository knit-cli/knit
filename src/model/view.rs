//! Per-user "views" over a project: named bundle shapes expressed as
//! include/exclude deltas on top of the project's `includeByDefault` repo set.
//!
//! Views are user-local config, stored at `.knit/views/<project-id>.views.json`
//! and synced to KnitHub as the user's own configuration. They never live inside
//! the shared project artifact.

use super::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VIEWS_KIND: &str = "KnitProjectViews";

/// All of a user's saved views for a single project, plus an optional default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitProjectViews {
    pub schema_version: String,
    pub kind: String,
    pub project_id: String,
    pub created_at: String,
    pub updated_at: String,
    /// Name of the view bare `knit bundle start` should apply, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view: Option<String>,
    #[serde(default)]
    pub views: BTreeMap<String, ProjectView>,
}

impl KnitProjectViews {
    pub fn new(project_id: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: VIEWS_KIND.to_string(),
            project_id,
            created_at: now.clone(),
            updated_at: now,
            default_view: None,
            views: BTreeMap::new(),
        }
    }
}

/// A single named view: deltas applied over the project default repo set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    /// Repo ids to add to the default set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Repo ids to drop from the default set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}
