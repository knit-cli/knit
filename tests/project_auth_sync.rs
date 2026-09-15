//! Remote pull/push integration with project-defined auth requirements:
//! auth definitions arriving before repository reconcile (so a failed private
//! fetch cannot block receiving its own requirements), the recoverable
//! failed-add entry, pending known-repos maintenance from the authoritative
//! membership, and the push-side preservation rules for scoped workspaces.
//!
//! Network is limited to local fixtures: loopback fake remotes and reserved
//! `.example` forge hosts; recovery rewrites the reserved URL onto a local
//! bare repository through Git's `insteadOf`, standing in for the access a
//! linked credential grants.

mod common;

use common::*;
use std::fs;
use std::path::Path;

fn membership_repo(id: &str, remote: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "path": "",
        "remote": remote,
        "baseBranch": "main",
    })
}

fn export_record(id: &str, remote: &str) -> serde_json::Value {
    serde_json::json!({
        "localId": id,
        "name": id,
        "defaultBranch": None::<String>,
        "remoteUrl": remote,
        "metadata": {},
    })
}

fn export_body(repos: &[(String, String)], auth: Option<serde_json::Value>) -> serde_json::Value {
    let mut knit_project = serde_json::json!({
        "schemaVersion": "1",
        "kind": "KnitProject",
        "id": "demo",
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-01T00:00:00Z",
        "repos": repos
            .iter()
            .map(|(id, remote)| membership_repo(id, remote))
            .collect::<Vec<_>>(),
    });
    if let Some(auth) = auth {
        knit_project["auth"] = auth;
    }
    serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": knit_project,
            "repositories": repos
                .iter()
                .map(|(id, remote)| export_record(id, remote))
                .collect::<Vec<_>>(),
            "bundles": [],
            "historyEvents": [],
        }
    })
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn make_bare_repo(root: &Path, name: &str) -> std::path::PathBuf {
    let work = root.join(format!("{name}-source"));
    init_repo(&work, name);
    let bare = root.join(format!("{name}-bare.git"));
    git(
        root,
        [
            "clone",
            "--bare",
            "--quiet",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn instead_of_config(root: &Path, url: &str, bare: &Path) -> std::path::PathBuf {
    // Serialize through `git config`: a hand-written `[url "C:\..."]` loses
    // the backslashes when Git parses the subsection, while `git config`
    // escapes it portably. `--replace-all` keeps the old overwrite semantic.
    let config = root.join("recovery.gitconfig");
    git(
        root,
        [
            "config",
            "--file",
            config.to_str().unwrap(),
            "--replace-all",
            &format!("url.{}.insteadOf", bare.display()),
            url,
        ],
    );
    config
}

/// The parent acceptance flow, end to end on the pull side: a remote gains a
/// new private repository and a new auth group for it. The pull receives the
/// group even though the repository fetch fails (definitions land first), the
/// failed repository stays a project entry so setup can bind it, and after
/// linking a credential the next pull clones it.
#[test]
fn pull_imports_auth_group_before_failed_add_and_recovers_after_setup() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let newrepo_url = "https://forge.example/org/newrepo.git";

    let fake_dir = root.join("fake-remote");
    let v1 = export_body(
        &[("backend".to_string(), backend.to_string_lossy().to_string())],
        None,
    );
    let base_url = spawn_fake_remote_api(&fake_dir, v1.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    let clone = knit_with_env(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[home_env],
    );
    assert!(clone.contains("cloned"), "{clone}");
    assert!(
        !target.join(".knit/projects/demo.known-repos.json").exists(),
        "an unscoped clone has no out-of-scope membership to pend"
    );

    // The remote gains a private repo plus the group that covers it.
    fs::write(
        fake_dir.join("export.json"),
        export_body(
            &[
                ("backend".to_string(), backend.to_string_lossy().to_string()),
                ("newrepo".to_string(), newrepo_url.to_string()),
            ],
            Some(serde_json::json!({"groups": [{
                "id": "gh", "name": "GitHub", "provider": "github",
                "host": "forge.example", "repos": ["newrepo"],
                "tokenTypes": ["classic_pat"],
            }]})),
        )
        .to_string(),
    )
    .unwrap();

    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    let auth_at = pull
        .find("Auth requirements: 1 group(s)")
        .unwrap_or_else(|| panic!("auth definitions must land on the pull: {pull}"));
    let failed_at = pull
        .find("Project repo add failed: newrepo")
        .unwrap_or_else(|| panic!("the private add must fail visibly: {pull}"));
    assert!(
        auth_at < failed_at,
        "auth definitions must arrive before the repository reconcile: {pull}"
    );
    assert!(
        pull.contains("local credential assignments were not changed"),
        "{pull}"
    );
    assert!(
        pull.contains("kept in the project without a checkout"),
        "the failed add must advertise the recovery path: {pull}"
    );

    // The group and the failed repo's entry are both in the local project —
    // never pending-only, which setup cannot map.
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["repos"],
        serde_json::json!(["newrepo"])
    );
    let newrepo_entry = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["id"] == "newrepo")
        .expect("the failed add keeps its project entry");
    assert_eq!(newrepo_entry["remote"], serde_json::json!(newrepo_url));
    assert!(!target.join(".knit/projects/demo.known-repos.json").exists());
    // Pulling new requirements never created or widened an assignment.
    assert!(!home.join("forge-auth.json").exists());

    // Recovery, executed: bind a credential for the new group's repository...
    knit_with_env(
        &target,
        [
            "auth",
            "add",
            "ci",
            "--provider",
            "github",
            "--host",
            "forge.example",
            "--token-env",
            "KNIT_TEST_TOKEN",
        ],
        &[home_env],
    );
    let assigned = knit_with_env(
        &target,
        [
            "auth",
            "use",
            "ci",
            "--project",
            "demo",
            "--repo",
            "newrepo",
        ],
        &[home_env],
    );
    assert!(
        assigned.contains("Assigned `ci` to newrepo"),
        "setup must be able to bind the failed repo: {assigned}"
    );

    // ...and the next pull clones it through the binding (the insteadOf
    // rewrite stands in for the access the credential grants).
    let bare = make_bare_repo(&root, "newrepo");
    let config = instead_of_config(&root, newrepo_url, &bare);
    let pull = knit_with_env(
        &target,
        ["pull", "--bundles"],
        &[
            home_env,
            ("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
            ("KNIT_TEST_TOKEN", "recovered-secret"),
        ],
    );
    assert!(
        pull.contains("recovered") && pull.contains("newrepo"),
        "the retried pull must clone the missing checkout: {pull}"
    );
    assert!(target.join("newrepo").join(".git").exists());

    fs::remove_dir_all(root).unwrap();
}

/// The pending known-repos map follows the authoritative membership: entries
/// for repos the membership dropped are pruned, the rest survive, and pulling
/// metadata alone never assigns anything.
#[test]
fn pull_prunes_known_pending_from_authoritative_membership() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let frontend_url = "https://forge.example/org/frontend.git";
    let docs_url = "https://forge.example/org/docs.git";

    let fake_dir = root.join("fake-remote");
    let v1 = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("frontend".to_string(), frontend_url.to_string()),
            ("docs".to_string(), docs_url.to_string()),
        ],
        None,
    );
    let base_url = spawn_fake_remote_api(&fake_dir, v1.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    knit_with_env(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--repo",
            "backend",
            "--no-worktree",
        ],
        &[home_env],
    );
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_url, "docs": docs_url})
    );

    // The membership drops `docs` on the remote; the pending map follows.
    fs::write(
        fake_dir.join("export.json"),
        export_body(
            &[
                ("backend".to_string(), backend.to_string_lossy().to_string()),
                ("frontend".to_string(), frontend_url.to_string()),
            ],
            None,
        )
        .to_string(),
    )
    .unwrap();
    knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_url}),
        "the pruned membership must leave the pending map"
    );
    assert!(!home.join("forge-auth.json").exists());

    fs::remove_dir_all(root).unwrap();
}

