use crate::ids::slugify;
use crate::model::{KnitProject, ViewBase};
use crate::store::{
    bundle_path, find_knit_root, infer_worktree_bundle, load_config, load_views, read_json,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Months, NaiveDate, Utc};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Resolve project-wide query context without requiring an active bundle.
/// A bundle worktree is more specific than the workspace's default project.
pub(crate) fn resolve_query_project(project: Option<&str>) -> Result<(PathBuf, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    if let Some(project) = project {
        return Ok((root, slugify(project)));
    }

    if let Some(bundle_id) = infer_worktree_bundle(&root, &cwd) {
        let path = bundle_path(&root, &bundle_id);
        if path.exists() {
            let bundle: crate::model::ChangeGroup = read_json(&path)?;
            if let Some(project_id) = bundle.project_id {
                return Ok((root, project_id));
            }
        }
    }

    let project_id = load_config(&root)?
        .active_project
        .context("No project selected. Pass --project or run from a project bundle worktree.")?;
    Ok((root, project_id))
}

/// Resolve a named view against current project membership. Historical repo
/// ids outside that set remain selectable explicitly with --repo.
pub(crate) fn view_repo_ids(
    root: &Path,
    project_id: &str,
    project: &KnitProject,
    name: &str,
) -> Result<Vec<String>> {
    let name = slugify(name);
    let views = load_views(root, project_id)?;
    let view = views.effective_view(&name).with_context(|| {
        format!(
            "Project `{project_id}` has no locally saved view or shared template named `{name}`."
        )
    })?;
    let mut ids = BTreeSet::new();
    if view.base == ViewBase::Default {
        ids.extend(
            project
                .repos
                .iter()
                .filter(|repo| repo.include_by_default)
                .map(|repo| repo.id.clone()),
        );
    }
    ids.extend(
        view.include
            .iter()
            .filter(|id| project.repos.iter().any(|repo| &repo.id == *id))
            .cloned(),
    );
    for excluded in &view.exclude {
        ids.remove(excluded);
    }
    Ok(ids.into_iter().collect())
}

pub(crate) fn intersect_repo_filters(
    explicit: &[String],
    view: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (explicit.is_empty(), view) {
        (true, None) => None,
        (false, None) => Some(unique(explicit)),
        (true, Some(view)) => Some(unique(&view)),
        (false, Some(view)) => {
            let view = view.into_iter().collect::<BTreeSet<_>>();
            Some(
                unique(explicit)
                    .into_iter()
                    .filter(|repo| view.contains(repo))
                    .collect(),
            )
        }
    }
}

fn unique(values: &[String]) -> Vec<String> {
    values
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Plain dates mean midnight UTC for both inclusive boundaries. Relative dates
/// are evaluated against the current UTC time without consulting a checkout.
pub(crate) fn parse_history_date(_root: &Path, value: &str, since: bool) -> Result<DateTime<Utc>> {
    parse_date_at(value, since, Utc::now())
}

fn parse_date_at(value: &str, since: bool, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(DateTime::from_naive_utc_and_offset(
            date.and_hms_opt(0, 0, 0).expect("midnight is valid"),
            Utc,
        ));
    }
    let normalized = value.to_ascii_lowercase();
    let words: Vec<_> = normalized.split_whitespace().collect();
    let parsed = match words.as_slice() {
        ["now"] => Some(now),
        ["today" | "midnight"] => Some(now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()),
        ["yesterday"] => now.checked_sub_signed(Duration::days(1)),
        ["tomorrow"] => now.checked_add_signed(Duration::days(1)),
        [count, unit, "ago"] => count.parse::<u32>().ok().and_then(|count| {
            let seconds = match unit.strip_suffix('s').unwrap_or(unit) {
                "second" => 1,
                "minute" => 60,
                "hour" => 3600,
                "day" => 86400,
                "week" => 604800,
                "month" => return now.checked_sub_months(Months::new(count)),
                "year" => {
                    return count
                        .checked_mul(12)
                        .and_then(|months| now.checked_sub_months(Months::new(months)))
                }
                _ => return None,
            };
            now.checked_sub_signed(Duration::seconds(i64::from(count) * seconds))
        }),
        _ => None,
    };
    let flag = if since { "--since" } else { "--until" };
    parsed.with_context(|| format!("Invalid {flag} date `{value}`. Use ISO 8601, YYYY-MM-DD (midnight UTC), now, today, yesterday, tomorrow, or `<number> seconds/minutes/hours/days/weeks/months/years ago`."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_have_explicit_utc_boundaries_and_validated_relative_units() {
        let now = DateTime::parse_from_rfc3339("2026-03-31T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_date_at("2 weeks ago", true, now).unwrap(),
            now - Duration::weeks(2)
        );
        assert_eq!(
            parse_date_at("1 month ago", true, now)
                .unwrap()
                .to_rfc3339(),
            "2026-02-28T12:34:56+00:00"
        );
        for since in [true, false] {
            assert_eq!(
                parse_date_at("2026-03-01", since, now)
                    .unwrap()
                    .to_rfc3339(),
                "2026-03-01T00:00:00+00:00"
            );
        }
        for invalid in [
            "",
            "123notadate",
            "2 weekss ago",
            "999999999999 years ago",
            "2026-02-30",
        ] {
            assert!(parse_date_at(invalid, true, now).is_err(), "{invalid}");
        }
    }
}
