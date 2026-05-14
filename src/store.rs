use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitAgentContext, KnitConfig, KnitContexts};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

static BUNDLE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);
static AGENT_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

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
    Agent,
    Context,
    Config,
}

impl BundleResolutionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Env => "env",
            Self::Worktree => "cwd",
            Self::Agent => "agent",
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

pub fn set_agent_override(agent_id: Option<String>) {
    *AGENT_OVERRIDE.lock().expect("agent override lock poisoned") = agent_id;
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
        .context("No Knit workspace found. Run `knit bundle start \"feature title\"` first.")?;
    let config = load_config(&root)?;
    let (bundle_id, resolution_source) = resolve_bundle_id(&root, &cwd, &config)?;
    let bundle_path = root
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let lock = if lock_for_update {
        Some(acquire_bundle_lock(&root, &bundle_id)?)
    } else {
        None
    };
    let bundle: ChangeGroup = read_json(&bundle_path)?;

    Ok(ActiveBundle {
        root,
        bundle_path,
        bundle,
        resolution_source,
        _lock: lock,
    })
}

pub fn save_active_bundle(active: &ActiveBundle) -> Result<()> {
    write_json(&active.bundle_path, &active.bundle)
}

pub fn load_config(root: &Path) -> Result<KnitConfig> {
    read_json(&root.join(".knit/config.json"))
}

pub fn save_config(root: &Path, config: &KnitConfig) -> Result<()> {
    write_json(&root.join(".knit/config.json"), config)
}

pub fn bundle_path(root: &Path, bundle_id: &str) -> PathBuf {
    root.join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"))
}

pub fn project_path(root: &Path, project_id: &str) -> PathBuf {
    root.join(".knit/projects")
        .join(format!("{project_id}.project.json"))
}

pub fn agent_path(root: &Path, agent_id: &str) -> PathBuf {
    root.join(".knit/agents")
        .join(format!("{}.agent.json", slugify(agent_id)))
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

pub fn current_agent_id() -> Option<String> {
    if let Some(agent_id) = AGENT_OVERRIDE
        .lock()
        .expect("agent override lock poisoned")
        .clone()
    {
        return normalize_agent_id(&agent_id);
    }

    std::env::var("KNIT_AGENT")
        .ok()
        .and_then(|value| normalize_agent_id(&value))
        .or_else(|| {
            std::env::var("CODEX_THREAD_ID")
                .ok()
                .and_then(|value| normalize_agent_id(&value))
        })
}

pub fn set_agent_active_bundle(root: &Path, agent_id: &str, bundle_id: &str) -> Result<String> {
    let agent_id = slugify(agent_id);
    let path = agent_path(root, &agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create Knit agent directory {}", parent.display())
        })?;
    }
    let context = KnitAgentContext::new(agent_id.clone(), bundle_id.to_string(), now_iso());
    write_json(&path, &context)?;
    Ok(agent_id)
}

pub fn clear_agent_active_bundle(root: &Path, agent_id: &str) -> Result<()> {
    let path = agent_path(root, agent_id);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove Knit agent context {}", path.display()))?;
    }
    Ok(())
}

pub fn clear_agent_contexts_for_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let dir = root.join(".knit/agents");
    if !dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let context: KnitAgentContext = read_json(&path)?;
        if context.active_bundle == bundle_id {
            fs::remove_file(&path).with_context(|| {
                format!("failed to remove Knit agent context {}", path.display())
            })?;
        }
    }

    Ok(())
}

pub fn load_agent_context(root: &Path, agent_id: &str) -> Result<Option<KnitAgentContext>> {
    let path = agent_path(root, agent_id);
    if path.exists() {
        Ok(Some(read_json(&path)?))
    } else {
        Ok(None)
    }
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

    if let Some(agent_id) = current_agent_id() {
        if let Some(context) = load_agent_context(root, &agent_id)? {
            ensure_bundle_exists(root, &context.active_bundle)?;
            return Ok((context.active_bundle, BundleResolutionSource::Agent));
        }
    }

    if let Some(bundle_id) = resolve_context_bundle(root, cwd)? {
        ensure_bundle_exists(root, &bundle_id)?;
        return Ok((bundle_id, BundleResolutionSource::Context));
    }

    if let Some(bundle_id) = &config.active_bundle {
        ensure_bundle_exists(root, bundle_id)?;
        return Ok((bundle_id.clone(), BundleResolutionSource::Config));
    }

    bail!("No active Knit bundle found. Run `knit bundle start \"feature title\"` first.")
}

fn normalize_agent_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(slugify(value))
    }
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
