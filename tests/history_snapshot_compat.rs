mod common;

use common::unique_temp_dir;
use knit::history_query::{query_bundle_history, HistoryQuery};
use knit::model::{BundleNode, ChangeGroup, CommitGroup, CommitRef};

#[test]
fn legacy_commit_groups_remain_inspectable_without_nodes_or_git() {
    let root = unique_temp_dir();
    let mut bundle = ChangeGroup::new(
        "legacy".into(),
        "Legacy history".into(),
        "2026-01-01T00:00:00Z".into(),
    );
    bundle.nodes.clear();
    bundle.commit_groups.push(CommitGroup {
        id: "group-legacy".into(),
        message: "Preserved legacy commit".into(),
        created_at: "2026-01-02T00:00:00Z".into(),
        commits: vec![CommitRef {
            repo_id: "api".into(),
            sha: "a".repeat(40),
        }],
        author: None,
    });
    let entries = query_bundle_history(&root, &bundle, &HistoryQuery::default()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "group-legacy");
    assert_eq!(entries[0].events[0].repo_id.as_deref(), Some("api"));
    assert!(!root.join(".knit/history").exists());
}

#[test]
fn checkpoint_without_commit_pins_remains_a_queryable_log_entry() {
    let root = unique_temp_dir();
    let mut bundle = ChangeGroup::new(
        "local".into(),
        "Local history".into(),
        "2026-01-01T00:00:00Z".into(),
    );
    bundle.nodes.push(BundleNode::checkpoint(
        "checkpoint-local".into(),
        "2026-01-02T00:00:00Z".into(),
        "Checkpoint local work".into(),
        vec!["api".into()],
        "group-local".into(),
    ));
    let query = HistoryQuery {
        repos: Some(vec!["api".into()]),
        ..Default::default()
    };
    let entries = query_bundle_history(&root, &bundle, &query).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "checkpoint-local");
    assert_eq!(entries[0].message, "Checkpoint local work");
    assert!(!root.join(".knit/history").exists());
}
