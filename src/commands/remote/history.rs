//! Remote sync for project history events.

use super::client::{
    effective_workspace_config, load_project_if_present, request_json, resolve_project_id,
    resolve_remote, resolve_token, with_first_available_remote,
};
use crate::history::{
    append_history_events, load_history_events, record_bundle_history, refresh_project_history,
};
use crate::model::{HistoryEvent, KnitRemote};
use crate::output as out;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteHistoryPush {
    inserted_count: usize,
    skipped_count: usize,
    // Older sync remotes report only inserted/skipped.
    #[serde(default)]
    updated_count: usize,
    #[serde(default)]
    failed_count: usize,
}

pub fn push_history_to_remote(project: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    // Only an explicit CLI selector scopes an upload. A cwd/workspace bundle
    // must not silently turn an ordinary project history push into a subset.
    let selected_bundle = crate::store::explicit_bundle_override()
        .map(|_| crate::store::load_active_bundle().map(|active| active.bundle))
        .transpose()?;
    let project = selected_bundle
        .as_ref()
        .and_then(|bundle| bundle.project_id.as_deref())
        .or(project);
    let project_id = resolve_project_id(&root, &config, project)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let local_project = load_project_if_present(&root, &project_id)?;
    let (remote_project, shape_push) = super::push::upsert_project_for_history(
        &root,
        remote,
        &token,
        &project_id,
        local_project.as_ref(),
    )?;
    // Repository records are the shared project shape: only a caller whose
    // project upsert succeeded may push them. A collaborator (PATCH refused,
    // project fetched read-only) still syncs history — the writable plane —
    // without touching the membership, mirroring the bundle push paths.
    if let (Some(project), super::push::ProjectShapePush::Pushed) =
        (local_project.as_ref(), shape_push)
    {
        super::push::push_repositories_for_history(
            remote,
            &token,
            &remote_project.slug,
            &project.repos,
        )?;
    }
    let pushed = push_history_events(
        remote,
        &token,
        &remote_project.slug,
        &root,
        &project_id,
        remote_name,
        selected_bundle.as_ref(),
    )?;
    println!(
        "{} {} {}",
        out::movement("pushed history"),
        out::repo(&project_id),
        out::muted(format!("{pushed} event(s)"))
    );
    Ok(())
}

pub fn pull_history_from_remote(project: Option<&str>, remote_name: Option<&str>) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, project)?;
    let events = with_first_available_remote(&config, remote_name, |_, remote, token| {
        fetch_project_history_events(remote, token, &project_id)
    })?;
    let appended = append_history_events(&root, &project_id, &events)?;
    println!(
        "{} {} {}",
        out::movement("pulled history"),
        out::repo(&project_id),
        out::muted(format!("{appended} new event(s)"))
    );
    Ok(())
}

pub(super) fn push_project_history_events(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    root: &Path,
    project_id: &str,
    remote_name: &str,
) -> Result<usize> {
    push_history_events(
        remote,
        token,
        project_slug,
        root,
        project_id,
        remote_name,
        None,
    )
}

