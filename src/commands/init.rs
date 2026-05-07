use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig};
use crate::output as out;
use crate::store::{find_knit_root, write_json};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

pub fn init_bundle(title: &str, force: bool, agents: bool) -> Result<()> {
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

    println!(
        "{} {}",
        out::heading("Active bundle:"),
        out::path(bundle_path.display())
    );

    if agents {
        let agents_path = write_agents_md(&root)?;
        println!(
            "{} {}",
            out::heading("AGENTS.md:"),
            out::path(agents_path.display())
        );
    }

    Ok(())
}

fn write_agents_md(root: &Path) -> Result<std::path::PathBuf> {
    let path = root.join("AGENTS.md");
    if path.exists() {
        return Ok(path);
    }

    fs::write(&path, agents_md())
        .with_context(|| format!("failed to write Knit agent tutorial at {}", path.display()))?;
    Ok(path)
}

fn agents_md() -> &'static str {
    r#"# AGENTS.md

This is a Knit workspace. Knit coordinates feature work that spans one or more Git repositories and records the work in `.knit/bundles/<slug>.bundle.json`.

## Knit Workflow

Start by checking the active bundle:

```sh
knit status
knit log
```

Track local repositories when a bundle is new:

```sh
knit track ../backend ../frontend ../scraper
```

Make code changes inside Knit checkouts, usually under:

```txt
.knit/worktrees/<bundle>/<repo>/
```

Inspect, stage, and commit cross-repo work:

```sh
knit diff
knit add
knit commit -m "Describe the feature change"
```

For a one-step stage and commit:

```sh
knit commit --stage -m "Describe the feature change"
```

## Useful Commands

- `knit bundle path` prints the active bundle file.
- `knit bundle validate` checks the bundle artifact.
- `knit show HEAD` explains the latest bundle ledger entry.
- `knit sync` records Git commits made outside Knit.
- `knit git --all status --short` runs Git across tracked checkouts.
- `knit checkpoint "note"` records non-Git progress in the bundle ledger.
- `knit close --reason "merged"` marks the bundle closed without deleting branches or worktrees.

## Knit And Gloss

Knit owns authoring: worktrees, feature branches, commits, sync, reverts, and the bundle ledger. Gloss reads Knit bundles later to prepare review plans, explanations, and UI views.

When using Gloss from this workspace, the active Knit bundle can usually be discovered automatically:

```sh
gloss prepare
gloss view
```
"#
}
