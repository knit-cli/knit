use knit::model::{ledger_relation, ChangeGroup, LedgerRelation};

fn seq(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[test]
fn ledger_relation_equal_sequences() {
    assert_eq!(
        ledger_relation(&seq(&["a", "b"]), &seq(&["a", "b"])),
        LedgerRelation::Equal
    );
    // Two empty ledgers are trivially equal.
    assert_eq!(ledger_relation(&[], &[]), LedgerRelation::Equal);
}

#[test]
fn ledger_relation_remote_strict_prefix_is_remote_ahead() {
    assert_eq!(
        ledger_relation(&seq(&["a"]), &seq(&["a", "b"])),
        LedgerRelation::RemoteAhead
    );
    // Empty local with non-empty remote is also remote-ahead.
    assert_eq!(
        ledger_relation(&[], &seq(&["a"])),
        LedgerRelation::RemoteAhead
    );
}

#[test]
fn ledger_relation_local_strict_prefix_is_local_ahead() {
    assert_eq!(
        ledger_relation(&seq(&["a", "b", "c"]), &seq(&["a", "b"])),
        LedgerRelation::LocalAhead
    );
}

#[test]
fn ledger_relation_mismatched_node_diverges() {
    // Same length, differing at a position.
    assert_eq!(
        ledger_relation(&seq(&["a", "x"]), &seq(&["a", "y"])),
        LedgerRelation::Diverged
    );
    // Different lengths but neither is a prefix of the other.
    assert_eq!(
        ledger_relation(&seq(&["a", "x"]), &seq(&["a", "y", "z"])),
        LedgerRelation::Diverged
    );
}

#[test]
fn node_id_sequence_follows_ledger_order() {
    let bundle = ChangeGroup::new(
        "venue-capacity".to_string(),
        "venue capacity".to_string(),
        "2026-05-05T00:00:00.000Z".to_string(),
    );
    assert_eq!(bundle.node_id_sequence(), seq(&["venue-capacity"]));
}

#[test]
fn new_change_group_starts_node_chain() {
    let bundle = ChangeGroup::new(
        "venue-capacity".to_string(),
        "venue capacity".to_string(),
        "2026-05-05T00:00:00.000Z".to_string(),
    );

    assert_eq!(bundle.head_node_id.as_deref(), Some("venue-capacity"));
    assert_eq!(bundle.nodes.len(), 1);
    assert_eq!(bundle.nodes[0].node_type, "feature.created");
    assert_eq!(bundle.nodes[0].title.as_deref(), Some("venue capacity"));
}

#[test]
fn ledger_relation_is_order_insensitive() {
    // Two replicas that merged the same divergent ledgers in different
    // interleavings record the same node set and compare Equal.
    assert_eq!(
        ledger_relation(&seq(&["a", "b"]), &seq(&["b", "a"])),
        LedgerRelation::Equal
    );
    // Supersets are ahead regardless of ordering.
    assert_eq!(
        ledger_relation(&seq(&["b", "a", "c"]), &seq(&["a", "b"])),
        LedgerRelation::LocalAhead
    );
    assert_eq!(
        ledger_relation(&seq(&["x", "a"]), &seq(&["a", "x", "y"])),
        LedgerRelation::RemoteAhead
    );
}

mod merge_ledgers_tests {
    use knit::model::{merge_ledgers, BundleNode, BundleState, ChangeGroup, LedgerRelation};

    fn bundle_with_commit(node_id: &str, created_at: &str) -> ChangeGroup {
        let mut bundle = ChangeGroup::new(
            "venue-capacity".to_string(),
            "venue capacity".to_string(),
            "2026-06-01T00:00:00.000Z".to_string(),
        );
        bundle.nodes.push(BundleNode::commit_group(
            node_id.to_string(),
            created_at.to_string(),
            format!("message {node_id}"),
            Vec::new(),
            Vec::new(),
        ));
        bundle.head_node_id = Some(node_id.to_string());
        bundle
    }

    #[test]
    fn unions_nodes_and_is_deterministic() {
        let local = bundle_with_commit("kg_local", "2026-06-02T00:00:00.000Z");
        let remote = bundle_with_commit("kg_remote", "2026-06-03T00:00:00.000Z");

        let ours = merge_ledgers(&local, &remote, "2026-06-04T00:00:00.000Z".to_string());
        let theirs = merge_ledgers(&remote, &local, "2026-06-04T00:00:00.000Z".to_string());

        assert_eq!(
            ours.node_id_sequence(),
            vec!["venue-capacity", "kg_local", "kg_remote"]
        );
        // Both users converge on an identical sequence, so their artifacts
        // later compare Equal instead of re-diverging.
        assert_eq!(ours.node_id_sequence(), theirs.node_id_sequence());
        assert_eq!(
            knit::model::ledger_relation(
                &ours.node_id_sequence(),
                &theirs.node_id_sequence()
            ),
            LedgerRelation::Equal
        );
        assert_eq!(ours.head_node_id.as_deref(), Some("kg_remote"));
        assert_eq!(ours.commit_groups.len(), 0);
        assert_eq!(ours.updated_at, "2026-06-04T00:00:00.000Z");
    }

    #[test]
    fn terminal_state_wins_over_open() {
        let local = bundle_with_commit("kg_local", "2026-06-02T00:00:00.000Z");
        let mut remote = bundle_with_commit("kg_remote", "2026-06-03T00:00:00.000Z");
        remote.state = Some(BundleState::Archived);
        remote.archived_at = Some("2026-06-03T01:00:00.000Z".to_string());

        let merged = merge_ledgers(&local, &remote, "2026-06-04T00:00:00.000Z".to_string());
        assert_eq!(merged.state, Some(BundleState::Archived));
        assert_eq!(
            merged.archived_at.as_deref(),
            Some("2026-06-03T01:00:00.000Z")
        );

        // And the reverse: a terminal local state is never reopened by a stale
        // open remote copy.
        let merged = merge_ledgers(&remote, &local, "2026-06-04T00:00:00.000Z".to_string());
        assert_eq!(merged.state, Some(BundleState::Archived));
    }
}
