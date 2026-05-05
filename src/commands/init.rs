use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig};
use crate::store::{find_knit_root, write_json};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::fs;

pub fn init_bundle(title: &str, force: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let existing_root = find_knit_root(&cwd);

    if existing_root.is_some() && !force {
        bail!(
            "An active Knit bundle already exists here. Use --force to replace the active bundle."
        );
    }

    let root = existing_root.unwrap_or(cwd);
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let bundle_path = bundle_dir.join(format!("{bundle_id}.bundle.json"));

    if bundle_path.exists() && !force {
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            bundle_path.display()
        );
    }

    fs::create_dir_all(&bundle_dir).context("failed to create .knit/bundles")?;
    fs::create_dir_all(&worktree_dir).context("failed to create .knit/worktrees")?;

    let bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now_iso());
    write_json(&bundle_path, &bundle)?;

    let config = KnitConfig::new(bundle_id);
    write_json(&knit_dir.join("config.json"), &config)?;

    println!("Active bundle: {}", bundle_path.display());
    Ok(())
}
