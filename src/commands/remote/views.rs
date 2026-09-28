//! `knit remote views` — list the views usable for `knit clone --view <name>`
//! on a remote project before cloning it, so a driver (like ivaldi) can offer
//! the names as choices. Works outside any workspace using the same
//! remote/token resolution as `knit clone`.
//!
//! The listing is the **effective** view set: the user's personal views plus
//! the project's admin-managed shared templates, with a personal view winning
//! over a same-named template. Each entry carries its `source` so drivers can
//! label template entries as shared.

use super::client::{fetch_project_export, request_json};
use super::clone::{parse_clone_reference, resolve_remote_for_clone_classified};
use super::{print_json_error_envelope, RemoteErrorKind, RemoteViews};
use crate::model::{ProjectView, ViewBase, ViewSource};
use crate::output as out;
use anyhow::{Context, Result};
use serde::Serialize;

/// Machine-readable `knit remote views --json` document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
/// `views` holds the effective set (personal overlaid on shared templates);
/// the additive per-entry `source` field distinguishes the two.
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
    /// `personal` is the caller's own saved view; `template` is an
    /// admin-managed shared template.
    source: &'static str,
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
    let (owner, slug) = super::client::split_project_identifier(&reference.project_identifier);
    // The views endpoint resolves a bare slug without an owner namespace, so
    // an `owner/slug` reference must first be pinned to the immutable project
    // id: the export endpoint is the one that honors `owner`, and slugs are
    // ambiguous across owners.
    let views_project_id = match owner {
        Some(owner) => {
            let export = fetch_project_export(&remote, Some(&token), &format!("{owner}/{slug}"))
                .map_err(|error| (RemoteErrorKind::Http, error))?;
            export.project.id.unwrap_or_else(|| slug.clone())
        }
        None => slug.clone(),
    };
    let views: RemoteViews = request_json(
        &remote,
        &token,
        "GET",
        &format!("/projects/{views_project_id}/view"),
        None,
    )
    .map_err(|error| (RemoteErrorKind::Http, error))?;
    let default_view = views.default_view.clone();
    let effective = effective_entries(views);
    Ok(RemoteViewsDocument {
        remote: remote_name,
        url: remote.url,
        project: slug,
        default_view,
        views: effective,
    })
}

/// The effective entries of a remote views response: personal views plus
/// templates not shadowed by a personal view of the same name, in name order.
fn effective_entries(views: RemoteViews) -> Vec<RemoteViewsEntry> {
    let mut entries: Vec<RemoteViewsEntry> = views
        .views
        .into_iter()
        .map(|(name, view)| views_entry(name, view, ViewSource::Personal))
        .collect();
    let shadowed: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();
    entries.extend(
        views
            .templates
            .into_iter()
            .filter(|(name, _)| !shadowed.contains(name))
            .map(|(name, view)| views_entry(name, view, ViewSource::Template)),
    );
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

fn views_entry(name: String, view: ProjectView, source: ViewSource) -> RemoteViewsEntry {
    RemoteViewsEntry {
        name,
        source: source.as_str(),
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
        if view.source == "template" {
            delta.push("shared template".to_string());
        }
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
            views: views
                .into_iter()
                .map(|(name, view)| views_entry(name, view, ViewSource::Personal))
                .collect(),
        };
        assert_eq!(
            serde_json::to_value(&document).unwrap(),
            serde_json::json!({
                "remote": "hosted",
                "url": "https://api.example.test",
                "project": "demo",
                "defaultView": "backend",
                "views": [
                    {"name": "backend", "source": "personal", "base": "default", "include": [], "exclude": ["frontend"]},
                    {"name": "legacy", "source": "personal", "base": "none", "include": ["api"], "exclude": []},
                ],
            })
        );
    }

    #[test]
    fn effective_entries_overlay_personal_over_templates_in_name_order() {
        let mut personal = BTreeMap::new();
        personal.insert(
            "shared".to_string(),
            ProjectView {
                base: ViewBase::Default,
                include: vec!["worker".to_string()],
                exclude: vec![],
            },
        );
        personal.insert(
            "mine".to_string(),
            ProjectView {
                base: ViewBase::Default,
                include: vec![],
                exclude: vec!["docs".to_string()],
            },
        );
        let mut templates = BTreeMap::new();
        templates.insert(
            "alpha".to_string(),
            ProjectView {
                base: ViewBase::None,
                include: vec!["api".to_string()],
                exclude: vec![],
            },
        );
        templates.insert(
            // Shadowed by the personal view of the same name.
            "shared".to_string(),
            ProjectView {
                base: ViewBase::None,
                include: vec!["api".to_string()],
                exclude: vec![],
            },
        );
        let entries = effective_entries(RemoteViews {
            default_view: None,
            views: personal,
            templates,
        });
        let names: Vec<(&str, &str)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.source))
            .collect();
        assert_eq!(
            names,
            vec![
                ("alpha", "template"),
                ("mine", "personal"),
                ("shared", "personal"),
            ]
        );
        // The shadowed template yields the personal shape.
        assert_eq!(entries[2].include, vec!["worker".to_string()]);
    }
}
