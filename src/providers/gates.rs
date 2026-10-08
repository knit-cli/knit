//! Landing gates: what still stands between a review and its merge, and who
//! has to act. Hosts with richer APIs (GitHub) build their own list; the rest
//! derive it from the review object, its checks, and the merge permission.

use super::{CheckRun, PullRequest};
use crate::model::{Gate, GateActor, GateState};

pub(crate) fn gate(kind: &str, state: GateState, actor: GateActor, summary: String) -> Gate {
    Gate {
        kind: kind.to_string(),
        state,
        actor,
        summary,
        url: None,
    }
}

pub(crate) fn draft() -> Gate {
    gate(
        "draft",
        GateState::Blocked,
        GateActor::You,
        "draft: mark it ready for review".to_string(),
    )
}

pub(crate) fn conflict() -> Gate {
    gate(
        "conflict",
        GateState::Blocked,
        GateActor::You,
        "conflicts with its base: run `knit land update`".to_string(),
    )
}

pub(crate) fn behind() -> Gate {
    gate(
        "behind",
        GateState::Blocked,
        GateActor::You,
        "behind its base, which must be up to date: run `knit land update`".to_string(),
    )
}

pub(crate) fn changes_requested() -> Gate {
    gate(
        "review",
        GateState::Blocked,
        GateActor::You,
        "changes requested".to_string(),
    )
}

pub(crate) fn merge_permission() -> Gate {
    gate(
        "merge_permission",
        GateState::Pending,
        GateActor::Maintainers,
        "a maintainer merges it; this account cannot".to_string(),
    )
}