fn push_history_events(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    root: &Path,
    project_id: &str,
    remote_name: &str,
    bundle: Option<&crate::model::ChangeGroup>,
) -> Result<usize> {
    if let Some(bundle) = bundle {
        record_bundle_history(root, bundle)?;
    } else {
        refresh_project_history(root, project_id)?;
    }
    let events = load_history_events(root, project_id)?
        .into_iter()
        .filter(|event| bundle.is_none_or(|bundle| event.bundle_id.as_deref() == Some(&bundle.id)))
        .collect::<Vec<_>>();
    let encoded = events
        .iter()
        .map(|event| serde_json::to_string(event).context("failed to encode history event"))
        .collect::<Result<Vec<_>>>()?;
    // A bundle slice must never read or update the project-wide cursor.
    let mut state = if bundle.is_some() {
        HistorySyncState::new()
    } else {
        load_history_sync_state(root, project_id)?
    };
    let plan = plan_history_push(&encoded, state.get(remote_name));
    let to_send: &[HistoryEvent] = match plan {
        HistoryPushPlan::UpToDate => {
            print_history_push_size(remote_name, 0, 0);
            return Ok(events.len());
        }
        HistoryPushPlan::Tail(from) => &events[from..],
        HistoryPushPlan::Full => &events,
    };

    let batches: Vec<&[HistoryEvent]> = to_send.chunks(HISTORY_PAGE_SIZE).collect();
    print_history_push_size(remote_name, to_send.len(), batches.len());
    // Receive in completion order: a slow or failed later request must not
    // delay saving an already accepted prefix (including before Ctrl-C).
    let from = match plan {
        HistoryPushPlan::Tail(from) => from,
        _ => 0,
    };
    let mut progress = HistoryPushProgress::new(batches.len());
    let mut accepted = from;
    let mut failed = 0;
    let mut completed = 0;
    let mut attempted = 0;
    let mut first_error = None;
    let path = format!("/projects/{project_slug}/history-events");
    std::thread::scope(|scope| -> Result<()> {
        for (group_index, group) in batches.chunks(HISTORY_PUSH_CONCURRENCY).enumerate() {
            attempted += group.iter().map(|batch| batch.len()).sum::<usize>();
            let (sender, receiver) = std::sync::mpsc::channel();
            for (offset, batch) in group.iter().enumerate() {
                let sender = sender.clone();
                let path = path.as_str();
                scope.spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let payload = json!({ "events": batch });
                        request_json::<RemoteHistoryPush>(
                            remote,
                            token,
                            "POST",
                            path,
                            Some(&payload),
                        )
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("history push thread panicked")));
                    let _ = sender.send((group_index * HISTORY_PUSH_CONCURRENCY + offset, outcome));
                });
            }
            drop(sender);
            // Drain this group even after an error: an earlier batch may still
            // complete and extend the durable prefix. Never skip over a hole.
            for (index, outcome) in receiver {
                completed += 1;
                let successful = match outcome {
                    Ok(response) => {
                        accepted += response.inserted_count
                            + response.updated_count
                            + response.skipped_count;
                        failed += response.failed_count;
                        response.failed_count == 0
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                        false
                    }
                };
                if let Some(prefix) = progress.complete(index, successful).filter(|_| bundle.is_none()) {
                    let end = (from + prefix * HISTORY_PAGE_SIZE).min(events.len());
                    record_history_sync(
                        root,
                        project_id,
                        remote_name,
                        &encoded[..end],
                        &mut state,
                    )?;
                }
                if batches.len() > 1 {
                    crate::human!("{}", out::muted(format!(
                        "syncing history to {remote_name}: {completed}/{} request(s) completed, {accepted} event(s) accepted",
                        batches.len()
                    )));
                }
            }
            if first_error.is_some() || failed > 0 {
                break;
            }
        }
        Ok(())
    })?;
    if first_error.is_some() || failed > 0 {
        let unattempted = to_send.len() - attempted;
        let message = format!(
            "incomplete history sync: {failed} event(s) rejected, {unattempted} event(s) unattempted; completed {completed}/{} request(s), {accepted} event(s) accepted; the next push resumes from the saved prefix",
            batches.len()
        );
        return match first_error {
            Some(error) => Err(error).context(message),
            None => Err(anyhow::anyhow!(message)),
        };
    }
    Ok(accepted)
}

fn print_history_push_size(remote_name: &str, events: usize, requests: usize) {
    crate::human!(
        "{}",
        out::muted(format!(
            "syncing history to {remote_name}: {events} event(s) in {requests} request(s)…"
        ))
    );
}

/// Successful requests may arrive in any order. Only a gap-free prefix is
/// safe to omit on retry; success beyond a failed request is replayed.
struct HistoryPushProgress {
    successful: Vec<bool>,
    prefix: usize,
}

impl HistoryPushProgress {
    fn new(batches: usize) -> Self {
        Self {
            successful: vec![false; batches],
            prefix: 0,
        }
    }

    fn complete(&mut self, index: usize, successful: bool) -> Option<usize> {
        self.successful[index] = successful;
        let previous = self.prefix;
        while self.successful.get(self.prefix) == Some(&true) {
            self.prefix += 1;
        }
        (self.prefix > previous).then_some(self.prefix)
    }
}

/// How many history requests are in flight at once during a push.
const HISTORY_PUSH_CONCURRENCY: usize = 4;

/// What one push has to send, given what the remote already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HistoryPushPlan {
    /// The ledger is byte-for-byte what was last pushed: nothing to send.
    UpToDate,
    /// The ledger only grew since the last push: send from this index on.
    Tail(usize),
    /// An earlier event changed (a rebuild enriched it) or nothing was ever
    /// pushed: send everything.
    Full,
}