/// A scoped workspace cannot shrink the remote's whole auth definitions: its
/// push merges the local shape over the remote membership and keeps the
/// remote's groups when it has any.
#[test]
fn scoped_project_push_preserves_remote_auth_definitions() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-push");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    // The remote carries the full membership plus its own groups.
    fs::write(
        fake_dir.join("export.json"),
        export_body(
            &[
                (
                    "backend".to_string(),
                    "https://forge.example/org/backend.git".to_string(),
                ),
                (
                    "frontend".to_string(),
                    "https://forge.example/org/frontend.git".to_string(),
                ),
            ],
            Some(serde_json::json!({"groups": [{
                "id": "remote-def", "name": "Remote", "provider": "github",
                "host": "forge.example", "repos": ["frontend"],
                "tokenTypes": ["classic_pat"],
            }]})),
        )
        .to_string(),
    )
    .unwrap();

    // A scoped workspace cloned only `backend`, and its (stale, local-only)
    // auth definitions differ from the remote's.
    let workspace = root.join("workspace");
    fs::create_dir_all(workspace.join(".knit/projects")).unwrap();
    fs::write(
        workspace.join(".knit/projects/demo.project.json"),
        serde_json::json!({
            "schemaVersion": "1",
            "kind": "KnitProject",
            "id": "demo",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "repos": [{
                "id": "backend",
                "path": workspace.join("backend").to_string_lossy(),
                "remote": "https://forge.example/org/backend.git",
                "baseBranch": "main",
            }],
            "auth": {"groups": [{
                "id": "stale-local", "name": "Stale", "provider": "github",
                "host": "forge.example", "repos": ["frontend"],
                "tokenTypes": ["classic_pat"],
            }]},
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        workspace.join(".knit/config.json"),
        serde_json::json!({
            "schemaVersion": "1",
            "activeProject": "demo",
            "scopeView": "scope",
            "syncRemotes": ["hosted"],
            "remotes": {"hosted": {"url": base_url, "token": "test-token"}},
        })
        .to_string(),
    )
    .unwrap();

    let output = knit(&workspace, ["project", "push"]);
    assert!(output.contains("pushed"), "{output}");

    let writes: Vec<serde_json::Value> =
        fs::read_to_string(fake_dir.join("project-shape-writes.jsonl"))
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let pushed = writes.last().expect("a project shape PATCH was recorded");
    let knit_project = &pushed["metadata"]["knitProject"];
    // The remote's definitions win from a scoped workspace: the stale local
    // group must not shrink or replace them.
    assert_eq!(
        knit_project["auth"]["groups"][0]["id"],
        serde_json::json!("remote-def")
    );
    // And the merged shape still carries the whole remote membership.
    let ids: Vec<&str> = knit_project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["backend", "frontend"]);

    fs::remove_dir_all(root).unwrap();
}

