//! Per-user "views" over a project: named bundle shapes expressed as
//! include/exclude deltas on top of the project's `includeByDefault` repo set.
//!
//! Views are user-local config, stored at `.knit/views/<project-id>.views.json`
//! and synced to the sync remotes as the user's own configuration. They never live inside
//! the shared project artifact.
//!
//! Next to the personal views, the artifact caches the project's **shared view
//! templates** (admin-managed on the server) under a separate `templates` map.
//! Templates are a read-only cache refreshed wholesale by
//! `knit sync pull --views`; they are never uploaded back (`PUT /view` carries
//! the personal document only) and are never merged into the personal `views`
//! map. Resolution overlays the two: a personal view with the same name wins
//! over the template it shadows.

use super::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VIEWS_KIND: &str = "KnitProjectViews";

/// All of a user's saved views for a single project, plus an optional default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitProjectViews {
    pub schema_version: String,
    pub kind: String,
    pub project_id: String,
    pub created_at: String,
    pub updated_at: String,
    /// Name of the view new bundles should apply, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view: Option<String>,
    #[serde(default)]
    pub views: BTreeMap<String, ProjectView>,
    /// Admin-managed shared templates cached from the sync remote. Kept beside
    /// — never inside — the personal `views` map, and replaced wholesale on
    /// every views pull so admin updates become visible.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub templates: BTreeMap<String, ProjectView>,
}

/// Where a named view came from when the personal and shared template maps
/// are overlaid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewSource {
    /// The user's own saved view.
    Personal,
    /// An admin-managed shared template the user has no personal view over.
    Template,
}

impl ViewSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ViewSource::Personal => "personal",
            ViewSource::Template => "template",
        }
    }
}

impl KnitProjectViews {
    pub fn new(project_id: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: VIEWS_KIND.to_string(),
            project_id,
            created_at: now.clone(),
            updated_at: now,
            default_view: None,
            views: BTreeMap::new(),
            templates: BTreeMap::new(),
        }
    }

    /// Resolve a view name against the overlay of personal views over shared
    /// templates: a personal view with the same name wins.
    pub fn effective_view(&self, name: &str) -> Option<&ProjectView> {
        self.views.get(name).or_else(|| self.templates.get(name))
    }

    /// The effective view for `name` together with its provenance.
    pub fn effective_view_with_source(&self, name: &str) -> Option<(&ProjectView, ViewSource)> {
        match self.views.get(name) {
            Some(view) => Some((view, ViewSource::Personal)),
            None => self
                .templates
                .get(name)
                .map(|view| (view, ViewSource::Template)),
        }
    }

    /// Every distinct view name across the personal map and the shared
    /// templates, in name order, with the winning shape and its source.
    pub fn effective_views(&self) -> Vec<(&String, &ProjectView, ViewSource)> {
        let mut names: Vec<&String> = self.views.keys().collect();
        for name in self.templates.keys() {
            if !self.views.contains_key(name) {
                names.push(name);
            }
        }
        names.sort_unstable();
        names
            .into_iter()
            .filter_map(|name| {
                self.effective_view_with_source(name)
                    .map(|(view, source)| (name, view, source))
            })
            .collect()
    }
}

/// How a view seeds its repo set before `include`/`exclude` are applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewBase {
    /// Seed from the project's `includeByDefault` set (delta view).
    #[default]
    Default,
    /// Seed empty: `include` is the complete repo list (absolute view).
    None,
}

impl ViewBase {
    pub fn is_default(&self) -> bool {
        matches!(self, ViewBase::Default)
    }
}

/// A single named view: deltas applied over the project default repo set, or —
/// with `base: none` — an absolute repo list that never absorbs default-set
/// changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    /// Seeding mode. Skipped when `default` so existing files stay unchanged.
    #[serde(default, skip_serializing_if = "ViewBase::is_default")]
    pub base: ViewBase,
    /// Repo ids to add to the seed set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Repo ids to drop from the seed set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(include: &[&str], exclude: &[&str]) -> ProjectView {
        ProjectView {
            base: ViewBase::Default,
            include: include.iter().map(|id| id.to_string()).collect(),
            exclude: exclude.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn older_artifacts_without_templates_stay_readable_and_unchanged() {
        let raw = serde_json::json!({
            "schemaVersion": "1",
            "kind": "KnitProjectViews",
            "projectId": "demo",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "defaultView": "backend",
            "views": {"backend": {"exclude": ["frontend"]}},
        });
        let parsed: KnitProjectViews = serde_json::from_value(raw.clone()).unwrap();
        assert!(parsed.templates.is_empty());
        // Round-tripping an artifact with no templates must not grow a
        // `templates` key: personal .views.json files stay byte-stable.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
    }

    #[test]
    fn personal_view_shadows_a_same_named_template() {
        let mut views = KnitProjectViews::new("demo".to_string(), "2026-01-01T00:00:00Z".into());
        views.templates.insert("shared".into(), view(&["api"], &[]));
        views
            .views
            .insert("shared".into(), view(&["worker"], &["api"]));

        let effective = views.effective_view("shared").unwrap();
        assert_eq!(effective.include, vec!["worker".to_string()]);
        assert_eq!(
            views.effective_view_with_source("shared").unwrap().1,
            ViewSource::Personal
        );
    }

    #[test]
    fn effective_views_union_both_maps_in_name_order() {
        let mut views = KnitProjectViews::new("demo".to_string(), "2026-01-01T00:00:00Z".into());
        views.templates.insert("alpha".into(), view(&["api"], &[]));
        views.templates.insert("shared".into(), view(&["api"], &[]));
        views.views.insert("personal".into(), view(&[], &["docs"]));
        views.views.insert("shared".into(), view(&["worker"], &[]));

        let effective = views.effective_views();
        let names: Vec<&str> = effective.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "personal", "shared"]);
        let sources: Vec<&str> = effective
            .iter()
            .map(|(_, _, source)| source.as_str())
            .collect();
        assert_eq!(sources, vec!["template", "personal", "personal"]);
        // The shadowed template yields the personal shape.
        assert_eq!(
            effective[2].1.include,
            vec!["worker".to_string()],
            "personal shape must win"
        );
    }
}
