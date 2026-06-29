use crate::model::{BundleState, ChangeGroup, KnitConfig};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

static BUNDLE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

pub struct ActiveBundle {
    pub root: PathBuf,
    pub bundle_path: PathBuf,
    pub bundle: ChangeGroup,
    pub resolution_source: BundleResolutionSource,
    _lock: Option<KnitLock>,
}

impl ActiveBundle {
    pub fn unlocked(root: PathBuf, bundle_path: PathBuf, bundle: ChangeGroup) -> Self {
        Self {
            root,
            bundle_path,
            bundle,
            resolution_source: BundleResolutionSource::Config,
            _lock: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleResolutionSource {
    Explicit,
    Env,
    Worktree,
    Config,
}

impl BundleResolutionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Env => "env",
            Self::Worktree => "cwd",
            Self::Config => "workspace",
        }
    }
}

pub struct KnitLock {
    path: PathBuf,
}

pub fn set_bundle_override(bundle_id: Option<String>) {
    *BUNDLE_OVERRIDE
        .lock()
        .expect("bundle override lock poisoned") = bundle_id;
}

pub fn load_active_bundle() -> Result<ActiveBundle> {
    load_active_bundle_inner(false)
}

pub fn load_active_bundle_for_update() -> Result<ActiveBundle> {
    load_active_bundle_inner(true)
}

fn load_active_bundle_inner(lock_for_update: bool) -> Result<ActiveBundle> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd)
        .context("No Knit workspace found. Run `knit bundle \"feature title\"` first.")?;
    let config = load_config(&root)?;
    let (bundle_id, resolution_source) = resolve_bundle_id(&root, &cwd, &config)?;
    let bundle_path = root
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let bundle: ChangeGroup = read_json(&bundle_path)?;
    if lock_for_update {
        ensure_root_workspace_fallback_is_unambiguous(
            &root,
            &cwd,
            &bundle_id,
            &resolution_source,
            &bundle,
            "update",
        )?;
    }
    let lock = if lock_for_update {
        Some(acquire_bundle_lock(&root, &bundle_id)?)
    } else {
        None
    };

    Ok(ActiveBundle {
        root,
        bundle_path,
        bundle,
        resolution_source,
        _lock: lock,
    })
}

pub fn save_active_bundle(active: &ActiveBundle) -> Result<()> {
    write_json(&active.bundle_path, &active.bundle)?;
    crate::history::record_bundle_history(&active.root, &active.bundle)?;
    Ok(())
}

pub fn load_config(root: &Path) -> Result<KnitConfig> {
    read_json(&root.join(".knit/config.json"))
}

pub fn save_config(root: &Path, config: &KnitConfig) -> Result<()> {
    write_json(&root.join(".knit/config.json"), config)
}

pub fn global_config_path() -> Result<PathBuf> {
    if let Some(home) = env::var_os("KNIT_HOME") {
        return Ok(PathBuf::from(home).join("config.json"));
    }
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join("knit/config.json"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config/knit/config.json"));
    }
    if cfg!(windows) {
        if let Some(appdata) = env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata).join("knit/config.json"));
        }
        if let Some(profile) = env::var_os("USERPROFILE") {
            return Ok(PathBuf::from(profile).join(".config/knit/config.json"));
        }
    }
    bail!(
        "No home directory found. Set KNIT_HOME, HOME, or APPDATA before using global Knit config."
    )
}

pub fn load_global_config() -> Result<KnitConfig> {
    let path = global_config_path()?;
    if path.exists() {
        read_json(&path)
    } else {
        Ok(KnitConfig::empty())
    }
}

pub fn save_global_config(config: &KnitConfig) -> Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_json(&path, config)
}

/// Merge user-global config with workspace config. Workspace remotes override global
/// remotes of the same name; workspace sync targets override global sync targets when set.
pub fn merge_effective_config(global: KnitConfig, workspace: KnitConfig) -> KnitConfig {
    let mut effective = global;

    effective.active_bundle = workspace.active_bundle;
    effective.active_project = workspace.active_project;

    if !workspace.sync_remotes.is_empty() {
        effective.sync_remotes = workspace.sync_remotes;
        effective.sync_remote = workspace
            .sync_remote
            .or_else(|| effective.sync_remotes.first().cloned());
    } else if workspace.sync_remote.is_some() {
        effective.sync_remote = workspace.sync_remote.clone();
        effective.sync_remotes = effective.sync_remote.iter().cloned().collect();
    }

    effective.advice = workspace.advice;
    if workspace.stealth.is_some() {
        effective.stealth = workspace.stealth;
    }
    effective.push_sync = workspace.push_sync;
    effective.remotes.extend(workspace.remotes);

    effective
}