/// An unscoped push is the authoritative shape: an explicit empty `groups`
/// roundtrips as a clear, and absent auth stays absent so older remotes keep
/// their hosted requirements.
#[test]
fn unscoped_project_push_roundtrips_clear_and_absent_auth() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-push");
    let base_url = spawn_fake_remote_push_api(&fake_dir);

    let config = |workspace: &Path| {
        fs::create_dir_all(workspace.join(".knit/projects")).unwrap();
        fs::write(
            workspace.join(".knit/config.json"),
            serde_json::json!({
                "schemaVersion": "1",
                "activeProject": "demo",
                "syncRemotes": ["hosted"],
                "remotes": {"hosted": {"url": base_url, "token": "test-token"}},
            })
            .to_string(),
        )
        .unwrap();
    };
    let project = |workspace: &Path, auth: Option<serde_json::Value>| {
        let mut project = serde_json::json!({
            "schemaVersion": "1",
            "kind": "KnitProject",
            "id": "demo",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "repos": [{
                "id": "backend",
                "path": workspace.join("backend").to_string_lossy(),
                "remote": "https://forge.example/org/backend.git",
                "baseBranch": "main",
            }],
        });
        if let Some(auth) = auth {
            project["auth"] = auth;
        }
        fs::write(
            workspace.join(".knit/projects/demo.project.json"),
            project.to_string(),
        )
        .unwrap();
    };

    // Explicit empty groups: an intentional clear, never collapsed to `{}`.
    let clearing = root.join("clearing");
    config(&clearing);
    project(&clearing, Some(serde_json::json!({"groups": []})));
    let output = knit(&clearing, ["project", "push"]);
    assert!(output.contains("pushed"), "{output}");

    // Absent auth: omitted entirely so the remote preserves its hosted state.
    let absent = root.join("absent");
    config(&absent);
    project(&absent, None);
    let output = knit(&absent, ["project", "push"]);
    assert!(output.contains("pushed"), "{output}");

    let writes: Vec<serde_json::Value> =
        fs::read_to_string(fake_dir.join("project-shape-writes.jsonl"))
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(writes.len(), 2, "one PATCH per workspace");
    assert_eq!(
        writes[0]["metadata"]["knitProject"]["auth"],
        serde_json::json!({"groups": []}),
        "an explicit clear must roundtrip as empty groups"
    );
    assert!(
        writes[1]["metadata"]["knitProject"]
            .as_object()
            .unwrap()
            .get("auth")
            .is_none(),
        "absent auth must be omitted, not serialized as null"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Partial exports (repos withheld from this token) carry a projection of
/// the auth groups: an empty or narrowed `groups` there means "nothing
/// visible", never an intentional clear. Local groups survive a partial
/// export with a message; a complete export's explicit `groups: []` clears.
#[test]
fn partial_export_preserves_local_groups_full_export_clears() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let hidden_url = "https://forge.example/org/hidden.git";

    let fake_dir = root.join("fake-remote");
    let v1 = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("hidden".to_string(), hidden_url.to_string()),
        ],
        Some(serde_json::json!({"groups": [{
            "id": "gh-a", "name": "A", "provider": "github",
            "host": "forge.example", "repos": ["hidden"],
            "tokenTypes": ["classic_pat"],
        }]})),
    );
    let base_url = spawn_fake_remote_api(&fake_dir, v1.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    // The private repo fails to clone, but its entry and the group land.
    knit_with_env(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[home_env],
    );
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["id"],
        serde_json::json!("gh-a")
    );

    // A partial export projects both the membership and the groups down to
    // what this token can see: it must not read as a clear.
    let mut v2 = export_body(
        &[("backend".to_string(), backend.to_string_lossy().to_string())],
        Some(serde_json::json!({"groups": []})),
    );
    v2["data"]["omittedRepositoryCount"] = serde_json::json!(1);
    fs::write(fake_dir.join("export.json"), v2.to_string()).unwrap();
    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(
        pull.contains("partial") && pull.contains("keeping 1 local group(s)"),
        "a partial export must preserve local groups with a message: {pull}"
    );
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["id"],
        serde_json::json!("gh-a"),
        "the withheld group must survive the partial export"
    );
    // The withheld repo's entry survives too (removals are disabled on a
    // partial export), so the group's reference still resolves.
    let ids: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["backend", "hidden"]);

    // A complete export is authoritative: its explicit empty groups clears.
    let v3 = export_body(
        &[("backend".to_string(), backend.to_string_lossy().to_string())],
        Some(serde_json::json!({"groups": []})),
    );
    fs::write(fake_dir.join("export.json"), v3.to_string()).unwrap();
    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(pull.contains("0 group(s)"), "{pull}");
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"],
        serde_json::json!({"groups": []}),
        "a full export's explicit groups[] must clear"
    );

    fs::remove_dir_all(root).unwrap();
}

