//! The bundle (`ChangeGroup`) and its contents: tracked repos, commit groups,
//! ledger nodes, and recorded publications.

use super::{CheckoutMode, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};

pub const CHANGE_GROUP_KIND: &str = "ChangeGroup";

/// Persisted lifecycle state of a bundle artifact. Serialized lowercase to
/// match existing artifacts. The user-facing state can additionally present a
/// derived `landed`; that lives in `commands::bundle::BundleStatus` and is
/// never written to the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleState {
    Open,
    Closed,
    Archived,
    Deleted,
}

impl BundleState {
    pub fn as_str(self) -> &'static str {
        match self {
            BundleState::Open => "open",
            BundleState::Closed => "closed",
            BundleState::Archived => "archived",
            BundleState::Deleted => "deleted",
        }
    }
}

impl std::fmt::Display for BundleState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeGroup {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<BundleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_node_id: Option<String>,
    pub repos: Vec<RepoEntry>,
    pub commit_groups: Vec<CommitGroup>,
    #[serde(default)]
    pub nodes: Vec<BundleNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publications: Vec<PublicationEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub work_item_ids: Vec<String>,
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
            state: Some(BundleState::Open),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
            created_at: now.clone(),
            updated_at: now,
            head_node_id,
            repos: Vec::new(),
            commit_groups: Vec::new(),
            nodes: vec![node],
            publications: Vec::new(),
            work_item_ids: Vec::new(),
        }
    }
}

/// How a local bundle artifact's append-only ledger relates to a remote copy of
/// the same bundle, decided purely from their node-id sequences. The ledger is
/// append-only, so a fast-forward is exactly the case where one sequence is a
/// (non-strict) prefix of the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerRelation {
    /// Both ledgers record the same node-id sequence; nothing to refresh.
    Equal,
    /// The local ledger is a strict prefix of the remote: the remote is strictly
    /// ahead and the local artifact can be fast-forwarded to it.
    RemoteAhead,
    /// The remote ledger is a strict prefix of the local: local has work the
    /// remote has not seen. Keep local; a later push reconciles the remote.
    LocalAhead,
    /// Neither sequence is a prefix of the other: the ledgers have diverged.
    Diverged,
}

/// Decide how a local bundle ledger relates to a remote one, comparing their
/// append-only node-id sequences position by position. Pure and total: it makes
/// no I/O and treats two empty ledgers as `Equal`.
pub fn ledger_relation(local_node_ids: &[String], remote_node_ids: &[String]) -> LedgerRelation {
    let common = local_node_ids.len().min(remote_node_ids.len());
    if local_node_ids[..common] != remote_node_ids[..common] {
        return LedgerRelation::Diverged;
    }
    use std::cmp::Ordering;
    match local_node_ids.len().cmp(&remote_node_ids.len()) {
        Ordering::Equal => LedgerRelation::Equal,
        Ordering::Less => LedgerRelation::RemoteAhead,
        Ordering::Greater => LedgerRelation::LocalAhead,
    }
}

impl ChangeGroup {
    /// The ordered node-id sequence of this bundle's append-only ledger, used to
    /// compare local and remote copies for fast-forward refresh.
    pub fn node_id_sequence(&self) -> Vec<String> {
        self.nodes.iter().map(|node| node.id.clone()).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationEntry {
    pub repo_id: String,
    pub provider: String,
    pub kind: String,
    pub number: u64,
    pub url: String,
    pub base_branch: String,
    pub head_branch: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default)]
    pub checkout_mode: CheckoutMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    pub feature_branch: Option<String>,
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitGroup {
    pub id: String,
    pub message: String,
    pub created_at: String,
    pub commits: Vec<CommitRef>,
    /// Git author of the commit, captured at commit time. Optional so older
    /// artifacts (and consumers) stay compatible. Used downstream to attribute
    /// ledger activity to the person who did the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<CommitAuthor>,
}

/// Identity of whoever authored a recorded commit, as Git records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRef {
    pub repo_id: String,
    pub sha: String,
}

/// How a repo's recorded head moved in a ledger node. Serialized lowercase to
/// match existing artifacts and history events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Movement {
    #[default]
    Advanced,
    Rewound,
    Diverged,
}

impl Movement {
    pub fn as_str(self) -> &'static str {
        match self {
            Movement::Advanced => "advanced",
            Movement::Rewound => "rewound",
            Movement::Diverged => "diverged",
        }
    }
}

impl std::fmt::Display for Movement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoChange {
    pub repo_id: String,
    #[serde(default)]
    pub movement: Movement,
    pub before_sha: Option<String>,
    pub after_sha: String,
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_commits: Vec<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publication_urls: Vec<String>,
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
            commits: Vec::new(),
            repo_changes,
        }
    }

    pub fn land_update(
        id: String,
        created_at: String,
        provider: String,
        repo_changes: Vec<RepoChange>,
    ) -> Self {
        Self {
            id,
            node_type: "land.update".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: None,
            message: Some("updated feature branches from base".to_string()),
            target_node_id: None,
            plan_id: None,
            run_id: None,
            provider: Some(provider),
            publication_urls: Vec::new(),
            commits: Vec::new(),
            repo_changes,
        }
    }

    pub fn feature_archived(id: String, created_at: String, reason: Option<String>) -> Self {
        Self {
            id,
            node_type: "feature.archived".to_string(),
            created_at,
            title: None,
            repo_ids: None,
            commit_group_id: None,
            message: reason,
            target_node_id: None,
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn feature_landed(
        id: String,
        created_at: String,
        plan_id: String,
        run_id: String,
        provider: String,
        repo_ids: Vec<String>,
        publication_urls: Vec<String>,
    ) -> Self {
        Self {
            id,
            node_type: "feature.landed".to_string(),
            created_at,
            title: None,
            repo_ids: Some(repo_ids),
            commit_group_id: None,
            message: None,
            target_node_id: None,
            plan_id: Some(plan_id),
            run_id: Some(run_id),
            provider: Some(provider),
            publication_urls,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn pr_revert(
        id: String,
        created_at: String,
        target_node_id: String,
        message: String,
        provider: String,
        repo_ids: Vec<String>,
        publication_urls: Vec<String>,
    ) -> Self {
        Self {
            id,
            node_type: "pr.revert".to_string(),
            created_at,
            title: None,
            repo_ids: Some(repo_ids),
            commit_group_id: None,
            message: Some(message),
            target_node_id: Some(target_node_id),
            plan_id: None,
            run_id: None,
            provider: Some(provider),
            publication_urls,
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }
}