/// Per-remote memory of what a push last sent: how many events, and a
/// fingerprint of exactly those encoded lines. A later push compares the
/// ledger's prefix against it; an append-only ledger (the normal case) then
/// costs one request for the new events, and an unchanged one costs none.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct HistorySyncCursor {
    pub event_count: usize,
    pub fingerprint: String,
}

pub(super) type HistorySyncState = std::collections::BTreeMap<String, HistorySyncCursor>;

/// FNV-1a over the encoded lines, in order. A content fingerprint, not a
/// security boundary; it is deliberately self-contained so it never changes
/// underneath a stored cursor.
pub(super) fn history_fingerprint(encoded_lines: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in encoded_lines {
        for byte in line.bytes().chain(std::iter::once(b'\n')) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("fnv1a64:{hash:016x}")
}

pub(super) fn plan_history_push(
    encoded_lines: &[String],
    cursor: Option<&HistorySyncCursor>,
) -> HistoryPushPlan {
    let Some(cursor) = cursor else {
        return HistoryPushPlan::Full;
    };
    if cursor.event_count == 0 || cursor.event_count > encoded_lines.len() {
        return HistoryPushPlan::Full;
    }
    if history_fingerprint(&encoded_lines[..cursor.event_count]) != cursor.fingerprint {
        return HistoryPushPlan::Full;
    }
    if cursor.event_count == encoded_lines.len() {
        HistoryPushPlan::UpToDate
    } else {
        HistoryPushPlan::Tail(cursor.event_count)
    }
}

fn history_sync_state_path(root: &Path, project_id: &str) -> std::path::PathBuf {
    crate::store::history_path(root, project_id)
        .with_file_name(format!("{project_id}.history-sync.json"))
}

pub(super) fn load_history_sync_state(root: &Path, project_id: &str) -> Result<HistorySyncState> {
    let path = history_sync_state_path(root, project_id);
    if !path.exists() {
        return Ok(HistorySyncState::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    // A cursor that cannot be read is a cursor that never existed: the next
    // push is a full one, which is always correct.
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

fn record_history_sync(
    root: &Path,
    project_id: &str,
    remote_name: &str,
    encoded_lines: &[String],
    state: &mut HistorySyncState,
) -> Result<()> {
    state.insert(
        remote_name.to_string(),
        HistorySyncCursor {
            event_count: encoded_lines.len(),
            fingerprint: history_fingerprint(encoded_lines),
        },
    );
    let path = history_sync_state_path(root, project_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let body =
        serde_json::to_string_pretty(&state).context("failed to encode history sync state")?;
    // A sibling temp + rename leaves the previous cursor intact if writing
    // fails or the process is interrupted. Never remove the old destination.
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(format!("{body}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        // On Unix, persist the renamed directory entry as well as the file.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("failed to sync directory {}", parent.display()))?;
        }
        // Non-Unix std APIs do not provide a portable directory-sync operation.
        // There we sync the file and rename only, without claiming that the
        // directory entry is durable across a crash.
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to write {}", path.display()))
}

/// How many history events ride in one request, both directions.
const HISTORY_PAGE_SIZE: usize = 500;

/// Fetch a project's history events page by page with the server's keyset
/// cursor (`before` + `beforeId`, taken from the last event of each page), so
/// a ledger of any size never arrives as one response. A server that ignores
/// the cursor resends the newest page; the no-new-events guard turns that
/// into "stop with what one page held" instead of a loop.
pub(super) fn fetch_project_history_events(
    remote: &KnitRemote,
    token: &str,
    project_identifier: &str,
) -> Result<Vec<HistoryEvent>> {
    let base_path = format!("/projects/{project_identifier}/history-events");
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cursor: Option<(String, String)> = None;

    loop {
        let path = match &cursor {
            Some((before, before_id)) => format!(
                "{base_path}?limit={HISTORY_PAGE_SIZE}&before={before}&beforeId={before_id}"
            ),
            None => format!("{base_path}?limit={HISTORY_PAGE_SIZE}"),
        };
        let page: Vec<serde_json::Value> = request_json(remote, token, "GET", &path, None)
            .with_context(|| {
                format!("failed to fetch history for project `{project_identifier}`")
            })?;
        let page_len = page.len();

        let mut new_events = 0;
        for event in page {
            let Some(event_id) = event.get("eventId").and_then(|value| value.as_str()) else {
                continue;
            };
            if seen.insert(event_id.to_string()) {
                all.push(event);
                new_events += 1;
            }
        }
        if page_len < HISTORY_PAGE_SIZE || new_events == 0 {
            break;
        }

        let Some(last) = all.last() else { break };
        let next = last
            .get("occurredAt")
            .and_then(|value| value.as_str())
            .zip(last.get("eventId").and_then(|value| value.as_str()))
            .map(|(before, before_id)| (before.to_string(), before_id.to_string()));
        // Without a usable cursor another request could only repeat this page.
        let Some(next) = next else { break };
        cursor = Some(next);
    }

    Ok(super::decode_history_events(&all, project_identifier))
}

#[cfg(test)]
mod push_plan_tests {
    use super::*;

    fn lines(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("{{\"eventId\":\"e{index}\"}}"))
            .collect()
    }

    #[test]
    fn second_of_four_fails_out_of_order_and_retry_starts_after_first() {
        let root = std::env::temp_dir().join(format!(
            "knit-history-prefix-{}",
            crate::ids::node_id("test")
        ));
        let encoded = lines(2000);
        let mut state = HistorySyncState::new();
        // Another remote's progress must survive every replacement.
        record_history_sync(&root, "demo", "mirror", &encoded, &mut state).unwrap();
        let mut progress = HistoryPushProgress::new(4);
        for (index, successful, expected) in [
            (2, true, None),
            (1, false, None),
            (0, true, Some(1)),
            (3, true, None),
        ] {
            let prefix = progress.complete(index, successful);
            assert_eq!(prefix, expected);
            if let Some(prefix) = prefix {
                record_history_sync(
                    &root,
                    "demo",
                    "hosted",
                    &encoded[..prefix * 500],
                    &mut state,
                )
                .unwrap();
            }
        }
        let saved = load_history_sync_state(&root, "demo").unwrap();
        assert_eq!(saved["hosted"].event_count, 500);
        assert_eq!(
            saved["hosted"].fingerprint,
            history_fingerprint(&encoded[..500])
        );
        assert_eq!(saved["mirror"].event_count, 2000);
        assert_eq!(
            plan_history_push(&encoded, saved.get("hosted")),
            HistoryPushPlan::Tail(500)
        );
        assert_eq!(
            std::fs::read_dir(history_sync_state_path(&root, "demo").parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn out_of_order_success_advances_only_when_the_gap_closes() {
        let mut progress = HistoryPushProgress::new(4);
        assert_eq!(progress.complete(3, true), None);
        assert_eq!(progress.complete(1, true), None);
        assert_eq!(progress.complete(0, true), Some(2));
        assert_eq!(progress.complete(2, true), Some(4));
    }

    #[test]
    fn nothing_recorded_means_a_full_push() {
        assert_eq!(plan_history_push(&lines(3), None), HistoryPushPlan::Full);
    }

    #[test]
    fn an_unchanged_ledger_sends_nothing_and_an_appended_one_sends_the_tail() {
        let pushed = lines(3);
        let cursor = HistorySyncCursor {
            event_count: 3,
            fingerprint: history_fingerprint(&pushed),
        };
        assert_eq!(
            plan_history_push(&pushed, Some(&cursor)),
            HistoryPushPlan::UpToDate
        );
        assert_eq!(
            plan_history_push(&lines(5), Some(&cursor)),
            HistoryPushPlan::Tail(3)
        );
    }

    #[test]
    fn a_rewritten_prefix_or_a_shrunken_ledger_sends_everything() {
        let pushed = lines(3);
        let cursor = HistorySyncCursor {
            event_count: 3,
            fingerprint: history_fingerprint(&pushed),
        };
        let mut rewritten = lines(4);
        rewritten[1] = "{\"eventId\":\"e1\",\"message\":\"enriched\"}".to_string();
        assert_eq!(
            plan_history_push(&rewritten, Some(&cursor)),
            HistoryPushPlan::Full
        );
        assert_eq!(
            plan_history_push(&lines(2), Some(&cursor)),
            HistoryPushPlan::Full
        );
    }
}