/// The scoped companion to the partial-export rule: here the hidden repo is
/// never a project entry — a scoped clone keeps it only in the pending
/// known-repos map. A partial export projecting `groups: []` must keep that
/// pending entry, because the preserved local auth doc still references it
/// and semantic validation would otherwise fail; the full authoritative
/// export prunes the pending entry and clears the groups together.
#[test]
fn scoped_partial_export_keeps_hidden_pending_and_validates() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let hidden_url = "https://forge.example/org/hidden.git";

    let fake_dir = root.join("fake-remote");
    let v1 = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("hidden".to_string(), hidden_url.to_string()),
        ],
        Some(serde_json::json!({"groups": [{
            "id": "gh-a", "name": "A", "provider": "github",
            "host": "forge.example", "repos": ["hidden"],
            "tokenTypes": ["classic_pat"],
        }]})),
    );
    let base_url = spawn_fake_remote_api(&fake_dir, v1.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    // Scoped clone: `hidden` is out of scope, so it exists only as a pending
    // known-repos entry while its group still lands (validated against the
    // full remote membership).
    knit_with_env(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--repo",
            "backend",
            "--no-worktree",
        ],
        &[home_env],
    );
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(pending["repos"], serde_json::json!({"hidden": hidden_url}));
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["id"],
        serde_json::json!("gh-a")
    );
    let status = knit_with_env(&target, ["auth", "status", "--json"], &[home_env]);
    assert!(status.contains("gh-a"), "{status}");

    // Partial export: `hidden` is withheld and groups project to []. The
    // pending entry must survive so the preserved group still resolves.
    let mut v2 = export_body(
        &[("backend".to_string(), backend.to_string_lossy().to_string())],
        Some(serde_json::json!({"groups": []})),
    );
    v2["data"]["omittedRepositoryCount"] = serde_json::json!(1);
    fs::write(fake_dir.join("export.json"), v2.to_string()).unwrap();
    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(
        pull.contains("partial") && pull.contains("keeping 1 local group(s)"),
        "a partial export must preserve local groups with a message: {pull}"
    );
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"hidden": hidden_url}),
        "the withheld repo's pending entry must survive the partial export"
    );
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["id"],
        serde_json::json!("gh-a"),
        "the withheld group must survive the partial export"
    );
    let status = knit_with_env(&target, ["auth", "status", "--json"], &[home_env]);
    assert!(
        status.contains("gh-a"),
        "semantic validation must succeed over the preserved state: {status}"
    );

    // A full authoritative export with backend only is authoritative over
    // absent keys: it prunes the pending entry and clears the groups.
    let v3 = export_body(
        &[("backend".to_string(), backend.to_string_lossy().to_string())],
        Some(serde_json::json!({"groups": []})),
    );
    fs::write(fake_dir.join("export.json"), v3.to_string()).unwrap();
    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(pull.contains("0 group(s)"), "{pull}");
    assert!(
        !target.join(".knit/projects/demo.known-repos.json").exists(),
        "the full export must prune the hidden pending entry"
    );
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"],
        serde_json::json!({"groups": []}),
        "a full export's explicit groups[] must clear"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Combined scope dynamics: a scoped pull whose scope view covers a newly
