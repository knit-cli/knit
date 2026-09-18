//! Shared view templates: admin-managed bundle shapes served beside the
//! user's personal views, cached locally, and shadowed by personal views of
//! the same name. These tests pin the contract end to end — resolution
//! precedence, the personal-only upload, the admin refresh path, scoped
//! clones from a template with zero shared mutation, older servers without
//! templates, and the provenance surfaces.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Write the local views artifact for project `demo` directly, the shape a
/// `sync pull --views` or `knit clone` would have left behind.
fn write_views_artifact(workspace: &Path, views: Value) {
    let path = workspace.join(".knit/views/demo.views.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&views).unwrap()),
    )
    .unwrap();
}

fn read_views_artifact(workspace: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/views/demo.views.json")).unwrap(),
    )
    .unwrap()
}

/// A local views artifact carrying both maps: a personal `shared` view that
/// must win over the same-named template, plus template-only names.
fn overlaid_views_artifact() -> Value {
    json!({
        "schemaVersion": "0.1",
        "kind": "KnitProjectViews",
        "projectId": "demo",
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-01T00:00:00Z",
        "views": {"shared": {}},
        "templates": {
            "shared": {"include": ["docs"]},
            "be-only": {"base": "none", "include": ["backend"]},
            "frozen": {"include": ["docs"]},
        },
    })
}

