use crate::model::{
    ChangeGroup, KnitConfig, KnitContexts, BUNDLE_STATE_ARCHIVED, BUNDLE_STATE_CLOSED,
    BUNDLE_STATE_DELETED,
};
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
    Context,
    Config,
}

impl BundleResolutionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Env => "env",
            Self::Worktree => "cwd",
            Self::Context => "folder",
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
    let home = env::var_os("HOME").context(
        "No home directory found. Set KNIT_HOME or HOME before using global Knit config.",
    )?;
    Ok(PathBuf::from(home).join(".config/knit/config.json"))
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
        effective.sync_remotes = effective
            .sync_remote
            .iter()
            .cloned()
            .collect();
    }

    effective.advice = workspace.advice;
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
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    write_json(&views_path(root, &views.project_id), views)
}

pub fn work_item_path(root: &Path, work_item_id: &str) -> PathBuf {
    root.join(".knit/work-items")
        .join(format!("{work_item_id}.work-item.json"))
}

pub fn bundle_exists(root: &Path, bundle_id: &str) -> bool {
    bundle_path(root, bundle_id).exists()
}

pub fn infer_worktree_bundle(root: &Path, cwd: &Path) -> Option<String> {
    let worktrees = root.join(".knit/worktrees");
    let relative = cwd.strip_prefix(worktrees).ok()?;
    let mut components = relative.components();
    match components.next()? {
        Component::Normal(bundle) => Some(bundle.to_string_lossy().to_string()),
        _ => None,
    }
}

pub fn relative_path_for_storage(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

pub fn set_workspace_active_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let mut config = load_config(root)?;
    config.active_bundle = Some(bundle_id.to_string());
    save_config(root, &config)
}

pub fn set_folder_active_bundle(root: &Path, path: &Path, bundle_id: &str) -> Result<()> {
    let contexts_path = root.join(".knit/contexts.json");
    let mut contexts = load_contexts(root)?;
    let stored_path = relative_path_for_storage(root, path);
    if let Some(entry) = contexts
        .contexts
        .iter_mut()
        .find(|entry| entry.path == stored_path)
    {
        entry.active_bundle = bundle_id.to_string();
    } else {
        contexts.contexts.push(crate::model::KnitContextEntry {
            path: stored_path,
            active_bundle: bundle_id.to_string(),
        });
    }
    write_json(&contexts_path, &contexts)
}

pub fn acquire_named_lock(root: &Path, name: &str) -> Result<KnitLock> {
    let dir = root.join(".knit/locks");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create Knit lock directory {}", dir.display()))?;
    let path = dir.join(format!("{name}.lock"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(_) => Ok(KnitLock { path }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "Another Knit process is updating this state. Remove {} only if you are sure no Knit process is running.",
                path.display()
            )
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to acquire Knit lock {}", path.display()))
        }
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("failed to serialize JSON")?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
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

    if let Some(bundle_id) = resolve_context_bundle(root, cwd)? {
        ensure_bundle_exists(root, &bundle_id)?;
        return Ok((bundle_id, BundleResolutionSource::Context));
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
    match bundle.state.as_deref() {
        Some(BUNDLE_STATE_ARCHIVED | BUNDLE_STATE_CLOSED | BUNDLE_STATE_DELETED) => return false,
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

fn resolve_context_bundle(root: &Path, cwd: &Path) -> Result<Option<String>> {
    let contexts = load_contexts(root)?;
    let mut best: Option<(usize, String)> = None;
    for entry in contexts.contexts {
        let path = resolve_stored_path(root, &entry.path);
        if cwd.starts_with(&path) {
            let score = path.components().count();
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score > *best_score)
            {
                best = Some((score, entry.active_bundle));
            }
        }
    }
    Ok(best.map(|(_, bundle_id)| bundle_id))
}

fn load_contexts(root: &Path) -> Result<KnitContexts> {
    let path = root.join(".knit/contexts.json");
    if path.exists() {
        read_json(&path)
    } else {
        Ok(KnitContexts::new())
    }
}

fn resolve_stored_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
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
