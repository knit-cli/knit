//! The base ledger is observed Git history, independent of bundle lifecycle.
use super::*;

pub(super) fn events(
    root: &Path,
    project_id: &str,
    bundles: &[(PathBuf, ChangeGroup)],
) -> Result<Vec<HistoryEvent>> {
    let path = project_path(root, project_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let project: crate::model::KnitProject = crate::store::read_json(&path)?;
    let depth = project
        .history
        .as_ref()
        .and_then(|history| history.base_commit_depth);
    let mut events = Vec::new();
    for repo in project.repos {
        let checkout = PathBuf::from(&repo.path);
        let checkout = if checkout.is_absolute() {
            checkout
        } else {
            root.join(checkout)
        };
        // Never substitute a feature checkout's HEAD or an unpushed local base
        // for the configured remote base. Refresh is deliberately offline.
        let reference = if repo.remote.is_some() {
            format!("refs/remotes/origin/{}", repo.base_branch)
        } else {
            format!("refs/heads/{}", repo.base_branch)
        };
        if !checkout.exists() {
            continue;
        }
        let Ok(head) = crate::git::git_output(&checkout, ["rev-parse", "--verify", &reference])
        else {
            continue;
        };
        let head = head.trim();
        let limit = match depth {
            Some(crate::model::BaseCommitDepth::Count(count)) => Some(count),
            Some(crate::model::BaseCommitDepth::All(_)) => None,
            None => Some(anchor_depth(&checkout, head, &anchors(bundles, &repo.id))?),
        };
        let mut args = vec![
            "log".to_string(),
            "--first-parent".into(),
            "--no-show-signature".into(),
            "--format=%H%x00%P%x00%cI%x00%s".into(),
        ];
        if let Some(limit) = limit {
            args.push(format!("--max-count={limit}"));
        }
        args.extend([head.to_string(), "--".into()]);
        let log = crate::git::git_output(&checkout, args)?;
        for line in log.lines() {
            let columns = line.splitn(4, '\0').collect::<Vec<_>>();
            if columns.len() != 4 {
                continue;
            }
            let [sha, parents, occurred_at, message] = <[&str; 4]>::try_from(columns).unwrap();
            events.push(HistoryEvent {
                schema_version: HISTORY_EVENT_SCHEMA_VERSION.into(),
                event_id: history_event_id(&[
                    project_id,
                    &repo.id,
                    repo.remote.as_deref().unwrap_or(""),
                    &repo.base_branch,
                    "base.commit",
                    sha,
                ]),
                project_id: project_id.into(),
                kind: "base.commit".into(),
                bundle_id: None,
                bundle_title: None,
                repo_id: Some(repo.id.clone()),
                repo_remote: repo.remote.clone(),
                base_branch: Some(repo.base_branch.clone()),
                branch: Some(repo.base_branch.clone()),
                commit: Some(sha.into()),
                before_sha: parents.split_whitespace().next().map(Into::into),
                after_sha: Some(sha.into()),
                movement: None,
                node_id: Some(format!("base:{}:{}:{sha}", repo.id, repo.base_branch)),
                node_type: Some("base.commit".into()),
                commit_group_id: None,
                message: Some(message.into()),
                occurred_at: Some(occurred_at.into()),
                recorded_at: now_iso(),
                recorded_by: "knit".into(),
                metadata: Some(serde_json::json!({"observedRef": reference, "observedHead": head})),
            });
        }
    }
    Ok(events)
}

/// All lifecycle states contribute. A repo added later records its base in the
/// same RepoEntry::base_sha field; repo.added nodes currently carry only IDs,
/// not a separate base tip. Never infer a historical tip from today's HEAD or
/// from feature commits on those nodes.
fn anchors(bundles: &[(PathBuf, ChangeGroup)], repo_id: &str) -> BTreeSet<String> {
    bundles
        .iter()
        .flat_map(|(_, bundle)| &bundle.repos)
        .filter(|repo| repo.id == repo_id)
        .filter_map(|repo| repo.base_sha.clone())
        .collect()
}

/// Find the oldest anchor on the observed first-parent chain, inclusively.
/// Count only the range above each recorded anchor, then verify its exact
/// first-parent position: ordinary ancestry also admits merged side branches.
/// Git performs the graph walk without returning the entire repository log;
/// the final metadata walk is bounded at the oldest usable anchor. With no
/// usable anchor it reads only the latest 200 commits. Dates are irrelevant.
fn anchor_depth(checkout: &Path, head: &str, anchors: &BTreeSet<String>) -> Result<u64> {
    let mut oldest = None;
    for anchor in anchors {
        // Recorded SHA fields must not become Git options or revision syntax.
        if !matches!(anchor.len(), 40 | 64) || !anchor.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(count) = crate::git::git_output(
            checkout,
            [
                "rev-list",
                "--first-parent",
                "--count",
                &format!("{anchor}..{head}"),
                "--",
            ],
        ) else {
            continue;
        };
        let count: u64 = count.trim().parse().context("invalid first-parent count")?;
        let Ok(at_depth) = crate::git::git_output(
            checkout,
            ["rev-parse", "--verify", &format!("{head}~{count}")],
        ) else {
            continue;
        };
        if at_depth.trim().eq_ignore_ascii_case(anchor) {
            oldest = Some(oldest.unwrap_or(0).max(count.saturating_add(1)));
        }
    }
    Ok(oldest.unwrap_or(200))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AllBaseCommits, BaseCommitDepth, BundleState, KnitProject, ProjectHistory};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        project: KnitProject,
        commits: Vec<String>,
        tree: String,
    }

    impl Fixture {
        fn new(count: usize) -> Self {
            let root = std::env::temp_dir().join(format!(
                "knit-base-depth-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("repo")).unwrap();
            fs::create_dir_all(root.join(".knit/projects")).unwrap();
            fs::create_dir_all(root.join(".knit/bundles")).unwrap();
            let mut fixture = Self {
                root,
                project: KnitProject::new("demo".into(), "2026-01-01T00:00:00Z".into()),
                commits: Vec::new(),
                tree: String::new(),
            };
            fixture.git(&["init", "-b", "main"]);
            fixture.tree = fixture.git(&["write-tree"]);
            for index in 0..count {
                let parents = fixture
                    .commits
                    .last()
                    .cloned()
                    .into_iter()
                    .collect::<Vec<_>>();
                let sha = fixture.commit(&format!("commit {index}"), &parents);
                fixture.commits.push(sha);
            }
            fixture.git(&[
                "update-ref",
                "refs/heads/main",
                fixture.commits.last().unwrap(),
            ]);
            fixture.project.repos.push(
                serde_json::from_value(serde_json::json!({
                    "id": "repo", "path": "repo", "remote": null, "baseBranch": "main"
                }))
                .unwrap(),
            );
            fixture
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(self.root.join("repo"))
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().into()
        }

        fn commit(&self, message: &str, parents: &[String]) -> String {
            let mut args = vec!["commit-tree", &self.tree, "-m", message];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            self.git(&args)
        }

        fn bundle(&self, id: &str, index: usize, state: BundleState) -> (PathBuf, ChangeGroup) {
            let mut bundle = ChangeGroup::new(
                id.into(),
                "Synthetic work".into(),
                "2026-01-01T00:00:00Z".into(),
            );
            bundle.project_id = Some("demo".into());
            bundle.state = Some(state);
            bundle.repos.push(
                serde_json::from_value(serde_json::json!({
                    "id": "repo", "path": "repo", "remote": null, "baseBranch": "main",
                    "baseSha": self.commits[index]
                }))
                .unwrap(),
            );
            (
                self.root.join(format!(".knit/bundles/{id}.bundle.json")),
                bundle,
            )
        }

        fn events(&self, bundles: &[(PathBuf, ChangeGroup)]) -> Vec<HistoryEvent> {
            crate::store::write_json(&project_path(&self.root, "demo"), &self.project).unwrap();
            events(&self.root, "demo", bundles).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn default_depth_and_explicit_overrides() {
        let mut fixture = Fixture::new(205);
        let observed = fixture.events(&[]);
        assert_eq!(observed.len(), 200);
        assert_eq!(
            observed.last().unwrap().commit.as_ref(),
            Some(&fixture.commits[5])
        );
        let bundles = vec![fixture.bundle("old", 0, BundleState::Archived)];
        for (depth, expected) in [
            (BaseCommitDepth::Count(3), 3),
            (BaseCommitDepth::Count(0), 0),
            (BaseCommitDepth::Count(999), 205),
            (BaseCommitDepth::All(AllBaseCommits::All), 205),
        ] {
            fixture.project.history = Some(ProjectHistory {
                base_commit_depth: Some(depth),
            });
            let explicit = fixture.events(&bundles);
            assert_eq!(explicit.len(), expected);
            if expected > 0 {
                assert_eq!(explicit[0].event_id, observed[0].event_id);
            }
        }
        fixture.project.history = None;
        assert_eq!(
            fixture.events(&bundles).len(),
            205,
            "anchors may exceed 200"
        );
    }

    #[test]
    fn oldest_bundle_anchor_includes_archived_landed_and_repo_additions() {
        let fixture = Fixture::new(8);
        let mut bundles = vec![
            fixture.bundle("new", 6, BundleState::Open),
            fixture.bundle("landed", 4, BundleState::Archived),
            fixture.bundle("archived", 2, BundleState::Archived),
        ];
        bundles[1].1.nodes.push(BundleNode::feature_landed(
            "landed".into(),
            "2026-01-01T00:00:00Z".into(),
            "plan".into(),
            "run".into(),
            "github".into(),
            vec!["repo".into()],
            Vec::new(),
            None,
        ));
        bundles[2].1.nodes.push(BundleNode::repos_added(
            "added".into(),
            "2026-01-01T00:00:00Z".into(),
            vec!["repo".into()],
        ));
        for (path, bundle) in &bundles {
            crate::store::write_json(path, bundle).unwrap();
        }
        let (other_path, mut other) = fixture.bundle("other-project", 0, BundleState::Open);
        other.project_id = Some("other".into());
        crate::store::write_json(&other_path, &other).unwrap();
        fixture.events(&[]); // Persist project before the project-wide sweep.
        refresh_project_history(&fixture.root, "demo").unwrap();
        let recorded = load_history_events(&fixture.root, "demo").unwrap();
        let base = recorded
            .iter()
            .filter(|event| event.kind == "base.commit")
            .collect::<Vec<_>>();
        assert_eq!(base.len(), 6);
        assert!(base
            .iter()
            .any(|event| event.commit.as_ref() == Some(&fixture.commits[2])));
        assert!(!base
            .iter()
            .any(|event| event.commit.as_ref() == Some(&fixture.commits[1])));
        assert_eq!(
            fixture
                .events(&[fixture.bundle("tip", 7, BundleState::Open)])
                .len(),
            1
        );
        assert_eq!(
            fixture
                .events(&[fixture.bundle("root", 0, BundleState::Open)])
                .len(),
            8
        );
    }

    #[test]
    fn unusable_and_side_branch_anchors_fall_back_without_changing_ledger() {
        let mut fixture = Fixture::new(8);
        let side = fixture.commit("side", &[fixture.commits[1].clone()]);
        let merge = fixture.commit("merge", &[fixture.commits[7].clone(), side.clone()]);
        fixture.git(&["update-ref", "refs/heads/main", &merge]);
        let head = merge.as_str();
        for anchor in [side, "f".repeat(40), "--all".into(), "HEAD".into()] {
            assert_eq!(
                anchor_depth(&fixture.root.join("repo"), head, &BTreeSet::from([anchor])).unwrap(),
                200
            );
        }
        let unrelated = fixture.commit("unrelated", &[]);
        assert_eq!(
            anchor_depth(
                &fixture.root.join("repo"),
                head,
                &BTreeSet::from([unrelated])
            )
            .unwrap(),
            200
        );
        let bundles = vec![fixture.bundle("old", 2, BundleState::Archived)];
        let observed = fixture.events(&bundles);
        assert_eq!(observed.len(), 7);
        append_history_events(&fixture.root, "demo", &observed).unwrap();
        fixture.project.history = Some(ProjectHistory {
            base_commit_depth: Some(BaseCommitDepth::Count(1)),
        });
        fixture.events(&[]);
        assert_eq!(refresh_project_history(&fixture.root, "demo").unwrap(), 0);
        let rebuilt = rebuild_project_history(&fixture.root, "demo").unwrap();
        assert_eq!(rebuilt.preserved, 6);
        assert_eq!(load_history_events(&fixture.root, "demo").unwrap().len(), 7);
    }

    #[test]
    fn depth_json_roundtrip_and_invalid_values() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/project.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for value in [
            serde_json::json!(0),
            serde_json::json!(200),
            serde_json::json!(2147483647),
            serde_json::json!("all"),
        ] {
            let mut project = serde_json::to_value(KnitProject::new(
                "demo".into(),
                "2026-01-01T00:00:00Z".into(),
            ))
            .unwrap();
            project["history"] = serde_json::json!({"baseCommitDepth": value});
            assert!(validator.is_valid(&project));
            let history: ProjectHistory =
                serde_json::from_value(serde_json::json!({"baseCommitDepth": value})).unwrap();
            assert_eq!(
                serde_json::to_value(history).unwrap()["baseCommitDepth"],
                value
            );
        }
        for value in [
            serde_json::json!(-1),
            serde_json::json!(2147483648u64),
            serde_json::json!(1.5),
            serde_json::json!("200"),
            serde_json::json!("ALL"),
            serde_json::json!(true),
        ] {
            assert!(serde_json::from_value::<ProjectHistory>(
                serde_json::json!({"baseCommitDepth": value})
            )
            .is_err());
        }
        let project = KnitProject::new("demo".into(), "now".into());
        assert!(serde_json::to_value(project)
            .unwrap()
            .get("history")
            .is_none());
    }
}
