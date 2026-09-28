//! The base ledger is observed Git history, independent of bundle lifecycle.
use super::*;

pub(super) fn events(root: &Path, project_id: &str) -> Result<Vec<HistoryEvent>> {
    let path = project_path(root, project_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let project: crate::model::KnitProject = crate::store::read_json(&path)?;
    let mut events = Vec::new();
    for repo in project.repos {
        let checkout = PathBuf::from(&repo.path);
        let checkout = if checkout.is_absolute() {
            checkout
        } else {
            root.join(checkout)
        };
        // Never substitute a feature checkout's HEAD or an unpushed local base
        // for the configured remote base. Refresh is deliberately offline.
        let reference = if repo.remote.is_some() {
            format!("refs/remotes/origin/{}", repo.base_branch)
        } else {
            format!("refs/heads/{}", repo.base_branch)
        };
        if !checkout.exists() {
            continue;
        }
        let Ok(head) = crate::git::git_output(&checkout, ["rev-parse", "--verify", &reference])
        else {
            continue;
        };
        let head = head.trim();
        let log = crate::git::git_output(
            &checkout,
            [
                "log",
                "--first-parent",
                "--no-show-signature",
                "--format=%H%x00%P%x00%cI%x00%s",
                head,
                "--",
            ],
        )?;
        for line in log.lines() {
            let columns = line.splitn(4, '\0').collect::<Vec<_>>();
            if columns.len() != 4 {
                continue;
            }
            let [sha, parents, occurred_at, message] = <[&str; 4]>::try_from(columns).unwrap();
            events.push(HistoryEvent {
                schema_version: HISTORY_EVENT_SCHEMA_VERSION.into(),
                event_id: history_event_id(&[
                    project_id,
                    &repo.id,
                    repo.remote.as_deref().unwrap_or(""),
                    &repo.base_branch,
                    "base.commit",
                    sha,
                ]),
                project_id: project_id.into(),
                kind: "base.commit".into(),
                bundle_id: None,
                bundle_title: None,
                repo_id: Some(repo.id.clone()),
                repo_remote: repo.remote.clone(),
                base_branch: Some(repo.base_branch.clone()),
                branch: Some(repo.base_branch.clone()),
                commit: Some(sha.into()),
                before_sha: parents.split_whitespace().next().map(Into::into),
                after_sha: Some(sha.into()),
                movement: None,
                node_id: Some(format!("base:{}:{}:{sha}", repo.id, repo.base_branch)),
                node_type: Some("base.commit".into()),
                commit_group_id: None,
                message: Some(message.into()),
                occurred_at: Some(occurred_at.into()),
                recorded_at: now_iso(),
                recorded_by: "knit".into(),
                metadata: Some(serde_json::json!({"observedRef": reference, "observedHead": head})),
            });
        }
    }
    Ok(events)
}
