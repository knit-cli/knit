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
        // Once the branches are already on origin, sync must not author-check them.
        for id in ["backend", "frontend"] {
            git(
                &bundle_root.join(id),
                ["push", "origin", "HEAD:refs/heads/knit/sync-identity"],
            );
        }
        let output = knit_with_env(
            &workspace,
            ["sync", "push", "--bundles"],
            &[("KNIT_REMOTE_TOKEN", "synthetic-token")],
        );
        assert!(!output.contains("sync skipped"), "{output}");
        assert!(hosted.join("artifact-sync-identity.states").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
