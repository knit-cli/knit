use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "0.1";
pub const CHANGE_GROUP_KIND: &str = "ChangeGroup";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitConfig {
    pub schema_version: String,
    pub active_bundle: String,
}

impl KnitConfig {
    pub fn new(active_bundle: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeGroup {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub repos: Vec<RepoEntry>,
    pub commit_groups: Vec<CommitGroup>,
}

impl ChangeGroup {
    pub fn new(id: String, title: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id,
            title,
            created_at: now.clone(),
            updated_at: now,
            repos: Vec::new(),
            commit_groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    pub feature_branch: Option<String>,
    pub worktree_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitGroup {
    pub id: String,
    pub message: String,
    pub created_at: String,
    pub commits: Vec<CommitRef>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRef {
    pub repo_id: String,
    pub sha: String,
}
