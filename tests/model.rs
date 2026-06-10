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
