//! Disposable local history index. The JSONL ledger remains authoritative.
use crate::model::{ChangeGroup, HistoryEvent};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
pub mod expression;
mod grep;
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default)]
pub enum HistoryGrouping {
    Event,
    #[default]
    Commit,
    Bundle,
}
#[derive(Clone, Copy, Debug, Default)]
pub enum RepoMatch {
    #[default]
    Any,
    All,
}
#[derive(Clone, Debug, Default)]
pub struct HistoryQuery {
    pub scope: Option<String>,
    pub open_bundles: Vec<String>,
    pub expression: Option<expression::Expression>,
    pub bundle_id: Option<String>,
    pub repos: Option<Vec<String>>,
    pub repo_match: RepoMatch,
    pub kinds: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub grep: Vec<String>,
    pub all_match: bool,
    pub ignore_case: bool,
    pub fixed_strings: bool,
    pub extended_regexp: bool,
    pub grouping: HistoryGrouping,
    pub full_context: bool,
    pub reverse: bool,
    pub limit: Option<usize>,
    pub skip: usize,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub bundle_id: Option<String>,
    pub bundle_title: Option<String>,
    pub occurred_at: String,
    pub message: String,
    pub events: Vec<HistoryEvent>,
}
const VERSION: i64 = 1;

