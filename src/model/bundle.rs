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

/// How a local bundle artifact's ledger relates to a remote copy of the same
/// bundle, decided purely from their node-id sets. The ledger invariant across
/// replicas is that node sets only grow: a replica may be refreshed from
/// another exactly when the other's set contains everything it records. (Node
/// order is display-level; concurrent merges may interleave it differently.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerRelation {
    /// Both ledgers record the same node set; nothing to refresh.
    Equal,
    /// The remote records every local node plus more: the local artifact can
    /// be replaced by the remote without losing recorded work.
    RemoteAhead,
    /// The local ledger records every remote node plus more: local has work
    /// the remote has not seen. Keep local; a later push reconciles the remote.
    LocalAhead,
    /// Each side records nodes the other lacks: the ledgers have diverged and
    /// need a union merge (`merge_ledgers`).
    Diverged,
}

/// Decide how a local bundle ledger relates to a remote one by comparing their
/// node-id sets. Pure and total: it makes no I/O and treats two empty ledgers
/// as `Equal`. Subset/superset (not sequence prefix) is the deciding shape so
/// that two replicas which merged the same divergent ledgers in different
/// interleavings still compare `Equal`.
pub fn ledger_relation(local_node_ids: &[String], remote_node_ids: &[String]) -> LedgerRelation {
    use std::collections::BTreeSet;
    let local: BTreeSet<&String> = local_node_ids.iter().collect();
    let remote: BTreeSet<&String> = remote_node_ids.iter().collect();
    let local_only = local.difference(&remote).next().is_some();
    let remote_only = remote.difference(&local).next().is_some();
    match (local_only, remote_only) {
        (false, false) => LedgerRelation::Equal,
        (false, true) => LedgerRelation::RemoteAhead,
        (true, false) => LedgerRelation::LocalAhead,
        (true, true) => LedgerRelation::Diverged,
    }
}

impl ChangeGroup {
    /// The ordered node-id sequence of this bundle's ledger, used to compare
    /// local and remote copies for refresh and merge decisions.
    pub fn node_id_sequence(&self) -> Vec<String> {
        self.nodes.iter().map(|node| node.id.clone()).collect()
    }
}