#[test]
fn personal_views_override_same_named_templates_everywhere() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    write_views_artifact(&workspace, overlaid_views_artifact());

    // `list` shows the effective set with provenance: the personal `shared`
    // entry is unmarked; template-only entries say so.
    let list = knit(&workspace, ["view", "list"]);
    let personal_line = list
        .lines()
        .find(|line| line.split_whitespace().any(|token| token == "shared"))
        .filter(|line| !line.contains("(shared template)"))
        .unwrap_or_else(|| panic!("personal `shared` entry must be unmarked: {list}"));
    assert!(personal_line.contains("(no deltas)"), "{list}");
    assert!(
        list.lines()
            .any(|line| line.contains("be-only") && line.contains("(shared template)")),
        "{list}"
    );

    // Resolution precedence: the personal `shared` (plain default set: no
    // deltas) wins over the template's `+docs`.
    knit(
        &workspace,
        [
            "bundle",
            "personal wins",
            "--view",
            "shared",
            "--no-worktree",
        ],
    );
    assert_eq!(
        bundle_repo_ids(&workspace, "personal-wins"),
        vec!["backend", "frontend"]
    );

    // A template-only name resolves like any saved view.
    knit(
        &workspace,
        [
            "bundle",
            "template only",
            "--view",
            "be-only",
            "--no-worktree",
        ],
    );
    assert_eq!(
        bundle_repo_ids(&workspace, "template-only"),
        vec!["backend"]
    );

    // The personal default may name a template; bare `knit bundle` uses it.
    knit(&workspace, ["view", "default", "be-only"]);
    knit(&workspace, ["bundle", "defaulted", "--no-worktree"]);
    assert_eq!(bundle_repo_ids(&workspace, "defaulted"), vec!["backend"]);

    // `show` prints the winning (personal) shape for a shadowed name.
    let shared: Value =
        serde_json::from_str(&knit(&workspace, ["view", "show", "shared"])).unwrap();
    assert_eq!(shared, json!({}));
    // The whole document keeps the two maps side by side.
    let document: Value = serde_json::from_str(&knit(&workspace, ["view", "show"])).unwrap();
    assert!(document["templates"]["shared"].is_object(), "{document}");
    assert!(document["views"]["shared"].is_object(), "{document}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn views_artifact_with_templates_matches_the_published_schema() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    write_views_artifact(&workspace, overlaid_views_artifact());

    let schema: Value =
        serde_json::from_str(&knit(&workspace, ["schema", "print", "views"])).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let instance = read_views_artifact(&workspace);
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "views artifact rejected by its schema: {errors:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_views_uploads_the_personal_document_only() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    write_views_artifact(
        &workspace,
        json!({
            "schemaVersion": "0.1",
            "kind": "KnitProjectViews",
            "projectId": "demo",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "defaultView": "shared",
            "views": {"shared": {}},
            "templates": {"be-only": {"base": "none", "include": ["backend"]}},
        }),
    );

    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, json!({"data": {}}).to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["sync", "push", "--views"], &env);
    assert!(output.contains("pushed views"), "{output}");

    let puts = recorded_views_puts(&fake_dir);
    assert_eq!(puts.len(), 1, "exactly one views upload: {puts:?}");
    let pushed = &puts[0];
    assert_eq!(pushed["defaultView"], "shared");
    assert_eq!(pushed["views"]["shared"], json!({}));
    // The shared template cache must never ride along in the upload.
    assert!(
        pushed.get("templates").is_none(),
        "templates leaked into the personal upload: {pushed}"
    );

    // The local cache survives the push untouched.
    let artifact = read_views_artifact(&workspace);
    assert!(artifact["templates"]["be-only"].is_object());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_pull_views_replaces_the_template_cache_but_not_personal_state() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, json!({"data": {}}).to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    // Admin v1: templates `core` and `wide`.
    fs::write(
        fake_dir.join("views.json"),
        json!({
            "data": {
                "defaultView": "mine",
                "views": {"mine": {"exclude": ["frontend"]}},
                "templates": {
                    "core": {"exclude": ["frontend", "docs"]},
                    "wide": {"include": ["docs"]},
                },
            }
        })
        .to_string(),
    )
    .unwrap();
    let pulled = knit_with_env(&workspace, ["sync", "pull", "--views"], &env);
    assert!(pulled.contains("1 view(s)"), "{pulled}");
    assert!(pulled.contains("2 shared template(s)"), "{pulled}");
    let artifact = read_views_artifact(&workspace);
    assert_eq!(
        artifact["templates"]["core"]["exclude"],
        json!(["frontend", "docs"])
    );
    assert!(artifact["templates"]["wide"].is_object());

    // Admin v2: `core` reshaped, `wide` removed, `pinned` added. The user's
    // personal view and default must not move.
    fs::write(
        fake_dir.join("views.json"),
        json!({
            "data": {
                "defaultView": "mine",
                "views": {"mine": {"exclude": ["frontend"]}},
                "templates": {
                    "core": {"exclude": ["docs"]},
                    "pinned": {"base": "none", "include": ["backend"]},
                },
            }
        })
        .to_string(),
    )
    .unwrap();
    knit_with_env(&workspace, ["sync", "pull", "--views"], &env);
    let artifact = read_views_artifact(&workspace);
    assert_eq!(artifact["templates"]["core"]["exclude"], json!(["docs"]));
    assert!(artifact["templates"]["pinned"].is_object());
    assert!(
        artifact["templates"].as_object().unwrap().len() == 2,
        "stale template must be dropped on refresh: {}",
        artifact["templates"]
    );
    assert_eq!(artifact["defaultView"], "mine");
    assert_eq!(artifact["views"]["mine"]["exclude"], json!(["frontend"]));

    // The refreshed templates resolve for bundles; the personal default still
    // wins for a bare start.
    knit(
        &workspace,
        ["bundle", "refreshed", "--view", "pinned", "--no-worktree"],
    );
    assert_eq!(bundle_repo_ids(&workspace, "refreshed"), vec!["backend"]);

    fs::remove_dir_all(root).unwrap();
}

/// An export with two cloneable repos and no bundles. `project.id` carries
/// the server's immutable id, the way a real export does.
fn two_repo_export(root: &Path) -> Value {
    let backend = root.join("backend-source");
    let frontend = root.join("frontend-source");
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    json!({
        "data": {
            "project": {"id": "8f14e45f-ceea-467f-ab69-2d4e1f7f4b2a", "slug": "demo"},
            "knitProject": null,
            "repositories": [
                {
                    "localId": "backend",
                    "name": "backend",
                    "defaultBranch": null,
                    "remoteUrl": backend.to_string_lossy(),
                    "metadata": {},
                },
                {
                    "localId": "frontend",
                    "name": "frontend",
                    "defaultBranch": null,
                    "remoteUrl": frontend.to_string_lossy(),
                    "metadata": {},
                },
            ],
            "bundles": [],
            "historyEvents": [],
        }
    })
}

