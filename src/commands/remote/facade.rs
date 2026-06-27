//! The single internal entry point for explicit artifact sync with KnitHub.
//!
//! `knit sync push` / `knit sync pull` (and the git-shaped `knit push --remote`,
//! `knit fetch --knit`, `knit pull --remote`, plus landing's automatic sync) all
//! funnel through this module. It owns selecting which artifact families to move
//! (bundles, history, views) and over which remotes, then delegates the actual
//! transport to the existing per-artifact helpers in the sibling submodules.
//!
//! There is one implementation; the several verbs are just differently shaped
//! doors into it.

use super::client::{effective_workspace_config, resolve_sync_remote_names};
use super::history::{pull_history_from_remote, push_history_to_remote};
use super::pull::pull_views_from_remote;
use super::push::{
    push_architecture_to_remote, push_bundle_to_remote, push_kg_graph_to_remote,
    push_views_to_remote,
};
use crate::output as out;
use anyhow::{bail, Result};

/// Which artifact families a `knit sync push`/`pull` should move. With no flags
/// passed on the CLI, the resolver below treats that as "everything".
#[derive(Clone, Copy, Debug)]
pub struct SyncTargets {
    pub bundles: bool,
    pub history: bool,
    pub views: bool,
    pub architecture: bool,
    pub kg: bool,
}

impl SyncTargets {
    /// Resolve the artifact selection from the CLI flags. Bare invocation (no
    /// `--bundles/--history/--views/--architecture/--kg/--all`) means everything;
    /// architecture/kg are no-op-with-note when no artifact has been produced.
    pub fn resolve(
        bundles: bool,
        history: bool,
        views: bool,
        architecture: bool,
        kg: bool,
        all: bool,
    ) -> Self {
        if all || !(bundles || history || views || architecture || kg) {
            SyncTargets {
                bundles: true,
                history: true,
                views: true,
                architecture: true,
                kg: true,
            }
        } else {
            SyncTargets {
                bundles,
                history,
                views,
                architecture,
                kg,
            }
        }
    }
}

/// Resolve the remote names this sync should hit. Explicit `--remote` overrides
/// win; otherwise fall back to configured sync remotes (`syncRemotes`,
/// `sync_remote`, then the sole configured remote).
fn resolve_remotes(remote_overrides: &[String]) -> Result<Vec<String>> {
    let (_, config) = effective_workspace_config()?;
    let remotes = resolve_sync_remote_names(&config, remote_overrides);
    if remotes.is_empty() {
        bail!(
            "No KnitHub remote configured. Run `knit remote add --global <name> <url>`, `knit remote add <name> <url>`, or `knit config set sync-remotes <name>[,<name>...]` first."
        );
    }
    Ok(remotes)
}

/// Push selected artifact families to KnitHub for the resolved project/bundle.
///
/// `knit sync push` and `knit sync push --bundles/--history/--views` route here,
/// as does `knit push --remote` (bundles only). Failures across remotes are
/// collected so one bad remote does not silently swallow the others.
pub fn sync_push(targets: SyncTargets, remote_overrides: &[String]) -> Result<()> {
    let remotes = resolve_remotes(remote_overrides)?;
    let multiple = remotes.len() > 1;
    let mut failures = Vec::new();

    for remote in &remotes {
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(remote));
        }
        // `push_bundle_to_remote` already pushes project + repos + bundle
        // artifact + project history in one call, so when both bundles and
        // history are requested we let it cover history and avoid a redundant
        // second history push to the same remote.
        if targets.bundles {
            if let Err(error) = push_bundle_to_remote(remote, None) {
                failures.push(format!("{remote} bundle: {error:#}"));
            }
        } else if targets.history {
            if let Err(error) = push_history_to_remote(None, remote) {
                failures.push(format!("{remote} history: {error:#}"));
            }
        }
        if targets.views {
            if let Err(error) = push_views_to_remote(None, remote) {
                failures.push(format!("{remote} views: {error:#}"));
            }
        }
        if targets.architecture {
            if let Err(error) = push_architecture_to_remote(None, remote) {
                failures.push(format!("{remote} architecture: {error:#}"));
            }
        }
        if targets.kg {
            if let Err(error) = push_kg_graph_to_remote(None, remote) {
                failures.push(format!("{remote} kg: {error:#}"));
            }
        }
    }

    finish(failures, "push")
}

/// Pull selected artifact families from KnitHub for the resolved project/bundle.
///
/// `knit sync pull` and `knit sync pull --bundles/--history/--views` route here,
/// as does `knit pull --remote`/`knit fetch --knit` (bundles only). Bundle pull
/// for the active bundle is delegated to the existing localize/refresh path in
/// `remote::pull`; this module does not reimplement that logic.
pub fn sync_pull(targets: SyncTargets, remote_overrides: &[String]) -> Result<()> {
    let remotes = resolve_remotes(remote_overrides)?;
    let multiple = remotes.len() > 1;
    let mut failures = Vec::new();

    for remote in &remotes {
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(remote));
        }
        if targets.bundles {
            // Reuse the existing active-bundle pull, which performs the
            // fast-forward localize/refresh that another change owns. Diverged
            // ledgers are reported, not merged; `knit pull --merge` is the
            // explicit door for that.
            if let Err(error) = super::pull::pull_remote_state(Some(remote), false, false) {
                failures.push(format!("{remote} bundle: {error:#}"));
            }
        }
        if targets.history {
            if let Err(error) = pull_history_from_remote(None, Some(remote)) {
                failures.push(format!("{remote} history: {error:#}"));
            }
        }
        if targets.views {
            if let Err(error) = pull_views_from_remote(None, remote) {
                failures.push(format!("{remote} views: {error:#}"));
            }
        }
    }

    finish(failures, "pull")
}

fn finish(failures: Vec<String>, verb: &str) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "KnitHub {verb} failed for {} target(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}
