use crate::model::{ChangeGroup, KnitConfig};
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ActiveBundle {
    pub root: PathBuf,
    pub bundle_path: PathBuf,
    pub bundle: ChangeGroup,
}

pub fn load_active_bundle() -> Result<ActiveBundle> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd)
        .context("No active Knit bundle found. Run `knit init \"feature title\"` first.")?;
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
