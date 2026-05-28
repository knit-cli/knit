use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: &str = "0.1";
pub const DEFAULT_LANDING_MERGE_METHOD: &str = "merge";
pub const CHANGE_GROUP_KIND: &str = "ChangeGroup";
pub const CHECKOUT_MODE_WORKTREE: &str = "worktree";
pub const CHECKOUT_MODE_IN_PLACE: &str = "inPlace";
pub const BUNDLE_STATE_OPEN: &str = "open";
pub const BUNDLE_STATE_CLOSED: &str = "closed";
pub const BUNDLE_STATE_ARCHIVED: &str = "archived";
pub const BUNDLE_STATE_DELETED: &str = "deleted";
pub const WORK_ITEM_KIND: &str = "KnitWorkItem";
pub const ORG_KIND: &str = "KnitOrg";
pub const WORK_ITEM_PLANNING_UNPLANNED: &str = "unplanned";
pub const WORK_ITEM_PLANNING_PLOTTED: &str = "plotted";
pub const WORK_ITEM_PLANNING_APPROVED: &str = "approved";
pub const WORK_ITEM_PLANNING_STALE: &str = "stale";
pub const WORK_ITEM_EXECUTION_IDLE: &str = "idle";
pub const WORK_ITEM_EXECUTION_CLAIMED: &str = "claimed";
pub const WORK_ITEM_EXECUTION_RUNNING: &str = "running";
pub const WORK_ITEM_EXECUTION_WAITING_REVIEW: &str = "waiting_review";
pub const WORK_ITEM_EXECUTION_WAITING_LAND: &str = "waiting_land";
pub const WORK_ITEM_EXECUTION_LANDED: &str = "landed";
pub const WORK_ITEM_EXECUTION_FAILED: &str = "failed";
pub const WORK_ITEM_EXECUTION_CANCELED: &str = "canceled";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitConfig {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_remote: Option<String>,
    #[serde(default = "default_advice")]
    pub advice: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remotes: BTreeMap<String, KnitRemote>,
}

impl KnitConfig {
    pub fn new(active_bundle: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle: Some(active_bundle),
            active_project: None,
            sync_remote: None,
            advice: true,
            remotes: BTreeMap::new(),
        }
    }

    pub fn new_project(active_project: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle: None,
            active_project: Some(active_project),
            sync_remote: None,
            advice: true,
            remotes: BTreeMap::new(),
        }
    }
}

fn default_advice() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitRemote {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
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
            landing: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitOrg {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub repos: Vec<OrgRepoEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl KnitOrg {
    pub fn new(id: String, name: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: ORG_KIND.to_string(),
            id,
            name,
            created_at: now.clone(),
            updated_at: now,
            repos: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default = "default_checkout_mode")]
    pub checkout_mode: String,
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
    pub method: Option<String>,
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
    pub mode: Option<String>,
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
    pub update: Option<String>,
}

fn default_include_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitContexts {
    pub schema_version: String,
    #[serde(default)]
    pub contexts: Vec<KnitContextEntry>,
}

impl KnitContexts {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            contexts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitContextEntry {
    pub path: String,
    pub active_bundle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeGroup {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
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
            state: Some(BUNDLE_STATE_OPEN.to_string()),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitWorkItem {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub item_kind: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub planning_status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plotted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    pub execution_status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundle_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl KnitWorkItem {
    pub fn new(id: String, title: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: WORK_ITEM_KIND.to_string(),
            id,
            item_kind: "feature".to_string(),
            title,
            description: String::new(),
            acceptance_criteria: Vec::new(),
            org_id: None,
            project_id: None,
            repo_hints: Vec::new(),
            priority: None,
            labels: Vec::new(),
            planning_status: WORK_ITEM_PLANNING_UNPLANNED.to_string(),
            depends_on: Vec::new(),
            lane: None,
            rank: None,
            planner: None,
            planning_rationale: None,
            plotted_at: None,
            approved_at: None,
            execution_status: WORK_ITEM_EXECUTION_IDLE.to_string(),
            bundle_ids: Vec::new(),
            claim_id: None,
            target: None,
            last_outcome: None,
            created_at: now.clone(),
            updated_at: now,
            metadata: BTreeMap::new(),
        }
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
            plan_id: None,
            run_id: None,
            provider: None,
            publication_urls: Vec::new(),
            commits: Vec::new(),
            repo_changes: Vec::new(),
        }
    }

    pub fn feature_closed(id: String, created_at: String, reason: Option<String>) -> Self {
        Self {
            id,
            node_type: "feature.closed".to_string(),
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
}
