use super::{policy::*, remote::PublishJob};
use crate::model::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    bundle: ChangeGroup,
    project: KnitProject,
    jobs: Vec<PublishJob>,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "knit-publish-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut bundle = ChangeGroup::new(
            "feature".into(),
            "Bundle title".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.repos = ["library","consumer","independent"].iter().map(|id| serde_json::from_value(json!({"id":id,"path":root.join(id),"remote":format!("https://github.com/example/{id}.git"),"baseBranch":"main","featureBranch":"knit/feature","headSha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})).unwrap()).collect();
        bundle.repos[0].source_remote = Some("https://github.com/contributor/library.git".into());
        bundle.repos[0].target_remote = bundle.repos[0].remote.clone();
        bundle.commit_groups.push(serde_json::from_value(json!({"id":"kg-1","createdAt":"2026-01-01T00:00:00Z","message":"Head group title\n\nDetails","commits":[{"repoId":"library","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]})).unwrap());
        let jobs = bundle
            .repos
            .iter()
            .enumerate()
            .map(|(repo_index, repo)| PublishJob {
                repo_index,
                repo: repo.clone(),
                base_branch: "main".into(),
            })
            .collect();
        let project = KnitProject::new("example".into(), "2026-01-01T00:00:00Z".into());
        Self {
            root,
            bundle,
            project,
            jobs,
        }
    }
    fn resolve(
        &self,
        options: &PublishOptions,
    ) -> anyhow::Result<BTreeMap<String, ResolvedPublish>> {
        resolve(
            &self.bundle,
            Some(&self.project),
            &self.root,
            &BTreeMap::new(),
            &self.jobs,
            false,
            options,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn no_config_keeps_original_title_body_and_ready_default() {
    let f = Fixture::new();
    let resolved = f.resolve(&PublishOptions::default()).unwrap();
    for job in &f.jobs {
        let r = &resolved[&job.repo.id];
        assert!(!r.draft);
        assert_eq!(r.title, format!("Bundle title ({})", job.repo.id));
        assert_eq!(
            r.body(&f.bundle, &job.repo, "github"),
            super::pr_body::initial_pr_body(&f.bundle, &job.repo.id, "github", &r.blocked_on)
        );
    }
}
#[test]
fn layered_overrides_and_cli_conflicts_are_explicit() {
    let mut f = Fixture::new();
    f.project.publish=Some(serde_json::from_value(json!({"draft":"all","title":"commit-group","repos":{"consumer":{"draft":true,"title":"Project literal"}}})).unwrap());
    f.project.repos = vec![serde_json::from_value(
        json!({"id":"consumer","repo":f.bundle.repos[1],"publish":{"draft":false}}),
    )
    .unwrap_or_else(|_| {
        // Project entries flatten the repository fields.
        let mut v = serde_json::to_value(&f.bundle.repos[1]).unwrap();
        v["publish"] = json!({"draft":false});
        serde_json::from_value(v).unwrap()
    })];
    let r = f.resolve(&PublishOptions::default()).unwrap();
    assert!(r["consumer"].draft);
    assert_eq!(r["library"].title, "Head group title");
    f.project.publish.as_mut().unwrap().repos.clear();
    assert!(!f.resolve(&PublishOptions::default()).unwrap()["consumer"].draft);
    f.bundle.publish =
        Some(serde_json::from_value(json!({"draft":"none","title":"bundle-title"})).unwrap());
    let options = PublishOptions {
        draft_repo: vec!["consumer".into()],
        title: vec!["consumer=CLI title".into()],
        ..Default::default()
    };
    let r = f.resolve(&options).unwrap();
    assert!(!r["library"].draft);
    assert!(r["consumer"].draft);
    assert_eq!(r["consumer"].title, "CLI title");
    let options = PublishOptions {
        ready: vec!["consumer".into()],
        draft_repo: vec!["consumer".into()],
        ..Default::default()
    };
    assert!(f
        .resolve(&options)
        .unwrap_err()
        .to_string()
        .contains("Conflicting"));
    assert!(resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &BTreeMap::new(),
        &f.jobs,
        true,
        &PublishOptions {
            ready: vec!["consumer".into()],
            ..Default::default()
        }
    )
    .is_err());
}
#[test]
fn cargo_fork_patch_requires_matching_feature_branch_and_orders_library_first() {
    let mut f = Fixture::new();
    f.project.publish=Some(serde_json::from_value(json!({"draft":"dependents","title":"file","body":{"file":"PR-{repo}.md","fallback":"knit"}})).unwrap());
    for id in ["library", "consumer", "independent"] {
        std::fs::write(
            f.root.join(format!("PR-{id}.md")),
            format!("Title: Review {id}\nAuthored text for {id}\n"),
        )
        .unwrap();
    }
    let consumer = f.root.join("consumer");
    std::fs::create_dir_all(&consumer).unwrap();
    std::fs::write(consumer.join("Cargo.toml"),"[package]\nname='consumer'\nversion='0.1.0'\n[patch.crates-io]\nlibrary={git='https://github.com/contributor/library.git', branch='knit/feature'}\n").unwrap();
    let paths = BTreeMap::from([("consumer".into(), consumer.clone())]);
    let r = resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &paths,
        &f.jobs,
        false,
        &PublishOptions::default(),
    )
    .unwrap();
    assert!(!r["library"].draft);
    assert!(r["consumer"].draft);
    assert!(!r["independent"].draft);
    assert_eq!(r["consumer"].title, "Review consumer");
    assert!(!r["consumer"].body.contains("Title:"));
    let waves = waves(f.jobs.iter().map(|j| j.repo.id.clone()), &r).unwrap();
    assert!(waves[0].contains("library"));
    assert!(waves[1].contains("consumer"));
    f.project.publish.as_mut().unwrap().title = Some(PublishTitle::BundleTitle);
    let authored = "Authored text for consumer  \n\n";
    std::fs::write(f.root.join("PR-consumer.md"), authored).unwrap();
    let r = resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &paths,
        &f.jobs,
        false,
        &PublishOptions::default(),
    )
    .unwrap();
    let preview = r["consumer"].body(&f.bundle, &f.bundle.repos[1], "github");
    let (prefix, block) = preview
        .split_once(super::pr_body::KNIT_PR_BLOCK_BEGIN)
        .unwrap();
    assert_eq!(prefix, format!("{authored}\n\n"));
    assert!(block.contains("\nBlocked on library\n"));
    f.bundle.publications.push(serde_json::from_value(json!({"provider":"github","kind":"pull_request","repoId":"library","number":12,"url":"https://github.com/example/library/pull/12","state":"OPEN","headBranch":"knit/feature","baseBranch":"main","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"})).unwrap());
    for provider in ["github", "bitbucket"] {
        let body = r["consumer"].body(&f.bundle, &f.bundle.repos[1], provider);
        let block = super::pr_body::initial_pr_body(
            &f.bundle,
            "consumer",
            provider,
            &r["consumer"].blocked_on,
        );
        assert_eq!(body, format!("{authored}\n\n{block}"));
        assert!(
            block.contains("Blocked on [library #12](https://github.com/example/library/pull/12)")
        );
        let synced = super::pr_body::sync_knit_pr_body(&f.bundle, "consumer", provider, &body);
        assert_eq!(synced, body);
    }
    let synced = super::pr_body::sync_knit_pr_body(&f.bundle, "consumer", "github", &preview);
    assert_eq!(
        synced,
        r["consumer"].body(&f.bundle, &f.bundle.repos[1], "github")
    );
    f.bundle.publications[0].number = 13;
    f.bundle.publications[0].url = "https://github.com/example/library/pull/13".into();
    let synced = super::pr_body::sync_knit_pr_body(&f.bundle, "consumer", "github", &synced);
    assert_eq!(
        synced,
        r["consumer"].body(&f.bundle, &f.bundle.repos[1], "github")
    );
    assert!(synced.contains("Blocked on [library #13](https://github.com/example/library/pull/13)"));
    assert!(!synced.contains("/pull/12"));
    let mut without_library = f.bundle.clone();
    without_library.repos.retain(|repo| repo.id != "library");
    without_library.publications.clear();
    let synced = super::pr_body::sync_knit_pr_body(&without_library, "consumer", "github", &synced);
    let (prefix, block) = synced
        .split_once(super::pr_body::KNIT_PR_BLOCK_BEGIN)
        .unwrap();
    assert_eq!(prefix, format!("{authored}\n\n"));
    assert!(block.contains("\nBlocked on library\n"));
    std::fs::write(consumer.join("Cargo.toml"),"[dependencies]\nlibrary={git='https://github.com/contributor/library.git', branch='main'}\n").unwrap();
    assert!(
        !resolve(
            &f.bundle,
            Some(&f.project),
            &f.root,
            &paths,
            &f.jobs,
            false,
            &PublishOptions::default()
        )
        .unwrap()["consumer"]
            .draft
    );
}
#[test]
fn landing_dependencies_lists_body_and_schema_roundtrip() {
    let mut f = Fixture::new();
    f.project.publish=Some(serde_json::from_value(json!({"draft":"dependents","future":{"flag":true},"body":{"fallback":"knit","extension":7},"repos":{"consumer":{"future":42}}})).unwrap());
    f.project.landing = Some(
        serde_json::from_value(
            json!({"dependencies":[{"library":"library","consumers":["consumer"]}]}),
        )
        .unwrap(),
    );
    let r = f.resolve(&PublishOptions::default()).unwrap();
    assert!(r["consumer"].draft);
    assert!(!r["independent"].draft);
    f.bundle.publish = f.project.publish.clone();
    for (value, schema) in [
        (
            serde_json::to_value(&f.project).unwrap(),
            include_str!("../../../schemas/project.schema.json"),
        ),
        (
            serde_json::to_value(&f.bundle).unwrap(),
            include_str!("../../../schemas/bundle.schema.json"),
        ),
    ] {
        assert_eq!(value["publish"]["future"]["flag"], true);
        let validator = jsonschema::validator_for(&serde_json::from_str(schema).unwrap()).unwrap();
        let errors = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "{errors:?}");
    }
    let decoded: ChangeGroup =
        serde_json::from_value(serde_json::to_value(&f.bundle).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(decoded.publish).unwrap()["repos"]["consumer"]["future"],
        42
    );
}
#[test]
fn cycle_is_detected_before_execution() {
    let r = BTreeMap::from([
        (
            "a".into(),
            ResolvedPublish {
                blocked_on: BTreeSet::from(["b".into()]),
                ..Default::default()
            },
        ),
        (
            "b".into(),
            ResolvedPublish {
                blocked_on: BTreeSet::from(["a".into()]),
                ..Default::default()
            },
        ),
    ]);
    assert!(waves(r.keys().cloned(), &r).is_err());
}

#[test]
fn release_gate_receipt_must_match_library_revision_and_review() {
    let mut f = Fixture::new();
    f.project.publish = Some(serde_json::from_value(json!({"draft":"dependents"})).unwrap());
    f.project.landing=Some(serde_json::from_value(json!({"dependencies":[{"library":"library","consumers":["consumer"],"release":{"instructions":"Publish library release"}}]})).unwrap());
    f.bundle.publications.push(serde_json::from_value(json!({"provider":"github","kind":"pull_request","repoId":"library","number":12,"url":"https://github.com/example/library/pull/12","state":"MERGED","headBranch":"knit/feature","baseBranch":"main","updatedAt":"2026-01-01T00:00:00Z"})).unwrap());
    let body_root = f.root.join(".knit/worktrees/feature");
    let runs = f.root.join(".knit/land-runs");
    std::fs::create_dir_all(&runs).unwrap();
    let resolve_now = |bundle: &ChangeGroup| {
        resolve(
            bundle,
            Some(&f.project),
            &body_root,
            &BTreeMap::new(),
            &f.jobs,
            false,
            &PublishOptions::default(),
        )
        .unwrap()
    };
    assert!(resolve_now(&f.bundle)["consumer"].draft);
    std::fs::write(runs.join("release.run.json"),serde_json::to_vec(&json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":"run-feature","bundleId":"feature","plan":{"bundleId":"feature","bundleHeads":{"library":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"sourceBundle":f.bundle,"steps":[{"id":"release-library","repoId":"library","type":"manual","status":"succeeded"}],"acknowledgements":{"release-library":{"at":"2026-01-01T00:00:00Z","notes":"Published"}}})).unwrap()).unwrap();
    assert!(!resolve_now(&f.bundle)["consumer"].draft);
    let mut changed = f.bundle.clone();
    changed.repos[0].head_sha = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
    assert!(resolve_now(&changed)["consumer"].draft);
}

#[test]
fn only_effective_dependents_policy_detects_dependencies_or_cycles() {
    let mut f = Fixture::new();
    f.project.landing=Some(serde_json::from_value(json!({"dependencies":[{"library":"library","consumers":["consumer"]},{"library":"consumer","consumers":["library"]}]})).unwrap());
    for policy in [
        json!({"title":"bundle-title"}),
        json!({"draft":"none"}),
        json!({"draft":"all"}),
        json!({"draft":["consumer"]}),
    ] {
        f.project.publish = Some(serde_json::from_value(policy).unwrap());
        let result = f.resolve(&PublishOptions::default()).unwrap();
        assert!(result.values().all(|r| r.blocked_on.is_empty()));
    }
    f.project.publish = Some(serde_json::from_value(json!({"draft":"dependents"})).unwrap());
    assert!(f
        .resolve(&PublishOptions::default())
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    for options in [
        PublishOptions {
            ready: vec!["consumer".into()],
            ..Default::default()
        },
        PublishOptions {
            draft_repo: vec!["consumer".into()],
            ..Default::default()
        },
    ] {
        assert!(f.resolve(&options).unwrap()["consumer"]
            .blocked_on
            .is_empty());
    }
    for draft in [false, true] {
        f.bundle.publish =
            Some(serde_json::from_value(json!({"repos":{"consumer":{"draft":draft}}})).unwrap());
        assert!(f.resolve(&PublishOptions::default()).unwrap()["consumer"]
            .blocked_on
            .is_empty());
    }
    f.bundle.publish = None;
    for draft in [false, true] {
        f.project.publish.as_mut().unwrap().repos.insert(
            "consumer".into(),
            serde_json::from_value(json!({"draft":draft})).unwrap(),
        );
        assert!(f.resolve(&PublishOptions::default()).unwrap()["consumer"]
            .blocked_on
            .is_empty());
        f.project.publish.as_mut().unwrap().repos.clear();
        let mut legacy = serde_json::to_value(&f.bundle.repos[1]).unwrap();
        legacy["publish"] = json!({"draft":draft});
        f.project.repos = vec![serde_json::from_value(legacy).unwrap()];
        assert!(f.resolve(&PublishOptions::default()).unwrap()["consumer"]
            .blocked_on
            .is_empty());
        f.project.repos.clear();
    }
    assert!(resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &BTreeMap::new(),
        &f.jobs,
        true,
        &PublishOptions::default()
    )
    .unwrap()
    .values()
    .all(|r| r.blocked_on.is_empty()));
}

#[test]
fn consumer_only_cargo_selection_recognizes_upstream_target_and_retains_library() {
    let mut f = Fixture::new();
    f.project.publish = Some(serde_json::from_value(json!({"draft":"dependents"})).unwrap());
    f.bundle.repos[0].remote = Some("https://github.com/contributor/library.git".into());
    f.bundle.repos[0].target_remote = Some("https://github.com/upstream/library.git".into());
    f.bundle.publications.push(serde_json::from_value(json!({"provider":"github","kind":"pull_request","repoId":"library","number":12,"url":"https://github.com/upstream/library/pull/12","state":"OPEN","headBranch":"knit/feature","baseBranch":"main","updatedAt":"2026-01-01T00:00:00Z"})).unwrap());
    let consumer = f.root.join("consumer");
    std::fs::create_dir_all(&consumer).unwrap();
    std::fs::write(consumer.join("Cargo.toml"),"[dependencies]\nlibrary={git='https://github.com/upstream/library.git',branch='knit/feature'}\n").unwrap();
    let paths = BTreeMap::from([("consumer".into(), consumer)]);
    f.jobs.retain(|j| j.repo.id == "consumer");
    let result = resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &paths,
        &f.jobs,
        false,
        &PublishOptions::default(),
    )
    .unwrap();
    assert!(result["consumer"].draft);
    assert!(result["consumer"].blocked_on.contains("library"));
    let body = result["consumer"].body(&f.bundle, &f.jobs[0].repo, "github");
    assert!(body.contains("Blocked on [library #12](https://github.com/upstream/library/pull/12)"));
    std::fs::write(paths["consumer"].join("Cargo.toml"), "invalid TOML [").unwrap();
    let options = PublishOptions {
        ready: vec!["consumer".into()],
        ..Default::default()
    };
    assert!(resolve(
        &f.bundle,
        Some(&f.project),
        &f.root,
        &paths,
        &f.jobs,
        false,
        &options
    )
    .unwrap()["consumer"]
        .blocked_on
        .is_empty());
}

#[test]
fn only_leading_title_line_is_metadata() {
    let mut f = Fixture::new();
    f.project.publish =
        Some(serde_json::from_value(json!({"body":{"file":"PR-{repo}.md"}})).unwrap());
    for text in [
        "Author introduction\nTitle: prose belongs to the author\n",
        "\nTitle: also prose\n",
    ] {
        std::fs::write(f.root.join("PR-consumer.md"), text).unwrap();
        assert_eq!(
            f.resolve(&PublishOptions::default()).unwrap()["consumer"].body,
            text
        );
    }
    std::fs::write(
        f.root.join("PR-consumer.md"),
        "Title: Metadata title\nText\nTitle: Later authored line\n",
    )
    .unwrap();
    assert_eq!(
        f.resolve(&PublishOptions::default()).unwrap()["consumer"].body,
        "Text\nTitle: Later authored line\n"
    );
}

#[test]
fn publish_schema_and_typed_model_agree_on_null_duplicates_and_invalid_values() {
    let f = Fixture::new();
    let cases = [
        (json!(null), true),
        (json!({"draft":null,"title":null,"body":null}), true),
        (json!({"draft":["consumer","consumer"]}), true), // Repeated selectors are idempotent.
        (
            json!({"body":{"file":null,"fallback":null},"repos":{"consumer":{"draft":null,"title":null,"bodyFile":null}}}),
            true,
        ),
        (json!({"draft":"sometimes"}), false),
        (json!({"draft":[1]}), false),
        (json!({"draft":true}), false),
        (json!({"title":"literal"}), false),
        (json!({"body":{"file":false}}), false),
        (json!({"body":{"fallback":"other"}}), false),
        (json!({"repos":null}), false),
        (json!({"repos":{"consumer":null}}), false),
        (json!({"repos":{"consumer":{"draft":"yes"}}}), false),
    ];
    for (project, schema) in [
        (true, include_str!("../../../schemas/project.schema.json")),
        (false, include_str!("../../../schemas/bundle.schema.json")),
    ] {
        let validator = jsonschema::validator_for(&serde_json::from_str(schema).unwrap()).unwrap();
        for (policy, accepted) in &cases {
            let mut document = if project {
                serde_json::to_value(&f.project).unwrap()
            } else {
                serde_json::to_value(&f.bundle).unwrap()
            };
            document["publish"] = policy.clone();
            let typed_ok = if project {
                serde_json::from_value::<KnitProject>(document.clone()).is_ok()
            } else {
                serde_json::from_value::<ChangeGroup>(document.clone()).is_ok()
            };
            assert_eq!(typed_ok, *accepted, "typed project={project}: {policy}");
            assert_eq!(
                validator.is_valid(&document),
                *accepted,
                "schema project={project}: {policy}"
            );
        }
    }
}
