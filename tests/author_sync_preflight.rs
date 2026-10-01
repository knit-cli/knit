mod common;
use common::*;
use std::fs;

#[test]
fn artifact_sync_checks_every_branch_before_pushing_the_first() {
    for signed in [false, true] {
        let root = unique_temp_dir();
        let (backend_remote, backend, _) = init_remote_repo(&root, "backend");
        let (frontend_remote, frontend, _) = init_remote_repo(&root, "frontend");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["init", "demo"]);
        for (id, source) in [("backend", &backend), ("frontend", &frontend)] {
            knit(&workspace, ["project", "add", id, source.to_str().unwrap()]);
        }
        knit(&workspace, ["bundle", "sync identity"]);
        let bundle_root = workspace.join(".knit/worktrees/sync-identity");
        for id in ["backend", "frontend"] {
            append_line(&bundle_root.join(id).join("app.txt"), "feature");
        }
        knit(&workspace, ["commit", "--all", "-m", "Feature changes"]);
        let consumer = bundle_root.join("frontend");
        if signed {
            git(&consumer, ["config", "commit.gpgsign", "true"]);
        } else {
            git(
                &consumer,
                [
                    "commit",
                    "--amend",
                    "--no-edit",
                    "--author=Other Author <other@example.com>",
                ],
            );
            knit(&workspace, ["sync"]);
        }
        let hosted = root.join("hosted");
        let base = spawn_fake_remote_push_api(&hosted);
        knit(&workspace, ["remote", "add", "hosted", &base]);
        let fake_bin = root.join("fake-bin");
        let fake_gh = root.join("fake-gh");
        write_fake_gh(&fake_bin, &fake_gh);
        let publication = knit_fails_with_fake_gh_env(
            &workspace,
            ["publish", "create", "backend"],
            &fake_bin,
            &fake_gh,
            &[("KNIT_REMOTE_TOKEN", "synthetic-token")],
        );
        assert!(
            publication.contains("outgoing commit preflight failed"),
            "{publication}"
        );
        assert!(!fake_gh.join("create-backend.args").exists());
        let push = knit_fails_with_env(
            &workspace,
            ["push", "backend"],
            &[("KNIT_REMOTE_TOKEN", "synthetic-token")],
        );
        assert!(push.contains("outgoing commit preflight failed"), "{push}");
        let output = knit_fails_with_env(
            &workspace,
            ["sync", "push", "--bundles"],
            &[("KNIT_REMOTE_TOKEN", "synthetic-token")],
        );
        assert!(
            output.contains("feature branches not pushed; artifact not synced"),
            "{output}"
        );
        assert!(
            output.contains(if signed {
                "unsigned"
            } else {
                "differs from git-config identity"
            }),
            "{output}"
        );
        for remote in [&backend_remote, &frontend_remote] {
            assert!(
                !git_success(
                    remote,
                    ["show-ref", "--verify", "refs/heads/knit/sync-identity"]
                ),
                "a branch was pushed before all commits passed preflight"
            );
        }
        assert!(!hosted.join("artifact-sync-identity.states").exists());
        // A stale implicit sync destination is skipped by the actual sync, so
        // it must not expand the branch scope of a selected push or publish.
        let config_path = workspace.join(".knit/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["syncRemotes"] = serde_json::json!(["missing"]);
        config.as_object_mut().unwrap().remove("syncRemote");
        fs::write(config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let explicit =
            knit_fails_with_env(&workspace, ["push", "backend", "--remote", "missing"], &[]);
        assert!(explicit.contains("missing"), "{explicit}");
        assert!(!git_success(
            &backend_remote,
            ["show-ref", "--verify", "refs/heads/knit/sync-identity"]
        ));
        knit(&workspace, ["push", "backend"]);
        knit_with_fake_gh(
            &workspace,
            ["publish", "create", "backend", "--no-sync"],
            &fake_bin,
            &fake_gh,
        );
        assert!(fake_gh.join("create-backend.args").exists());
        assert!(!git_success(
            &frontend_remote,
            ["show-ref", "--verify", "refs/heads/knit/sync-identity"]
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
