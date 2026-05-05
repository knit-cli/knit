use crate::model::{ChangeGroup, KnitConfig};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

pub struct ActiveBundle {
    pub root: PathBuf,
    pub bundle_path: PathBuf,
    pub bundle: ChangeGroup,
    _lock: Option<BundleLock>,
}

struct BundleLock {
    path: PathBuf,
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
        .context("No active Knit bundle found. Run `knit init \"feature title\"` first.")?;
    let lock = if lock_for_update {
        Some(acquire_bundle_lock(&root)?)
    } else {
        None
    };
    let config_path = root.join(".knit/config.json");
    let config: KnitConfig = read_json(&config_path)?;
    let bundle_path = root
        .join(".knit/bundles")
        .join(format!("{}.bundle.json", config.active_bundle));
    let bundle: ChangeGroup = read_json(&bundle_path)?;

    Ok(ActiveBundle {
        root,
        bundle_path,
        bundle,
        _lock: lock,
    })
}

pub fn save_active_bundle(active: &ActiveBundle) -> Result<()> {
    write_json(&active.bundle_path, &active.bundle)
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

fn acquire_bundle_lock(root: &Path) -> Result<BundleLock> {
    let path = root.join(".knit/knit.lock");
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(_) => Ok(BundleLock { path }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "Another Knit process is updating this bundle. Remove {} only if you are sure no Knit process is running.",
                path.display()
            )
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to acquire Knit lock {}", path.display()))
        }
    }
}

impl Drop for BundleLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
