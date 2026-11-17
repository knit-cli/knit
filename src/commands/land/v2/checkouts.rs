//! Pinned execution checkouts of a landing run.
//!
//! Command steps run in detached `git worktree`s at immutable revisions,
//! created under `<run>.run.checkouts/<hash>` next to the run receipt. They
//! are a cache: the receipt records every pinned revision, and the executor
//! recreates a missing checkout from it, so a resume or a recovery never
//! depends on one surviving. A run that finished no longer needs them, and
//! each holds a full working tree plus build output, so they are removed:
//!
//! - a run that succeeded, or whose recovery restored everything, right away;
//! - a failed run once a newer run of the same bundle replaced it, its bundle
//!   is finished, or it has not been touched for [`FAILED_RUN_RETENTION_DAYS`]
//!   (until then they stay so the failure can be debugged and resumed as-is).
//!
//! Runs that are paused, pending or still marked running can be resumed and
//! keep their checkouts until a newer run or a finished bundle replaces them.
//! Removal uses `git worktree remove` without `--force` unless the caller asks
//! for it, so a checkout holding untracked files is kept and reported.

use crate::commands::bundle::BundleStatus;
use crate::model::ChangeGroup;
use crate::output as out;
use crate::store::read_json;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Days a failed run keeps its checkouts for debugging and resume before
/// they count as leftovers.
pub(crate) const FAILED_RUN_RETENTION_DAYS: i64 = 3;

/// `<dir>/<name>.run.json` keeps its checkouts in `<dir>/<name>.run.checkouts`.
pub(super) fn dir_for(run_path: &Path) -> PathBuf {
    run_path.with_extension("checkouts")
}

/// The few receipt fields the cleanup decision reads. Receipts embed whole
/// plans and bundles; deserializing only these keeps a sweep over hundreds of
/// runs cheap.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunHead {
    #[serde(default)]
    bundle_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    recovery_status: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    plan: PlanHead,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanHead {
    #[serde(default)]
    source_project_id: Option<String>,
}

impl RunHead {
    fn read(path: &Path) -> Result<Self> {
        let file =
            fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_reader(std::io::BufReader::new(file))
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    fn from_value(run: &Value) -> Self {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        Self {
            bundle_id: text(&run["bundleId"]),
            status: text(&run["status"]),
            recovery_status: text(&run["recoveryStatus"]),
            created_at: text(&run["createdAt"]),
            updated_at: text(&run["updatedAt"]),
            plan: PlanHead {
                source_project_id: text(&run["plan"]["sourceProjectId"]),
            },
        }
    }

    fn project(&self) -> &str {
        self.plan.source_project_id.as_deref().unwrap_or("local")
    }
}

fn timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// Why a run no longer needs its checkouts, or `None` while it may still be
/// resumed or recovered from them.
fn finished_reason(
    head: &RunHead,
    run_path: &Path,
    workspace: Option<&Path>,
    now: DateTime<Utc>,
) -> Option<String> {
    if head.status.as_deref() == Some("succeeded") {
        return Some("succeeded".into());
    }
    // Forward resume is permanently blocked once recovery starts, and a
    // restored recovery has nothing left to retry.
    if head.recovery_status.as_deref() == Some("restored") {
        return Some("recovered".into());
    }
    if let Some(reason) = superseded(head, run_path, workspace) {
        return Some(reason);
    }
    if head.status.as_deref() == Some("failed") {
        let last =
            timestamp(head.updated_at.as_deref()).or(timestamp(head.created_at.as_deref()))?;
        if now - last >= Duration::days(FAILED_RUN_RETENTION_DAYS) {
            return Some(format!(
                "failed more than {FAILED_RUN_RETENTION_DAYS} days ago"
            ));
        }
    }
    None
}

/// A run is superseded once its bundle is finished or a newer run of the same
/// bundle exists: Knit refuses to resume an older run over a newer one.
fn superseded(head: &RunHead, run_path: &Path, workspace: Option<&Path>) -> Option<String> {
    let bundle_id = head.bundle_id.as_deref()?;
    if let Some(root) = workspace {
        let path = crate::store::bundle_path(root, bundle_id);
        if let Ok(bundle) = read_json::<ChangeGroup>(&path) {
            let state = crate::commands::bundle::bundle_state(&bundle);
            if matches!(
                state,
                BundleStatus::Archived | BundleStatus::Closed | BundleStatus::Deleted
            ) {
                return Some(format!("superseded: bundle {bundle_id} is finished"));
            }
        }
    }
    let created = timestamp(head.created_at.as_deref())?;
    let dir = run_path.parent()?;
    // Local runs are named `land-<bundle>-<run>.run.json`; other runs worth
    // comparing are the ones that still hold checkouts. Neither filter is
    // exact, so the parsed bundle id decides.
    let prefix = format!("land-{bundle_id}-");
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path == run_path || !name.ends_with(".run.json") {
            continue;
        }
        if !name.starts_with(&prefix) && !dir_for(&path).is_dir() {
            continue;
        }
        let Ok(other) = RunHead::read(&path) else {
            continue;
        };
        if other.bundle_id.as_deref() == Some(bundle_id)
            && timestamp(other.created_at.as_deref()).is_some_and(|other| other > created)
        {
            return Some("superseded by a newer run of the same bundle".into());
        }
    }
    None
}

