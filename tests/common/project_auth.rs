use super::{git, init_repo};
use std::{fs, path::Path};

/// A knitProject membership entry: local id, forge remote, base branch.
pub fn membership_repo(id: &str, remote: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "path": "",
        "remote": remote,
        "baseBranch": "main",
    })
}

/// An export repository record for the same repo.
pub fn export_record(id: &str, remote: &str) -> serde_json::Value {
    serde_json::json!({
        "localId": id,
        "name": id,
        "defaultBranch": None::<String>,
        "remoteUrl": remote,
        "metadata": {},
    })
}

pub fn auth_group(id: &str, host: &str, repos: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "provider": "github",
        "host": host,
        "repos": repos,
        "tokenTypes": ["classic_pat"],
    })
}

/// Build a project export body with the given membership, records, and auth.
pub fn export_body(
    repos: &[(String, String)],
    auth: Option<serde_json::Value>,
) -> serde_json::Value {
    let membership: Vec<_> = repos
        .iter()
        .map(|(id, remote)| membership_repo(id, remote))
        .collect();
    let records: Vec<_> = repos
        .iter()
        .map(|(id, remote)| export_record(id, remote))
        .collect();
    let mut knit_project = serde_json::json!({
        "schemaVersion": "1",
        "kind": "KnitProject",
        "id": "demo",
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-01T00:00:00Z",
        "repos": membership,
    });
    if let Some(auth) = auth {
        knit_project["auth"] = auth;
    }
    serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": knit_project,
            "repositories": records,
            "bundles": [],
            "historyEvents": [],
        }
    })
}

pub fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A global git config that maps one reserved forge URL onto a local bare
/// repository: the stand-in for the access a linked credential grants.
pub fn instead_of_config(root: &Path, url: &str, bare: &Path) -> std::path::PathBuf {
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

pub fn make_bare_named(root: &Path, name: &str) -> std::path::PathBuf {
    let work = root.join(format!("{name}-source"));
    init_repo(&work, name);
    let bare = root.join(format!("{name}.git"));
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
