use knit::status::{has_staged_changes, status_label};

#[test]
fn labels_status() {
    assert_eq!(status_label(""), "clean");
    assert_eq!(status_label(" M src/main.rs"), "modified");
    assert_eq!(status_label("M  src/main.rs"), "staged");
    assert_eq!(status_label("MM src/main.rs"), "staged+modified");
    assert_eq!(status_label("?? scratch.txt"), "modified");
}

#[test]
fn detects_staged_changes() {
    assert!(!has_staged_changes(""));
    assert!(!has_staged_changes(" M src/main.rs"));
    assert!(!has_staged_changes("?? scratch.txt"));
    assert!(has_staged_changes("A  src/main.rs"));
}