/// Checkouts of a run that no longer needs them.
#[derive(Debug)]
pub(crate) struct Leftover {
    pub run: PathBuf,
    pub checkouts: PathBuf,
    pub count: usize,
    pub reason: String,
    project: Option<String>,
}

fn checkout_count(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0)
}

/// Every `*.run.checkouts` directory in `runs_dir`: the leftovers of
/// finished runs, and how many runs that can still resume keep theirs.
pub(crate) fn scan(
    runs_dir: &Path,
    workspace: Option<&Path>,
    now: DateTime<Utc>,
) -> Result<(Vec<Leftover>, usize)> {
    let mut leftovers = Vec::new();
    let mut kept = 0usize;
    if !runs_dir.is_dir() {
        return Ok((leftovers, kept));
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(runs_dir)
        .with_context(|| format!("failed to read {}", runs_dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".run.checkouts"))
        })
        .collect();
    dirs.sort();
    for checkouts in dirs {
        let run = checkouts.with_extension("json");
        let count = checkout_count(&checkouts);
        if !run.exists() {
            // Nothing can resume a run whose receipt is gone.
            leftovers.push(Leftover {
                run,
                checkouts,
                count,
                reason: "run receipt is missing".into(),
                project: None,
            });
            continue;
        }
        let head = match RunHead::read(&run) {
            Ok(head) => head,
            // An unreadable receipt is doctor's business; keep its checkouts.
            Err(_) => continue,
        };
        match finished_reason(&head, &run, workspace, now) {
            Some(reason) => leftovers.push(Leftover {
                run,
                checkouts,
                count,
                reason,
                project: Some(head.project().to_owned()),
            }),
            None => kept += 1,
        }
    }
    Ok((leftovers, kept))
}

/// What removing one run's checkouts did.
#[derive(Debug, Default)]
pub(crate) struct Removal {
    pub removed: usize,
    /// Checkouts git refused to remove, with its reason.
    pub kept: Vec<(PathBuf, String)>,
}

