use knit::model::ChangeGroup;

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