#[test]
fn clone_with_a_template_only_view_clones_its_repos_without_any_shared_mutation() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, two_repo_export(&root).to_string());
    // The user keeps no personal views; the admin publishes one template.
    fs::write(
        fake_dir.join("views.json"),
        json!({
            "data": {
                "templates": {"be": {"exclude": ["frontend"]}},
            }
        })
        .to_string(),
    )
    .unwrap();
    let target = root.join("workspace");

    let (stdout, stderr, success) = knit_split_output(
        &root,
        &[
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
            "be",
            "--no-worktree",
            "--json",
        ],
        &[],
    );
    assert!(success, "template-scoped clone failed: {stderr}");
    let document: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["scopeView"], "be");
    assert_eq!(
        document["repos"],
        json!([{"id": "backend", "status": "cloned"}])
    );
    assert!(target.join("backend").exists());
    assert!(!target.join("frontend").exists());

    // The template cache lands in the local artifact; the personal map stays
    // exactly what the user had (empty).
    let artifact = read_views_artifact(&target);
    assert_eq!(artifact["views"], json!({}));
    assert_eq!(artifact["templates"]["be"]["exclude"], json!(["frontend"]));

    // Zero shared mutation: nothing was PUT, personal or otherwise.
    assert!(
        recorded_views_puts(&fake_dir).is_empty(),
        "a template-only clone must not upload anything"
    );

    // The cloned workspace can list and use the template.
    let list = knit(&target, ["view", "list"]);
    assert!(
        list.lines()
            .any(|line| line.contains("be") && line.contains("(shared template)")),
        "{list}"
    );
    knit(
        &target,
        ["bundle", "template work", "--view", "be", "--no-worktree"],
    );
    assert_eq!(bundle_repo_ids(&target, "template-work"), vec!["backend"]);

    // The scoped artifact matches the published schema, templates included.
    let schema: Value = serde_json::from_str(&knit(&target, ["schema", "print", "views"])).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&read_views_artifact(&target))
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "cloned views artifact rejected by its schema: {errors:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_older_server_without_templates_keeps_working() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, two_repo_export(&root).to_string());
    // An older server: personal views only, no `templates` key at all.
    fs::write(
        fake_dir.join("views.json"),
        json!({
            "data": {
                "defaultView": "backend",
                "views": {"backend": {"exclude": ["frontend"]}},
            }
        })
        .to_string(),
    )
    .unwrap();
    let target = root.join("workspace");

    let (_stdout, stderr, success) = knit_split_output(
        &root,
        &[
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
        &[],
    );
    assert!(success, "clone against an older server failed: {stderr}");

    let artifact = read_views_artifact(&target);
    assert!(artifact.get("templates").is_none(), "{artifact}");
    assert_eq!(artifact["defaultView"], "backend");
    assert_eq!(artifact["views"]["backend"]["exclude"], json!(["frontend"]));
    let list = knit(&target, ["view", "list"]);
    assert!(list.contains("backend"), "{list}");
    assert!(!list.contains("(shared template)"), "{list}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn template_only_names_are_admin_managed_for_personal_mutations() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    write_views_artifact(&workspace, overlaid_views_artifact());

    // Removing a template-only name is refused clearly, never silent.
    let failure = knit_fails(&workspace, ["view", "rm", "be-only"]);
    assert!(
        failure.contains("shared template") && failure.contains("admin"),
        "rm must explain the template is admin-managed: {failure}"
    );
    // Freeze likewise.
    let failure = knit_fails(&workspace, ["view", "freeze", "frozen"]);
    assert!(
        failure.contains("shared template") && failure.contains("admin"),
        "freeze must explain the template is admin-managed: {failure}"
    );
    // Nothing was mutated by the refusals: no personal override appeared and
    // the templates are intact.
    let artifact = read_views_artifact(&workspace);
    assert!(artifact["views"].get("be-only").is_none(), "{artifact}");
    assert!(artifact["views"].get("frozen").is_none(), "{artifact}");
    assert!(artifact["templates"]["be-only"].is_object());
    assert!(artifact["templates"]["frozen"].is_object());

    // Copying a template into a personal view is the supported fork.
    let saved = knit(&workspace, ["view", "save", "fork", "--from", "frozen"]);
    assert!(saved.contains("saved view"), "{saved}");
    let artifact = read_views_artifact(&workspace);
    assert_eq!(artifact["views"]["fork"]["include"], json!(["docs"]));
    assert!(artifact["templates"]["frozen"].is_object());

    // Editing a template-only name seeds a personal override from it.
    let included = knit(&workspace, ["view", "include", "be-only", "frontend"]);
    assert!(included.contains("personal override"), "{included}");
    let artifact = read_views_artifact(&workspace);
    assert_eq!(
        artifact["views"]["be-only"],
        json!({"base": "none", "include": ["backend", "frontend"]})
    );
    // The template itself is untouched by the override.
    assert_eq!(
        artifact["templates"]["be-only"]["include"],
        json!(["backend"])
    );

    // Removing a personal override reveals the template again and says so.
    let removed = knit(&workspace, ["view", "rm", "be-only"]);
    assert!(removed.contains("shared template"), "{removed}");
    let artifact = read_views_artifact(&workspace);
    assert!(artifact["views"].get("be-only").is_none());
    assert!(artifact["templates"]["be-only"].is_object());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_views_json_tags_each_entry_with_its_source() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, two_repo_export(&root).to_string());
    fs::write(
        fake_dir.join("views.json"),
        json!({
            "data": {
                "defaultView": "backend",
                "views": {"backend": {"exclude": ["frontend"]}},
                "templates": {
                    "all": {"base": "none", "include": ["backend", "frontend"]},
                    // Shadowed by the personal view of the same name.
                    "backend": {"include": ["frontend"]},
                },
            }
        })
        .to_string(),
    )
    .unwrap();

    let (stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "remote",
            "views",
            "acme/demo",
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--json",
        ],
        &[("KNIT_REMOTE_TOKEN", "test-token")],
    );
    assert!(success, "remote views failed: {stderr}");
    let document: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["defaultView"], "backend");
    assert_eq!(
        document["views"],
        json!([
            {
                "name": "all",
                "source": "template",
                "base": "none",
                "include": ["backend", "frontend"],
                "exclude": [],
            },
            {
                "name": "backend",
                "source": "personal",
                "base": "default",
                "include": [],
                "exclude": ["frontend"],
            },
        ]),
        "effective set with per-entry provenance: {document}"
    );

    // The human table labels templates too.
    let output = knit_with_env(
        &root,
        [
            "remote",
            "views",
            "acme/demo",
            "--remote",
            "hosted",
            "--url",
            &base_url,
        ],
        &[("KNIT_REMOTE_TOKEN", "test-token")],
    );
    assert!(output.contains("shared template"), "{output}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn owner_qualified_remote_views_resolve_the_immutable_project_id() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, two_repo_export(&root).to_string());
    fs::write(
        fake_dir.join("views.json"),
        json!({"data": {"views": {"backend": {"exclude": ["frontend"]}}}}).to_string(),
    )
    .unwrap();

    // An owner/slug reference must not resolve the views by bare slug: slugs
    // are ambiguous across owners, so it is pinned to the export's immutable
    // project id first.
    let (_stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "remote",
            "views",
            "acme/demo",
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--json",
        ],
        &[("KNIT_REMOTE_TOKEN", "test-token")],
    );
    assert!(success, "owner-qualified remote views failed: {stderr}");
    let gets = recorded_view_gets(&fake_dir);
    assert_eq!(
        gets,
        vec!["/api/v1/projects/8f14e45f-ceea-467f-ab69-2d4e1f7f4b2a/view".to_string()],
        "views must be fetched by the immutable project id: {gets:?}"
    );

    // A bare slug keeps the direct, cheaper path.
    let (_stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "remote", "views", "demo", "--remote", "hosted", "--url", &base_url, "--json",
        ],
        &[("KNIT_REMOTE_TOKEN", "test-token")],
    );
    assert!(success, "bare-slug remote views failed: {stderr}");
    let gets = recorded_view_gets(&fake_dir);
    assert_eq!(
        gets,
        vec![
            "/api/v1/projects/8f14e45f-ceea-467f-ab69-2d4e1f7f4b2a/view".to_string(),
            "/api/v1/projects/demo/view".to_string(),
        ],
        "{gets:?}"
    );

    fs::remove_dir_all(root).unwrap();
}
