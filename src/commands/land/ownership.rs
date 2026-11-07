//! Who holds a local landing lock, and whether a lock left behind by a
//! process that exited can be proven safe to take over.
//!
//! A lock outlives its process only when that process was killed before it
//! could release it. The process being gone is not enough to take the lock:
//! a deploy it started may still be running. Every managed step command runs
//! in its own process group, so the owner records each group it starts; a
//! lock is reclaimable once the owner and every recorded group are gone.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct Record {
    path: PathBuf,
    groups: Vec<u32>,
    spawning: usize,
}

impl Record {
    fn persist(&self) -> Result<()> {
        write_atomically(
            &self.path,
            &json!({
                "pid": std::process::id(),
                "groups": self.groups,
                "spawning": self.spawning,
            }),
        )
    }
}

fn records() -> &'static Mutex<Vec<Record>> {
    static RECORDS: OnceLock<Mutex<Vec<Record>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(Vec::new()))
}

fn with_records(f: impl Fn(&mut Record)) -> Result<()> {
    let mut records = records()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for record in records.iter_mut() {
        f(record);
        record.persist()?;
    }
    Ok(())
}

/// The owner record for the locks one landing takes. Dropping it removes the
/// record, so it must be dropped after the locks that name it.
pub(crate) struct Owner {
    path: PathBuf,
}

impl Owner {
    pub(crate) fn begin(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("owner-{}.json", unique_suffix()));
        let record = Record {
            path: path.clone(),
            groups: Vec::new(),
            spawning: 0,
        };
        record.persist()?;
        records()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
        Ok(Self { path })
    }

    pub(crate) fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        records()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|record| record.path != self.path);
        let _ = fs::remove_file(&self.path);
    }
}

/// Called before a managed command is spawned. Until `spawned` records its
/// group, a crash leaves the owner marked as mid-spawn, which no takeover
/// treats as proof of quiescence.
pub(super) fn spawning() -> Result<()> {
    with_records(|record| record.spawning += 1)
}

pub(super) fn spawned(group: Option<u32>) -> Result<()> {
    with_records(|record| {
        record.spawning = record.spawning.saturating_sub(1);
        if let Some(group) = group {
            record.groups.push(group);
        }
    })
}

/// Called once a managed command's group has been terminated and reaped.
pub(super) fn quiesced(group: u32) -> Result<()> {
    with_records(|record| record.groups.retain(|g| *g != group))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The owning process is still running.
    Running { pid: u32 },
    /// The owner exited, but something it started may still be running, or
    /// the lock does not say what it started.
    Unverified { pid: Option<u32>, reason: String },
    /// The owner exited and every step process group it recorded is gone.
    Exited { pid: u32 },
}

pub(crate) fn assess(lock: &Path) -> Holder {
    let Ok(text) = fs::read_to_string(lock) else {
        return Holder::Unverified {
            pid: None,
            reason: "the lock cannot be read".into(),
        };
    };
    let text = text.trim();
    if let Ok(pid) = text.parse::<u32>() {
        if process_alive(pid) {
            return Holder::Running { pid };
        }
        return Holder::Unverified {
            pid: Some(pid),
            reason: "it was taken by an earlier Knit release that did not record the step processes it started".into(),
        };
    }
    let Ok(lock_value) = serde_json::from_str::<Value>(text) else {
        return Holder::Unverified {
            pid: None,
            reason: "the lock does not name its owner".into(),
        };
    };
    let Some(pid) = lock_value["pid"].as_u64().map(|pid| pid as u32) else {
        return Holder::Unverified {
            pid: None,
            reason: "the lock does not name its owner".into(),
        };
    };
    if process_alive(pid) {
        return Holder::Running { pid };
    }
    let owner = lock_value["owner"]
        .as_str()
        .and_then(|name| lock.parent().map(|dir| dir.join(name)));
    let Some(record) = owner
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return Holder::Unverified {
            pid: Some(pid),
            reason: "its owner record is missing".into(),
        };
    };
    if record["spawning"].as_u64().unwrap_or(0) > 0 {
        return Holder::Unverified {
            pid: Some(pid),
            reason: "it exited while starting a step command".into(),
        };
    }
    let live: Vec<u64> = record["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .filter(|group| group_alive(*group as u32))
        .collect();
    if !live.is_empty() {
        return Holder::Unverified {
            pid: Some(pid),
            reason: format!(
                "step process group(s) {} it started are still running",
                live.iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    Holder::Exited { pid }
}

/// Remove owner records whose process exited and that no lock names.
pub(crate) fn sweep_owner_records(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    let named: Vec<String> = paths
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "lock"))
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<Value>(text.trim()).ok())
        .filter_map(|value| value["owner"].as_str().map(str::to_owned))
        .collect();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("owner-") || named.iter().any(|n| n == name) {
            continue;
        }
        let pid = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value["pid"].as_u64());
        if pid.is_some_and(|pid| !process_alive(pid as u32)) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(unix)]