/// added private repo fails that add (keeping the entry), gains a new group
/// for an out-of-scope repo (kept in the pending map), and the pending map
/// is written even when the pull errors earlier on invalid auth
/// requirements — leaving a state the grouped setup's semantic validation
/// accepts. Binding the failed repo and pulling again recovers it, still
/// inside the scope.
#[test]
fn scoped_pull_failed_add_out_of_scope_group_and_pending_before_errors() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let frontend_url = "https://forge.example/org/frontend.git";
    let newrepo_url = "https://forge.example/org/newrepo.git";
    let extra_url = "https://forge.example/org/extra.git";

    let fake_dir = root.join("fake-remote");
    let v1 = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("frontend".to_string(), frontend_url.to_string()),
        ],
        None,
    );
    let base_url = spawn_fake_remote_api(&fake_dir, v1.to_string());
    // The scope view names a repo that does not exist yet: when `newrepo`
    // arrives on the remote, it is in scope and its private add fails here.
    fs::write(
        fake_dir.join("views.json"),
        serde_json::json!({
            "data": {"views": {"core": {"base": "none", "include": ["backend", "newrepo"]}}}
        })
        .to_string(),
    )
    .unwrap();
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    knit_with_env(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--view",
            "core",
            "--no-worktree",
        ],
        &[home_env],
    );
    let config = read_json(&target.join(".knit/config.json"));
    assert_eq!(config["scopeView"], serde_json::json!("core"));
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_url})
    );

    let groups = serde_json::json!({"groups": [
        {
            "id": "gh-new", "name": "New", "provider": "github",
            "host": "forge.example", "repos": ["newrepo"],
            "tokenTypes": ["classic_pat"],
        },
        {
            "id": "gh-out", "name": "Out", "provider": "github",
            "host": "forge.example", "repos": ["extra"],
            "tokenTypes": ["classic_pat"],
        },
    ]});
    let v2_repos = vec![
        ("backend".to_string(), backend.to_string_lossy().to_string()),
        ("frontend".to_string(), frontend_url.to_string()),
        ("newrepo".to_string(), newrepo_url.to_string()),
        ("extra".to_string(), extra_url.to_string()),
    ];

    // First an invalid variant: a group references a repo no membership
    // carries. The pull must fail on it — after the pending map was already
    // written, so the out-of-scope membership stays discoverable.
    let mut invalid = export_body(&v2_repos, Some(groups.clone()));
    invalid["data"]["knitProject"]["auth"]["groups"][0]["repos"] = serde_json::json!(["ghost"]);
    fs::write(fake_dir.join("export.json"), invalid.to_string()).unwrap();
    let failed = knit_fails_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(
        failed.contains("Refusing to import invalid project auth requirements"),
        "{failed}"
    );
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({
            "frontend": frontend_url,
            "extra": extra_url,
            // newrepo is known membership with no local entry yet (the repo
            // reconcile never ran): the early snapshot keeps it discoverable
            // too, and the post-reconcile refresh below narrows it.
            "newrepo": newrepo_url,
        }),
        "the pending map must be written before the auth reconcile errors"
    );

    // The valid export: the in-scope private add fails and keeps its entry,
    // the new out-of-scope group lands, and `extra` stays pending.
    fs::write(
        fake_dir.join("export.json"),
        export_body(&v2_repos, Some(groups)).to_string(),
    )
    .unwrap();
    let pull = knit_with_env(&target, ["pull", "--bundles"], &[home_env]);
    assert!(
        pull.contains("Project repo add failed: newrepo"),
        "the in-scope private add must fail visibly: {pull}"
    );
    assert!(
        pull.contains("outside scope view core"),
        "the out-of-scope repos must be reported as skipped by scope: {pull}"
    );
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    let group_ids: Vec<&str> = project["auth"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["id"].as_str().unwrap())
        .collect();
    assert_eq!(group_ids, vec!["gh-new", "gh-out"]);
    // Semantic-validation state: every group repo resolves against the
    // project entries or the pending map (what grouped setup validates
    // against), and nothing was assigned silently.
    let entry_ids: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert!(entry_ids.contains(&"newrepo"), "{entry_ids:?}");
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_url, "extra": extra_url})
    );
    assert!(!home.join("forge-auth.json").exists());
    // `auth status` runs clean over that state.
    let status = knit_with_env(
        &target,
        ["auth", "status", "--project", "demo"],
        &[home_env],
    );
    assert!(status.contains("demo"), "{status}");

    // Recovery, executed: bind the failed repo and pull — the retry works
    // because the scope view was recorded and the entry never left.
    knit_with_env(
        &target,
        [
            "auth",
            "add",
            "ci",
            "--provider",
            "github",
            "--host",
            "forge.example",
            "--token-env",
            "KNIT_TEST_TOKEN",
        ],
        &[home_env],
    );
    let assigned = knit_with_env(
        &target,
        [
            "auth",
            "use",
            "ci",
            "--project",
            "demo",
            "--repo",
            "newrepo",
        ],
        &[home_env],
    );
    assert!(
        assigned.contains("Assigned `ci` to newrepo"),
        "setup must bind the failed in-scope repo: {assigned}"
    );
    let bare = make_bare_repo(&root, "newrepo");
    let gitconfig = instead_of_config(&root, newrepo_url, &bare);
    let pull = knit_with_env(
        &target,
        ["pull", "--bundles"],
        &[
            home_env,
            ("GIT_CONFIG_GLOBAL", gitconfig.to_str().unwrap()),
            ("KNIT_TEST_TOKEN", "scoped-secret"),
        ],
    );
    assert!(
        pull.contains("recovered") && pull.contains("newrepo"),
        "the scoped retry must clone the missing checkout: {pull}"
    );
    assert!(target.join("newrepo").join(".git").exists());
    assert!(
        !target.join("extra").exists(),
        "out-of-scope stays uncloned"
    );

    fs::remove_dir_all(root).unwrap();
}