/// Hash the project identifier so even unusual identifiers cannot escape the cache.
pub fn index_path(root: &Path, project_id: &str) -> PathBuf {
    root.join(".knit/cache/history").join(format!(
        "{:x}.sqlite",
        Sha256::digest(project_id.as_bytes())
    ))
}
fn validate_project(project: &str) -> Result<()> {
    if project.is_empty() || project.contains(['/', '\\']) || project == "." || project == ".." {
        bail!("invalid history project identifier");
    }
    Ok(())
}
fn schema(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE events (
        seq INTEGER PRIMARY KEY, event_id TEXT NOT NULL, bundle TEXT, title TEXT,
        repo TEXT, kind TEXT NOT NULL, at TEXT NOT NULL, message TEXT NOT NULL,
        commit_key TEXT NOT NULL, bundle_key TEXT NOT NULL, selector TEXT NOT NULL,
        payload TEXT NOT NULL);
        CREATE INDEX events_commit ON events(commit_key);
        CREATE INDEX events_bundle ON events(bundle_key);
        CREATE INDEX events_time ON events(at DESC,event_id);
        CREATE INDEX events_repo ON events(repo);
        CREATE TABLE source (singleton INTEGER PRIMARY KEY CHECK(singleton=1), size INTEGER NOT NULL, digest BLOB NOT NULL);
        PRAGMA user_version=1;")?;
    Ok(())
}
fn open_index(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path.parent().context("index has no parent")?)?;
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(10))?;
    let health = (|| -> rusqlite::Result<bool> {
        let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if check != "ok" || version != VERSION {
            return Ok(false);
        }
        db.prepare("SELECT seq,event_id,bundle,title,repo,kind,at,message,commit_key,bundle_key,selector,payload FROM events")?;
        db.prepare("SELECT size,digest FROM source")?;
        Ok(true)
    })();
    match health {
        Ok(true) => return Ok(db),
        Ok(false) => {}
        Err(rusqlite::Error::SqliteFailure(ref e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::Unknown
            ) => {}
        Err(e) => return Err(e).context("checking history cache"),
    }
    drop(db);
    // All callers hold the same history named lock, including across rebuilds.
    fs::remove_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        match fs::remove_file(sidecar) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(10))?;
    schema(&db)?;
    Ok(db)
}
fn normalized(value: &str) -> Result<String> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid history timestamp {value:?}"))?
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true))
}
fn insert(db: &Connection, event: &HistoryEvent, payload: &str) -> Result<()> {
    let at = normalized(event.occurred_at.as_deref().unwrap_or(&event.recorded_at))?;
    let selector = event
        .node_id
        .as_ref()
        .or(event.commit_group_id.as_ref())
        .unwrap_or(&event.event_id);
    // JSON tuple keys avoid delimiter collisions and keep anonymous events independent.
    let commit_key = serde_json::to_string(&(&event.bundle_id, selector))?;
    let bundle_key = serde_json::to_string(&(
        &event.bundle_id,
        event.bundle_id.is_none().then_some(&event.event_id),
    ))?;
    db.execute("INSERT INTO events(event_id,bundle,title,repo,kind,at,message,commit_key,bundle_key,selector,payload) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        params![event.event_id,event.bundle_id,event.bundle_title,event.repo_id,event.kind,at,event.message.as_deref().unwrap_or(&event.kind),commit_key,bundle_key,selector,payload])?;
    Ok(())
}
fn reconcile(db: &mut Connection, bytes: &[u8]) -> Result<()> {
    let previous: Option<(usize, Vec<u8>)> = db
        .query_row(
            "SELECT size,digest FROM source WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let offset = previous
        .as_ref()
        .filter(|(n, hash)| {
            *n <= bytes.len()
                && Sha256::digest(&bytes[..*n]).as_slice() == hash.as_slice()
                && (*n == 0 || bytes[*n - 1] == b'\n')
        })
        .map(|(n, _)| *n);
    if offset == Some(bytes.len()) {
        return Ok(());
    }
    let tx = db.transaction()?;
    if offset.is_none() {
        tx.execute("DELETE FROM events", [])?;
    }
    let start = offset.unwrap_or(0);
    for (line, raw) in bytes[start..].split(|b| *b == b'\n').enumerate() {
        let raw = std::str::from_utf8(raw).context("history ledger is not UTF-8")?;
        if raw.trim().is_empty() {
            continue;
        }
        let event: HistoryEvent = serde_json::from_str(raw).with_context(|| {
            format!(
                "invalid history event after byte {start}, line {}",
                line + 1
            )
        })?;
        insert(&tx, &event, raw)
            .with_context(|| format!("indexing history event {}", event.event_id))?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO source VALUES(1,?,?)",
        params![bytes.len() as i64, Sha256::digest(bytes).to_vec()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Read local JSONL only; a missing ledger is empty. Never refreshes or syncs it.
/// Callers may suggest an explicit history refresh when no local history exists.
/// Each call verifies the source prefix (O(ledger bytes)); only changed/tail
/// events are parsed and inserted, and only the selected page is deserialized.
/// Queries retry named-lock contention for up to five seconds, then return an
/// error. Other IO failures are returned immediately; stale data is never served.
pub fn query_project_history(
    root: &Path,
    project_id: &str,
    query: &HistoryQuery,
) -> Result<Vec<HistoryEntry>> {
    let mut query = query.clone();
    if matches!(
        query.scope.as_deref(),
        Some("base-and-proposals" | "proposals")
    ) {
        let dir = root.join(".knit/bundles");
        if dir.exists() {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let bundle: ChangeGroup = crate::store::read_json(&path)?;
                if bundle.project_id.as_deref() != Some(project_id) {
                    continue;
                }
                let open = match bundle.state {
                    Some(crate::model::BundleState::Open) => true,
                    Some(_) => false,
                    None => {
                        bundle.archived_at.is_none()
                            && !bundle
                                .nodes
                                .iter()
                                .any(crate::model::is_terminal_landed_node)
                    }
                };
                if open {
                    query.open_bundles.push(bundle.id);
                }
            }
        }
    }
    with_project_index(root, project_id, |db| execute(db, &query, false))
}

fn query_lock(root: &Path, project_id: &str, timeout: Duration) -> Result<crate::store::KnitLock> {
    let started = std::time::Instant::now();
    loop {
        match crate::store::acquire_named_lock(root, &format!("history-{project_id}")) {
            Ok(lock) => return Ok(lock),
            Err(error) => {
                // Store contention currently has no typed error. Do not retry IO
                // failures, and never remove a lock ourselves.
                let contention = error.downcast_ref::<std::io::Error>().is_none()
                    && error.to_string().starts_with("Another Knit process");
                if !contention || started.elapsed() >= timeout {
                    return Err(error);
                }
                std::thread::sleep(
                    Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                );
            }
        }
    }
}

fn with_project_index<T>(
    root: &Path,
    project_id: &str,
    read: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    validate_project(project_id)?;
    let _lock = query_lock(root, project_id, Duration::from_secs(5))?;
    let path = crate::store::history_path(root, project_id);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut db = open_index(&index_path(root, project_id))?;
    reconcile(&mut db, &bytes).with_context(|| format!("reconciling {}", path.display()))?;
    read(&db)
}
/// Combine preserved project history with missing events from the current
/// artifact. Recorded payloads win by event ID, including enriched detail and
/// orphan events. The union exists only in memory; neither input is rewritten.
/// Commit groups use node order/dates; event groups keep event timestamps.
pub fn query_bundle_history(
    root: &Path,
    bundle: &ChangeGroup,
    query: &HistoryQuery,
) -> Result<Vec<HistoryEntry>> {
    let mut q = query.clone();
    if q.bundle_id.as_ref().is_some_and(|id| id != &bundle.id) {
        return Ok(Vec::new());
    }
    q.bundle_id = Some(bundle.id.clone());
    if matches!(bundle.state, Some(crate::model::BundleState::Open))
        || (bundle.state.is_none()
            && bundle.archived_at.is_none()
            && !bundle
                .nodes
                .iter()
                .any(crate::model::is_terminal_landed_node))
    {
        q.open_bundles.push(bundle.id.clone());
    }
    let mut db = Connection::open_in_memory()?;
    schema(&db)?;
    let tx = db.transaction()?;
    let mut recorded_ids = std::collections::BTreeSet::new();
    if let Some(project) = &bundle.project_id {
        // Read only this bundle's rows, under the same reconciliation/snapshot
        // lock. Preserve their original order and raw payload, including fields
        // unknown to this version. Never apply user filters before the union.
        with_project_index(root, project, |source| {
            let mut stmt =
                source.prepare("SELECT payload FROM events WHERE bundle=? ORDER BY seq")?;
            let mut rows = stmt.query([&bundle.id])?;
            while let Some(row) = rows.next()? {
                let payload: String = row.get(0)?;
                let event: HistoryEvent = serde_json::from_str(&payload)?;
                recorded_ids.insert(event.event_id.clone());
                insert(&tx, &event, &payload)?;
            }
            Ok(())
        })?;
    }
    for event in crate::history::bundle_history_snapshot(bundle) {
        if recorded_ids.insert(event.event_id.clone()) {
            insert(&tx, &event, &serde_json::to_string(&event)?)?;
        }
    }
    let node_chronology = matches!(q.grouping, HistoryGrouping::Commit);
    if node_chronology {
        // This is an ephemeral bundle-only projection. Payload timestamps stay
        // untouched; group dates describe when the node was recorded. Ordinals
        // follow the artifact sequence used by HEAD/HEAD~ (including equal or
        // clock-skewed node timestamps). Recorded-only orphans sort afterwards.
        tx.execute_batch(
            "ALTER TABLE events ADD COLUMN node_order INTEGER;
            CREATE INDEX events_selector ON events(selector);",
        )?;
        for (position, node) in crate::history::bundle_history_nodes(bundle)
            .iter()
            .enumerate()
        {
            let at = normalized(&node.created_at)?;
            tx.execute(
                "UPDATE events SET at=?,node_order=? WHERE selector=?",
                params![at, position as i64, node.id],
            )?;
            // Older recorded events can carry only the commit-group selector.
            if let Some(group_id) = node
                .commit_group_id
                .as_ref()
                .filter(|_| matches!(node.node_type.as_str(), "commit.group" | "revert.group"))
            {
                tx.execute(
                    "UPDATE events SET at=?,node_order=? WHERE selector=?",
                    params![at, position as i64, group_id],
                )?;
            }
        }
    }
    tx.commit()?;
    execute(&db, &q, node_chronology)
}

fn query_sql(
    db: &Connection,
    q: &HistoryQuery,
    node_chronology: bool,
) -> Result<(String, Vec<Value>)> {
    let flags = rusqlite::functions::FunctionFlags::SQLITE_UTF8
        | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC;
    db.create_scalar_function("history_contains", 2, flags, |ctx| {
        let payload: String = ctx.get(0)?;
        let needle: String = ctx.get(1)?;
        let event: HistoryEvent = serde_json::from_str(&payload)
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        Ok([
            event.bundle_id,
            event.bundle_title,
            event.repo_id,
            event.branch,
            event.commit,
            event.message,
        ]
        .iter()
        .flatten()
        .any(|s| s.to_lowercase().contains(&needle)))
    })?;
    db.create_scalar_function("history_activity", 1, flags, |ctx| {
        let payload: String = ctx.get(0)?;
        let event: HistoryEvent = serde_json::from_str(&payload)
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        normalized(event.occurred_at.as_deref().unwrap_or(&event.recorded_at))
            .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))
    })?;
    let mut values: Vec<Value> = Vec::new();
    let mut bind = |v: Value| {
        values.push(v);
        format!("?{}", values.len())
    };
    let key = match q.grouping {
        HistoryGrouping::Event => "seq",
        HistoryGrouping::Commit => "commit_key",
        HistoryGrouping::Bundle => "bundle_key",
    };
    let base = "(kind = 'base.commit' OR (kind = 'branch.landed' AND json_extract(payload, '$.branch') != '' AND json_extract(payload, '$.branch') = json_extract(payload, '$.baseBranch')))";
    let proposal_ids = q
        .open_bundles
        .iter()
        .map(|id| bind(id.clone().into()))
        .collect::<Vec<_>>()
        .join(",");
    let proposals = if proposal_ids.is_empty() {
        "0".to_string()
    } else {
        format!("bundle IN ({proposal_ids}) AND kind NOT IN ('base.commit','branch.landed','bundle.landed')")
    };
    let scope = match q.scope.as_deref() {
        Some("base") => base.to_string(),
        Some("base-and-proposals") => format!("({base} OR ({proposals}))"),
        Some("proposals") => proposals,
        Some("landings") => "(kind = 'branch.landed' OR (kind = 'bundle.landed' AND COALESCE(json_extract(payload, '$.metadata.hasBranchReceipts'), 0) != 1))".to_string(),
        _ => "1".to_string(),
    };
    let mut conditions = Vec::new();
    if let Some(bundle) = &q.bundle_id {
        conditions.push(format!("e.bundle={}", bind(bundle.clone().into())));
    }
    if !q.kinds.is_empty() {
        let p = q
            .kinds
            .iter()
            .map(|k| bind(k.clone().into()))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!("e.kind IN ({p})"));
    }
    if let Some(since) = &q.since {
        conditions.push(format!(
            "e.at>={}",
            bind(since.to_rfc3339_opts(SecondsFormat::Nanos, true).into())
        ));
    }
    if let Some(until) = &q.until {
        conditions.push(format!(
            "e.at<={}",
            bind(until.to_rfc3339_opts(SecondsFormat::Nanos, true).into())
        ));
    }
    let mut filters = Vec::new();
    if !conditions.is_empty() {
        filters.push(format!(
            "{key} IN (SELECT {key} FROM events e WHERE {})",
            conditions.join(" AND ")
        ));
    }
    if let Some(repos) = &q.repos {
        if repos.is_empty() {
            filters.push("0".into());
        } else {
            let unique: std::collections::BTreeSet<_> = repos.iter().collect();
            let count = unique.len();
            let placeholders = unique
                .into_iter()
                .map(|r| bind(r.clone().into()))
                .collect::<Vec<_>>()
                .join(",");
            let having = match q.repo_match {
                RepoMatch::Any => String::new(),
                RepoMatch::All => format!(" HAVING COUNT(DISTINCT repo)={count}"),
            };
            filters.push(format!("{key} IN (SELECT {key} FROM events WHERE repo IN ({placeholders}) GROUP BY {key}{having})"));
        }
    }
    let mut grep_clauses = Vec::new();
    for (i, pattern) in q.grep.iter().enumerate() {
        let regex = grep::compile(pattern, q)?;
        let name = format!("history_grep_{i}");
        db.create_scalar_function(
            name.as_str(),
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                Ok(ctx
                    .get::<String>(0)?
                    .split_terminator('\n')
                    .any(|line| regex.is_match(line)))
            },
        )?;
        grep_clauses.push(format!(
            "{key} IN (SELECT {key} FROM events WHERE {name}(message))"
        ));
    }
    if !grep_clauses.is_empty() {
        filters.push(format!(
            "({})",
            grep_clauses.join(if q.all_match { " AND " } else { " OR " })
        ));
    }
    if let Some(expression) = &q.expression {
        filters.push(format!("({})", expression.sql(key, &mut bind)?));
    }
    let limit = bind(
        q.limit
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
            .unwrap_or(-1)
            .into(),
    );
    let skip = bind(i64::try_from(q.skip).unwrap_or(i64::MAX).into());
    let ordinal = if node_chronology {
        "MAX(node_order)"
    } else {
        "NULL"
    };
    let sql = format!(
        "WITH selected AS MATERIALIZED (
        SELECT {key} AS g,{ordinal} AS ordinal,MAX(at) AS newest,MIN(event_id) AS tie FROM events
        WHERE {} GROUP BY {key} ORDER BY ordinal DESC,newest DESC,tie ASC,g ASC LIMIT {limit} OFFSET {skip})
        SELECT e.payload,e.at FROM selected s JOIN events e ON e.{key}=s.g
        ORDER BY s.ordinal DESC,s.newest DESC,s.tie ASC,s.g ASC,e.at DESC,e.event_id ASC,e.seq ASC",
        if filters.is_empty() {
            "1".into()
        } else {
            filters.join(" AND ")
        }
    );
    let sql = if matches!(q.scope.as_deref(), None | Some("activity")) {
        sql
    } else {
        let sql = sql
            .replace("FROM events", "FROM scoped_events")
            .replace("JOIN events", "JOIN scoped_events");
        sql.replacen("WITH selected", &format!("WITH scoped_events AS NOT MATERIALIZED (SELECT * FROM events WHERE {scope}), selected"), 1)
    };
    Ok((sql, values))
}

