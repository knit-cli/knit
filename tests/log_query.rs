mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn setup_history_workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, root);
    knit(
        &workspace,
        [
            "bundle",
            "cross repo change",
            "--repo",
            "backend",
            "--repo",
            "frontend",
        ],
    );
    for repo in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/cross-repo-change")
                .join(repo)
                .join("app.txt"),
            "shared update",
        );
    }
    knit(
        &workspace,
        ["commit", "--all", "-m", "Shared history subject"],
    );
    knit(
        &workspace,
        ["bundle", "frontend followup", "--repo", "frontend"],
    );
    append_line(
        &workspace.join(".knit/worktrees/frontend-followup/frontend/app.txt"),
        "followup",
    );
    knit(
        &workspace,
        [
            "--bundle",
            "frontend-followup",
            "commit",
            "--all",
            "-m",
            "Frontend followup subject",
        ],
    );
    workspace
}

fn clear_workspace_bundle(workspace: &Path) {
    let path = workspace.join(".knit/config.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("activeBundle");
    fs::write(path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
}

fn history_bytes(workspace: &Path) -> Vec<u8> {
    fs::read(workspace.join(".knit/history/demo.history.jsonl")).unwrap()
}

#[test]
fn default_log_handles_project_and_ad_hoc_bundles_newest_first() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    let regular = knit(
        &workspace,
        ["--bundle", "cross-repo-change", "log", "--oneline"],
    );
    assert!(regular.contains("Shared history subject"), "{regular}");

    let ad_hoc_root = root.join("standalone");
    let repo = root.join("standalone-repo");
    fs::create_dir_all(&ad_hoc_root).unwrap();
    init_repo(&repo, "standalone");
    knit(&ad_hoc_root, ["bundle", "local experiment"]);
    knit(&ad_hoc_root, ["bundle", "add", repo.to_str().unwrap()]);
    let checkout = ad_hoc_root.join(".knit/worktrees/local-experiment/standalone-repo");
    append_line(&checkout.join("app.txt"), "first");
    knit(
        &ad_hoc_root,
        ["commit", "--all", "-m", "First local change"],
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    append_line(&checkout.join("app.txt"), "second");
    knit(
        &ad_hoc_root,
        ["commit", "--all", "-m", "Second local change"],
    );
    let log = knit(&ad_hoc_root, ["log", "--oneline"]);
    assert!(
        log.find("Second local change").unwrap() < log.find("First local change").unwrap(),
        "{log}"
    );
}

#[test]
fn all_scope_needs_no_bundle_and_json_is_clean_and_read_only() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    clear_workspace_bundle(&workspace);
    let ledger_before = history_bytes(&workspace);
    let bundles_before = fs::read_dir(workspace.join(".knit/bundles"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect::<Vec<_>>();

    let output = knit(&workspace, ["log", "--all", "--project", "demo", "--json"]);
    let entries: Value = serde_json::from_str(&output).unwrap();
    assert!(entries.as_array().unwrap().len() >= 2, "{entries:#}");
    assert!(entries.as_array().unwrap().iter().all(|entry| {
        entry.get("bundleId").is_some()
            && entry.get("occurredAt").is_some()
            && entry.get("bundle_id").is_none()
    }));
    assert_eq!(history_bytes(&workspace), ledger_before);
    for (name, before) in bundles_before {
        assert_eq!(
            fs::read(workspace.join(".knit/bundles").join(name)).unwrap(),
            before
        );
    }

    let conflict = knit_fails(
        &workspace,
        ["--bundle", "cross-repo-change", "log", "--all"],
    );
    assert!(conflict.contains("cannot be combined"), "{conflict}");
}

#[test]
fn repo_views_intersect_and_full_context_preserves_companions() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    fs::create_dir_all(workspace.join(".knit/views")).unwrap();
    fs::write(
        workspace.join(".knit/views/demo.views.json"),
        serde_json::to_string_pretty(&json!({
            "schemaVersion": "1",
            "kind": "KnitProjectViews",
            "projectId": "demo",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "defaultView": "ignored-default",
            "views": {
                "shared": {"base": "none", "include": ["backend"]},
                "empty": {"base": "none"},
                "ignored-default": {"base": "none", "include": ["docs"]}
            },
            "templates": {
                "shared": {"base": "none", "include": ["frontend"]},
                "template-only": {"base": "none", "include": ["frontend"]}
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let personal: Value = serde_json::from_str(&knit(
        &workspace,
        [
            "log", "--all", "--view", "shared", "--grep", "Shared", "--json",
        ],
    ))
    .unwrap();
    let events = personal[0]["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{personal:#}");
    assert_eq!(events[0]["repoId"], "backend");

    let full: Value = serde_json::from_str(&knit(
        &workspace,
        [
            "log",
            "--all",
            "--view",
            "shared",
            "--grep",
            "Shared",
            "--full-context",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(full[0]["events"].as_array().unwrap().len(), 2, "{full:#}");

    let all_repos: Value = serde_json::from_str(&knit(
        &workspace,
        [
            "log",
            "--all",
            "--repo",
            "backend",
            "--repo",
            "frontend",
            "--repo-match",
            "all",
            "--kind",
            "commit.recorded",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(all_repos.as_array().unwrap().len(), 1, "{all_repos:#}");

    let template = knit(
        &workspace,
        ["log", "--all", "--view", "template-only", "--oneline"],
    );
    assert!(template.contains("Frontend followup subject"), "{template}");

    for extra in [
        vec!["--view", "empty"],
        vec!["--view", "shared", "--repo", "frontend"],
    ] {
        let mut args = vec!["log", "--all", "--json"];
        args.extend(extra);
        let empty: Value = serde_json::from_str(&knit(&workspace, args)).unwrap();
        assert_eq!(empty, json!([]));
    }

    // A saved default view never narrows history unless --view is explicit.
    let unscoped = knit(&workspace, ["log", "--all", "--oneline"]);
    assert!(unscoped.contains("Shared history subject"), "{unscoped}");
    // Explicit ids retain historical access; views use current membership.
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["repos"]
        .as_array_mut()
        .unwrap()
        .retain(|repo| repo["id"] != "backend");
    fs::write(
        project_path,
        serde_json::to_string_pretty(&project).unwrap(),
    )
    .unwrap();

    let removed = knit(&workspace, ["log", "--all", "-r", "backend", "--oneline"]);
    assert!(removed.contains("Shared history subject"));
    let empty: Value = serde_json::from_str(&knit(
        &workspace,
        ["log", "--all", "--view", "shared", "--json"],
    ))
    .unwrap();
    assert_eq!(empty, json!([]));
}

#[test]
fn filters_limits_reverse_and_legacy_list_use_the_local_query_engine() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    let newest: Value = serde_json::from_str(&knit(
        &workspace,
        [
            "log",
            "--all",
            "--kind",
            "commit.recorded",
            "--grep",
            "subject$",
            "-i",
            "-n",
            "1",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(newest.as_array().unwrap().len(), 1);
    assert!(newest[0]["message"].as_str().unwrap().contains("Frontend"));

    let reversed: Value = serde_json::from_str(&knit(
        &workspace,
        [
            "log",
            "--all",
            "--kind",
            "commit.recorded",
            "--max-count",
            "2",
            "--reverse",
            "--json",
        ],
    ))
    .unwrap();
    let messages = reversed
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(messages[0].contains("Shared"), "{messages:?}");
    assert!(messages[1].contains("Frontend"), "{messages:?}");

    let dated = knit(
        &workspace,
        ["log", "--all", "--since", "2 weeks ago", "--oneline"],
    );
    assert!(!dated.contains("Invalid --since"), "{dated}");
    let invalid = knit_fails(
        &workspace,
        ["log", "--all", "--since", "definitely-not-a-date"],
    );
    assert!(invalid.contains("Invalid --since"), "{invalid}");

    let ledger_before = history_bytes(&workspace);
    let legacy = knit(
        &workspace,
        ["history", "list", "--kind", "commit.recorded", "-n", "2"],
    );
    assert!(legacy.lines().count() <= 2, "{legacy}");
    assert_eq!(history_bytes(&workspace), ledger_before);
}

#[test]
fn project_show_resolves_entries_and_orphans_remain_inspectable() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    let entries: Value = serde_json::from_str(&knit(
        &workspace,
        ["log", "--all", "--grep", "Frontend followup", "--json"],
    ))
    .unwrap();
    let id = entries[0]["id"].as_str().unwrap();
    let shown: Value = serde_json::from_str(&knit(
        &workspace,
        ["show", id, "--all", "--project", "demo", "--json"],
    ))
    .unwrap();
    assert_eq!(shown["id"], id);

    fs::remove_file(workspace.join(".knit/bundles/frontend-followup.bundle.json")).unwrap();
    let orphan = knit(&workspace, ["show", id, "--all", "--project", "demo"]);
    assert!(orphan.contains("Frontend followup subject"), "{orphan}");
    assert!(orphan.contains("Patch unavailable locally"), "{orphan}");
}

#[test]
fn offline_dates_parser_flags_and_missing_project_artifact() {
    let root = unique_temp_dir();
    fs::create_dir_all(&root).unwrap();
    knit(&root, ["init", "demo"]);
    fs::create_dir_all(root.join(".knit/views")).unwrap();
    fs::write(
        root.join(".knit/views/demo.views.json"),
        serde_json::to_vec(&json!({
            "schemaVersion":"1", "kind":"KnitProjectViews", "projectId":"demo",
            "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
            "views":{"empty":{"base":"default","include":["removed-repo"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let empty: Value =
        serde_json::from_str(&knit(&root, ["log", "--all", "--view", "empty", "--json"])).unwrap();
    assert_eq!(empty, json!([]));
    for date in [
        "2 weeks ago",
        "1 month ago",
        "now",
        "2026-01-01",
        "2026-01-01T12:00:00Z",
    ] {
        let output = knit(&root, ["log", "--all", "--since", date, "--json"]);
        assert!(serde_json::from_str::<Value>(&output).unwrap().is_array());
    }
    for date in [
        "123notadate",
        "2 weeks garbage",
        "2026-02-30",
        "yesterday junk",
    ] {
        assert!(knit_fails(&root, ["log", "--all", "--after", date]).contains("Invalid --since"));
    }
    for flag in ["--unknown", "-wat", "-2x"] {
        knit_fails(&root, ["log", "--all", flag]);
    }
    for flags in [
        vec!["-n"],
        vec!["-n=10"],
        vec!["-2"],
        vec!["--limit", "2"],
        vec!["-E", "--grep", "foo|bar"],
    ] {
        let mut args = vec!["log", "--all", "--json"];
        args.extend(flags);
        assert!(serde_json::from_str::<Value>(&knit(&root, args))
            .unwrap()
            .is_array());
    }
    fs::remove_file(root.join(".knit/projects/demo.project.json")).unwrap();
    assert!(serde_json::from_str::<Value>(&knit(
        &root,
        [
            "log",
            "--all",
            "--project",
            "demo",
            "-r",
            "removed-repo",
            "--json"
        ]
    ))
    .unwrap()
    .is_array());
}

#[test]
fn bundle_views_and_every_group_selector_round_trip() {
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    fs::create_dir_all(workspace.join(".knit/views")).unwrap();
    fs::write(
        workspace.join(".knit/views/demo.views.json"),
        serde_json::to_vec(&json!({
            "schemaVersion":"1", "kind":"KnitProjectViews", "projectId":"demo",
            "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
            "views":{"backend":{"base":"none","include":["backend"]},"empty":{"base":"none"}}
        }))
        .unwrap(),
    )
    .unwrap();
    let checkout = workspace.join(".knit/worktrees/cross-repo-change/knit");
    // The generated bundle directory itself supplies worktree context.
    let checkout = checkout.parent().unwrap();
    let selected: Value = serde_json::from_str(&knit(
        checkout,
        [
            "log",
            "--view",
            "backend",
            "--kind",
            "commit.recorded",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(selected[0]["events"].as_array().unwrap().len(), 1);
    for args in [
        vec!["log", "--view", "empty", "--json"],
        vec!["log", "--view", "backend", "-r", "frontend", "--json"],
    ] {
        assert_eq!(
            serde_json::from_str::<Value>(&knit(checkout, args)).unwrap(),
            json!([])
        );
    }
    for group in ["event", "commit", "bundle"] {
        let entries: Value = serde_json::from_str(&knit(
            &workspace,
            ["log", "--all", "--group", group, "--json"],
        ))
        .unwrap();
        for entry in entries.as_array().unwrap() {
            let id = entry["id"].as_str().unwrap();
            let shown: Value =
                serde_json::from_str(&knit(&workspace, ["show", id, "--all", "--json"])).unwrap();
            assert_eq!(shown["id"], id);
        }
    }
    let config_path = workspace.join(".knit/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["activeProject"] = json!("another-project");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let inferred = knit(checkout, ["log", "--all", "--oneline"]);
    assert!(inferred.contains("Shared history subject"));
    let explicit: Value = serde_json::from_str(&knit(
        checkout,
        ["log", "--all", "--project", "another-project", "--json"],
    ))
    .unwrap();
    assert_eq!(explicit, json!([]));
    config["activeProject"] = json!("demo");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let ambiguous = knit_fails(&workspace, ["show", "kg_", "--all"]);
    assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
    let sha = selected[0]["events"][0]["commit"].as_str().unwrap();
    knit(&workspace, ["show", sha, "--all", "--json"]);
}

#[test]
fn legacy_ad_hoc_groups_show_head_without_writing_artifacts() {
    let root = unique_temp_dir();
    fs::create_dir_all(&root).unwrap();
    knit(&root, ["bundle", "local sample"]);
    let path = root.join(".knit/bundles/local-sample.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bundle["nodes"] = json!([]);
    bundle["commitGroups"] = json!([{"id":"kg_sample", "createdAt":"2026-01-01T00:00:00Z", "message":"Legacy sample", "commits":[{"repoId":"sample", "sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}]);
    let bytes = serde_json::to_vec(&bundle).unwrap();
    fs::write(&path, &bytes).unwrap();
    let output = knit(&root, ["show", "HEAD"]);
    assert!(output.contains("Legacy sample"), "{output}");
    let shown: Value = serde_json::from_str(&knit(&root, ["show", "HEAD", "--json"])).unwrap();
    assert_eq!(shown["id"], "kg_sample");
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn parser_rejects_unknown_options_and_preserves_count_forms() {
    use clap::Parser;
    use knit::{Cli, Commands};
    for flag in ["--unknown", "-wat", "-2x"] {
        assert!(Cli::try_parse_from(["knit", "log", flag]).is_err());
    }
    for flag in ["-n", "-n=10"] {
        let cli = Cli::try_parse_from(["knit", "log", flag, "--json"]).unwrap();
        let Commands::Log { args } = cli.command else {
            panic!("expected log")
        };
        assert_eq!(args.limit, Some(10));
        assert!(args.json);
    }
    let cli = Cli::try_parse_from(["knit", "log", "-2", "--json", "-E"]).unwrap();
    let Commands::Log { args } = cli.command else {
        panic!("expected log")
    };
    assert_eq!(args.shorthand_limit.as_deref(), Some("-2"));
    assert!(args.json && args.extended_regexp);
}

#[cfg(unix)]
#[test]
fn show_git_subprocess_cannot_lazy_fetch_or_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let root = unique_temp_dir();
    let workspace = setup_history_workspace(&root);
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_git = fake_bin.join("git");
    fs::write(&fake_git, "#!/bin/sh\nif [ \"$1\" != show ] || [ \"$GIT_NO_LAZY_FETCH\" != 1 ] || [ \"$GIT_TERMINAL_PROMPT\" != 0 ]; then\n  echo unsafe-inspection >&2\n  exit 1\nfi\necho offline-inspection-verified\n").unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let shown = knit_with_env(
        &workspace,
        ["--bundle", "cross-repo-change", "show", "HEAD"],
        &[("PATH", fake_bin.to_str().unwrap())],
    );
    assert!(shown.contains("offline-inspection-verified"), "{shown}");
    assert!(!shown.contains("unsafe-inspection"), "{shown}");
}

#[test]
fn mixed_legacy_groups_and_lifecycle_nodes_preserve_head_timeline() {
    use knit::model::BundleNode;
    let root = unique_temp_dir();
    fs::create_dir_all(&root).unwrap();
    knit(&root, ["bundle", "mixed sample"]);
    let path = root.join(".knit/bundles/mixed-sample.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let creation = bundle["nodes"][0].clone();
    assert_eq!(creation["type"], "feature.created");
    bundle["commitGroups"] = json!([
        {"id":"kg_first", "createdAt":"2026-01-01T00:00:00Z", "message":"First legacy", "commits":[]},
        {"id":"kg_second", "createdAt":"2026-01-02T00:00:00Z", "message":"Second legacy", "commits":[]}
    ]);
    for represented_group in [None, Some("kg_first"), Some("kg_second")] {
        bundle["nodes"] = json!([creation.clone()]);
        if let Some(represented_group) = represented_group {
            bundle["nodes"].as_array_mut().unwrap().extend([
                serde_json::to_value(BundleNode::commit_group(
                    represented_group.into(),
                    if represented_group == "kg_first" {
                        "2026-01-01T00:00:00Z"
                    } else {
                        "2026-01-02T00:00:00Z"
                    }
                    .into(),
                    "Represented legacy".into(),
                    vec![],
                    vec![],
                ))
                .unwrap(),
                serde_json::to_value(BundleNode::checkpoint(
                    "checkpoint_sample".into(),
                    "2026-01-03T00:00:00Z".into(),
                    "Later checkpoint".into(),
                    vec![],
                    represented_group.into(),
                ))
                .unwrap(),
            ]);
        }
        let bytes = serde_json::to_vec(&bundle).unwrap();
        fs::write(&path, &bytes).unwrap();
        let expected = if represented_group.is_some() {
            vec!["checkpoint_sample", "kg_second", "kg_first"]
        } else {
            vec!["kg_second", "kg_first"]
        };
        let logged: Value = serde_json::from_str(&knit(&root, ["log", "--json"])).unwrap();
        let logged_ids = logged
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .filter(|id| expected.contains(id))
            .collect::<Vec<_>>();
        assert_eq!(
            logged_ids, expected,
            "log and HEAD must share the legacy timeline"
        );
        for (offset, id) in expected.iter().enumerate() {
            let selector = if offset == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{offset}")
            };
            let shown: Value =
                serde_json::from_str(&knit(&root, ["show", &selector, "--json"])).unwrap();
            assert_eq!(shown["id"], *id);
            knit(&root, ["show", &selector]);
        }
        knit_fails(&root, ["show", &format!("HEAD~{}", expected.len())]);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
