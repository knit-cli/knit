//! The review state Knit already has on record, read without asking the host.
//!
//! `knit status` and `knit publish status` stay offline: they read the gates
//! that `knit publish create` and `knit publish sync` last recorded for each
//! review, and say how old that record is. A missing or old record sends the
//! reader to `knit publish status --live`, which asks the host.

use crate::model::{ChangeGroup, Gate, GateActor, GateState, PublicationEntry};
use crate::providers::{gates, publication_for_repo};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// How long a recorded reading is trusted before `Next:` asks for a live check.
/// Recorded gates keep their timestamp while they stay the same, so this
/// bounds the time since the reading last changed.
pub(crate) const FRESH_FOR_MINUTES: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mergeable {
    /// The review conflicts with its base.
    Conflict,
    /// The base requires the review to be up to date and it is not.
    Behind,
    /// Gates were recorded and none of them reported a conflict.
    Clean,
    /// No gates are on record for this review.
    Unknown,
}

impl Mergeable {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mergeable::Conflict => "conflict",
            Mergeable::Behind => "behind",
            Mergeable::Clean => "clean",
            Mergeable::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Readiness {
    /// A recorded review needs action from you before it can land.
    Blocked,
    /// A recorded review's state is unknown or too old to trust.
    Unverified,
    /// Every open review was recorded recently with nothing for you to do.
    Ready,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewReading {
    #[serde(skip)]
    pub repo_id: String,
    pub number: u64,
    pub url: String,
    /// The host state as recorded, lowercased: `open`, `merged`, `closed`.
    pub state: String,
    /// Null for a review that is no longer open.
    pub mergeable: Option<Mergeable>,
    /// `ready` or the recorded gates' headline; null when nothing is recorded.
    pub verdict: Option<String>,
    /// A recorded gate is blocked on you (conflict, draft, failing checks...).
    pub blocked: bool,
    pub open_gates: Vec<Gate>,
    /// When the recorded gates last changed.
    pub recorded_at: Option<String>,
    pub age_seconds: Option<i64>,
    /// The record is older than the freshness window, or commits were
    /// recorded in this repo after it.
    pub stale: bool,
}

impl ReviewReading {
    fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// An open review whose recorded state cannot be trusted as current.
    pub(crate) fn needs_live_check(&self) -> bool {
        self.is_open() && (self.mergeable == Some(Mergeable::Unknown) || self.stale)
    }
}

pub(crate) fn read_reviews(bundle: &ChangeGroup, now: DateTime<Utc>) -> Vec<ReviewReading> {
    bundle
        .repos
        .iter()
        .filter_map(|repo| publication_for_repo(bundle, &repo.id))
        .map(|publication| read_review(bundle, publication, now))
        .collect()
}

fn read_review(
    bundle: &ChangeGroup,
    publication: &PublicationEntry,
    now: DateTime<Utc>,
) -> ReviewReading {
    let state = publication.state.to_lowercase();
    let mut reading = ReviewReading {
        repo_id: publication.repo_id.clone(),
        number: publication.number,
        url: publication.url.clone(),
        state,
        mergeable: None,
        verdict: None,
        blocked: false,
        open_gates: Vec::new(),
        recorded_at: None,
        age_seconds: None,
        stale: false,
    };
    if !reading.is_open() {
        return reading;
    }
    let Some(recorded) = publication.gates.as_ref().filter(|g| !g.items.is_empty()) else {
        reading.mergeable = Some(Mergeable::Unknown);
        return reading;
    };

    let items = &recorded.items;
    let open = |kind: &str| {
        items
            .iter()
            .any(|gate| gate.kind == kind && gate.state != GateState::Met)
    };
    reading.mergeable = Some(if open("conflict") {
        Mergeable::Conflict
    } else if open("behind") {
        Mergeable::Behind
    } else {
        Mergeable::Clean
    });
    reading.verdict = Some(gates::headline(items).unwrap_or_else(|| "ready".to_string()));
    reading.blocked = items
        .iter()
        .any(|gate| gate.state == GateState::Blocked && gate.actor == GateActor::You);
    reading.open_gates = gates::open(items).cloned().collect();
    reading.recorded_at = Some(recorded.updated_at.clone());

    match parse_time(&recorded.updated_at) {
        Some(at) => {
            let age = now.signed_duration_since(at);
            reading.age_seconds = Some(age.num_seconds().max(0));
            let committed_since = bundle.commit_groups.iter().any(|group| {
                group
                    .commits
                    .iter()
                    .any(|commit| commit.repo_id == publication.repo_id)
                    && parse_time(&group.created_at).is_some_and(|created| created > at)
            });
            reading.stale = age > Duration::minutes(FRESH_FOR_MINUTES) || committed_since;
        }
        None => reading.stale = true,
    }
    reading
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

pub(crate) fn readiness(readings: &[ReviewReading]) -> Readiness {
    let open: Vec<&ReviewReading> = readings.iter().filter(|r| r.is_open()).collect();
    if open.iter().any(|r| r.blocked) {
        Readiness::Blocked
    } else if open.iter().any(|r| r.needs_live_check()) {
        Readiness::Unverified
    } else {
        Readiness::Ready
    }
}

/// The `Next:` advice the recorded readings call for, or `None` when the
/// usual landing advice applies.
pub(crate) fn advice(readings: &[ReviewReading]) -> Option<String> {
    let open: Vec<&ReviewReading> = readings.iter().filter(|r| r.is_open()).collect();

    let behind_base: Vec<&ReviewReading> = open
        .iter()
        .copied()
        .filter(|r| matches!(r.mergeable, Some(Mergeable::Conflict | Mergeable::Behind)))
        .collect();
    if !behind_base.is_empty() {
        let described = behind_base
            .iter()
            .map(|r| {
                let what = match r.mergeable {
                    Some(Mergeable::Conflict) => "conflicts with its base",
                    _ => "is behind its base",
                };
                format!("{} #{} {what}", r.repo_id, r.number)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let repos = behind_base
            .iter()
            .map(|r| r.repo_id.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        return Some(format!(
            "do not land yet: as last recorded, {described}. Run `knit land update --push {repos}` to merge the base in, then confirm with `knit publish status --live`."
        ));
    }

    let blocked: Vec<&ReviewReading> = open.iter().copied().filter(|r| r.blocked).collect();
    if !blocked.is_empty() {
        let described = blocked
            .iter()
            .map(|r| {
                format!(
                    "{} #{} {}",
                    r.repo_id,
                    r.number,
                    r.verdict.as_deref().unwrap_or("needs action")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "do not land yet: as last recorded, {described}. Fix it, then confirm with `knit publish status --live`."
        ));
    }

    let unverified: Vec<&str> = open
        .iter()
        .filter(|r| r.needs_live_check())
        .map(|r| r.repo_id.as_str())
        .collect();
    if !unverified.is_empty() {
        return Some(format!(
            "the recorded review state of {} is unknown or older than {FRESH_FOR_MINUTES} minutes; check it with `knit publish status --live` before `knit land`, and resolve conflicts with a review's base with `knit land update --push <repo>`.",
            unverified.join(", ")
        ));
    }
    None
}

/// The recorded reading of one review for the human tables.
pub(crate) fn describe(reading: &ReviewReading) -> String {
    let Some(mergeable) = reading.mergeable else {
        return reading.state.clone();
    };
    let Some(verdict) = &reading.verdict else {
        return format!("{:<9} no gates recorded; check live", mergeable.as_str());
    };
    let when = match reading.age_seconds {
        Some(age) => format!("recorded {}", age_label(age)),
        None => "recorded at an unreadable time".to_string(),
    };
    let stale = if reading.stale { ", stale" } else { "" };
    format!("{:<9} {verdict}  ({when}{stale})", mergeable.as_str())
}

pub(crate) fn age_label(seconds: i64) -> String {
    match seconds {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommitGroup, CommitRef, ReviewGates};

    fn now() -> DateTime<Utc> {
        parse_time("2026-03-10T12:00:00.000Z").unwrap()
    }

    fn gate(kind: &str, state: GateState, actor: GateActor) -> Gate {
        Gate {
            kind: kind.to_string(),
            state,
            actor,
            summary: format!("{kind} summary"),
            url: None,
        }
    }

    type Review<'a> = (&'a str, Option<(Vec<Gate>, &'a str)>);

    fn bundle(reviews: &[Review]) -> ChangeGroup {
        let mut bundle: ChangeGroup = serde_json::from_value(serde_json::json!({
            "schemaVersion": "0.1",
            "kind": "ChangeGroup",
            "id": "sample",
            "title": "sample",
            "createdAt": "2026-03-01T00:00:00.000Z",
            "updatedAt": "2026-03-01T00:00:00.000Z",
            "repos": reviews.iter().map(|(repo, _)| serde_json::json!({
                "id": repo,
                "path": format!("/tmp/{repo}"),
                "remote": null,
                "baseBranch": "main",
                "featureBranch": "knit/sample",
                "worktreePath": null,
            })).collect::<Vec<_>>(),
            "commitGroups": [],
        }))
        .unwrap();
        for (number, (repo, gates)) in reviews.iter().enumerate() {
            bundle.publications.push(PublicationEntry {
                repo_id: repo.to_string(),
                provider: "github".to_string(),
                kind: "pull_request".to_string(),
                number: number as u64 + 1,
                url: format!("https://example.test/acme/{repo}/pull/{}", number + 1),
                base_branch: "main".to_string(),
                head_branch: "knit/sample".to_string(),
                state: "OPEN".to_string(),
                title: None,
                author: None,
                applied: None,
                gates: gates.as_ref().map(|(items, at)| ReviewGates {
                    updated_at: at.to_string(),
                    items: items.clone(),
                }),
                checks: None,
                updated_at: "2026-03-01T00:00:00.000Z".to_string(),
            });
        }
        bundle
    }

    fn clean_gates() -> Vec<Gate> {
        vec![
            gate("review", GateState::Met, GateActor::Maintainers),
            gate("checks", GateState::Met, GateActor::Host),
        ]
    }

    fn conflict_gates() -> Vec<Gate> {
        let mut gates = clean_gates();
        gates.insert(0, gate("conflict", GateState::Blocked, GateActor::You));
        gates
    }

    const RECENT: &str = "2026-03-10T11:50:00.000Z";
    const OLD: &str = "2026-03-07T12:00:00.000Z";

    #[test]
    fn a_recorded_conflict_blocks_landing_and_points_at_land_update() {
        let bundle = bundle(&[
            ("api", Some((conflict_gates(), OLD))),
            ("web", Some((clean_gates(), RECENT))),
        ]);
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, Some(Mergeable::Conflict));
        assert!(readings[0].blocked);
        assert!(readings[0].stale);
        assert_eq!(readings[0].age_seconds, Some(3 * 86_400));
        assert_eq!(
            readings[0].verdict.as_deref(),
            Some("action needed: conflict")
        );
        assert_eq!(readiness(&readings), Readiness::Blocked);
        let advice = advice(&readings).unwrap();
        assert!(advice.starts_with("do not land yet"), "{advice}");
        assert!(
            advice.contains("api #1 conflicts with its base"),
            "{advice}"
        );
        assert!(advice.contains("`knit land update --push api`"), "{advice}");
        assert!(advice.contains("`knit publish status --live`"), "{advice}");
    }

    #[test]
    fn a_review_without_recorded_gates_needs_a_live_check() {
        let bundle = bundle(&[("api", None), ("web", Some((clean_gates(), RECENT)))]);
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, Some(Mergeable::Unknown));
        assert_eq!(readings[0].verdict, None);
        assert_eq!(readings[0].recorded_at, None);
        assert_eq!(readiness(&readings), Readiness::Unverified);
        let advice = advice(&readings).unwrap();
        assert!(
            advice.contains("recorded review state of api is"),
            "{advice}"
        );
        assert!(advice.contains("`knit publish status --live`"), "{advice}");
    }

    #[test]
    fn a_recent_clean_reading_keeps_the_usual_landing_advice() {
        let bundle = bundle(&[("api", Some((clean_gates(), RECENT)))]);
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, Some(Mergeable::Clean));
        assert_eq!(readings[0].verdict.as_deref(), Some("ready"));
        assert!(!readings[0].stale);
        assert_eq!(readings[0].age_seconds, Some(600));
        assert_eq!(readiness(&readings), Readiness::Ready);
        assert_eq!(advice(&readings), None);
    }

    #[test]
    fn an_old_clean_reading_is_stale() {
        let bundle = bundle(&[("api", Some((clean_gates(), OLD)))]);
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, Some(Mergeable::Clean));
        assert!(readings[0].stale);
        assert_eq!(readiness(&readings), Readiness::Unverified);
        assert!(advice(&readings).unwrap().contains("older than 60 minutes"));
    }

    #[test]
    fn commits_recorded_after_the_reading_make_it_stale() {
        let mut bundle = bundle(&[("api", Some((clean_gates(), RECENT)))]);
        bundle.commit_groups.push(CommitGroup {
            id: "g1".to_string(),
            message: "later work".to_string(),
            created_at: "2026-03-10T11:55:00.000Z".to_string(),
            commits: vec![CommitRef {
                repo_id: "api".to_string(),
                sha: "abc".to_string(),
            }],
            author: None,
        });
        let readings = read_reviews(&bundle, now());

        assert!(readings[0].stale);
        assert_eq!(readiness(&readings), Readiness::Unverified);
    }

    #[test]
    fn a_failing_check_blocks_without_suggesting_land_update() {
        let gates = vec![gate("checks", GateState::Blocked, GateActor::You)];
        let bundle = bundle(&[("api", Some((gates, RECENT)))]);
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, Some(Mergeable::Clean));
        assert_eq!(readiness(&readings), Readiness::Blocked);
        let advice = advice(&readings).unwrap();
        assert!(advice.contains("api #1 action needed: checks"), "{advice}");
        assert!(!advice.contains("land update"), "{advice}");
    }

    #[test]
    fn merged_reviews_are_not_judged() {
        let mut bundle = bundle(&[("api", None)]);
        bundle.publications[0].state = "MERGED".to_string();
        let readings = read_reviews(&bundle, now());

        assert_eq!(readings[0].mergeable, None);
        assert_eq!(readiness(&readings), Readiness::Ready);
        assert_eq!(advice(&readings), None);
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age_label(5), "just now");
        assert_eq!(age_label(600), "10m ago");
        assert_eq!(age_label(7_200), "2h ago");
        assert_eq!(age_label(3 * 86_400 + 5), "3d ago");
    }
}
