//! Transport adapter for handoff. Keep remote credentials and export internals here.
use super::{client, clone, helpers, RemoteProjectExport};
use crate::model::{ChangeGroup, KnitProject, KnitRemote, LedgerRelation};
use crate::store::{self, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::{collections::BTreeSet, path::Path};

pub(crate) struct HandoffExport {
    pub unpublished: bool,
    pub remote_name: String,
    pub bundle: ChangeGroup,
    pub project: Option<KnitProject>,
    remote: KnitRemote,
    token: String,
    export: RemoteProjectExport,
    artifact_hash: String,
    remote_bundle_id: String,
}

impl HandoffExport {
    pub fn fetch(project: &str, slug: &str, allow_unpublished: bool) -> Result<Self> {
        let (name, remote, _stored, token) =
            clone::resolve_remote_for_clone_classified(None, None, None).map_err(|(_, e)| e)?;
        let token = token
            .context("A sync remote token is required. Configure it in global Knit config.")?;
        let global = store::load_global_config()?;
        let global_remote = global.remotes.get(&name).context("Handoff requires the sync remote in global config; run `knit remote add <name> <url> --global`.")?;
        if global_remote.url.trim_end_matches('/') != remote.url.trim_end_matches('/') {
            bail!("Workspace and global sync remote URLs differ; correct the remote before handing off.");
        }
        let _: serde_json::Value =
            client::request_json(&remote, &token, "GET", "/me/access-token", None)
                .context("Sync remote token validation failed")?;
        let export = client::fetch_project_export(&remote, Some(&token), project)?;
        let entry = export.bundles.iter().find(|b| b.slug == slug);
        let unpublished = entry.is_none();
        let (bundle, artifact_hash, remote_bundle_id) = if let Some(entry) = entry {
            if entry.lifecycle_state != "open" {
                bail!("Bundle `{slug}` is not open.");
            }
            let (bundle, hash) =
                client::resolve_export_bundle_payload(&remote, Some(&token), entry)?;
            (bundle, hash, entry.id.clone())
        } else if allow_unpublished {
            let mut bundle = ChangeGroup::new(slug.into(), slug.into(), crate::time::now_iso());
            bundle.project_id = Some(export.project.slug.clone());
            for record in &export.repositories {
                bundle.repos.push(serde_json::from_value(serde_json::json!({
                    "id": clone::export_repo_local_id(record), "path": "", "remote": record.remote_url,
                    "baseBranch": record.default_branch.as_deref().unwrap_or("main")
                }))?);
            }
            (bundle, String::new(), String::new())
        } else {
            bail!("Remote has no bundle `{slug}` in `{project}`; publish handoff out first.");
        };
        if !bundle.sync_targets.is_empty()
            && !bundle
                .sync_targets
                .iter()
                .any(|t| t.api_url.trim_end_matches('/') == remote.url.trim_end_matches('/'))
        {
            bail!("Bundle sync targets do not include this sync remote API.");
        }
        Ok(Self {
            unpublished,
            remote_name: name,
            bundle,
            project: export.knit_project.clone(),
            remote,
            token,
            export,
            artifact_hash,
            remote_bundle_id,
        })
    }

    /// Resolve transport on this machine, without persisting a rewritten origin URL.
    pub fn probe_repositories(&mut self, cwd: &Path) -> Vec<(String, Result<()>)> {
        let mut results = Vec::new();
        let hosts = helpers::connected_forge_hosts(&self.remote, &self.token).unwrap_or_default();
        for repo in &self.bundle.repos {
            let result = (|| {
                let record = self
                    .export
                    .repositories
                    .iter_mut()
                    .find(|r| clone::export_repo_local_id(r) == repo.id)
                    .with_context(|| {
                        format!(
                            "{} is missing from the export (check project access)",
                            repo.id
                        )
                    })?;
                let remote = record
                    .remote_url
                    .as_deref()
                    .context("Repository has no remote URL")?;
                // Install a temporary exact-host helper only on this Git invocation.
                // Probe must not mutate the user's global Git configuration.
                if reachable(cwd, remote, &self.remote_name, &hosts).is_ok() {
                    return Ok(());
                }
                if let Some(https) = prefer_https_url(remote, &hosts) {
                    reachable(cwd, &https, &self.remote_name, &hosts)?;
                    record.remote_url = Some(https);
                    return Ok(());
                }
                bail!("Cannot reach repository origin with this machine's Git credentials")
            })();
            results.push((repo.id.clone(), result));
        }
        results
    }

    pub fn materialize(mut self, root: &Path) -> Result<ActiveBundle> {
        let slug = self.bundle.id.clone();
        self.export.bundles.retain(|b| b.slug == slug);
        if !root.join(".knit/config.json").exists() {
            // Persist the bootstrap before cloning so a failed repository clone
            // resumes through the same missing-repository path on retry.
            if root.exists() && std::fs::read_dir(root)?.next().is_some() {
                bail!("Target directory is not empty");
            }
            for directory in [".knit/projects", ".knit/bundles", ".knit/worktrees"] {
                std::fs::create_dir_all(root.join(directory))?;
            }
            let mut project = self.project.clone().unwrap_or_else(|| {
                KnitProject::new(self.export.project.slug.clone(), crate::time::now_iso())
            });
            if self.project.is_none() {
                project.repos = self
                    .export
                    .repositories
                    .iter()
                    .map(|record| {
                        clone::project_repo_entry_from_export(
                            record,
                            &root.join(clone::export_repo_local_id(record)),
                        )
                    })
                    .collect();
            } else {
                for repo in &mut project.repos {
                    repo.path = root.join(&repo.id).to_string_lossy().into_owned();
                }
            }
            let mut config = crate::model::KnitConfig::new_project(project.id.clone());
            config.active_bundle = Some(slug.clone());
            config.sync_remote = Some(self.remote_name.clone());
            config.sync_remotes = vec![self.remote_name.clone()];
            // Tokens stay in the target user's global config.
            store::write_json(&store::project_path(root, &project.id), &project)?;
            store::save_config(root, &config)?;
        }
        {
            let config = store::load_effective_config(root)?;
            let project_id = config
                .active_project
                .context("Target workspace has no active project")?;
            let expected_id = self
                .project
                .as_ref()
                .map(|p| p.id.as_str())
                .unwrap_or(&self.export.project.slug);
            if project_id != expected_id {
                bail!(
                    "Target workspace belongs to project `{project_id}`, expected `{expected_id}`."
                );
            }
            let path = store::bundle_path(root, &slug);
            let _lock = store::acquire_named_lock(root, &slug)?;
            let local: Option<ChangeGroup> =
                path.exists().then(|| store::read_json(&path)).transpose()?;
            if let Some(local) = &local {
                match crate::model::ledger_relation(&local.node_id_sequence(), &self.bundle.node_id_sequence()) {
                    LedgerRelation::Diverged => bail!("Local and remote ledgers diverged; run `knit --bundle {slug} pull --merge` in {} and retry handoff in.", root.display()),
                    LedgerRelation::LocalAhead => {
                        // A previous acceptance may be saved but its artifact push failed.
                        self.bundle = local.clone();
                    }
                    _ => {}
                }
            }
            let mut project: KnitProject =
                store::read_json(&store::project_path(root, &project_id))?;
            helpers::ensure_helpers_for_git(&self.remote_name);
            ensure_bundle_repositories(root, &mut project, &self.export, &self.bundle)?;
            let mut localized = client::localize_bundle(self.bundle, &project)?;
            if let Some(local) = local {
                for repo in &mut localized.repos {
                    repo.worktree_path = local
                        .repos
                        .iter()
                        .find(|r| r.id == repo.id)
                        .and_then(|r| r.worktree_path.clone());
                }
            }
            localized.record_sync_target_with_artifact(
                &self.remote_name,
                &self.remote_bundle_id,
                &self.remote.url,
                Some(&self.artifact_hash),
            );
            store::write_json(&path, &localized)?;
            drop(_lock);
            helpers::ensure_helpers_for_git(&self.remote_name);
            clone::materialize_imported_bundle(root, &slug)?;
        }
        let path = store::bundle_path(root, &slug);
        Ok(ActiveBundle::unlocked(
            root.to_path_buf(),
            path.clone(),
            store::read_json(&path)?,
        ))
    }
}

/// Clone only missing bundle repositories. Existing checkouts retain their branches.
pub(super) fn ensure_bundle_repositories(
    root: &Path,
    project: &mut KnitProject,
    export: &RemoteProjectExport,
    bundle: &ChangeGroup,
) -> Result<()> {
    for repo in &bundle.repos {
        if project
            .repos
            .iter()
            .any(|r| r.id == repo.id && Path::new(&r.path).exists())
        {
            continue;
        }
        let record = export
            .repositories
            .iter()
            .find(|r| clone::export_repo_local_id(r) == repo.id)
            .with_context(|| {
                format!(
                    "{} is missing from the export; check project access",
                    repo.id
                )
            })?;
        let (_, path) = clone::clone_one_export_repository(root, record)?;
        let entry = clone::project_repo_entry_from_export(record, &path);
        project.repos.retain(|r| r.id != repo.id);
        project.repos.push(entry);
        store::write_json(&store::project_path(root, &project.id), project)?;
    }
    Ok(())
}

pub(crate) fn prefer_https_url(remote: &str, hosts: &BTreeSet<String>) -> Option<String> {
    let (host, path) = if let Some(rest) = remote.strip_prefix("git@") {
        rest.split_once(':')?
    } else {
        return None;
    };
    if !hosts.contains(host)
        || path.is_empty()
        || path.starts_with('/')
        || path.contains(['?', '#', '\\'])
        || path.split('/').any(|s| s == "..")
    {
        return None;
    }
    Some(format!("https://{host}/{path}"))
}

pub(super) fn reachable(
    cwd: &Path,
    remote: &str,
    name: &str,
    hosts: &BTreeSet<String>,
) -> Result<()> {
    let mut args = Vec::<String>::new();
    for host in hosts {
        args.extend([
            "-c".into(),
            format!(
                "credential.https://{host}.helper={}",
                helpers::helper_command(name)?
            ),
        ]);
    }
    args.extend(["ls-remote".into(), "--".into(), remote.into()]);
    crate::commands::handoff::probe::command_output("git", &args, cwd).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn https_rewrite_is_exact_host_and_path_scoped() {
        let hosts = BTreeSet::from(["github.com".into()]);
        assert_eq!(
            prefer_https_url("git@github.com:o/r.git", &hosts).as_deref(),
            Some("https://github.com/o/r.git")
        );
        assert!(prefer_https_url("git@evil.github.com:o/r", &hosts).is_none());
        assert!(prefer_https_url("git@github.com:../r", &hosts).is_none());
        assert!(prefer_https_url("https://github.com/o/r", &hosts).is_none());
    }
}

/// Compare equivalent SSH/HTTPS forge forms without relaxing the repository path.
pub(crate) fn same_repository_url(left: &str, right: &str) -> bool {
    fn identity(value: &str) -> String {
        let value = value.trim_end_matches('/').trim_end_matches(".git");
        if let Some(rest) = value.strip_prefix("git@") {
            if let Some((host, path)) = rest.split_once(':') {
                return format!("{}/{}", host.to_ascii_lowercase(), path);
            }
        }
        if let Ok(url) = url::Url::parse(value) {
            if let Some(host) = url.host_str() {
                return format!(
                    "{}{}{}",
                    host.to_ascii_lowercase(),
                    url.port().map(|p| format!(":{p}")).unwrap_or_default(),
                    url.path()
                );
            }
        }
        value.to_string()
    }
    identity(left) == identity(right)
}
