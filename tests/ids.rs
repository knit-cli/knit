use knit::ids::{short_sha, slugify, unique_repo_id};
use knit::model::{ChangeGroup, CheckoutMode, RepoEntry};

fn empty_bundle() -> ChangeGroup {
    ChangeGroup::new(
        "venue-capacity".to_string(),
        "venue capacity".to_string(),
        "2026-05-05T00:00:00.000Z".to_string(),
    )
}

#[test]
fn slugifies_titles() {
    assert_eq!(slugify("venue capacity"), "venue-capacity");
    assert_eq!(slugify(" Venue: Capacity! "), "venue-capacity");
    assert_eq!(slugify(""), "bundle");
}

#[test]
fn makes_unique_repo_ids() {
    let mut bundle = empty_bundle();
    assert_eq!(unique_repo_id(&bundle, "backend"), "backend");
    bundle.repos.push(RepoEntry {
        id: "backend".to_string(),
        path: "/tmp/backend".to_string(),
        remote: None,
        base_branch: "main".to_string(),
        checkout_mode: CheckoutMode::Worktree,
        base_sha: None,
        feature_branch: None,
        worktree_path: None,
        head_sha: None,
    });
    assert_eq!(unique_repo_id(&bundle, "backend"), "backend-2");
}

#[test]
fn shortens_shas() {
    assert_eq!(short_sha("abcdef1234567890"), "abcdef1");
    assert_eq!(short_sha(" abc "), "abc");
}
