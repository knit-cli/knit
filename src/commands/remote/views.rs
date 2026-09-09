//! `knit remote views` — list the current user's saved views for a remote
//! project before cloning it, so a driver (like ivaldi) can offer
//! `knit clone --view <name>` as a choice. Works outside any workspace using
//! the same remote/token resolution as `knit clone`.

use super::client::request_json;
use super::clone::{parse_clone_reference, resolve_remote_for_clone_classified};
use super::{print_json_error_envelope, RemoteErrorKind, RemoteViews};
use crate::model::{ProjectView, ViewBase};
use crate::output as out;
use anyhow::{Context, Result};
use serde::Serialize;

/// Machine-readable `knit remote views --json` document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteViewsDocument {
    remote: String,
    url: String,
    project: String,
    default_view: Option<String>,
    views: Vec<RemoteViewsEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteViewsEntry {
    name: String,
    /// `default` seeds from the project's default repo set, `none` means the
    /// include list is the complete shape.
    base: &'static str,
    include: Vec<String>,
    exclude: Vec<String>,
}

pub fn list_remote_views(
    project_identifier: &str,
    remote_name: Option<&str>,
    url: Option<&str>,
    json: bool,
) -> Result<()> {
    match fetch_remote_views_document(project_identifier, remote_name, url) {
        Ok(document) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document)
                        .context("failed to serialize remote views document")?
                );
            } else {
                print_views_table(&document);
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

fn fetch_remote_views_document(
    project_identifier: &str,
    remote_name: Option<&str>,
    url: Option<&str>,
) -> std::result::Result<RemoteViewsDocument, (RemoteErrorKind, anyhow::Error)> {
    let reference = parse_clone_reference(project_identifier, url)
        .map_err(|error| (RemoteErrorKind::NoRemote, error))?;
    let (remote_name, remote, _stored_token, token) =
        resolve_remote_for_clone_classified(remote_name, reference.remote_url.as_deref(), None)?;
    let token = token
        .context(
            "No remote token configured. Set KNIT_REMOTE_<NAME>_TOKEN or KNIT_REMOTE_TOKEN, or run `knit remote token <name> <token>`.",
        )
        .map_err(|error| (RemoteErrorKind::NoToken, error))?;
    let (_owner, slug) = super::client::split_project_identifier(&reference.project_identifier);
    let views: RemoteViews = request_json(
        &remote,
        &token,
        "GET",
        &format!("/projects/{slug}/view"),
        None,
    )
    .map_err(|error| (RemoteErrorKind::Http, error))?;
    Ok(RemoteViewsDocument {
        remote: remote_name,
        url: remote.url,
        project: slug,
        default_view: views.default_view,
        views: views.views.into_iter().map(views_entry).collect(),
    })
}

fn views_entry((name, view): (String, ProjectView)) -> RemoteViewsEntry {
    RemoteViewsEntry {
        name,
        base: match view.base {
            ViewBase::Default => "default",
            ViewBase::None => "none",
        },
        include: view.include,
        exclude: view.exclude,
    }
}

fn print_views_table(document: &RemoteViewsDocument) {
    println!(
        "{} {} {}",
        out::heading("Views:"),
        out::repo(&document.project),
        out::muted(format!("({} @ {})", document.remote, document.url))
    );
    if document.views.is_empty() {
        println!(
            "  {}",
            out::muted("no saved views; clone the whole project or pass `--repo <id>`")
        );
        return;
    }
    for view in &document.views {
        let marker = if document.default_view.as_deref() == Some(view.name.as_str()) {
            "*"
        } else {
            " "
        };
        let mut delta = Vec::new();
        if view.base == "none" {
            delta.push("absolute".to_string());
        }
        if !view.include.is_empty() {
            delta.push(format!("+{}", view.include.join(" +")));
        }
        if !view.exclude.is_empty() {
            delta.push(format!("-{}", view.exclude.join(" -")));
        }
        println!(
            "{marker} {} {}",
            out::repo(&view.name),
            out::muted(delta.join(" "))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn document_serializes_to_the_contract_shape() {
        let mut views = BTreeMap::new();
        views.insert(
            "backend".to_string(),
            ProjectView {
                base: ViewBase::Default,
                include: vec![],
                exclude: vec!["frontend".to_string()],
            },
        );
        views.insert(
            "legacy".to_string(),
            ProjectView {
                base: ViewBase::None,
                include: vec!["api".to_string()],
                exclude: vec![],
            },
        );
        let document = RemoteViewsDocument {
            remote: "hosted".to_string(),
            url: "https://api.example.test".to_string(),
            project: "demo".to_string(),
            default_view: Some("backend".to_string()),
            views: views.into_iter().map(views_entry).collect(),
        };
        assert_eq!(
            serde_json::to_value(&document).unwrap(),
            serde_json::json!({
                "remote": "hosted",
                "url": "https://api.example.test",
                "project": "demo",
                "defaultView": "backend",
                "views": [
                    {"name": "backend", "base": "default", "include": [], "exclude": ["frontend"]},
                    {"name": "legacy", "base": "none", "include": ["api"], "exclude": []},
                ],
            })
        );
    }
}