/// Union-merge two diverged copies of the same bundle into one artifact that
/// records every node either side has seen, restoring the grow-only ledger
/// invariant. Pure: git-level reconciliation of the feature branches is the
/// caller's concern.
///
/// Deterministic by construction — nodes and commit groups are ordered by
/// `(created_at, id)` — so two users merging the same pair of ledgers produce
/// identical artifacts and later compare `Equal`.
///
/// Local wins wherever both sides describe the same thing: repo entries keep
/// this workspace's paths and recorded heads (remote-only repos are appended;
/// callers localize the remote copy first). Publications keep one record per
/// repo, preferring the most recently updated. A terminal lifecycle state on
/// either side wins over `open`, so a landed/archived bundle never reopens by
/// being merged with a stale open copy.
pub fn merge_ledgers(local: &ChangeGroup, remote: &ChangeGroup, now: String) -> ChangeGroup {
    use std::collections::BTreeSet;
    let mut merged = local.clone();

    let known: BTreeSet<String> = local.nodes.iter().map(|node| node.id.clone()).collect();
    let mut nodes = local.nodes.clone();
    nodes.extend(
        remote
            .nodes
            .iter()
            .filter(|node| !known.contains(&node.id))
            .cloned(),
    );
    nodes.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    merged.head_node_id = nodes.last().map(|node| node.id.clone());
    merged.nodes = nodes;

    let known: BTreeSet<String> = local
        .commit_groups
        .iter()
        .map(|group| group.id.clone())
        .collect();
    let mut commit_groups = local.commit_groups.clone();
    commit_groups.extend(
        remote
            .commit_groups
            .iter()
            .filter(|group| !known.contains(&group.id))
            .cloned(),
    );
    commit_groups.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    merged.commit_groups = commit_groups;

    for repo in &remote.repos {
        if !merged
            .repos
            .iter()
            .any(|local_repo| local_repo.id == repo.id)
        {
            merged.repos.push(repo.clone());
        }
    }

    for publication in &remote.publications {
        match merged
            .publications
            .iter_mut()
            .find(|existing| existing.repo_id == publication.repo_id)
        {
            Some(existing) => {
                if publication.updated_at > existing.updated_at {
                    *existing = publication.clone();
                }
            }
            None => merged.publications.push(publication.clone()),
        }
    }

    for work_item_id in &remote.work_item_ids {
        if !merged.work_item_ids.contains(work_item_id) {
            merged.work_item_ids.push(work_item_id.clone());
        }
    }

    let local_terminal = !matches!(merged.state, None | Some(BundleState::Open));
    let remote_terminal = !matches!(remote.state, None | Some(BundleState::Open));
    if !local_terminal && remote_terminal {
        merged.state = remote.state;
        merged.closed_at = remote.closed_at.clone();
        merged.archived_at = remote.archived_at.clone();
        merged.deleted_at = remote.deleted_at.clone();
    }

    merged.updated_at = now;
    merged
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

/// The acting human behind a ledger node on a shared environment, from the
/// T3_ACTOR_* variables the environment server exports per provider session.
/// Distinct from `session_id` (the conversation) and from git authorship
/// (which records identity per commit): the actor names who drove the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeActor {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
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
    /// Opaque identity of the session that produced this node. Agent
    /// harnesses set KNIT_SESSION per conversation so the ledger records
    /// which conversation drove the work; absent for plain CLI use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Acting human on a shared environment, when the harness exports
    /// T3_ACTOR_*; absent for plain CLI use and single-user setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<NodeActor>,
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

/// Ambient session identity for ledger attribution, from KNIT_SESSION.
fn ambient_session_id() -> Option<String> {
    std::env::var("KNIT_SESSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Ambient acting-human identity, from the T3_ACTOR_* contract.
fn ambient_actor() -> Option<NodeActor> {
    non_empty_env("T3_ACTOR_SESSION").map(|session| NodeActor {
        session,
        label: non_empty_env("T3_ACTOR_LABEL"),
        email: non_empty_env("T3_ACTOR_EMAIL"),
    })
}

impl BundleNode {
    /// Every node starts here so cross-cutting attribution (session id)
    /// is recorded uniformly regardless of which command created it.
    fn base(id: String, node_type: &str, created_at: String) -> Self {
        Self {
            id,
            node_type: node_type.to_string(),
            created_at,
            session_id: ambient_session_id(),
            actor: ambient_actor(),
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
            repo_changes: Vec::new(),
        }
    }

    pub fn feature_created(feature_id: String, created_at: String, title: String) -> Self {
        Self {
            title: Some(title),
            ..Self::base(feature_id, "feature.created", created_at)
        }
    }

    pub fn repos_added(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            repo_ids: Some(repo_ids),
            ..Self::base(id, "repo.added", created_at)
        }
    }

    pub fn repos_removed(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            repo_ids: Some(repo_ids),
            ..Self::base(id, "repo.removed", created_at)
        }
    }

    pub fn worktrees_materialized(id: String, created_at: String, repo_ids: Vec<String>) -> Self {
        Self {
            repo_ids: Some(repo_ids),
            ..Self::base(id, "worktree.materialized", created_at)
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
            commit_group_id: Some(group_id.clone()),
            message: Some(message),
            commits,
            repo_changes,
            ..Self::base(group_id, "commit.group", created_at)
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
            commit_group_id: Some(group_id.clone()),
            message: Some(message),
            target_node_id: Some(target_node_id),
            commits,
            repo_changes,
            ..Self::base(group_id, "revert.group", created_at)
        }
    }

    pub fn git_observed(id: String, created_at: String, repo_changes: Vec<RepoChange>) -> Self {
        Self {
            repo_changes,
            ..Self::base(id, "git.observed", created_at)
        }
    }

    pub fn land_update(
        id: String,
        created_at: String,
        provider: String,
        repo_changes: Vec<RepoChange>,
    ) -> Self {
        Self {
            message: Some("updated feature branches from base".to_string()),
            provider: Some(provider),
            repo_changes,
            ..Self::base(id, "land.update", created_at)
        }
    }

    /// A recorded check verdict: a named check (`ci`, `functional`, ...)
    /// passed or failed against the exact per-repo heads in `pins`. `title`
    /// carries the check name, `message` the verdict (`pass — ...` /
    /// `fail — ...`), and `commits` the head pins the verdict applies to.
    pub fn check_recorded(
        id: String,
        created_at: String,
        name: String,
        message: String,
        pins: Vec<CommitRef>,
    ) -> Self {
        Self {
            title: Some(name),
            message: Some(message),
            commits: pins,
            ..Self::base(id, "check.recorded", created_at)
        }
    }

    pub fn feature_archived(id: String, created_at: String, reason: Option<String>) -> Self {
        Self {
            message: reason,
            ..Self::base(id, "feature.archived", created_at)
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
            repo_ids: Some(repo_ids),
            plan_id: Some(plan_id),
            run_id: Some(run_id),
            provider: Some(provider),
            publication_urls,
            ..Self::base(id, "feature.landed", created_at)
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
            repo_ids: Some(repo_ids),
            message: Some(message),
            target_node_id: Some(target_node_id),
            provider: Some(provider),
            publication_urls,
            ..Self::base(id, "pr.revert", created_at)
        }
    }
}
