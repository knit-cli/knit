//! `knit remote projects` — list the remote projects visible to the resolved
//! remote token via `GET /api/v1/projects`. Works outside any workspace using
//! the same remote/token resolution as `knit clone`, so a driver (like ivaldi)
//! can discover what it may clone before cloning it.

use super::client::request_json;
use super::clone::resolve_remote_for_clone_classified;
use super::{print_json_error_envelope, RemoteErrorKind, RemoteOwnerSummary};
use crate::output as out;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One project as the projects index returns it. Only the fields the CLI
/// surfaces are decoded; unknown fields are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectListItem {
    id: String,
    name: String,
    slug: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    owner: Option<RemoteOwnerSummary>,
    #[serde(default)]
    organization: Option<RemoteOwnerSummary>,
    #[serde(default)]
    repository_count: Option<u64>,
}

/// Machine-readable `knit remote projects --json` document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectsDocument {
    remote: String,
    url: String,
    projects: Vec<RemoteProjectsEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectsEntry {
    id: String,
    /// Username or org slug: the `owner` half of a `knit clone owner/slug`
    /// reference.
    owner: String,
    slug: String,
    name: String,
    description: Option<String>,
    visibility: String,
    /// Omitted (never faked) when the index response carries no count.
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_count: Option<u64>,
}

pub fn list_remote_projects(remote_name: Option<&str>, json: bool) -> Result<()> {
    match fetch_remote_projects(remote_name) {
        Ok(document) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document)
                        .context("failed to serialize remote projects document")?
                );
            } else {
                print_projects_table(&document);
            }
            Ok(())
        }
        Err((kind, error)) => {
            if json {
                print_json_error_envelope(kind, &error);
            }
            Err(error)
        }
    }
}

fn fetch_remote_projects(
    remote_name: Option<&str>,
) -> std::result::Result<RemoteProjectsDocument, (RemoteErrorKind, anyhow::Error)> {
    let (remote_name, remote, _stored_token, token) =
        resolve_remote_for_clone_classified(remote_name, None, None)?;
    let projects: Vec<RemoteProjectListItem> =
        request_json(&remote, &token, "GET", "/projects", None)
            .map_err(|error| (RemoteErrorKind::Http, error))?;
    Ok(RemoteProjectsDocument {
        remote: remote_name,
        url: remote.url,
        projects: projects.into_iter().map(projects_entry).collect(),
    })
}

fn projects_entry(project: RemoteProjectListItem) -> RemoteProjectsEntry {
    // The API serializes `owner` for both owner kinds with `slug` carrying the
    // namespace handle (username or org slug); `organization.slug` is the
    // fallback for older servers that only attach the organization summary.
    let owner = project
        .owner
        .and_then(|owner| owner.slug)
        .or_else(|| {
            project
                .organization
                .and_then(|organization| organization.slug)
        })
        .unwrap_or_default();
    RemoteProjectsEntry {
        id: project.id,
        owner,
        slug: project.slug,
        name: project.name,
        description: project.description,
        visibility: project.visibility.unwrap_or_default(),
        repository_count: project.repository_count,
    }
}

fn clone_reference(entry: &RemoteProjectsEntry) -> String {
    if entry.owner.is_empty() {
        entry.slug.clone()
    } else {
        format!("{}/{}", entry.owner, entry.slug)
    }
}

fn print_projects_table(document: &RemoteProjectsDocument) {
    if document.projects.is_empty() {
        println!("{}", out::muted("No projects visible to this token."));
        return;
    }
    let reference_width = document
        .projects
        .iter()
        .map(|entry| clone_reference(entry).len())
        .max()
        .unwrap_or(0);
    let name_width = document
        .projects
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    for entry in &document.projects {
        println!(
            "{} {:<name_width$} {}",
            out::repo_field(&clone_reference(entry), reference_width),
            entry.name,
            out::muted(&entry.visibility)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(value: serde_json::Value) -> RemoteProjectListItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn entry_takes_owner_slug_from_user_owner() {
        let entry = projects_entry(item(json!({
            "id": "p-1",
            "name": "Knit Tools",
            "slug": "knit-tools",
            "description": "workspace",
            "visibility": "private",
            "owner": {"type": "user", "id": "u-1", "name": "Marc", "slug": "marc-merino"},
            "organization": null,
        })));
        assert_eq!(entry.owner, "marc-merino");
        assert_eq!(clone_reference(&entry), "marc-merino/knit-tools");
    }

    #[test]
    fn entry_falls_back_to_organization_slug() {
        let entry = projects_entry(item(json!({
            "id": "p-2",
            "name": "Acme",
            "slug": "acme",
            "organization": {"id": "o-1", "slug": "acme-org"},
        })));
        assert_eq!(entry.owner, "acme-org");
    }

    #[test]
    fn document_serializes_to_the_contract_shape() {
        let document = RemoteProjectsDocument {
            remote: "origin".to_string(),
            url: "https://knit.example.com".to_string(),
            projects: vec![RemoteProjectsEntry {
                id: "p-1".to_string(),
                owner: "marc-merino".to_string(),
                slug: "knit-tools".to_string(),
                name: "knit-tools".to_string(),
                description: None,
                visibility: "private".to_string(),
                repository_count: None,
            }],
        };
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(
            value,
            json!({
                "remote": "origin",
                "url": "https://knit.example.com",
                "projects": [{
                    "id": "p-1",
                    "owner": "marc-merino",
                    "slug": "knit-tools",
                    "name": "knit-tools",
                    "description": null,
                    "visibility": "private",
                }],
            })
        );
    }

    #[test]
    fn repository_count_appears_when_the_index_provides_it() {
        let entry = projects_entry(item(json!({
            "id": "p-1",
            "name": "n",
            "slug": "s",
            "owner": {"slug": "o"},
            "repositoryCount": 8,
        })));
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["repositoryCount"], json!(8));
    }

    #[test]
    fn error_envelope_matches_the_contract() {
        let error = anyhow::anyhow!("No remote selected.");
        let value = super::super::json_error_envelope(RemoteErrorKind::NoRemote, &error);
        assert_eq!(
            value,
            json!({"error": {"kind": "noRemote", "message": "No remote selected."}})
        );
    }

    #[test]
    fn cli_parses_remote_projects_and_clone_json() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "knit", "remote", "projects", "--remote", "hosted", "--json",
        ])
        .unwrap();
        match cli.command {
            crate::cli::Commands::Remote {
                command:
                    crate::cli::RemoteCommand::Projects {
                        remote: Some(remote),
                        json: true,
                    },
            } => assert_eq!(remote, "hosted"),
            _ => panic!("unexpected parse for remote projects"),
        }

        let cli = crate::Cli::try_parse_from(["knit", "clone", "acme/widgets", "--json"]).unwrap();
        match cli.command {
            crate::cli::Commands::Clone { project, json, .. } => {
                assert_eq!(project, "acme/widgets");
                assert!(json);
            }
            _ => panic!("unexpected parse for clone --json"),
        }
    }
}