pub(crate) fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// The first few names, so a summary stays one readable line.
pub(crate) fn names(names: &[&str]) -> String {
    const SHOWN: usize = 3;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{} and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

pub(crate) enum CheckOutcome {
    Passed,
    Pending,
    Failed,
}

pub(crate) fn check_outcome(run: &CheckRun) -> CheckOutcome {
    if matches!(run.bucket.as_deref(), Some("fail" | "cancel"))
        || matches!(run.state.as_deref(), Some("FAILURE" | "CANCELLED"))
    {
        CheckOutcome::Failed
    } else if matches!(run.bucket.as_deref(), Some("pass" | "skipping"))
        || matches!(run.state.as_deref(), Some("SUCCESS" | "SKIPPED"))
    {
        CheckOutcome::Passed
    } else {
        CheckOutcome::Pending
    }
}

pub(crate) fn checks(runs: &[CheckRun]) -> Option<Gate> {
    if runs.is_empty() {
        return None;
    }
    let matching = |wanted: fn(&CheckOutcome) -> bool| -> Vec<&str> {
        runs.iter()
            .filter(|run| wanted(&check_outcome(run)))
            .map(|run| run.name.as_str())
            .collect()
    };
    let failed = matching(|o| matches!(o, CheckOutcome::Failed));
    if !failed.is_empty() {
        return Some(gate(
            "checks",
            GateState::Blocked,
            GateActor::You,
            format!("checks failing: {}", names(&failed)),
        ));
    }
    let pending = matching(|o| matches!(o, CheckOutcome::Pending));
    if !pending.is_empty() {
        return Some(gate(
            "checks",
            GateState::Pending,
            GateActor::Host,
            format!("checks running: {}", names(&pending)),
        ));
    }
    Some(gate(
        "checks",
        GateState::Met,
        GateActor::Host,
        "checks passed".to_string(),
    ))
}

/// Gates for hosts without a dedicated implementation. GitLab's detailed merge
/// status arrives in `merge_state_status` and names most of its gates directly.
pub(crate) fn from_review(
    pr: &PullRequest,
    runs: Option<&[CheckRun]>,
    can_merge: Option<bool>,
) -> Vec<Gate> {
    let mut gates = Vec::new();
    if pr.is_draft == Some(true) {
        gates.push(draft());
    }
    if pr.is_conflicting() {
        gates.push(conflict());
    }
    match pr.merge_state_status.as_deref() {
        Some("NOT_APPROVED") => gates.push(gate(
            "review",
            GateState::Pending,
            GateActor::Maintainers,
            "needs approval".to_string(),
        )),
        Some("REQUESTED_CHANGES") => gates.push(changes_requested()),
        Some("CI_MUST_PASS" | "CI_STILL_RUNNING") => gates.push(gate(
            "checks",
            GateState::Pending,
            GateActor::Host,
            "the pipeline must pass".to_string(),
        )),
        Some("DISCUSSIONS_NOT_RESOLVED") => gates.push(gate(
            "discussions",
            GateState::Pending,
            GateActor::You,
            "unresolved discussions".to_string(),
        )),
        Some("NEED_REBASE" | "BEHIND") => gates.push(behind()),
        Some("BLOCKED_STATUS") => gates.push(gate(
            "host",
            GateState::Blocked,
            GateActor::Host,
            "blocked by another merge request".to_string(),
        )),
        _ => {}
    }
    if !gates.iter().any(|g| g.kind == "review") {
        match pr.review_decision.as_deref() {
            Some("APPROVED") => gates.push(gate(
                "review",
                GateState::Met,
                GateActor::Maintainers,
                "approved".to_string(),
            )),
            Some("CHANGES_REQUESTED") => gates.push(changes_requested()),
            Some("REVIEW_REQUIRED") => gates.push(gate(
                "review",
                GateState::Pending,
                GateActor::Maintainers,
                "needs an approving review".to_string(),
            )),
            _ => {}
        }
    }
    if !gates.iter().any(|g| g.kind == "checks") {
        match runs {
            Some(runs) => gates.extend(checks(runs)),
            None => gates.push(gate(
                "checks",
                GateState::Unknown,
                GateActor::Host,
                "checks could not be read".to_string(),
            )),
        }
    }
    if can_merge == Some(false) {
        gates.push(merge_permission());
    }
    gates
}

pub(crate) fn open(gates: &[Gate]) -> impl Iterator<Item = &Gate> {
    gates.iter().filter(|g| g.state != GateState::Met)
}

/// A one-line reading of the open gates: what you must do first, otherwise
/// whom the review is waiting on.
pub(crate) fn headline(gates: &[Gate]) -> Option<String> {
    let open: Vec<&Gate> = open(gates).collect();
    if open.is_empty() {
        return None;
    }
    let kinds = |actor: GateActor| -> Vec<&str> {
        let mut kinds: Vec<&str> = Vec::new();
        for gate in open.iter().filter(|g| g.actor == actor) {
            if !kinds.contains(&label(&gate.kind)) {
                kinds.push(label(&gate.kind));
            }
        }
        kinds
    };
    let yours = kinds(GateActor::You);
    if !yours.is_empty() {
        return Some(format!("action needed: {}", yours.join(", ")));
    }
    let mut parts = Vec::new();
    let maintainers = kinds(GateActor::Maintainers);
    if !maintainers.is_empty() {
        parts.push(format!("maintainers ({})", maintainers.join(", ")));
    }
    let host = kinds(GateActor::Host);
    if !host.is_empty() {
        parts.push(format!("host ({})", host.join(", ")));
    }
    Some(format!("waiting on {}", parts.join(", ")))
}

pub(crate) fn label(kind: &str) -> &str {
    match kind {
        "ci_approval" => "CI approval",
        "merge_permission" => "merge",
        "signatures" => "signed commits",
        "behind" => "update branch",
        "host" => "host rules",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 7,
            "url": "https://example.test/acme/api/pull/7",
            "state": "OPEN",
            "mergeable": "MERGEABLE",
        }))
        .unwrap()
    }

    fn run(name: &str, bucket: &str) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            state: None,
            bucket: Some(bucket.to_string()),
            url: None,
        }
    }

    #[test]
    fn a_review_awaiting_approval_is_not_ready() {
        let mut review = pr();
        review.review_decision = Some("REVIEW_REQUIRED".to_string());
        let gates = from_review(&review, Some(&[]), Some(false));
        assert_eq!(
            headline(&gates).as_deref(),
            Some("waiting on maintainers (review, merge)")
        );
    }

    #[test]
    fn your_own_gates_come_before_waiting_on_others() {
        let mut review = pr();
        review.review_decision = Some("REVIEW_REQUIRED".to_string());
        let gates = from_review(&review, Some(&[run("build", "fail")]), Some(true));
        assert_eq!(headline(&gates).as_deref(), Some("action needed: checks"));
        assert!(gates
            .iter()
            .any(|g| g.summary == "checks failing: build" && g.state == GateState::Blocked));
    }

    #[test]
    fn an_approved_review_with_green_checks_has_no_open_gates() {
        let mut review = pr();
        review.review_decision = Some("APPROVED".to_string());
        let gates = from_review(&review, Some(&[run("build", "pass")]), Some(true));
        assert_eq!(gates.len(), 2);
        assert_eq!(headline(&gates), None);
    }

    #[test]
    fn gitlab_detailed_merge_status_names_the_gate() {
        let mut review = pr();
        review.merge_state_status = Some("NOT_APPROVED".to_string());
        let gates = from_review(&review, Some(&[]), None);
        assert_eq!(gates[0].kind, "review");
        assert_eq!(gates[0].actor, GateActor::Maintainers);
    }

    #[test]
    fn long_name_lists_are_shortened() {
        assert_eq!(names(&["a", "b", "c"]), "a, b, c");
        assert_eq!(names(&["a", "b", "c", "d", "e"]), "a, b, c and 2 more");
    }

    #[test]
    fn unreadable_checks_stay_open() {
        let gates = from_review(&pr(), None, None);
        assert_eq!(gates[0].state, GateState::Unknown);
        assert!(headline(&gates).is_some());
    }

    #[test]
    fn newer_gate_values_still_load() {
        let gate: Gate = serde_json::from_value(serde_json::json!({
            "kind": "merge_queue",
            "state": "queued",
            "actor": "bot",
            "summary": "in the merge queue"
        }))
        .unwrap();
        assert_eq!(gate.state, GateState::Unknown);
        assert_eq!(gate.actor, GateActor::Host);
    }
}