/// Remove every checkout of one run and then its empty directory. Each one
/// is removed through git so the source repository forgets it too; one git
/// refuses to remove (untracked files, without `force`) is kept and reported.
pub(crate) fn remove(checkouts: &Path, force: bool) -> Result<Removal> {
    let mut removal = Removal::default();
    if !checkouts.is_dir() {
        return Ok(removal);
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(checkouts)
        .with_context(|| format!("failed to read {}", checkouts.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    for path in entries {
        let result = if path.join(".git").is_file() {
            crate::commands::clean::remove_git_worktree_from_self(&path, force)
        } else {
            // Only `git worktree add` creates entries here. One without its
            // `.git` link is what an interrupted removal leaves behind: git
            // already forgot it, and nothing can run in it any more.
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))
        };
        match result {
            Ok(()) => removal.removed += 1,
            Err(error) => removal.kept.push((path, format!("{error:#}"))),
        }
    }
    if removal.kept.is_empty() {
        // Fails harmlessly when something other than checkouts sits inside.
        let _ = fs::remove_dir(checkouts);
    }
    Ok(removal)
}

fn report(run: &Path, removal: &Removal, reason: &str) {
    if removal.removed > 0 {
        crate::human!(
            "{} {} landing checkout(s) of {} ({reason})",
            out::movement("removed"),
            removal.removed,
            out::path(run.display())
        );
    }
    for (path, error) in &removal.kept {
        crate::human!(
            "{} {} {}",
            out::warn("kept landing checkout:"),
            out::path(path.display()),
            out::muted(format!(
                "({error}; remove it with `knit clean --land-runs --force`)"
            ))
        );
    }
}

/// Run once a landing execution ends, while it still holds the project's
/// landing ownership. Removes this run's checkouts when it finished, and
/// those of earlier runs of the same project it made redundant; ownership
/// guarantees none of those is executing. Best effort: cleanup never changes
/// the outcome of the landing or its receipt.
pub(super) fn after_run(run_path: &Path, run: &Value, workspace: Option<&Path>) {
    let head = RunHead::from_value(run);
    let now = Utc::now();
    if let Some(reason) = finished_reason(&head, run_path, workspace, now) {
        match remove(&dir_for(run_path), false) {
            Ok(removal) => report(run_path, &removal, &reason),
            Err(error) => crate::human!(
                "{} landing checkouts were not removed: {error:#}",
                out::warn("Warn:")
            ),
        }
    }
    let Some(dir) = run_path.parent() else {
        return;
    };
    let Ok((leftovers, _)) = scan(dir, workspace, now) else {
        return;
    };
    for leftover in leftovers {
        if leftover.run == run_path || leftover.project.as_deref() != Some(head.project()) {
            continue;
        }
        if let Ok(removal) = remove(&leftover.checkouts, false) {
            report(&leftover.run, &removal, &leftover.reason);
        }
    }
}

/// `knit clean --land-runs`: remove the checkouts of every finished run in
/// the workspace, keeping those of runs that can still resume.
pub(crate) fn clean(root: &Path, force: bool) -> Result<()> {
    let runs_dir = root.join(".knit/land-runs");
    let (leftovers, kept) = scan(&runs_dir, Some(root), Utc::now())?;
    let mut removed = 0usize;
    let mut refused = 0usize;
    for leftover in &leftovers {
        // Re-read right before removing: a run of another project may have
        // been resumed since the scan.
        if leftover.run.exists() {
            let Ok(head) = RunHead::read(&leftover.run) else {
                continue;
            };
            if finished_reason(&head, &leftover.run, Some(root), Utc::now()).is_none() {
                continue;
            }
        }
        let removal = remove(&leftover.checkouts, force)?;
        report(&leftover.run, &removal, &leftover.reason);
        removed += removal.removed;
        refused += removal.kept.len();
    }
    if removed == 0 && refused == 0 {
        crate::human!("{}", out::muted("No landing run checkouts to clean."));
    }
    if kept > 0 {
        crate::human!(
            "{}",
            out::muted(format!(
                "Kept the checkouts of {} landing run(s) that can still resume (failed runs keep theirs for {FAILED_RUN_RETENTION_DAYS} days).",
                kept
            ))
        );
    }
    if refused > 0 {
        anyhow::bail!("{refused} landing checkout(s) could not be removed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(crate::ids::node_id(label));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(bundle: &str, status: &str, created: &str, updated: &str) -> Value {
        json!({"schemaVersion":"0.2","kind":"KnitLandRun","bundleId":bundle,"status":status,
            "createdAt":created,"updatedAt":updated,"plan":{"sourceProjectId":"demo"}})
    }

    fn write(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    #[test]
    fn finished_runs_release_checkouts_and_resumable_runs_keep_them() {
        let dir = temp_dir("land-checkouts");
        let now = timestamp(Some("2026-03-10T00:00:00.000Z")).unwrap();
        let cases = [
            (
                "ok",
                run(
                    "a",
                    "succeeded",
                    "2026-03-09T00:00:00.000Z",
                    "2026-03-09T00:00:00.000Z",
                ),
                true,
            ),
            (
                "fresh",
                run(
                    "b",
                    "failed",
                    "2026-03-09T00:00:00.000Z",
                    "2026-03-09T00:00:00.000Z",
                ),
                false,
            ),
            (
                "stale",
                run(
                    "c",
                    "failed",
                    "2026-03-01T00:00:00.000Z",
                    "2026-03-02T00:00:00.000Z",
                ),
                true,
            ),
            (
                "paused",
                run(
                    "d",
                    "paused",
                    "2026-03-01T00:00:00.000Z",
                    "2026-03-01T00:00:00.000Z",
                ),
                false,
            ),
            (
                "running",
                run(
                    "e",
                    "running",
                    "2026-03-01T00:00:00.000Z",
                    "2026-03-01T00:00:00.000Z",
                ),
                false,
            ),
            (
                "old-f",
                run(
                    "f",
                    "failed",
                    "2026-03-09T00:00:00.000Z",
                    "2026-03-09T00:00:00.000Z",
                ),
                true,
            ),
            (
                "new-f",
                run(
                    "f",
                    "failed",
                    "2026-03-09T01:00:00.000Z",
                    "2026-03-09T01:00:00.000Z",
                ),
                false,
            ),
        ];
        for (name, value, _) in &cases {
            let path = dir.join(format!("{name}.run.json"));
            write(&path, value);
            fs::create_dir_all(dir_for(&path).join("checkout")).unwrap();
        }
        let mut recovered = run(
            "g",
            "failed",
            "2026-03-09T00:00:00.000Z",
            "2026-03-09T00:00:00.000Z",
        );
        recovered["recoveryStatus"] = json!("restored");
        write(&dir.join("recovered.run.json"), &recovered);
        fs::create_dir_all(dir.join("recovered.run.checkouts/checkout")).unwrap();
        fs::create_dir_all(dir.join("orphan.run.checkouts/checkout")).unwrap();

        let (leftovers, kept) = scan(&dir, None, now).unwrap();
        let mut finished: Vec<_> = leftovers
            .iter()
            .map(|l| l.run.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        finished.sort();
        let mut expected: Vec<_> = cases
            .iter()
            .filter(|(_, _, finished)| *finished)
            .map(|(name, ..)| format!("{name}.run.json"))
            .chain(["orphan.run.json".into(), "recovered.run.json".into()])
            .collect();
        expected.sort();
        assert_eq!(finished, expected);
        assert_eq!(kept, 4);
        assert!(leftovers.iter().all(|l| l.count == 1));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finished_bundle_supersedes_its_failed_runs() {
        let root = temp_dir("land-checkouts-bundle");
        let runs = root.join(".knit/land-runs");
        fs::create_dir_all(&runs).unwrap();
        let mut bundle = ChangeGroup::new(
            "demo".into(),
            "Synthetic".into(),
            "2026-03-01T00:00:00Z".into(),
        );
        let path = crate::store::bundle_path(&root, "demo");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::store::write_json(&path, &bundle).unwrap();
        let run_path = runs.join("land-demo-1.run.json");
        let head = RunHead::from_value(&run(
            "demo",
            "failed",
            "2026-03-09T00:00:00.000Z",
            "2026-03-09T00:00:00.000Z",
        ));
        let now = timestamp(Some("2026-03-10T00:00:00.000Z")).unwrap();
        assert_eq!(finished_reason(&head, &run_path, Some(&root), now), None);
        bundle.state = Some(crate::model::BundleState::Archived);
        crate::store::write_json(&path, &bundle).unwrap();
        assert!(finished_reason(&head, &run_path, Some(&root), now)
            .unwrap()
            .contains("finished"));
        fs::remove_dir_all(root).unwrap();
    }
}