fn signal_zero(target: i32) -> bool {
    // EPERM means the target exists but belongs to someone else.
    let result = unsafe { libc::kill(target, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    pid == std::process::id() || signal_zero(pid as i32)
}

#[cfg(unix)]
fn group_alive(group: u32) -> bool {
    signal_zero(-(group as i32))
}

// Liveness is not checked off Unix, so a held lock always counts as running.
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(not(unix))]
fn group_alive(_group: u32) -> bool {
    true
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}-{nanos:x}-{:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_atomically(path: &Path, value: &Value) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file =
        fs::File::create(&temp).with_context(|| format!("failed to write {}", temp.display()))?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("knit-ownership-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn exited_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn lock_for(dir: &Path, pid: u32, owner: &str, record: Value) -> PathBuf {
        fs::write(dir.join(owner), record.to_string()).unwrap();
        let lock = dir.join("resource.lock");
        fs::write(&lock, json!({"pid": pid, "owner": owner}).to_string()).unwrap();
        lock
    }

    #[test]
    fn a_lock_of_a_running_process_is_held() {
        let dir = dir();
        let lock = lock_for(
            &dir,
            std::process::id(),
            "owner-a.json",
            json!({"groups": [], "spawning": 0}),
        );
        assert_eq!(
            assess(&lock),
            Holder::Running {
                pid: std::process::id()
            }
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_exited_owner_with_no_live_groups_can_be_taken_over() {
        let dir = dir();
        let pid = exited_pid();
        let group = exited_pid();
        let lock = lock_for(
            &dir,
            pid,
            "owner-b.json",
            json!({"groups": [group], "spawning": 0}),
        );
        assert_eq!(assess(&lock), Holder::Exited { pid });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_step_group_that_outlived_its_owner_keeps_the_lock() {
        use std::os::unix::process::CommandExt;
        let dir = dir();
        let mut step = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let lock = lock_for(
            &dir,
            exited_pid(),
            "owner-c.json",
            json!({"groups": [step.id()], "spawning": 0}),
        );
        let holder = assess(&lock);
        step.kill().unwrap();
        step.wait().unwrap();
        assert!(
            matches!(&holder, Holder::Unverified { reason, .. } if reason.contains("still running")),
            "{holder:?}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_crash_mid_spawn_or_a_bare_pid_cannot_be_verified() {
        let dir = dir();
        let pid = exited_pid();
        let lock = lock_for(
            &dir,
            pid,
            "owner-d.json",
            json!({"groups": [], "spawning": 1}),
        );
        assert!(matches!(assess(&lock), Holder::Unverified { .. }));
        fs::write(&lock, format!("{pid}\n")).unwrap();
        assert!(matches!(
            assess(&lock),
            Holder::Unverified { pid: Some(p), .. } if p == pid
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sweeping_keeps_records_that_a_lock_still_names() {
        let dir = dir();
        let pid = exited_pid();
        let lock = lock_for(
            &dir,
            pid,
            "owner-e.json",
            json!({"pid": pid, "groups": [], "spawning": 0}),
        );
        fs::write(
            dir.join("owner-f.json"),
            json!({"pid": pid, "groups": [], "spawning": 0}).to_string(),
        )
        .unwrap();
        sweep_owner_records(&dir);
        assert!(dir.join("owner-e.json").exists());
        assert!(!dir.join("owner-f.json").exists());
        fs::remove_file(lock).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