pub fn load_effective_config(root: &Path) -> Result<KnitConfig> {
    Ok(merge_effective_config(
        load_global_config()?,
        load_config(root)?,
    ))
}

pub fn bundle_path(root: &Path, bundle_id: &str) -> PathBuf {
    root.join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"))
}

pub fn project_path(root: &Path, project_id: &str) -> PathBuf {
    root.join(".knit/projects")
        .join(format!("{project_id}.project.json"))
}

pub fn views_path(root: &Path, project_id: &str) -> PathBuf {
    root.join(".knit/views")
        .join(format!("{project_id}.views.json"))
}

pub fn history_path(root: &Path, project_id: &str) -> PathBuf {
    root.join(".knit/history")
        .join(format!("{project_id}.history.jsonl"))
}

/// Load the current user's views artifact for a project, returning an empty
/// document when none exists yet.
pub fn load_views(root: &Path, project_id: &str) -> Result<crate::model::KnitProjectViews> {
    let path = views_path(root, project_id);
    if path.exists() {
        read_json(&path)
    } else {
        Ok(crate::model::KnitProjectViews::new(
            project_id.to_string(),
            crate::time::now_iso(),
        ))
    }
}

pub fn save_views(root: &Path, views: &crate::model::KnitProjectViews) -> Result<()> {
    let dir = root.join(".knit/views");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    write_json(&views_path(root, &views.project_id), views)
}

pub fn bundle_exists(root: &Path, bundle_id: &str) -> bool {
    bundle_path(root, bundle_id).exists()
}

pub fn infer_worktree_bundle(root: &Path, cwd: &Path) -> Option<String> {
    let worktrees = root.join(".knit/worktrees");
    let relative = crate::paths::strip_path_prefix(cwd, &worktrees)?;
    let mut components = relative.components();
    match components.next()? {
        Component::Normal(bundle) => Some(bundle.to_string_lossy().to_string()),
        _ => None,
    }
}

/// Paths inside JSON artifacts always use forward slashes so bundles written
/// on Windows stay readable on Unix and vice versa. `PathBuf::from` accepts
/// `/`-separated relative paths on every platform when resolving them back.
pub fn relative_path_for_storage(root: &Path, path: &Path) -> String {
    let stored = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    if cfg!(windows) {
        stored.replace('\\', "/")
    } else {
        stored
    }
}

pub fn set_workspace_active_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let mut config = load_config(root)?;
    config.active_bundle = Some(bundle_id.to_string());
    save_config(root, &config)
}

pub fn acquire_named_lock(root: &Path, name: &str) -> Result<KnitLock> {
    let dir = root.join(".knit/locks");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create Knit lock directory {}", dir.display()))?;
    let path = dir.join(format!("{name}.lock"));
    if let Some(lock) = try_create_lock(&path)? {
        return Ok(lock);
    }

    // A lock whose recorded holder process is gone is a leftover from a crash;
    // reclaim it instead of demanding manual cleanup.
    if lock_holder_is_dead(&path) {
        let _ = fs::remove_file(&path);
        if let Some(lock) = try_create_lock(&path)? {
            return Ok(lock);
        }
    }

    let holder = lock_holder_pid(&path)
        .map(|pid| format!(" (pid {pid})"))
        .unwrap_or_default();
    bail!(
        "Another Knit process{holder} is updating this state. Remove {} only if you are sure no Knit process is running.",
        path.display()
    )
}

fn try_create_lock(path: &Path) -> Result<Option<KnitLock>> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            use std::io::Write;
            let _ = write!(file, "{}", std::process::id());
            Ok(Some(KnitLock {
                path: path.to_path_buf(),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to acquire Knit lock {}", path.display()))
        }
    }
}