fn execute(db: &Connection, q: &HistoryQuery, node_chronology: bool) -> Result<Vec<HistoryEntry>> {
    let (sql, values) = query_sql(db, q, node_chronology)?;
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(values))?;
    let mut result: Vec<HistoryEntry> = Vec::new();
    let mut last_key = None;
    while let Some(row) = rows.next()? {
        let event: HistoryEvent = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let id = match q.grouping {
            HistoryGrouping::Event => &event.event_id,
            HistoryGrouping::Commit => event
                .node_id
                .as_ref()
                .or(event.commit_group_id.as_ref())
                .unwrap_or(&event.event_id),
            HistoryGrouping::Bundle => event.bundle_id.as_ref().unwrap_or(&event.event_id),
        }
        .clone();
        let group_key = (event.bundle_id.clone(), id.clone());
        if last_key.as_ref() != Some(&group_key) || matches!(q.grouping, HistoryGrouping::Event) {
            result.push(HistoryEntry {
                id,
                bundle_id: event.bundle_id.clone(),
                bundle_title: event.bundle_title.clone(),
                occurred_at: row.get(1)?,
                message: event.message.clone().unwrap_or_else(|| event.kind.clone()),
                events: Vec::new(),
            });
            last_key = Some(group_key);
        }
        if q.full_context
            || q.repos
                .as_ref()
                .is_none_or(|repos| event.repo_id.as_ref().is_some_and(|r| repos.contains(r)))
        {
            result.last_mut().unwrap().events.push(event);
        }
    }
    if q.reverse {
        result.reverse();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "knit-history-query-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join(".knit/history")).unwrap();
            Self(root)
        }
        fn ledger(&self) -> PathBuf {
            crate::store::history_path(&self.0, "demo")
        }
        fn write(&self, events: &[HistoryEvent]) {
            fs::write(
                self.ledger(),
                events
                    .iter()
                    .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
                    .collect::<String>(),
            )
            .unwrap();
        }
        fn query(&self, q: &HistoryQuery) -> Vec<HistoryEntry> {
            query_project_history(&self.0, "demo", q).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn event(id: &str, node: &str, repo: &str, at: &str) -> HistoryEvent {
        serde_json::from_value(serde_json::json!({"schemaVersion":"1","eventId":id,"projectId":"demo","kind":"commit.created","bundleId":"sample","bundleTitle":"Sample","repoId":repo,"nodeId":node,"occurredAt":at,"recordedAt":"2026-01-01T00:00:00Z","recordedBy":"test","message":format!("Update {repo}")})).unwrap()
    }
    fn a() -> HistoryEvent {
        event("a", "node-a", "backend", "2026-01-01T12:00:00Z")
    }
    fn b() -> HistoryEvent {
        event("b", "node-b", "frontend", "2026-01-02T12:00:00Z")
    }
    fn count(f: &Fixture) -> usize {
        f.query(&HistoryQuery::default()).len()
    }
    #[test]
    fn empty_text_matches_unscoped_events_without_searchable_fields() {
        let f = Fixture::new();
        let mut row = a();
        row.bundle_id = None;
        row.bundle_title = None;
        row.repo_id = None;
        row.branch = None;
        row.commit = None;
        row.message = None;
        f.write(&[row]);
        for (input, expected) in [(r#""""#, 1), (r#"NOT """#, 0)] {
            assert_eq!(
                f.query(&HistoryQuery {
                    expression: expression::parse(input).unwrap(),
                    ..Default::default()
                })
                .len(),
                expected
            );
        }
    }
    #[test]
    fn shared_boolean_fixtures_compile_and_execute() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../docs/history-query-v1.json")).unwrap();
        let f = Fixture::new();
        let mut events = Vec::new();
        for unit in fixtures["units"].as_array().unwrap() {
            for (n, row) in unit["events"].as_array().unwrap().iter().enumerate() {
                let id = unit["id"].as_str().unwrap();
                let mut e = event(
                    &format!("{id}-{n}"),
                    id,
                    row["repo"].as_str().unwrap(),
                    row["at"].as_str().unwrap(),
                );
                e.bundle_id = Some(unit["bundle"].as_str().unwrap().into());
                e.bundle_title = Some(unit["title"].as_str().unwrap().into());
                e.message = Some(row["message"].as_str().unwrap().into());
                e.branch = Some(row["branch"].as_str().unwrap().into());
                e.commit = Some(row["sha"].as_str().unwrap().into());
                events.push(e);
            }
        }
        f.write(&events);
        assert_eq!(
            f.query(&HistoryQuery {
                expression: expression::parse(&"api ".repeat(256)).unwrap(),
                ..Default::default()
            })
            .len(),
            2
        );
        let parse = |input: &str| -> Result<Option<expression::Expression>> {
            let mut e = expression::parse(input)?;
            if let Some(e) = &mut e {
                e.resolve_views(&mut |name| {
                    let repos = fixtures["views"].get(name).context("unknown view")?;
                    Ok(repos
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().into())
                        .collect())
                })?;
            }
            Ok(e)
        };
        for case in fixtures["cases"].as_array().unwrap() {
            let input = case["query"].as_str().unwrap();
            for full_context in [false, true] {
                let q = HistoryQuery {
                    expression: parse(input).unwrap(),
                    full_context,
                    ..Default::default()
                };
                let mut actual: Vec<_> = f.query(&q).into_iter().map(|e| e.id).collect();
                actual.sort();
                let expected: Vec<_> = case["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                assert_eq!(actual, expected, "{input}");
                let mut limited = q;
                limited.limit = Some(1);
                assert_eq!(f.query(&limited).len(), expected.len().min(1));
            }
        }
        for field in ["invalid", "invalidViews"] {
            for input in fixtures[field].as_array().unwrap() {
                assert!(parse(input.as_str().unwrap()).is_err(), "{input}");
            }
        }
        for (query, count) in [("repo:api AND repo:web", 0), ("until:2026-09-01", 1)] {
            assert_eq!(
                f.query(&HistoryQuery {
                    expression: parse(query).unwrap(),
                    grouping: HistoryGrouping::Event,
                    ..Default::default()
                })
                .len(),
                count
            );
        }
        assert_eq!(
            f.query(&HistoryQuery {
                expression: parse("repo:api AND repo:web").unwrap(),
                grouping: HistoryGrouping::Bundle,
                ..Default::default()
            })
            .len(),
            1
        );
        // Expression and legacy grep/repo/date constraints intersect independently.
        assert_eq!(
            f.query(&HistoryQuery {
                expression: parse("repo:api").unwrap(),
                grep: vec!["^Update".into()],
                repos: Some(vec!["web".into()]),
                ..Default::default()
            })
            .len(),
            1
        );
    }

    #[test]
    fn cold_warm_append_and_no_canonical_writes() {
        let f = Fixture::new();
        f.write(&[a()]);
        let bytes = fs::read(f.ledger()).unwrap();
        assert_eq!(count(&f), 1);
        let cache = index_path(&f.0, "demo");
        let before = fs::read(&cache).unwrap();
        assert_eq!(count(&f), 1);
        assert_eq!(before, fs::read(&cache).unwrap());
        assert_eq!(bytes, fs::read(f.ledger()).unwrap());
        crate::history::append_history_events(&f.0, "demo", &[b()]).unwrap();
        assert_eq!(count(&f), 2);
        let db = Connection::open(cache).unwrap();
        assert_eq!(
            db.query_row("SELECT seq FROM events WHERE event_id='a'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(!f.0.join(".knit/bundles").exists());
    }
    #[test]
    fn replacement_truncation_and_same_size_rewrite() {
        let f = Fixture::new();
        f.write(&[a(), b()]);
        assert_eq!(count(&f), 2);
        let before = fs::read(f.ledger()).unwrap();
        let text = String::from_utf8(before.clone())
            .unwrap()
            .replace("Update backend", "Change backend");
        assert_eq!(before.len(), text.len());
        fs::write(f.ledger(), text).unwrap();
        assert_eq!(
            f.query(&HistoryQuery {
                reverse: true,
                ..Default::default()
            })[0]
                .message,
            "Change backend"
        );
        f.write(&[b()]);
        assert_eq!(count(&f), 1);
        let replacement = f.ledger().with_extension("replacement");
        fs::write(
            &replacement,
            format!("{}\n", serde_json::to_string(&a()).unwrap()),
        )
        .unwrap();
        fs::rename(replacement, f.ledger()).unwrap();
        assert_eq!(f.query(&HistoryQuery::default())[0].id, "node-a");
    }
    #[test]
    fn malformed_append_rolls_back_and_never_serves_stale() {
        let f = Fixture::new();
        f.write(&[a()]);
        count(&f);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(f.ledger())
            .unwrap();
        writeln!(file, "{}\n{{bad", serde_json::to_string(&b()).unwrap()).unwrap();
        for _ in 0..2 {
            assert!(query_project_history(&f.0, "demo", &HistoryQuery::default()).is_err());
        }
        let db = Connection::open(index_path(&f.0, "demo")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(db);
        f.write(&[a(), b()]);
        assert_eq!(count(&f), 2);
    }
    #[test]
    fn disposable_cache_and_missing_ledger() {
        let f = Fixture::new();
        assert_eq!(count(&f), 0);
        assert!(!f.ledger().exists());
        f.write(&[a()]);
        assert_eq!(count(&f), 1);
        let path = index_path(&f.0, "demo");
        fs::remove_file(&path).unwrap();
        assert_eq!(count(&f), 1);
        fs::write(&path, b"broken sqlite").unwrap();
        assert_eq!(count(&f), 1);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA user_version=999").unwrap();
        drop(db);
        assert_eq!(count(&f), 1);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE events").unwrap();
        drop(db);
        assert_eq!(count(&f), 1);
        fs::remove_file(f.ledger()).unwrap();
        assert_eq!(count(&f), 0);
    }
    #[test]
    fn grouping_repos_and_companion_details() {
        let f = Fixture::new();
        let mut companion = b();
        companion.node_id = a().node_id;
        companion.kind = "commit.observed".into();
        f.write(&[a(), companion, b()]);
        let mut q = HistoryQuery {
            repos: Some(vec!["backend".into()]),
            ..Default::default()
        };
        let result = f.query(&q);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].events.len(), 1);
        q.full_context = true;
        assert_eq!(f.query(&q)[0].events.len(), 2);
        q.kinds = vec!["commit.created".into()];
        assert_eq!(f.query(&q)[0].events.len(), 2);
        q.repos = Some(vec!["backend".into(), "frontend".into()]);
        assert_eq!(f.query(&q).len(), 2);
        q.repo_match = RepoMatch::All;
        assert_eq!(f.query(&q).len(), 1);
        q.grouping = HistoryGrouping::Event;
        assert_eq!(f.query(&q).len(), 0);
        q.grouping = HistoryGrouping::Bundle;
        assert_eq!(f.query(&q)[0].events.len(), 3);
        q.repos = Some(vec![]);
        assert!(f.query(&q).is_empty());
        q.repos = None;
        q.grouping = HistoryGrouping::Event;
        q.kinds.clear();
        assert_eq!(f.query(&q).len(), 3);
    }
    #[test]
    fn timezone_fallback_ties_dates_and_selected_page_reverse() {
        let f = Fixture::new();
        let mut c = event("c", "node-c", "backend", "2026-01-01T07:00:00-05:00");
        c.occurred_at = None;
        c.recorded_at = "2026-01-01T13:00:00+01:00".into();
        f.write(&[c, a(), b()]);
        let mut q = HistoryQuery::default();
        assert_eq!(
            f.query(&q)
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["node-b", "node-a", "node-c"]
        );
        q.limit = Some(2);
        q.reverse = true;
        assert_eq!(
            f.query(&q)
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["node-a", "node-b"]
        );
        q.skip = 1;
        assert_eq!(f.query(&q)[0].id, "node-c");
        q.skip = 0;
        q.since = Some("2026-01-01T07:00:00-05:00".parse().unwrap());
        q.until = q.since;
        assert_eq!(f.query(&q).len(), 2);
        assert_eq!(f.query(&q)[0].occurred_at, "2026-01-01T12:00:00.000000000Z");
        q.limit = Some(0);
        assert!(f.query(&q).is_empty());
    }
    #[test]
    fn grep_regex_fixed_case_and_group_all_match() {
        let f = Fixture::new();
        let mut companion = b();
        companion.node_id = a().node_id;
        f.write(&[a(), companion, b()]);
        let mut q = HistoryQuery {
            grep: vec!["^update BACK".into()],
            ignore_case: true,
            ..Default::default()
        };
        assert_eq!(f.query(&q)[0].events.len(), 2);
        q.fixed_strings = true;
        assert!(f.query(&q).is_empty());
        q.grep = vec!["backend".into(), "frontend".into()];
        q.all_match = true;
        assert_eq!(f.query(&q).len(), 1);
        q.all_match = false;
        assert_eq!(f.query(&q).len(), 2);
        q.grep = vec!["[".into()];
        q.fixed_strings = false;
        assert!(query_project_history(&f.0, "demo", &q).is_err());
    }
    #[test]
    fn orphan_history_and_bundle_snapshot_without_git_or_ledger_writes() {
        let f = Fixture::new();
        f.write(&[a()]);
        assert_eq!(count(&f), 1); // No bundle artifact ever exists.
        let mut bundle = ChangeGroup::new(
            "sample".into(),
            "Synthetic bundle".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.project_id = Some("demo".into());
        assert!(
            query_bundle_history(&f.0, &bundle, &HistoryQuery::default())
                .unwrap()
                .iter()
                .any(|entry| entry.message == "Update backend")
        );
        let q = HistoryQuery {
            grep: vec!["Synthetic".into()],
            ..Default::default()
        };
        assert_eq!(query_bundle_history(&f.0, &bundle, &q).unwrap().len(), 1);
        let snapshot = Fixture::new();
        bundle.project_id = None;
        let entries = query_bundle_history(&snapshot.0, &bundle, &HistoryQuery::default()).unwrap();
        assert!(!entries.is_empty());
        assert!(!snapshot.ledger().exists());
        assert!(!snapshot.0.join(".knit/cache").exists());
    }
    #[test]
    fn selectors_bundle_isolation_and_recorded_snapshot_detail() {
        let f = Fixture::new();
        let mut first = a();
        first.node_id = None;
        first.commit_group_id = Some("group-a".into());
        let mut second = first.clone();
        second.event_id = "other".into();
        second.bundle_id = Some("other-bundle".into());
        let mut fallback = b();
        fallback.node_id = None;
        f.write(&[first, second, fallback]);
        let entries = f.query(&HistoryQuery::default());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "b");
        assert_eq!(entries[1].id, "group-a");
        let mut bundle = ChangeGroup::new(
            "snapshot".into(),
            "Snapshot".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.nodes.push(crate::model::BundleNode::git_observed(
            "observed".into(),
            "2026-01-04T00:00:00Z".into(),
            vec![crate::model::RepoChange {
                repo_id: "backend".into(),
                movement: crate::model::Movement::Advanced,
                before_sha: None,
                after_sha: "abc123".into(),
                commits: vec!["abc123".into()],
                dropped_commits: vec![],
                commit_details: std::collections::BTreeMap::from([(
                    "abc123".into(),
                    crate::model::CommitDetail {
                        subject: "Recorded subject".into(),
                        authored_at: "2026-01-03T00:00:00Z".into(),
                    },
                )]),
            }],
        ));
        let entries = query_bundle_history(&f.0, &bundle, &HistoryQuery::default()).unwrap();
        assert!(entries.iter().any(|e| e.message == "Recorded subject"
            && e.occurred_at == "2026-01-04T00:00:00.000000000Z"));
    }

    #[test]
    fn named_lock_contention_is_explicit_and_recoverable() {
        let f = Fixture::new();
        f.write(&[a()]);
        count(&f);
        let lock = crate::store::acquire_named_lock(&f.0, "history-demo").unwrap();
        assert!(query_lock(&f.0, "demo", Duration::from_millis(20)).is_err());
        drop(lock);
        assert_eq!(count(&f), 1);
    }
    #[test]
    fn simultaneous_queries_and_brief_writer_lock_succeed() {
        let f = Fixture::new();
        f.write(&[a()]);
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        query_project_history(&f.0, "demo", &HistoryQuery::default())
                    })
                })
                .collect();
            barrier.wait();
            for handle in handles {
                assert_eq!(handle.join().unwrap().unwrap().len(), 1);
            }
        });
        let lock = crate::store::acquire_named_lock(&f.0, "history-demo").unwrap();
        std::thread::scope(|scope| {
            let reader =
                scope.spawn(|| query_project_history(&f.0, "demo", &HistoryQuery::default()));
            std::thread::sleep(Duration::from_millis(25));
            f.write(&[a(), b()]);
            drop(lock);
            assert_eq!(reader.join().unwrap().unwrap().len(), 2);
        });
        fs::remove_dir_all(f.0.join(".knit/locks")).unwrap();
        fs::write(f.0.join(".knit/locks"), "not a directory").unwrap();
        let error = query_lock(&f.0, "demo", Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn set_based_query_plans_scale_without_correlated_scans() {
        fn measure(groups: usize, q: &HistoryQuery) -> i32 {
            let mut db = Connection::open_in_memory().unwrap();
            schema(&db).unwrap();
            let tx = db.transaction().unwrap();
            for n in 0..groups {
                for repo in ["api", "web"] {
                    let mut e = event(
                        &format!("{n}-{repo}"),
                        &format!("node-{n}"),
                        repo,
                        "2026-01-01T00:00:00Z",
                    );
                    e.bundle_id = Some(format!("bundle-{}", n / 20));
                    insert(&tx, &e, &serde_json::to_string(&e).unwrap()).unwrap();
                }
            }
            tx.commit().unwrap();
            let (sql, values) = query_sql(&db, q, false).unwrap();
            let mut plan = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let details = plan
                .query_map(params_from_iter(values.clone()), |row| {
                    row.get::<_, String>(3)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                !details.iter().any(|line| line.contains("CORRELATED")),
                "{details:?}"
            );
            let mut stmt = db.prepare(&sql).unwrap();
            let rows = stmt
                .query_map(params_from_iter(values), |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(!rows.is_empty());
            stmt.get_status(rusqlite::StatementStatus::VmStep)
        }
        for grouping in [
            HistoryGrouping::Commit,
            HistoryGrouping::Bundle,
            HistoryGrouping::Event,
        ] {
            for repo_match in [RepoMatch::Any, RepoMatch::All] {
                let q = HistoryQuery {
                    expression: expression::parse("(repo:api OR repo:web) AND update").unwrap(),
                    grouping,
                    repo_match,
                    repos: Some(if matches!(grouping, HistoryGrouping::Event) {
                        vec!["api".into()]
                    } else {
                        vec!["api".into(), "web".into()]
                    }),
                    grep: vec!["^Update".into()],
                    limit: Some(20),
                    ..Default::default()
                };
                let small = measure(100, &q);
                let large = measure(1000, &q);
                // Tenfold growth must stay comfortably below quadratic (100x).
                // VM instruction counts are deterministic, unlike wall-clock timing.
                assert!(
                    large < small * 20,
                    "{grouping:?}/{repo_match:?}: {small} -> {large}"
                );
            }
        }
    }

    #[test]
    fn basic_and_extended_patterns_match_git_on_synthetic_lines() {
        let f = Fixture::new();
        let messages="api\nweb\napi|web\napiapi\naab\nab\na^b\na$b\n42\nCache [entry]\n&\n~\n]\n\\\na\nb\n_api\ncapillary\n";
        fs::write(f.0.join("messages"), messages).unwrap();
        for extended in [false, true] {
            for pattern in [
                r"api|web",
                r"api\|web",
                r"\(api\)\{2\}",
                r"a\+b",
                r"a\?b",
                r"[[:digit:]]",
                r"Cache \[entry\]",
                r"\bapi\b",
                r"\<api\>",
                r"^^",
                r"[.foo.]",
                r"a^b",
                r"a$b",
                r"[a&&b]",
                r"[a~~b]",
                r"[]a]",
                r"[\]",
                r"(api|web)",
                r"a{1,2}b",
                "api\nweb",
            ] {
                let output = std::process::Command::new("git")
                    .current_dir(&f.0)
                    .env("LC_ALL", "C")
                    .args([
                        "grep",
                        "--no-index",
                        "-h",
                        if extended { "-E" } else { "-G" },
                        "-e",
                        pattern,
                        "--",
                        "messages",
                    ])
                    .output()
                    .unwrap();
                assert!(
                    matches!(output.status.code(), Some(0 | 1)),
                    "git rejected {pattern:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let q = HistoryQuery {
                    extended_regexp: extended,
                    ..Default::default()
                };
                if extended && [r"\bapi\b", r"\<api\>"].contains(&pattern) {
                    assert!(grep::compile(pattern, &q).is_err());
                    continue;
                }
                let re = grep::compile(pattern, &q).unwrap();
                let actual = messages
                    .lines()
                    .filter(|line| re.is_match(line))
                    .collect::<Vec<_>>();
                let expected = String::from_utf8(output.stdout).unwrap();
                assert_eq!(
                    actual,
                    expected.lines().collect::<Vec<_>>(),
                    "{pattern:?}, extended={extended}"
                );
            }
        }
        for pattern in [r"\(api\)\1", r"\d", r"\p{Greek}", r"[[=a=]]"] {
            assert!(
                grep::compile(pattern, &HistoryQuery::default()).is_err(),
                "{pattern}"
            );
        }
        assert!(grep::compile(
            "(?i)api",
            &HistoryQuery {
                extended_regexp: true,
                ..Default::default()
            }
        )
        .is_err());
    }
    #[test]
    fn grep_modes_and_line_anchors_select_complete_groups() {
        let f = Fixture::new();
        let mut first = a();
        first.message = Some("Update api|web\n\nCache details\n".into());
        let mut second = b();
        second.message = Some("Update api\n".into());
        f.write(&[first, second]);
        let mut q = HistoryQuery {
            grep: vec!["api|web".into()],
            ..Default::default()
        };
        assert_eq!(f.query(&q).len(), 1);
        q.extended_regexp = true;
        assert_eq!(f.query(&q).len(), 2);
        q.fixed_strings = true;
        assert_eq!(f.query(&q).len(), 1);
        q.fixed_strings = false;
        q.extended_regexp = false;
        q.grep = vec![r"api\|web".into()];
        assert_eq!(f.query(&q).len(), 2);
        for pattern in ["^Cache", "details$", "^$"] {
            q.grep = vec![pattern.into()];
            let result = f.query(&q);
            assert_eq!(result.len(), 1, "{pattern}");
            assert_eq!(result[0].id, "node-a");
        }
    }
    #[test]
    fn partial_ledger_is_supplemented_without_overwriting_recorded_detail() {
        use crate::model::{BundleNode, CommitGroup, CommitRef};
        let f = Fixture::new();
        let mut bundle = ChangeGroup::new(
            "sample".into(),
            "Partial history".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.project_id = Some("demo".into());
        bundle.nodes.push(BundleNode::commit_group(
            "node-partial".into(),
            "2026-01-02T00:00:00Z".into(),
            "Artifact message".into(),
            vec![
                CommitRef {
                    repo_id: "api".into(),
                    sha: "a".repeat(40),
                },
                CommitRef {
                    repo_id: "web".into(),
                    sha: "b".repeat(40),
                },
            ],
            vec![],
        ));
        bundle.nodes.push(BundleNode::checkpoint(
            "checkpoint-new".into(),
            "2026-01-04T00:00:00Z".into(),
            "New checkpoint".into(),
            vec!["api".into()],
            "group-checkpoint".into(),
        ));
        bundle.commit_groups.push(CommitGroup {
            id: "legacy-missing".into(),
            created_at: "2026-01-03T00:00:00Z".into(),
            message: "Legacy work".into(),
            commits: vec![CommitRef {
                repo_id: "api".into(),
                sha: "c".repeat(40),
            }],
            author: None,
        });
        let snapshot = crate::history::bundle_history_snapshot(&bundle);
        let mut recorded = snapshot
            .iter()
            .find(|e| {
                e.node_id.as_deref() == Some("node-partial") && e.repo_id.as_deref() == Some("api")
            })
            .unwrap()
            .clone();
        recorded.message = Some("Enriched recorded message".into());
        recorded.occurred_at = Some("2026-01-02T05:00:00+01:00".into());
        recorded.metadata = Some(serde_json::json!({"detail":"Preserved enrichment"}));
        recorded.recorded_by = "original recorder".into();
        let orphan = event("orphan-event", "orphan-node", "api", "2026-01-01T12:00:00Z");
        f.write(&[recorded.clone(), orphan]);
        let ledger_before = fs::read(f.ledger()).unwrap();
        let artifact_before = serde_json::to_vec(&bundle).unwrap();
        // Warm the project cache so we can also prove snapshot-only events never leak into it.
        assert_eq!(count(&f), 2);
        let cache_before = fs::read(index_path(&f.0, "demo")).unwrap();
        let q = HistoryQuery {
            repos: Some(vec!["api".into(), "web".into()]),
            repo_match: RepoMatch::All,
            full_context: true,
            ..Default::default()
        };
        let entries = query_bundle_history(&f.0, &bundle, &q).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "node-partial");
        assert_eq!(entries[0].events.len(), 2); // Missing companion event, not just a missing node.
        let actual = entries[0]
            .events
            .iter()
            .find(|e| e.event_id == recorded.event_id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(&recorded).unwrap()
        );
        let latest = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(latest[0].id, "checkpoint-new"); // Union before limit.
        let entries = query_bundle_history(&f.0, &bundle, &HistoryQuery::default()).unwrap();
        for id in [
            "legacy-missing",
            "orphan-node",
            "node-partial",
            "checkpoint-new",
        ] {
            assert!(entries.iter().any(|e| e.id == id), "{id}");
        }
        let filtered = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                grep: vec!["Artifact message".into()],
                repos: Some(vec!["api".into()]),
                full_context: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].events.len(), 2); // Filter after supplementation, retain enrichment.
        assert_eq!(count(&f), 2);
        assert_eq!(ledger_before, fs::read(f.ledger()).unwrap());
        assert_eq!(cache_before, fs::read(index_path(&f.0, "demo")).unwrap());
        assert_eq!(artifact_before, serde_json::to_vec(&bundle).unwrap());
    }
    #[test]
    fn bundle_commit_groups_follow_nodes_not_old_authored_dates() {
        use crate::model::{BundleNode, CommitDetail, CommitRef, Movement, RepoChange};
        let f = Fixture::new();
        let mut bundle = ChangeGroup::new(
            "sample".into(),
            "Node chronology".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.project_id = Some("demo".into());
        bundle.nodes.push(BundleNode::commit_group(
            "node-commit".into(),
            "2026-01-02T12:00:00.900Z".into(),
            "Recent commit".into(),
            vec![CommitRef {
                repo_id: "api".into(),
                sha: "a".repeat(40),
            }],
            vec![],
        ));
        bundle.nodes.push(BundleNode::git_observed(
            "node-observed".into(),
            "2026-01-03T12:00:00Z".into(),
            vec![RepoChange {
                repo_id: "api".into(),
                movement: Movement::Advanced,
                before_sha: None,
                after_sha: "b".repeat(40),
                commits: vec!["b".repeat(40)],
                dropped_commits: vec![],
                commit_details: std::collections::BTreeMap::from([(
                    "b".repeat(40),
                    CommitDetail {
                        subject: "Old commit observed today".into(),
                        authored_at: "2020-01-01T00:00:00Z".into(),
                    },
                )]),
            }],
        ));
        let recorded = crate::history::bundle_history_snapshot(&bundle);
        f.write(&recorded);
        let before = fs::read(f.ledger()).unwrap();
        let query = HistoryQuery {
            limit: Some(1),
            repos: Some(vec!["api".into()]),
            ..Default::default()
        };
        let latest = query_bundle_history(&f.0, &bundle, &query).unwrap();
        assert_eq!(
            latest[0].id,
            crate::selectors::resolve_log_node(&bundle.nodes, "HEAD")
                .unwrap()
                .id
        );
        assert_eq!(latest[0].occurred_at, "2026-01-03T12:00:00.000000000Z");
        assert_eq!(
            latest[0].events[0].occurred_at.as_deref(),
            Some("2020-01-01T00:00:00Z")
        );
        let original = recorded
            .iter()
            .find(|e| e.event_id == latest[0].events[0].event_id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&latest[0].events[0]).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        let dated_expression = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                expression: expression::parse("until:2020-01-01").unwrap(),
                ..query.clone()
            },
        )
        .unwrap();
        assert_eq!(dated_expression.len(), 1);
        assert_eq!(dated_expression[0].id, "node-observed");
        assert_eq!(
            query_bundle_history(
                &f.0,
                &bundle,
                &HistoryQuery {
                    expression: expression::parse("since:2026-01-01").unwrap(),
                    ..query.clone()
                }
            )
            .unwrap()[0]
                .id,
            "node-commit"
        );
        let previous = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                skip: 1,
                ..query.clone()
            },
        )
        .unwrap();
        assert_eq!(
            previous[0].id,
            crate::selectors::resolve_log_node(&bundle.nodes, "HEAD~1")
                .unwrap()
                .id
        );
        let project = query_project_history(&f.0, "demo", &query).unwrap();
        assert_eq!(project[0].id, "node-commit");
        let events = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                grouping: HistoryGrouping::Event,
                ..query.clone()
            },
        )
        .unwrap();
        assert_eq!(events[0].events[0].node_id.as_deref(), Some("node-commit"));
        let dated = query_bundle_history(
            &f.0,
            &bundle,
            &HistoryQuery {
                since: Some("2026-01-03T00:00:00Z".parse().unwrap()),
                ..query.clone()
            },
        )
        .unwrap();
        assert_eq!(dated[0].id, "node-observed");
        // Node sequence remains the HEAD contract even with equal/clock-skewed times.
        bundle.nodes.last_mut().unwrap().created_at = "2026-01-02T12:00:00.900Z".into();
        assert_eq!(
            query_bundle_history(&f.0, &bundle, &query).unwrap()[0].id,
            "node-observed"
        );
        bundle.nodes.last_mut().unwrap().created_at = "2026-01-02T11:00:00Z".into();
        assert_eq!(
            query_bundle_history(&f.0, &bundle, &query).unwrap()[0].id,
            "node-observed"
        );
        assert_eq!(before, fs::read(f.ledger()).unwrap());
    }
}
