use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "0.1";
pub const CHANGE_GROUP_KIND: &str = "ChangeGroup";
pub const CHECKOUT_MODE_WORKTREE: &str = "worktree";
pub const CHECKOUT_MODE_IN_PLACE: &str = "inPlace";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeGroup {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_node_id: Option<String>,
    pub repos: Vec<RepoEntry>,
    pub commit_groups: Vec<CommitGroup>,
    #[serde(default)]
    pub nodes: Vec<BundleNode>,
}

impl ChangeGroup {
    pub fn new(id: String, title: String, now: String) -> Self {
        let node = BundleNode::feature_created(id.clone(), now.clone(), title.clone());
        let head_node_id = Some(node.id.clone());
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id,
            title,
            created_at: now.clone(),
            updated_at: now,
            head_node_id,
            repos: Vec::new(),
            commit_groups: Vec::new(),
            nodes: vec![node],
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
    #[serde(default = "default_checkout_mode")]
    pub checkout_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    pub feature_branch: Option<String>,
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

fn default_checkout_mode() -> String {
    CHECKOUT_MODE_WORKTREE.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitGroup {
    pub id: String,
    pub message: String,
    pub created_at: String,
    pub commits: Vec<CommitRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRef {
    pub repo_id: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoChange {
    pub repo_id: String,
    #[serde(default = "default_movement")]
    pub movement: String,
    pub before_sha: Option<String>,
    pub after_sha: String,
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_commits: Vec<String>,
}

fn default_movement() -> String {
    "advanced".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<CommitRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_changes: Vec<RepoChange>,
}

impl BundleNode {
    pub fn feature_created(feature_id: String, created_at: String, title: String) -> Self {
        Self {
            id: feature_id,
            node_type: "feature.created".to_string(),
            created_at,
            title: Some(title),
            repo_ids: None,
            commit_group_id: None,
            message: None,
            target_node_id: None,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn repos_added(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            id,
            node_type: "repo.added".to_string(),
            created_at,
            title: None,
            repo_ids: Some(repo_ids),
            commit_group_id: None,
            message: None,
            target_node_id: None,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn repos_removed(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            id,
            node_type: "repo.removed".to_string(),
            created_at,
            title: None,
            repo_ids: Some(repo_ids),
            commit_group_id: None,
            message: None,
            target_node_id: None,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn worktrees_materialized(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            id,
            node_type: "worktree.materialized".to_string(),
            created_at,
            title: None,
            repo_ids: Some(repo_ids),
            commit_group_id: None,
            message: None,
            target_node_id: None,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn commit_group(
        group_id: String,
        created_at: String,
        message: String,
        commits: Vec<CommitRef>,
        repo_changes: Vec<RepoChange>,
    ) -> Self {
        Self {
            id: group_id.clone(),
            node_type: "commit.group".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: Some(group_id),
            message: Some(message),
            target_node_id: None,
            commits,
            repo_changes,
        }
    }

    pub fn revert_group(
        group_id: String,
        created_at: String,
        target_node_id: String,
        message: String,
        commits: Vec<CommitRef>,
        repo_changes: Vec<RepoChange>,
    ) -> Self {
        Self {
            id: group_id.clone(),
            node_type: "revert.group".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: Some(group_id),
            message: Some(message),
            target_node_id: Some(target_node_id),
            commits,
            repo_changes,
        }
    }

    pub fn git_observed(id: String, created_at: String, repo_changes: Vec<RepoChange>) -> Self {
        Self {
            id,
            node_type: "git.observed".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: None,
            message: None,
            target_node_id: None,
            commits: Vec::new(),
            repo_changes,
        }
    }

    pub fn checkpoint(id: String, created_at: String, message: String) -> Self {
        Self {
            id,
            node_type: "checkpoint".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: None,
            message: Some(message),
            target_node_id: None,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }
}