fn lock_holder_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// True only when the lock file records a holder pid and that process is
/// verifiably gone. Locks without a pid (older Knit versions) and cases
/// where liveness cannot be checked stay treated as held — never reclaim a
/// lock that might still be active.
fn lock_holder_is_dead(path: &Path) -> bool {
    let Some(pid) = lock_holder_pid(path) else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    !process_is_running(pid)
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(true)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use std::ffi::c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut c_void;
        fn GetExitCodeProcess(hProcess: *mut c_void, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Only treat "invalid parameter" as a definite "process gone"; other
            // failures (e.g. access denied) mean we cannot verify liveness.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Write JSON atomically: serialize to a sibling temp file, then rename over
/// the target. A crash mid-write can no longer leave a torn artifact for a
/// concurrent reader (another knit process or agent in the same workspace).
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("failed to serialize JSON")?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "knit".to_string());
    let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temp_path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        // Windows refuses to rename over an existing file; fall back to
        // replace-then-rename, accepting the tiny non-atomic window there.
        Err(_) if cfg!(windows) && path.exists() => {
            fs::remove_file(path)
                .with_context(|| format!("failed to replace {}", path.display()))?;
            fs::rename(&temp_path, path)
                .with_context(|| format!("failed to write {}", path.display()))
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(error).with_context(|| format!("failed to write {}", path.display()))
        }
    }
}

pub fn find_knit_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".knit/config.json").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn resolve_bundle_id(
    root: &Path,
    cwd: &Path,
    config: &KnitConfig,
) -> Result<(String, BundleResolutionSource)> {
    if let Some(bundle_id) = BUNDLE_OVERRIDE
        .lock()
        .expect("bundle override lock poisoned")
        .clone()
    {
        ensure_bundle_exists(root, &bundle_id)?;
        return Ok((bundle_id, BundleResolutionSource::Explicit));
    }

    if let Ok(bundle_id) = std::env::var("KNIT_BUNDLE") {
        let bundle_id = bundle_id.trim().to_string();
        if !bundle_id.is_empty() {
            ensure_bundle_exists(root, &bundle_id)?;
            return Ok((bundle_id, BundleResolutionSource::Env));
        }
    }

    if let Some(bundle_id) = infer_worktree_bundle(root, cwd) {
        ensure_bundle_exists(root, &bundle_id)?;
        return Ok((bundle_id, BundleResolutionSource::Worktree));
    }

    if let Some(bundle_id) = &config.active_bundle {
        ensure_bundle_exists(root, bundle_id)?;
        return Ok((bundle_id.clone(), BundleResolutionSource::Config));
    }

    bail!("No active Knit bundle found. Run `knit bundle \"feature title\"` first.")
}

pub fn ensure_workspace_fallback_status_is_unambiguous(active: &ActiveBundle) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    ensure_root_workspace_fallback_is_unambiguous(
        &active.root,
        &cwd,
        &active.bundle.id,
        &active.resolution_source,
        &active.bundle,
        "show status for",
    )
}

fn ensure_root_workspace_fallback_is_unambiguous(
    root: &Path,
    cwd: &Path,
    bundle_id: &str,
    resolution_source: &BundleResolutionSource,
    bundle: &ChangeGroup,
    action: &str,
) -> Result<()> {
    if resolution_source != &BundleResolutionSource::Config || cwd != root {
        return Ok(());
    }
    if bundle.repos.is_empty() {
        return Ok(());
    }

    let open_bundles = open_bundle_ids(root)?;
    if open_bundles.len() <= 1 {
        return Ok(());
    }

    bail!(
        "Refusing to {action} bundle `{bundle_id}` from the workspace root via the shared workspace fallback because multiple open bundles exist: {}. Use the same command with `--bundle {bundle_id}`, run from `.knit/worktrees/{bundle_id}/<repo>/`, or close/archive bundles you are not using.",
        open_bundles.join(", ")
    )
}

fn open_bundle_ids(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read bundle directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bundle: ChangeGroup = read_json(&path)?;
        if is_open_bundle(&bundle) {
            ids.push(bundle.id);
        }
    }
    ids.sort();
    Ok(ids)
}

fn is_open_bundle(bundle: &ChangeGroup) -> bool {
    match bundle.state {
        Some(BundleState::Archived | BundleState::Closed | BundleState::Deleted) => return false,
        _ => {}
    }

    !bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.closed" || node.node_type == "feature.landed")
}

fn ensure_bundle_exists(root: &Path, bundle_id: &str) -> Result<()> {
    if bundle_exists(root, bundle_id) {
        Ok(())
    } else {
        bail!("No Knit bundle named `{bundle_id}` found.")
    }
}

fn acquire_bundle_lock(root: &Path, bundle_id: &str) -> Result<BundleLock> {
    acquire_named_lock(root, bundle_id)
}

type BundleLock = KnitLock;

impl Drop for KnitLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
