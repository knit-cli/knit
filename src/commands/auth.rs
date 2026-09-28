//! CLI-owned forge setup; no hosted account or desktop application is required.
use crate::auth::{self, CredentialSpec};
use crate::cli::AuthCommand;
use crate::model::{KnitProject, ProjectAuthGroup, ProjectRepoEntry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

/// `knit auth` with no subcommand is the primary credential flow. `None`
/// runs the personal default-token wizard (no project needed, any cwd);
/// `--project NAME` runs the project-scoped wizard. Advanced subcommands
/// keep their existing behavior and reject the parent `--project` flag.
pub fn dispatch(command: Option<AuthCommand>, project: Option<&str>) -> Result<()> {
    match (command, project) {
        (Some(command), None) => run(command),
        (None, Some(project)) => entry(Some(project)),
        (None, None) => entry(None),
        (Some(_), Some(_)) => bail!(
            "`--project` belongs to the bare `knit auth` wizard; the advanced subcommands take their own flags where supported"
        ),
    }
}

/// The primary `knit auth` wizard. Global mode manages one default token per
/// forge — used for everything on that forge unless a project overrides it.
/// Project mode decides, per forge of one project, between the shared
/// default token and a project-only token.
pub fn entry(project: Option<&str>) -> Result<()> {
    require_terminal()?;
    match project {
        None => global_defaults_wizard(),
        Some(name) => project_wizard(name),
    }
}

fn read_hidden(message: &str) -> Result<String> {
    rpassword::prompt_password(message).context("Could not read token")
}

/// The four supported forges, in menu order: (menu label, provider, host).
const WIZARD_FORGES: [(&str, &str, &str); 4] = [
    ("GitHub", "github", "github.com"),
    ("GitLab", "gitlab", "gitlab.com"),
    ("Bitbucket", "bitbucket", "bitbucket.org"),
    ("Forgejo (Codeberg)", "forgejo", "codeberg.org"),
];

fn describe_default(store: &auth::AuthStore, host: &str) -> String {
    match store.default_for_host(host) {
        Some((name, auth::DefaultSource::Chosen)) => format!("default token `{name}`"),
        Some((name, auth::DefaultSource::Inherited)) => {
            format!("default token `{name}` (the only token saved for this forge)")
        }
        None => "no default token".to_string(),
    }
}

/// The global default-token wizard: pick a forge, paste one hidden token,
/// done. Existing defaults are shown and kept unless deliberately updated;
/// the first token saved for a forge becomes its default automatically. `r`
/// opens the separate sync-remote token flow — a hosted-service credential,
/// never interchangeable with the forge tokens managed here.
fn global_defaults_wizard() -> Result<()> {
    println!(
        "Personal forge tokens — one default token per forge, used for every project, clone, fetch, and push on that forge unless a project overrides it."
    );
    println!(
        "Sync remote tokens (hosted Knit service) are separate: choose `r` below, or run `knit auth remote <name>`."
    );
    loop {
        let store = auth::load()?;
        println!("\nCurrent default tokens:");
        let hosts: Vec<&str> = store
            .credentials
            .values()
            .map(|spec| spec.host.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if hosts.is_empty() {
            println!("  (none yet)");
        }
        for host in &hosts {
            println!("  {host}: {}", describe_default(&store, host));
        }
        println!("\nAdd or update a token:");
        for (i, (label, _, host)) in WIZARD_FORGES.iter().enumerate() {
            println!("  {}. {label} ({host})", i + 1);
        }
        println!("  r. Sync remote token (hosted Knit service)");
        println!("  Enter when done");
        let choice = prompt("Choice (1-4 for Git hosting, r for sync, Enter to finish): ")?;
        if choice.is_empty() {
            // Activating inside a project: plain Git picks up the saved
            // tokens here, without a separate `knit auth status` run.
            refresh_plain_git_helper_for_cwd()
                .context("Tokens were saved, but the plain-Git helper could not be refreshed")?;
            println!("Done. Tokens are saved in your personal Knit store; nothing was synced.");
            return Ok(());
        }
        if matches!(choice.as_str(), "r" | "R") {
            // The same flow `knit auth remote` runs, on the user-level config
            // only. A failure there (rejected token, unreachable service,
            // cancellation) leaves this wizard's forge state untouched, so
            // stay in the loop afterwards.
            if let Err(error) = crate::commands::remote::auth_remote(None, None, false, false) {
                println!("Sync remote token was not saved: {error:#}");
            }
            continue;
        }
        let Some((_, provider, host)) = choice
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| WIZARD_FORGES.get(i))
        else {
            println!("Choose one of the listed forges, or press Enter to finish.");
            continue;
        };
        let store = auth::load()?;
        match store.default_for_host(host) {
            Some((name, _)) => {
                println!("{host}: {}", describe_default(&store, host));
                let answer = prompt("Enter to keep it, or `t` to paste a replacement token: ")?;
                if !matches!(answer.trim(), "t" | "T") {
                    continue;
                }
                println!("{}", permission_help(provider));
                let token = read_hidden(&format!(
                    "New token for {host} (input hidden, press Enter to submit): "
                ))?;
                let token = token.trim();
                if token.is_empty() {
                    println!("Kept the current token.");
                    continue;
                }
                // A replacement can be a different Bitbucket token kind.
                // Classify the new secret before saving it, rather than
                // retaining the previous token's authentication scheme.
                let bitbucket_identity = if provider == &"bitbucket" {
                    let kind = ask_bitbucket_token_type(&mut prompt)?;
                    let username = bitbucket_username_for_token_type(&kind, &mut prompt)?;
                    Some((kind, username))
                } else {
                    None
                };
                // A deliberate update rotates the default token in place:
                // same name, same default, new secret. A pasted replacement
                // always becomes a local secret, so an environment reference
                // is cleared first — the two token sources never compete.
                let _lock = auth::lock()?;
                let mut store = auth::load()?;
                if let Some(spec) = store.credentials.get_mut(&name) {
                    spec.token_env = None;
                    if let Some((kind, username)) = bitbucket_identity {
                        spec.token_type = Some(kind);
                        spec.username = username;
                    }
                }
                store.scoped_credentials.remove(&name);
                auth::save_credential(&store, &name, Some(token))?;
                println!("Updated the token on `{name}`; it stays the default for {host}.");
            }
            None => {
                println!("{}", permission_help(provider));
                let token_type = if provider == &"bitbucket" {
                    Some(ask_bitbucket_token_type(&mut prompt)?)
                } else {
                    None
                };
                let username = match &token_type {
                    Some(kind) => bitbucket_username_for_token_type(kind, &mut prompt)?,
                    None => None,
                };
                let token = read_hidden(&format!(
                    "Token for {host} (input hidden, press Enter to submit): "
                ))?;
                let token = token.trim();
                if token.is_empty() {
                    println!("Skipped {host}.");
                    continue;
                }
                let spec = CredentialSpec {
                    provider: provider.to_string(),
                    host: host.to_string(),
                    username,
                    token_type,
                    token_env: None,
                };
                let name = unique_credential_name(host)?;
                save_new_credential(&name, &spec, token)?;
                {
                    // Choosing a forge in this wizard is an explicit default
                    // selection: it applies even when several legacy tokens
                    // already share the host (where an implicit default
                    // would be ambiguous and refused).
                    let _lock = auth::lock()?;
                    let mut store = auth::load()?;
                    store.defaults.insert(host.to_string(), name.clone());
                    store.scoped_credentials.remove(&name);
                    auth::save(&store)?;
                }
                println!("`{name}` is now the default token for {host}.");
            }
        }
    }
}

fn project_wizard(project_name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, project) = auth::project_context(&cwd, Some(project_name))?;
    setup_with_prompt(&root, &project, &[], &mut prompt, &mut read_hidden)
}

/// Explicit project setup edits only the selected repositories. Requirements
/// supply group boundaries and token guidance; ungrouped repos share a host.
fn setup_with_prompt(
    root: &Path,
    project: &KnitProject,
    ids: &[String],
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let pending = auth::load_known_pending_repos(root, &project.id);
    auth::validate_project_auth_with_pending(project, &pending)
        .context("Cannot run grouped auth setup")?;
    let editable: BTreeSet<_> = selected_repos(project, ids)?
        .iter()
        .map(|r| r.id.clone())
        .collect();
    let key = auth::project_key(root, &project.id)?;
    let mut groups = project
        .auth
        .as_ref()
        .map(|a| a.groups.clone())
        .unwrap_or_default();
    let declared: BTreeSet<_> = groups
        .iter()
        .flat_map(|g| g.repos.iter().cloned())
        .collect();
    let ungrouped: Vec<_> = project
        .repos
        .iter()
        .filter(|r| editable.contains(&r.id) && !declared.contains(&r.id))
        .filter_map(|r| {
            r.remote
                .as_ref()
                .map(|remote| (r.id.clone(), remote.clone()))
        })
        .collect();
    groups.extend(
        inferred_groups(&ungrouped, ask)?
            .into_iter()
            .map(|mut group| {
                group.id = project.id.clone();
                group
            }),
    );
    println!(
        "Project `{}` — per forge, use the shared default token or give this project its own.",
        project.id
    );
    for group in groups {
        let repos: Vec<_> = group
            .repos
            .iter()
            .filter(|r| editable.contains(*r))
            .cloned()
            .collect();
        if repos.is_empty() {
            println!(
                "Skipping group `{}`: no repositories in the selected workspace scope.",
                group.id
            );
            continue;
        }
        describe_group(&group, &[]);
        let host = &group.host;
        let store = auth::load()?;
        let default = store.default_for_host(host);
        println!(
            "{host} ({}): {}",
            repos.join(", "),
            describe_default(&store, host)
        );
        if let Some(bindings) = store.projects.get(&key) {
            for repo in &repos {
                if let Some(name) = bindings.get(repo) {
                    println!("  {repo}: project token `{name}`");
                }
            }
        }
        loop {
            let answer = ask(
                "Use the default token (Enter), `t` for a project-only token, or `s` to skip: ",
            )?;
            match answer.trim() {
                "" => {
                    if default.is_none() {
                        println!("No default token for {host}; run `knit auth` to add one, or choose `t`.");
                        continue;
                    }
                    let _lock = auth::lock()?;
                    let mut store = auth::load()?;
                    if let Some(bindings) = store.projects.get_mut(&key) {
                        bindings.retain(|repo, _| !repos.contains(repo));
                        if bindings.is_empty() {
                            store.projects.remove(&key);
                        }
                    }
                    auth::save(&store)?;
                    println!("Using the default token for {host}.");
                }
                "t" | "T" => {
                    if let Some(name) = create_group_credential(&group, true, ask, read_token)? {
                        assign_in(root, project, &repos, &name)?;
                        println!("`{name}` is used for {} in this project only; the default token for {host} is untouched.", repos.join(", "));
                    }
                }
                "s" | "S" => (),
                _ => {
                    println!("Choose Enter, t, or s.");
                    continue;
                }
            }
            break;
        }
    }
    show_setup_mapping(root, project, ids)?;
    // The wizard saved credential state above; a helper-install failure must
    // not roll that back, but it must not look like success either.
    refresh_plain_git_helper(root, project)
        .context("Credential choices were saved, but the plain-Git helper could not be set up")?;
    println!("Done.");
    Ok(())
}

pub fn run(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Remote {
            name,
            url,
            token_stdin,
            offline,
        } => crate::commands::remote::auth_remote(
            name.as_deref(),
            url.as_deref(),
            token_stdin,
            offline,
        ),
        AuthCommand::Setup { project, repos } => setup(project.as_deref(), &repos),
        AuthCommand::Add {
            name,
            provider,
            host,
            username,
            token_type,
            token_env,
            token_stdin,
            replace,
        } => {
            let spec = CredentialSpec {
                host: host.unwrap_or_else(|| default_host(&provider).into()),
                provider,
                username,
                token_type,
                token_env,
            };
            add(&name, spec, token_stdin, replace)
        }
        AuthCommand::Default { name } => set_default(&name),
        AuthCommand::Use {
            name,
            project,
            repos,
        } => assign(&name, project.as_deref(), &repos),
        AuthCommand::Status {
            project,
            check,
            json,
        } => status(project.as_deref(), check, json),
        AuthCommand::List => {
            let store = auth::load()?;
            for (name, spec) in &store.credentials {
                let count = store
                    .projects
                    .values()
                    .flat_map(|p| p.values())
                    .filter(|v| *v == name)
                    .count();
                let default_note = match store.default_for_host(&spec.host) {
                    Some((default, source)) if &default == name => match source {
                        auth::DefaultSource::Chosen => {
                            format!("\tdefault for {}", spec.host)
                        }
                        auth::DefaultSource::Inherited => {
                            format!("\tdefault for {} (only credential on host)", spec.host)
                        }
                    },
                    _ => String::new(),
                };
                println!(
                    "{name}\t{}\t{}\t{count} repository assignments\t{}{default_note}",
                    spec.provider,
                    spec.host,
                    if spec.token_env.is_some() {
                        "environment reference"
                    } else {
                        "private token file"
                    }
                );
            }
            if store.credentials.is_empty() {
                println!("No saved credentials. Run `knit auth setup` in a project.");
            }
            Ok(())
        }
        AuthCommand::Remove { name } => {
            let _lock = auth::lock()?;
            let mut store = auth::load()?;
            if store
                .projects
                .values()
                .any(|p| p.values().any(|v| v == &name))
            {
                bail!("Credential `{name}` is assigned to repositories. Reassign them with `knit auth use`, or clear their assignments first.");
            }
            if store.credentials.remove(&name).is_none() {
                bail!("Unknown credential `{name}`.");
            }
            // A removed default leaves the host without one until the next
            // first-add or explicit choice; its scoping marker goes with it.
            store.defaults.retain(|_, default| default != &name);
            store.scoped_credentials.remove(&name);
            auth::save(&store)?;
            auth::remove_token(&name)?;
            refresh_plain_git_helper_for_cwd().context(
                "The credential was removed, but the plain-Git helper could not be refreshed",
            )?;
            println!("Removed credential `{name}` from this machine.");
            Ok(())
        }
        AuthCommand::Clear { project, repos } => {
            let cwd = std::env::current_dir()?;
            let (root, project) = auth::project_context(&cwd, project.as_deref())?;
            selected_repos(&project, &repos)?;
            let key = auth::project_key(&root, &project.id)?;
            let _lock = auth::lock()?;
            let mut store = auth::load()?;
            if repos.is_empty() {
                store.projects.remove(&key);
            } else if let Some(bindings) = store.projects.get_mut(&key) {
                for repo in &repos {
                    bindings.remove(repo);
                }
                if bindings.is_empty() {
                    store.projects.remove(&key);
                }
            }
            let explicit = store.projects.contains_key(&key);
            auth::save(&store)?;
            // With no credential resolving anymore, generated plain-Git entries are
            // removed and the checkouts return to inherited Git behavior. The saved
            // clearing stays in place even if the uninstall reports trouble.
            refresh_plain_git_helper(&root, &project).context(
        "Credential assignments were cleared, but the plain-Git helper could not be refreshed",
    )?;
            println!(
                "Cleared assignments for {}. {}",
                project.id,
                if explicit {
                    "Unassigned repositories will require setup."
                } else {
                    "The project now uses existing Git/forge authentication."
                }
            );
            Ok(())
        }
        AuthCommand::GitCredential {
            credential,
            host,
            path,
            resolve,
            operation,
            workspace,
            project,
        } => {
            let context = match (workspace, project) {
                (None, None) => None,
                (Some(workspace), Some(project)) => Some((workspace, project)),
                _ => bail!("--workspace and --project must be passed together"),
            };
            if !resolve && context.is_some() {
                bail!("--workspace and --project apply only to the dynamic --resolve helper");
            }
            if resolve {
                crate::auth_git::resolve_helper(operation, context)
            } else {
                let (credential, host, path) = match (credential, host, path) {
                    (Some(credential), Some(host), Some(path)) => (credential, host, path),
                    _ => bail!(
                        "fixed-selection helper mode requires --credential, --host, and --path"
                    ),
                };
                crate::auth_git::credential_helper(&credential, &host, &path, operation)
            }
        }
    }
}

/// Refresh the plain-Git credential helper for one project's source checkouts
/// plus its materialized bundle worktrees in this workspace (never another
/// project's). `active` counts checkouts carrying Knit policy, `cleared` only
/// ones whose generated entries were just removed; installation errors are
/// collected into one error noting saved credentials are unaffected.
pub(crate) fn refresh_plain_git_helper(
    root: &Path,
    project: &KnitProject,
) -> Result<(usize, usize)> {
    let mut checkouts: Vec<(String, std::path::PathBuf)> = Vec::new();
    for repo in &project.repos {
        let path = Path::new(&repo.path);
        checkouts.push((
            repo.id.clone(),
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            },
        ));
    }
    let bundles_dir = root.join(".knit/bundles");
    if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(bundle) = crate::store::read_json::<crate::model::ChangeGroup>(&path) else {
                continue;
            };
            if bundle.project_id.as_deref() != Some(project.id.as_str()) {
                continue;
            }
            for repo in &bundle.repos {
                let Some(worktree) = &repo.worktree_path else {
                    continue;
                };
                let path = Path::new(worktree);
                checkouts.push((
                    format!("{}/{}", bundle.id, repo.id),
                    if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        root.join(path)
                    },
                ));
            }
        }
    }
    checkouts.sort_by(|a, b| a.1.cmp(&b.1));
    checkouts.dedup_by(|a, b| a.1 == b.1);
    let mut active = 0;
    let mut cleared = 0;
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for (name, path) in &checkouts {
        if !path.is_dir() {
            continue;
        }
        match crate::auth_git::install_for_project(path, root, project) {
            Ok(crate::auth_git::InstallOutcome::Active { .. }) => active += 1,
            Ok(crate::auth_git::InstallOutcome::Cleared { changed: true }) => cleared += 1,
            Ok(crate::auth_git::InstallOutcome::Cleared { changed: false }) => {}
            Err(error) => failures.push((name.clone(), error)),
        }
    }
    if failures.is_empty() {
        return Ok((active, cleared));
    }
    let detail = failures
        .iter()
        .map(|(repo, error)| format!("  {repo}: {error:#}"))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "plain-Git credential helper setup failed for {} checkout(s):\n{detail}\nSaved credentials are unaffected; run `knit auth status` in this project to repair the plain-Git integration.",
        failures.len()
    );
}

/// Refresh the plain-Git helper for the project context the process already
/// resolves from `cwd`, when there is one. Used by credential mutations that
/// do not carry an explicit project (`auth add`, `auth default`, `auth remove`,
/// the global wizard's done path).
fn refresh_plain_git_helper_for_cwd() -> Result<()> {
    let Ok(cwd) = std::env::current_dir() else {
        return Ok(());
    };
    if let Ok((root, project)) = auth::project_context(&cwd, None) {
        refresh_plain_git_helper(&root, &project)?;
    }
    Ok(())
}

fn default_host(provider: &str) -> &str {
    match provider {
        "github" => "github.com",
        "gitlab" => "gitlab.com",
        "bitbucket" => "bitbucket.org",
        _ => "codeberg.org",
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 100
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        bail!("Credential names may contain letters, numbers, hyphens, underscores, and dots (1–100 characters).");
    }
    Ok(())
}

/// Pin the host's current *implicit* default — its single credential — as
/// the explicit default before another token joins the host: a legacy
/// one-token-per-forge default is never lost or displaced by a second add.
/// Called before the new credential is inserted, with the lock held.
fn pin_legacy_default(store: &mut auth::AuthStore, host: &str) {
    if let Some((current, auth::DefaultSource::Inherited)) = store.default_for_host(host) {
        store.defaults.insert(host.to_ascii_lowercase(), current);
    }
}

fn add(name: &str, spec: CredentialSpec, token_stdin: bool, replace: bool) -> Result<()> {
    validate_name(name)?;
    auth::validate_spec(&spec)?;
    let mut store = auth::load()?;
    if store.credentials.contains_key(name) && !replace {
        bail!("Credential `{name}` exists. Use --replace to rotate it for every assigned project.");
    }
    if let Some(old) = store.credentials.get(name) {
        if !old.host.eq_ignore_ascii_case(&spec.host) || old.provider != spec.provider {
            bail!("A replacement must keep the existing provider and host. Add a different name for a different forge.");
        }
    }
    let token = if spec.token_env.is_none() {
        // Both entry paths share the common token reader: `--token-stdin` at
        // a terminal is a hidden prompt that submits on Enter (not a
        // read-to-EOF that would wait on Ctrl-D), while piped stdin keeps
        // the bounded EOF read for secret managers.
        let token = if token_stdin {
            crate::token_entry::read_token(
                &format!("Token for `{name}` (input hidden, press Enter to submit): "),
                "forge token",
            )?
        } else {
            require_terminal()?;
            println!("{}", permission_help(&spec.provider));
            crate::token_entry::read_token(
                "Token (input hidden, press Enter to submit): ",
                "forge token",
            )?
        };
        let token = token.trim();
        if token.is_empty() || token.chars().any(char::is_control) {
            bail!("Token must be nonempty and on one line.");
        }
        Some(token.to_owned())
    } else {
        None
    };
    let _lock = auth::lock()?;
    store = auth::load()?;
    if let Some(old) = store.credentials.get(name) {
        if !replace {
            bail!("Credential `{name}` exists. Use --replace to rotate it.");
        }
        if !old.host.eq_ignore_ascii_case(&spec.host) || old.provider != spec.provider {
            bail!("A replacement must keep the existing provider and host.");
        }
    }
    let is_env = spec.token_env.is_some();
    let host = spec.host.to_ascii_lowercase();
    pin_legacy_default(&mut store, &host);
    store.credentials.insert(name.into(), spec);
    // The first token on a host becomes that host's default automatically;
    // adding further tokens never displaces the chosen default.
    let becomes_default = !replace && auth::stage_default_if_absent(&mut store, &host, name);
    auth::save_credential(&store, name, token.as_deref())?;
    refresh_plain_git_helper_for_cwd()
        .context("The credential was saved, but the plain-Git helper could not be refreshed")?;
    println!("Saved `{name}` in your personal Knit credential store{}. Repository access has not been checked.{}",
        if is_env { " as an environment reference" } else { " (private file, not encrypted)" },
        if becomes_default { format!(" It is the default credential for {host}.") } else { String::new() });
    Ok(())
}

/// `knit auth default NAME`: the explicit choice among several credentials
/// on one host. Narrow by design — it only selects the host default; it
/// never touches repository assignments or tokens.
fn set_default(name: &str) -> Result<()> {
    validate_name(name)?;
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    let host = store
        .credentials
        .get(name)
        .map(|spec| spec.host.to_ascii_lowercase())
        .with_context(|| format!("Unknown credential `{name}`. Run `knit auth add` first."))?;
    store.defaults.insert(host.clone(), name.to_owned());
    // An explicit global choice overrides project-only scoping.
    store.scoped_credentials.remove(name);
    auth::save(&store)?;
    refresh_plain_git_helper_for_cwd()
        .context("The default was recorded, but the plain-Git helper could not be refreshed")?;
    println!("Default credential for {host} is now `{name}`; repository assignments still win where made.");
    Ok(())
}

fn selected_repos<'a>(
    project: &'a KnitProject,
    ids: &[String],
) -> Result<Vec<&'a ProjectRepoEntry>> {
    if ids.is_empty() {
        return Ok(project.repos.iter().collect());
    }
    ids.iter()
        .map(|id| {
            project
                .repos
                .iter()
                .find(|r| r.id == *id)
                .with_context(|| format!("Unknown repository `{id}` in project `{}`.", project.id))
        })
        .collect()
}

pub fn assign(name: &str, project_name: Option<&str>, ids: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, project) = auth::project_context(&cwd, project_name)?;
    assign_in(&root, &project, ids, name)
}

/// Assignment against an explicitly resolved project, so callers that own a
/// workspace root (grouped setup during `knit clone`) need no cwd trickery.
fn assign_in(root: &Path, project: &KnitProject, ids: &[String], name: &str) -> Result<()> {
    let repos = selected_repos(project, ids)?;
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    let spec = store
        .credentials
        .get(name)
        .with_context(|| format!("Unknown credential `{name}`. Run `knit auth add` first."))?;
    for repo in &repos {
        let remote = repo
            .remote
            .as_deref()
            .with_context(|| format!("Repository `{}` has no forge remote.", repo.id))?;
        let (host, _) = auth::remote_target(remote)?;
        if !host.eq_ignore_ascii_case(&spec.host) {
            bail!(
                "Credential `{name}` is for {}, but repository `{}` is on {host}.",
                spec.host,
                repo.id
            );
        }
    }
    let key = auth::project_key(root, &project.id)?;
    let bindings = store.projects.entry(key).or_default();
    for repo in &repos {
        bindings.insert(repo.id.clone(), name.into());
    }
    auth::save(&store)?;
    refresh_plain_git_helper(root, project)
        .context("The assignment was saved, but the plain-Git helper could not be refreshed")?;
    println!("Assigned `{name}` to {} in {}. Run `knit auth status --project {} --check` to check Git read access.",
        repos.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "), project.id, project.id);
    Ok(())
}

fn require_terminal() -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("Interactive setup needs a terminal. Use `knit auth add NAME --provider PROVIDER --token-stdin` (or --token-env), then `knit auth use NAME --project PROJECT --repo REPO`.");
    }
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        bail!("Setup cancelled: input closed.");
    }
    Ok(input.trim().into())
}

// Resolve the entire selection before any assignment is saved. IDs take precedence
// over numbers; numbers refer only to the displayed compatible, editable list.
fn show_setup_mapping(root: &Path, project: &KnitProject, ids: &[String]) -> Result<Vec<String>> {
    let store = auth::load()?;
    let key = auth::project_key(root, &project.id)?;
    let bindings = store.projects.get(&key);
    let mut missing = Vec::new();
    let mut total = 0;
    println!("\nProject: {} — repository → credential", project.id);
    for repo in &project.repos {
        let name = bindings.and_then(|p| p.get(&repo.id));
        let target = repo
            .remote
            .as_deref()
            .and_then(|r| auth::remote_target(r).ok());
        let state = if let Some((host, path)) = target {
            total += 1;
            let linked = name
                .and_then(|n| store.credentials.get(n))
                .is_some_and(|c| c.host == host);
            // The host's default serves repositories with no link of their
            // own; it is not a link, and switching it later applies here.
            let host_default = store.default_for_host(&host);
            if !linked && host_default.is_none() {
                missing.push(repo.id.clone());
            }
            let shown = if linked {
                name.map(String::as_str).unwrap_or("unassigned").to_string()
            } else if let Some((default, _)) = &host_default {
                format!("default `{default}`")
            } else {
                "unassigned".to_string()
            };
            format!(
                "{host}/{path} → {shown}{}",
                if name.is_some() && !linked {
                    " (invalid link)"
                } else {
                    ""
                }
            )
        } else if repo
            .remote
            .as_deref()
            .is_some_and(|r| !auth::is_local_remote(r))
        {
            total += 1;
            missing.push(repo.id.clone());
            "unsupported forge remote; correct host/protocol/path".into()
        } else {
            "local / no forge remote".into()
        };
        println!(
            "  {}: {state}{}",
            repo.id,
            if !ids.is_empty() && !ids.contains(&repo.id) {
                " [outside --repo scope]"
            } else {
                ""
            }
        );
    }
    println!(
        "Credential links: {}/{} forge repositories (access unchecked).",
        total - missing.len(),
        total
    );
    if !missing.is_empty() {
        println!("Missing links: {}", missing.join(", "));
    }
    Ok(missing)
}

pub fn setup(project_name: Option<&str>, ids: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, project) = auth::project_context(&cwd, project_name)?;
    selected_repos(&project, ids)?;
    require_terminal()?;
    setup_with_prompt(&root, &project, ids, &mut prompt, &mut read_hidden)
}

fn canonical_provider(value: &str) -> String {
    match value {
        "codeberg" | "gitea" => "forgejo".to_owned(),
        value => value.to_owned(),
    }
}

fn credential_matches_group(spec: &CredentialSpec, group: &ProjectAuthGroup) -> bool {
    canonical_provider(&spec.provider) == group.provider
        && spec.host.eq_ignore_ascii_case(&group.host)
}

fn describe_group(group: &ProjectAuthGroup, absent: &[String]) {
    println!(
        "\nGroup {}: {} ({} @ {})",
        group.id, group.name, group.provider, group.host
    );
    if !group.token_types.is_empty() {
        println!("  Token type(s): {}", group.token_types.join(", "));
    }
    if !group.permissions.is_empty() {
        println!("  Permissions: {}", group.permissions.join(", "));
    }
    if let Some(url) = &group.token_url {
        println!("  Create a token at: {url}");
    }
    if let Some(instructions) = &group.instructions {
        println!("  Instructions: {instructions}");
    }
    if !absent.is_empty() {
        println!(
            "  Not in this workspace (out of clone scope or not cloned yet): {}",
            absent.join(", ")
        );
    }
    println!("  Links do not grant provider permissions; Knit never checks them automatically.");
}

/// Ask which of the group's declared token kinds the user is creating.
fn choose_group_token_type(
    group: &ProjectAuthGroup,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    if group.token_types.len() == 1 {
        return Ok(group.token_types[0].clone());
    }
    for (i, token_type) in group.token_types.iter().enumerate() {
        println!("  {}. {token_type}", i + 1);
    }
    loop {
        let choice = ask("Token type (name or number): ")?;
        if let Some(found) = group.token_types.iter().find(|t| *t == choice.as_str()) {
            return Ok(found.clone());
        }
        if let Ok(number) = choice.parse::<usize>() {
            if let Some(found) = number.checked_sub(1).and_then(|n| group.token_types.get(n)) {
                return Ok(found.clone());
            }
        }
        println!("Choose one of the group's token types.");
    }
}

/// Bitbucket's two token kinds authenticate differently: an Atlassian API
/// token needs the account email; a repository/project/workspace access
/// token must not carry one. Ask for exactly what the chosen kind needs.
fn bitbucket_username_for_token_type(
    token_type: &str,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<Option<String>> {
    if token_type == "atlassian_api_token" {
        loop {
            let email = ask("Atlassian account email for this API token: ")?;
            if auth::is_bitbucket_account_email(&email) {
                return Ok(Some(email));
            }
            println!("An Atlassian API token needs the account email address.");
        }
    }
    Ok(None)
}

/// Ask which of Bitbucket's token kinds a new token is. The kind decides the
/// Git username scheme, so an unclassified Bitbucket credential would
/// authenticate as the wrong identity later; one question buys correctness.
/// Other providers' opaque tokens all authenticate the same way and stay
/// unclassified until their owner says otherwise.
fn ask_bitbucket_token_type(ask: &mut impl FnMut(&str) -> Result<String>) -> Result<String> {
    let kinds = crate::model::BITBUCKET_TOKEN_TYPES;
    for (index, kind) in kinds.iter().enumerate() {
        println!("  {}. {kind}", index + 1);
    }
    loop {
        let choice = ask("Which kind of Bitbucket token is it? (name/number): ")?;
        if let Some(found) = kinds.iter().find(|kind| **kind == choice.as_str()) {
            return Ok(found.to_string());
        }
        if let Ok(number) = choice.parse::<usize>() {
            if let Some(found) = number.checked_sub(1).and_then(|n| kinds.get(n)) {
                return Ok(found.to_string());
            }
        }
        println!("Choose one of the listed token kinds.");
    }
}

/// Make sure a saved Bitbucket credential carries its token kind. A recorded
/// account email implies the Atlassian API-token mode; otherwise the owner is
/// asked once and the answer is persisted.
fn ensure_bitbucket_token_type(
    name: &str,
    spec: &CredentialSpec,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    if let Some(kind) = spec.token_type.as_deref() {
        return Ok(kind.to_string());
    }
    let kind = if spec
        .username
        .as_deref()
        .is_some_and(auth::is_bitbucket_account_email)
    {
        println!(
            "`{name}` has an account email recorded; treating it as an \
             `atlassian_api_token` token."
        );
        "atlassian_api_token".to_string()
    } else {
        ask_bitbucket_token_type(ask)?
    };
    save_credential_token_type(name, &kind)?;
    println!("Recorded `{name}` as a `{kind}` token.");
    Ok(kind)
}

/// Record how an existing opaque token should be treated. The type is never
/// guessed from the token value; it stays unclassified until its owner says.
fn save_credential_token_type(name: &str, token_type: &str) -> Result<()> {
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    let Some(spec) = store.credentials.get(name) else {
        bail!("Credential `{name}` is not configured");
    };
    let mut updated = spec.clone();
    updated.token_type = Some(token_type.to_owned());
    auth::validate_spec(&updated)?;
    store.credentials.insert(name.to_owned(), updated);
    auth::save(&store)
}

/// Set or clear the Bitbucket account email on a saved credential.
fn save_credential_username(name: &str, username: Option<&str>) -> Result<()> {
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    let Some(spec) = store.credentials.get(name) else {
        bail!("Credential `{name}` is not configured");
    };
    let mut updated = spec.clone();
    updated.username = username.map(str::to_owned);
    auth::validate_spec(&updated)?;
    store.credentials.insert(name.to_owned(), updated);
    auth::save(&store)
}

/// Reconcile a Bitbucket credential's account email with its classified
/// token kind: an Atlassian API token authenticates as the account email; a
/// repository/project/workspace access token must not carry one. A recorded
/// username that is not a valid email address never satisfies the API-token
/// kind — the owner is asked for the real one.
fn reconcile_bitbucket_username(
    name: &str,
    token_type: &str,
    username: Option<&str>,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    if token_type == "atlassian_api_token" {
        if username.is_some_and(auth::is_bitbucket_account_email) {
            return Ok(());
        }
        let Some(email) = bitbucket_username_for_token_type(token_type, ask)? else {
            return Ok(());
        };
        save_credential_username(name, Some(&email))?;
        println!("Recorded the account email for `{name}`.");
    } else if token_type == "access_token" && username.is_some() {
        let confirmed = ask(
            "This credential is recorded with an account email, but a repository/project/workspace access token must not use one. Clear the email? [y/N]: ",
        )?;
        if matches!(confirmed.to_ascii_lowercase().as_str(), "y" | "yes") {
            save_credential_username(name, None)?;
            println!("Cleared the account email from `{name}`.");
        }
    }
    Ok(())
}

/// Clone and pull reuse defaults and existing assignments. Rejected access
/// creates a project-scoped replacement; shared tokens are never rotated.
pub(crate) fn guided_group_setup(
    root: &Path,
    project: &KnitProject,
    repair: &BTreeSet<String>,
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let auth_requirements = project
        .auth
        .as_ref()
        .context("Project has no auth requirements")?;
    let key = auth::project_key(root, &project.id)?;
    let mut bound_any = false;
    for group in &auth_requirements.groups {
        if group.repos.is_empty() {
            continue;
        }
        let store = auth::load()?;
        let bindings = store.projects.get(&key);
        // Whether a default token already covered this host when the group
        // was entered: a failing repository then had a working-looking
        // credential rejected (repair), rather than plain missing access.
        let entry_default = store.default_for_host(&group.host).is_some();
        let linked = |repo: &str| bindings.is_some_and(|b| b.contains_key(repo));
        let unlinked: Vec<String> = group
            .repos
            .iter()
            .filter(|repo| !linked(repo))
            .cloned()
            .collect();
        let failing: Vec<String> = group
            .repos
            .iter()
            .filter(|repo| repair.contains(*repo))
            .cloned()
            .collect();
        if unlinked.is_empty() && failing.is_empty() {
            // Fully linked and working: never re-prompt.
            continue;
        }
        // The host's default credential — explicit or the host's only token —
        // covers this group with no prompts and no per-repository links: a
        // working default is never re-classified or mutated because a group
        // recommends a different token kind. Groups guide missing access.
        // Deliberate repository assignments stay untouched and keep winning.
        if failing.is_empty() {
            if let Some((default, source)) = store.default_for_host(&group.host) {
                let provider_matches = store
                    .credentials
                    .get(&default)
                    .is_some_and(|spec| canonical_provider(&spec.provider) == group.provider);
                if provider_matches {
                    println!(
                        "Host default `{default}`{} covers {} — using it for group `{}` without repository links.",
                        match source {
                            auth::DefaultSource::Chosen => "".to_string(),
                            auth::DefaultSource::Inherited => {
                                " (the only credential on this host)".to_string()
                            }
                        },
                        group.host,
                        group.id
                    );
                    continue;
                }
            }
        }
        // Sibling extension: part of the group shares one credential —
        // extend it to the rest without a prompt. Deliberate per-repo
        // overrides to other credentials are respected, not overwritten.
        if unlinked.len() < group.repos.len() && failing.is_empty() {
            let bound_credentials: BTreeSet<&String> = group
                .repos
                .iter()
                .filter_map(|repo| bindings.and_then(|b| b.get(repo)))
                .collect();
            if let Some(shared) = bound_credentials.iter().next() {
                if bound_credentials.len() == 1 {
                    let shared = (*shared).clone();
                    assign_in(root, project, &unlinked, &shared)?;
                    bound_any = true;
                    continue;
                }
            }
        }
        describe_group(group, &[]);
        // Repair: authentication was just rejected for these repositories.
        // The replacement is ALWAYS a new local credential bound to exactly
        // the affected repositories — the shared or default credential keeps
        // its token, its environment reference, and its default status.
        // Rotating a shared secret is a deliberate `knit auth add --replace`,
        // never an automatic repair.
        if !failing.is_empty() {
            let shared = if unlinked.is_empty() {
                let bound: BTreeSet<&String> = failing
                    .iter()
                    .filter_map(|repo| bindings.and_then(|b| b.get(repo)))
                    .collect();
                (bound.len() == 1).then(|| (*bound.iter().next().unwrap()).clone())
            } else {
                // Unlinked failing repositories were served by the host's
                // default; name it when there is one.
                store.default_for_host(&group.host).map(|(name, _)| name)
            };
            if let Some(name) = shared {
                if let Some(spec) = store.credentials.get(&name).cloned() {
                    println!(
                        "Git authentication failed while credential `{name}` was selected for {}.",
                        failing.join(", ")
                    );
                    println!(
                        "Saved credentials override ordinary Git credential helpers and SSH for those repositories, so this failure does not establish that repository access is missing."
                    );
                    println!("{}", permission_help(&spec.provider));
                    let token = read_token(&format!(
                        "Replacement token for {} (saved as a new local credential for {}; `{name}` keeps its token — hidden): ",
                        spec.host,
                        failing.join(", ")
                    ))?;
                    let token = token.trim();
                    if !token.is_empty() {
                        let mut new_spec = CredentialSpec {
                            provider: spec.provider.clone(),
                            host: spec.host.to_ascii_lowercase(),
                            username: spec.username.clone(),
                            token_type: spec.token_type.clone(),
                            // A pasted replacement never rides the old
                            // environment reference.
                            token_env: None,
                        };
                        if spec.provider == "bitbucket" {
                            // A replacement Bitbucket token is classified
                            // afresh: the old classification was just
                            // rejected, and a fresh token may be a different
                            // kind than the one it replaces.
                            let kind = ask_bitbucket_token_type(ask)?;
                            new_spec.token_type = Some(kind.clone());
                            new_spec.username = bitbucket_username_for_token_type(&kind, ask)?;
                        }
                        let new_name = unique_credential_name(&format!(
                            "{}-{}",
                            sanitize_credential_name(&spec.host),
                            sanitize_credential_name(&group.id)
                        ))?;
                        save_new_credential_scoped(&new_name, &new_spec, token, true)?;
                        assign_in(root, project, &failing, &new_name)?;
                        bound_any = true;
                    }
                    continue;
                }
            }
        }
        let compatible: Vec<(String, CredentialSpec)> = store
            .credentials
            .iter()
            .filter(|(name, spec)| {
                !store.scoped_credentials.contains(*name) && credential_matches_group(spec, group)
            })
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect();
        // Tiered: a declared-kind (or unclassified) match outranks a
        // classified kind the group does not declare.
        let tier = |spec: &CredentialSpec| match (&spec.token_type, group.token_types.is_empty()) {
            (None, _) => 1,
            (Some(_), true) => 1,
            (Some(kind), false) if group.token_types.contains(kind) => 0,
            (Some(_), false) => 2,
        };
        let mut ordered = compatible;
        ordered.sort_by_key(|(_, spec)| tier(spec));
        let preferred: Vec<&(String, CredentialSpec)> =
            ordered.iter().filter(|(_, spec)| tier(spec) < 2).collect();
        let name = match preferred.len() {
            1 => {
                let (name, spec) = &preferred[0];
                println!(
                    "Using saved credential `{name}` ({} @ {}) for this group.",
                    spec.provider, spec.host,
                );
                name.clone()
            }
            _ if !ordered.is_empty() => {
                println!("Saved credentials for {} @ {}:", group.provider, group.host);
                for (i, (name, spec)) in ordered.iter().enumerate() {
                    let kind = match &spec.token_type {
                        Some(kind) if group.token_types.contains(kind) => {
                            format!("`{kind}`")
                        }
                        Some(kind) => format!("`{kind}`, outside this group's kinds"),
                        None => "token type unclassified".to_owned(),
                    };
                    println!("  {}. {name} ({kind})", i + 1);
                }
                println!("  n. enter a new token for this group");
                loop {
                    let choice = ask(&format!(
                        "Credential for {} @ {} (1-{}, n, Enter = 1): ",
                        group.provider,
                        group.host,
                        ordered.len()
                    ))?;
                    if choice.eq_ignore_ascii_case("n") {
                        if let Some(name) = create_group_credential(group, false, ask, read_token)?
                        {
                            break name;
                        }
                        continue;
                    }
                    let picked = if choice.is_empty() {
                        Some(ordered[0].0.clone())
                    } else {
                        choice
                            .parse::<usize>()
                            .ok()
                            .and_then(|number| number.checked_sub(1))
                            .and_then(|index| ordered.get(index).map(|(name, _)| name.clone()))
                            .or_else(|| {
                                ordered
                                    .iter()
                                    .find(|(name, _)| *name == choice)
                                    .map(|(name, _)| name.clone())
                            })
                    };
                    match picked {
                        Some(name) => break name,
                        None => println!("Choose a displayed credential, n, or press Enter."),
                    }
                }
            }
            _ => match create_group_credential(group, false, ask, read_token)? {
                Some(name) => name,
                None => {
                    println!(
                        "Skipped group `{}`; its repositories keep whatever access they have.",
                        group.id
                    );
                    continue;
                }
            },
        };
        // A reused Bitbucket credential may have drifted from its token kind
        // (API token without its account email, an unclassified token, or
        // the reverse); one question each fixes what would otherwise
        // authenticate with the wrong Git username.
        let store = auth::load()?;
        if let Some(spec) = store.credentials.get(&name) {
            if group.provider == "bitbucket" {
                let kind = ensure_bitbucket_token_type(&name, spec, ask)?;
                reconcile_bitbucket_username(&name, &kind, spec.username.as_deref(), ask)?;
            }
        }
        // When the resolved credential is the host's default — inherited, or
        // the one this setup just created and promoted — no per-repository
        // links are written: the default serves every project on the host,
        // and switching the default later applies everywhere. Only a genuine
        // alternative (picked from several) or a repaired repository gets a
        // binding; a failing (rejected) repository is rebound because its
        // previous link just failed.
        let is_host_default = auth::load()?
            .default_for_host(&group.host)
            .is_some_and(|(default, _)| default == name);
        // First-token flow (no default existed at entry): the new default
        // serves with no per-repository links. A repair of a rejected
        // default or link still binds exactly the affected repositories.
        let mut to_assign = if is_host_default {
            if entry_default {
                failing.clone()
            } else {
                Vec::new()
            }
        } else {
            unlinked
        };
        if !is_host_default {
            to_assign.extend(failing.iter().cloned());
        }
        to_assign.sort();
        to_assign.dedup();
        if is_host_default && to_assign.is_empty() {
            println!(
                "No repository links needed: `{name}` is the default credential for {}.",
                group.host
            );
        }
        if !to_assign.is_empty() {
            assign_in(root, project, &to_assign, &name)?;
        }
        bound_any = true;
    }
    if !bound_any {
        // A repair run that changed nothing must say so truthfully: the
        // groups were not "already linked" — the offers were declined.
        if repair.is_empty() {
            println!("Every group is already linked; nothing to set up.");
        } else {
            println!("No credential changes were made.");
        }
    }
    refresh_plain_git_helper(root, project)
        .context("Credentials were linked, but the plain-Git helper could not be refreshed")?;
    Ok(())
}

/// Create a brand-new credential for a guided group: one hidden token
/// prompt, an automatic name, no choreography. `None` means the answer was
/// empty and the group is declined for now.
fn create_group_credential(
    group: &ProjectAuthGroup,
    scoped: bool,
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
) -> Result<Option<String>> {
    let token_type = if group.token_types.is_empty() {
        // No declared kinds: Bitbucket still needs its actual mode — the kind
        // picks the Git username and the email question. Other providers'
        // opaque tokens authenticate identically and stay unclassified.
        if group.provider == "bitbucket" {
            Some(ask_bitbucket_token_type(ask)?)
        } else {
            None
        }
    } else {
        Some(choose_group_token_type(group, ask)?)
    };
    println!("{}", permission_help(&group.provider));
    let token = read_token(&format!(
        "Token for {} ({} — hidden): ",
        group.host, group.id
    ))?;
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let username = if group.provider == "bitbucket" {
        match &token_type {
            Some(kind) => bitbucket_username_for_token_type(kind, ask)?,
            None => None,
        }
    } else {
        None
    };
    let spec = CredentialSpec {
        provider: group.provider.clone(),
        // Group hosts are case-insensitive; store the canonical lowercase
        // form so later exact host comparisons hold.
        host: group.host.to_ascii_lowercase(),
        username,
        token_type,
        token_env: None,
    };
    let name = unique_credential_name(&auto_credential_name(group))?;
    save_new_credential_scoped(&name, &spec, token, scoped)?;
    Ok(Some(name))
}

/// Credential name for a guided group: the host, plus the group id when the
/// group is not the host's own inferred stand-in, with characters outside the
/// credential-name charset folded to hyphens.
fn auto_credential_name(group: &ProjectAuthGroup) -> String {
    if group.id.eq_ignore_ascii_case(&group.host) {
        sanitize_credential_name(&group.host)
    } else {
        format!(
            "{}-{}",
            sanitize_credential_name(&group.host),
            sanitize_credential_name(&group.id)
        )
    }
}

/// Fold a free-form label into the credential-name charset.
fn sanitize_credential_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The base name, or its `-2`, `-3`, … successors until one is unused.
fn unique_credential_name(base: &str) -> Result<String> {
    let store = auth::load()?;
    if !store.credentials.contains_key(base) {
        return Ok(base.to_owned());
    }
    Ok((2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !store.credentials.contains_key(candidate))
        .expect("a finite credential namespace always has a free name"))
}

/// Save a brand-new credential with an already-read token (guided setup read
/// it hidden; `knit auth add`'s interactive path stays in `add`).
fn save_new_credential(name: &str, spec: &CredentialSpec, token: &str) -> Result<()> {
    save_new_credential_scoped(name, spec, token, false)
}

/// `scoped` marks the credential as project-only personal state: it never
/// becomes the host's implicit global default — not even when it is the
/// first token saved for the host — and only the repositories it was bound
/// to use it. An explicit `knit auth default NAME` unmarks it.
fn save_new_credential_scoped(
    name: &str,
    spec: &CredentialSpec,
    token: &str,
    scoped: bool,
) -> Result<()> {
    validate_name(name)?;
    auth::validate_spec(spec)?;
    if token.is_empty() || token.chars().any(char::is_control) {
        bail!("Token must be nonempty and on one line.");
    }
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    if store.credentials.contains_key(name) {
        bail!("Credential `{name}` exists. Use `knit auth add {name} --replace` to rotate it.");
    }
    // Only a global (unscoped) creation can pin a legacy implicit default.
    if !scoped {
        pin_legacy_default(&mut store, &spec.host);
    }
    store.credentials.insert(name.to_owned(), spec.clone());
    if scoped {
        store.scoped_credentials.insert(name.to_owned());
    }
    let becomes_default = !scoped && auth::stage_default_if_absent(&mut store, &spec.host, name);
    auth::save_credential(&store, name, Some(token))?;
    println!(
        "Saved `{name}` in your personal Knit credential store (private file, not encrypted). Repository access has not been checked."
    );
    if becomes_default {
        println!("`{name}` is now the default credential for {}.", spec.host);
    }
    Ok(())
}

fn inferred_groups(
    repos: &[(String, String)],
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<Vec<ProjectAuthGroup>> {
    let mut by_host: BTreeMap<String, ProjectAuthGroup> = BTreeMap::new();
    for (id, remote) in repos {
        let Ok((host, _)) = auth::remote_target(remote) else {
            continue;
        };
        if !by_host.contains_key(&host) {
            let provider = match crate::providers::for_remote(remote) {
                Some(forge) => forge.id().to_owned(),
                None => loop {
                    let answer = ask(&format!(
                        "Provider for {host} (github/gitlab/bitbucket/forgejo): "
                    ))?;
                    if ["github", "gitlab", "bitbucket", "forgejo"].contains(&answer.as_str()) {
                        break answer;
                    }
                    println!("Choose one of the listed providers.");
                },
            };
            by_host.insert(
                host.clone(),
                ProjectAuthGroup {
                    id: host.clone(),
                    name: host.clone(),
                    host: host.clone(),
                    provider,
                    repos: Vec::new(),
                    token_types: Vec::new(),
                    permissions: Vec::new(),
                    instructions: None,
                    token_url: None,
                },
            );
        }
        by_host.get_mut(&host).unwrap().repos.push(id.clone());
    }
    Ok(by_host.into_values().collect())
}

/// Clone and pull share group projection and repair; callers retry failed Git
/// operations once after setup. Successful repositories are never included.
fn repair_with_prompt(
    root: &Path,
    project: &KnitProject,
    failing: &[(String, String)],
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let repair: BTreeSet<String> = failing.iter().map(|(id, _)| id.clone()).collect();
    let mut setup = project.clone();
    let groups = &mut setup.auth.get_or_insert_with(Default::default).groups;
    for group in groups.iter_mut() {
        group.repos.retain(|repo| repair.contains(repo));
    }
    groups.retain(|group| !group.repos.is_empty());
    let declared: BTreeSet<_> = groups
        .iter()
        .flat_map(|group| group.repos.iter().cloned())
        .collect();
    let uncovered: Vec<_> = failing
        .iter()
        .filter(|(id, _)| !declared.contains(id))
        .cloned()
        .collect();
    groups.extend(inferred_groups(&uncovered, ask)?);
    guided_group_setup(root, &setup, &repair, ask, read_token)
}

pub(crate) fn repair_credentials(
    root: &Path,
    project: &KnitProject,
    failing: &[(String, String)],
) -> Result<()> {
    require_terminal()?;
    repair_with_prompt(root, project, failing, &mut prompt, &mut read_hidden)
}

/// Initial setup uses declared guidance; host defaults need no assignments.
pub(crate) fn clone_group_setup(root: &Path, project: &KnitProject) -> Result<()> {
    require_terminal()?;
    guided_group_setup(
        root,
        project,
        &BTreeSet::new(),
        &mut prompt,
        &mut read_hidden,
    )
}

fn permission_help(provider: &str) -> &'static str {
    match provider {
        "github" => "GitHub fine-grained PAT: select your repositories; Contents and Pull requests: Read + Write; Checks and Commit statuses: Read; Metadata: Read (automatic).\nOr use a classic PAT with repo. To change workflow files, add Workflows: Write (fine-grained) or workflow (classic).",
        "bitbucket" => "Unscoped Atlassian API tokens don't work with Bitbucket.\nChoose 'Create API token with scopes' → Bitbucket → Repositories: Read + Write; Pull requests: Read + Write.",
        "gitlab" => "GitLab personal access token: api (Git and merge requests).\nProject/group access token: api + write_repository; choose a role allowed to push and merge.",
        _ => "Forgejo access token: write:repository (Git, pull requests, and checks).\nInclude the repositories you use; public-only tokens cannot access private repositories.",
    }
}

pub fn status(project_name: Option<&str>, check: bool, json_output: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, project) = auth::project_context(&cwd, project_name)?;
    status_project(&root, &project, check, json_output)
}

/// One repository row of `knit auth status`. `covered` tracks whether the
/// repository has a working credential link (assignment + host/provider
/// match) independently of the display text, so a successful `--check` probe
/// rewriting the status line cannot flip group coverage to "missing".
struct RepoStatusRow {
    repo: String,
    host: Option<String>,
    credential: Option<String>,
    state: String,
    auth_group: Option<String>,
    covered: bool,
    ambient: bool,
    /// The host default serving this repository when no assignment does, and
    /// whether it was chosen explicitly or inherited as the host's only
    /// credential.
    default_credential: Option<String>,
    default_source: Option<&'static str>,
}

type Probe = dyn FnMut(&Path, &str) -> Result<()>;

/// Status against an explicitly resolved workspace, so grouped setup during
/// `knit clone` can report on the workspace being created rather than the
/// directory the user invoked `knit clone` from.
fn status_project(
    root: &Path,
    project: &KnitProject,
    check: bool,
    json_output: bool,
) -> Result<()> {
    status_project_with(root, project, check, json_output, &mut probe_read_access)
}

fn status_project_with(
    root: &Path,
    project: &KnitProject,
    check: bool,
    json_output: bool,
    probe: &mut Probe,
) -> Result<()> {
    // Status can select a project different from the workspace fallback.
    auth::set_project_override(Some(project.id.clone()));
    // Report broken group declarations instead of rendering coverage over
    // collapsed maps: a typo'd reference or off-host group must fail
    // clearly, with or without --check. References the project's full
    // membership knows (recorded as pending) stay valid and merely count as
    // absent from this workspace.
    let pending = auth::load_known_pending_repos(root, &project.id);
    auth::validate_project_auth_with_pending(project, &pending).context(
        "Project auth requirements are invalid; fix the project's auth groups, then run `knit auth setup`",
    )?;
    let store = auth::load()?;
    let key = auth::project_key(root, &project.id)?;
    let bindings = store.projects.get(&key);
    let ambient = store.ambient.get(&key).cloned().unwrap_or_default();
    let explicit = bindings.is_some_and(|p| !p.is_empty());
    let groups = project
        .auth
        .as_ref()
        .map(|auth| auth.groups.as_slice())
        .unwrap_or_default();
    let group_by_repo: std::collections::BTreeMap<&str, &ProjectAuthGroup> = groups
        .iter()
        .flat_map(|group| group.repos.iter().map(move |repo| (repo.as_str(), group)))
        .collect();
    let mut failed = false;
    let mut rows: Vec<RepoStatusRow> = Vec::new();
    for repo in &project.repos {
        let target = repo
            .remote
            .as_deref()
            .and_then(|r| auth::remote_target(r).ok());
        let unsupported = target.is_none()
            && repo
                .remote
                .as_deref()
                .is_some_and(|r| !auth::is_local_remote(r));
        let name = bindings.and_then(|p| p.get(&repo.id));
        let group = group_by_repo.get(repo.id.as_str()).copied();
        let host_default = target
            .as_ref()
            .and_then(|(host, _)| store.default_for_host(host));
        // An ambient allowance recorded for exactly this repository's current
        // remote: it was cloned or verified without a Knit credential. It is
        // not a link (no credential counts), but the repository is allowed
        // and probeable. A declared group member never inherits it — the
        // allowance went stale when the group claimed the repository — and a
        // URL change invalidates it: the recorded target no longer matches,
        // and the repository falls back to strict reporting.
        let ambient_ok = group.is_none()
            && target
                .as_ref()
                .is_some_and(|(host, path)| auth::ambient_allows(&ambient, &repo.id, host, path));
        let mut covered = false;
        let state = if unsupported {
            failed = true;
            "unsupported forge remote; correct host/protocol/path".to_string()
        } else if target.is_none() {
            "local / no forge remote".to_string()
        } else if let Some(selected) = name
            .map(String::as_str)
            .or_else(|| host_default.as_ref().map(|(name, _)| name.as_str()))
        {
            let prefix = if name.is_some() { "" } else { "host default " };
            match auth::credential(selected) {
                Ok(c) if target.as_ref().is_some_and(|(host, _)| *host == c.host) => {
                    let spec = &store.credentials[selected];
                    if let Some(group) =
                        group.filter(|group| canonical_provider(&spec.provider) != group.provider)
                    {
                        failed = true;
                        format!(
                            "{prefix}credential provider mismatch (group `{}` expects {})",
                            group.id, group.provider
                        )
                    } else {
                        covered = true;
                        if name.is_none() {
                            let inherited = host_default.as_ref().is_some_and(|(_, source)| {
                                *source == auth::DefaultSource::Inherited
                            });
                            format!(
                                "host default `{selected}` ({}unchecked)",
                                if inherited {
                                    "only credential on host, "
                                } else {
                                    ""
                                }
                            )
                        } else if let Some((group, kind)) = group
                            .zip(spec.token_type.as_deref())
                            .filter(|(group, kind)| {
                                !group
                                    .token_types
                                    .iter()
                                    .any(|token_type| token_type == kind)
                            })
                        {
                            format!("configured (unchecked; `{kind}` not among group `{}` recommended types)", group.id)
                        } else {
                            "configured (unchecked)".into()
                        }
                    }
                }
                result => {
                    failed = true;
                    let reason = if result.is_ok() {
                        "host mismatch"
                    } else {
                        "unavailable"
                    };
                    format!("{prefix}credential {reason}")
                }
            }
        } else if let Some(group) = group {
            failed = true;
            format!("needs credential assignment (auth group `{}`)", group.id)
        } else if explicit && !ambient_ok {
            failed = true;
            "needs credential assignment".into()
        } else if explicit {
            "ambient Git access (recorded for this remote)".into()
        } else {
            "existing Git/forge authentication".into()
        };
        // A group repository without an assigned credential must not be
        // reported as fine just because ambient Git access happens to work:
        // the project explicitly asked for a credential there.
        let probe_eligible = if group.is_some() {
            covered
        } else {
            !explicit || covered || ambient_ok
        };
        let state = if check && target.is_some() && probe_eligible {
            // Use the project root and explicit remote, including when checkout does not yet exist.
            match probe(root, repo.remote.as_deref().unwrap()) {
                Ok(()) => "Git read access confirmed; API/write permissions unchecked".to_string(),
                Err(_) => {
                    failed = true;
                    "Git read access failed; check credential and repository access".to_string()
                }
            }
        } else {
            state
        };
        rows.push(RepoStatusRow {
            repo: repo.id.clone(),
            host: target.as_ref().map(|(h, _)| h).cloned(),
            credential: name.cloned(),
            state,
            auth_group: group.map(|g| g.id.clone()),
            covered,
            ambient: ambient_ok && name.is_none(),
            default_credential: if name.is_none() {
                host_default.as_ref().map(|(name, _)| name.clone())
            } else {
                None
            },
            default_source: if name.is_none() {
                host_default.as_ref().map(|(_, source)| match source {
                    auth::DefaultSource::Chosen => "chosen",
                    auth::DefaultSource::Inherited => "inherited",
                })
            } else {
                None
            },
        });
    }
    // Group coverage is reported even while drafting: repositories no group
    // covers are allowed, but never silently assigned. Coverage follows the
    // boolean link state, never the display text, so a successful --check
    // probe keeps a repository counted as linked.
    let covered_repo = |repo_id: &str| {
        rows.iter()
            .find(|row| row.repo == repo_id)
            .is_some_and(|row| row.covered)
    };
    let auth_group_rows: Vec<(&ProjectAuthGroup, usize, Vec<&str>)> = groups
        .iter()
        .map(|group| {
            let missing: Vec<&str> = group
                .repos
                .iter()
                .filter(|repo_id| !covered_repo(repo_id))
                .map(|repo_id| repo_id.as_str())
                .collect();
            (group, group.repos.len() - missing.len(), missing)
        })
        .collect();
    let uncovered: Vec<&str> = project
        .repos
        .iter()
        .filter(|repo| !group_by_repo.contains_key(repo.id.as_str()))
        .filter(|repo| {
            repo.remote
                .as_deref()
                .and_then(|r| auth::remote_target(r).ok())
                .is_some()
        })
        .map(|repo| repo.id.as_str())
        .collect();
    if json_output {
        let row_json: Vec<_> = rows
            .iter()
            .map(|row| {
                json!({
                    "repo": row.repo,
                    "host": row.host,
                    "credential": row.credential,
                    "defaultCredential": row.default_credential,
                    "defaultSource": row.default_source,
                    "status": row.state,
                    "authGroup": row.auth_group,
                    "ambient": row.ambient,
                })
            })
            .collect();
        let group_json: Vec<_> = auth_group_rows
            .iter()
            .map(|(group, linked, missing)| {
                json!({
                    "id": group.id,
                    "name": group.name,
                    "provider": group.provider,
                    "host": group.host,
                    "tokenTypes": group.token_types,
                    "repos": group.repos,
                    "linked": linked,
                    "missing": missing,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": project.id,
                "explicit": explicit,
                "repositories": row_json,
                "authGroups": group_json,
                "reposWithoutAuthGroup": uncovered,
            }))?
        );
    } else {
        println!("Project: {}", project.id);
        for row in &rows {
            println!(
                "  {}\t{}\t{}",
                row.repo,
                row.credential.as_deref().unwrap_or("—"),
                row.state
            );
        }
        for (group, linked, missing) in &auth_group_rows {
            if missing.is_empty() {
                println!(
                    "Auth group {} ({} @ {}): {}/{} repository(ies) linked",
                    group.id,
                    group.provider,
                    group.host,
                    linked,
                    group.repos.len()
                );
            } else {
                println!(
                    "Auth group {} ({} @ {}): {}/{} repository(ies) linked; missing: {}",
                    group.id,
                    group.provider,
                    group.host,
                    linked,
                    group.repos.len(),
                    missing.join(", ")
                );
            }
        }
        if !uncovered.is_empty() {
            println!(
                "No auth group covers: {} (allowed; using existing links or ambient Git authentication)",
                uncovered.join(", ")
            );
        }
        if !groups.is_empty()
            && auth_group_rows
                .iter()
                .any(|(_, _, missing)| !missing.is_empty())
        {
            println!(
                "Run `knit auth setup --project {}` to link the missing repositories.",
                project.id
            );
        }
        if !explicit && groups.is_empty() {
            println!("Run `knit auth setup` to choose credentials for this project.");
        }
    }
    // Existing installs activate plain-Git credential integration here: the
    // refresh is noninteractive and uses only saved credentials, so an
    // upgrade never asks for a token again. A failed installation must fail
    // the status report instead of letting it claim success.
    let (active, cleared) = refresh_plain_git_helper(root, project)?;
    if !json_output {
        if active > 0 {
            println!("Git authentication configured for {active} checkouts.");
        }
        if cleared > 0 {
            println!("Git authentication removed from {cleared} checkouts.");
        }
    }
    if check && failed {
        bail!(
            "Some repositories need attention. Run `knit auth setup --project {}`.",
            project.id
        );
    }
    Ok(())
}

fn probe_read_access(root: &Path, remote: &str) -> Result<()> {
    crate::git::git_output_with_timeout(
        root,
        ["ls-remote", remote, "HEAD"],
        std::time::Duration::from_secs(30),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grouped_project_json(with_typo: bool) -> serde_json::Value {
        let gh_repos = if with_typo {
            vec!["appp"]
        } else {
            vec!["api", "web"]
        };
        json!({
            "schemaVersion":"0.1", "kind":"KnitProject", "id":"tools",
            "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
            "repos":[
                {"id":"api","path":"api","remote":"https://github.com/org/api.git","baseBranch":"main"},
                {"id":"web","path":"web","remote":"git@github.com:org/web.git","baseBranch":"main"},
                {"id":"bb","path":"bb","remote":"https://bitbucket.org/team/backend.git","baseBranch":"main"},
                {"id":"bb2","path":"bb2","remote":"https://bitbucket.org/team/worker.git","baseBranch":"main"}
            ],
            "auth": {"groups": [
                {
                    "id": "gh-work", "name": "GitHub work token", "provider": "github",
                    "host": "github.com", "repos": gh_repos, "tokenTypes": ["fine_grained_pat"],
                    "permissions": ["contents:read", "pull_requests:write"],
                    "instructions": "Create the token in the org, scoped to the two repos.",
                    "tokenUrl": "https://github.com/settings/personal-access-tokens/new"
                },
                {
                    "id": "bb-cloud", "name": "Bitbucket API", "provider": "bitbucket",
                    "host": "bitbucket.org", "repos": ["bb"],
                    "tokenTypes": ["atlassian_api_token"]
                },
                {
                    "id": "bb-second", "name": "Bitbucket access", "provider": "bitbucket",
                    "host": "bitbucket.org", "repos": ["bb2"],
                    "tokenTypes": ["access_token"]
                },
                {
                    "id": "out-scope", "name": "Extra forge", "provider": "github",
                    "host": "github.com", "repos": ["extra"], "tokenTypes": ["classic_pat"]
                }
            ]}
        })
    }

    fn grouped_child_root(mode: &str, typo: bool) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("knit-group-{mode}-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".knit/projects")).unwrap();
        std::fs::write(
            root.join(".knit/config.json"),
            r#"{"schemaVersion":"0.1","activeProject":"tools"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".knit/projects/tools.project.json"),
            serde_json::to_vec(&grouped_project_json(typo)).unwrap(),
        )
        .unwrap();
        if !typo {
            auth::save_known_pending_repos(
                &root,
                "tools",
                &std::collections::BTreeMap::from([(
                    "extra".to_string(),
                    "https://github.com/org/extra.git".to_string(),
                )]),
            )
            .unwrap();
        }
        root
    }

    /// Run one grouped-setup scenario in its own child process so concurrent
    /// library tests cannot see this synthetic personal store.
    fn run_grouped_child(mode: &str, typo: bool) -> (String, String, bool) {
        let root = grouped_child_root(mode, typo);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::auth::tests::grouped_setup_child",
                "--nocapture",
            ])
            .current_dir(&root)
            .env("KNIT_GROUP_SETUP_MODE", mode)
            .env("KNIT_HOME", root.join("home"))
            .env("KNIT_WIZARD_TOKEN", "synthetic-token")
            .env_remove("KNIT_BUNDLE")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
        let _ = std::fs::remove_dir_all(&root);
        (stdout, stderr, result.status.success())
    }

    /// A two-repository Bitbucket project for the guided-repair scenarios:
    /// `bb` sits in the declared group (the failing repository), `bb2` is an
    /// ungrouped repository that must keep its binding through the repair.
    fn bitbucket_repair_project(root: &std::path::Path) -> KnitProject {
        let path = root.join(".knit/projects/tools.project.json");
        let project = json!({
            "schemaVersion":"0.1", "kind":"KnitProject", "id":"tools",
            "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
            "repos":[
                {"id":"bb","path":"bb","remote":"https://bitbucket.org/team/backend.git","baseBranch":"main"},
                {"id":"bb2","path":"bb2","remote":"https://bitbucket.org/team/worker.git","baseBranch":"main"}
            ],
            "auth": {"groups": [{
                "id": "bb-cloud", "name": "Bitbucket API", "provider": "bitbucket",
                "host": "bitbucket.org", "repos": ["bb"],
                "tokenTypes": ["atlassian_api_token"]
            }]}
        });
        std::fs::write(&path, serde_json::to_vec(&project).unwrap()).unwrap();
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    }

    /// The child body for every grouped scenario; dispatched by
    /// KNIT_GROUP_SETUP_MODE so each runs with an isolated KNIT_HOME.
    #[test]
    fn grouped_setup_child() {
        let Ok(mode) = std::env::var("KNIT_GROUP_SETUP_MODE") else {
            return;
        };
        let root = std::env::current_dir().unwrap();
        let project: KnitProject = serde_json::from_slice(
            &std::fs::read(root.join(".knit/projects/tools.project.json")).unwrap(),
        )
        .unwrap();
        let key = auth::project_key(&root, "tools").unwrap();
        let scripted = |answers: &[&str]| {
            let mut queue: Vec<String> = answers.iter().map(|a| a.to_string()).collect();
            let ask = move |prompt: &str| -> Result<String> {
                if queue.is_empty() {
                    panic!("unexpected prompt: {prompt}");
                }
                // Echo the prompt like the real reader so transcript
                // assertions see the same output a user would.
                print!("{prompt}");
                Ok(queue.remove(0))
            };
            ask
        };
        match mode.as_str() {
            "two-forges" | "scoped" | "mixed-case" => {
                let mut project = project.clone();
                if mode == "mixed-case" {
                    project.auth.as_mut().unwrap().groups[0].host = "GitHub.COM".into();
                }
                let ids = if mode == "two-forges" {
                    vec![]
                } else {
                    vec!["api".into()]
                };
                let answers = if ids.is_empty() {
                    vec!["t", "t", "dev@example.com", "t"]
                } else {
                    vec!["invalid", "", "t"]
                };
                setup_with_prompt(&root, &project, &ids, &mut scripted(&answers), &mut |_| {
                    Ok("synthetic-secret".into())
                })
                .unwrap();
                let store = auth::load().unwrap();
                assert!(
                    store.defaults.is_empty(),
                    "project setup must never set global defaults"
                );
                let gh = &store.projects[&key]["api"];
                assert_eq!(store.credentials[gh].host, "github.com");
                assert!(store.scoped_credentials.contains(gh));
                assert!(store.default_for_host("github.com").is_none());
                if ids.is_empty() {
                    assert_eq!(store.projects[&key]["web"], *gh);
                    let bb = &store.credentials[&store.projects[&key]["bb"]];
                    assert_eq!(bb.username.as_deref(), Some("dev@example.com"));
                    assert_eq!(bb.token_type.as_deref(), Some("atlassian_api_token"));
                    assert_ne!(store.projects[&key]["bb"], store.projects[&key]["bb2"]);
                    assert_eq!(store.projects[&key].len(), 4);
                } else {
                    assert_eq!(store.projects[&key].len(), 1);
                }
                // Choosing a default clears only the selected override and
                // does not mutate any token, recommendation, or other repo.
                let mut defaults = store.clone();
                defaults
                    .credentials
                    .insert("shared".into(), store.credentials[gh].clone());
                defaults
                    .defaults
                    .insert("github.com".into(), "shared".into());
                auth::save(&defaults).unwrap();
                setup_with_prompt(
                    &root,
                    &project,
                    &["api".into()],
                    &mut scripted(&[""]),
                    &mut |_| panic!("default must not ask for a token"),
                )
                .unwrap();
                let after = auth::load().unwrap();
                assert!(after
                    .projects
                    .get(&key)
                    .is_none_or(|p| !p.contains_key("api")));
                assert_eq!(after.defaults, defaults.defaults);
                assert_eq!(
                    serde_json::to_value(&after.credentials).unwrap(),
                    serde_json::to_value(&defaults.credentials).unwrap()
                );
                if ids.is_empty() {
                    assert_eq!(after.projects[&key]["web"], *gh);
                }
            }
            "scoped-reuse" => {
                let mut project = project.clone();
                project.auth.as_mut().unwrap().groups.truncate(1);
                let spec = CredentialSpec {
                    provider: "github".into(),
                    host: "github.com".into(),
                    username: None,
                    token_type: Some("fine_grained_pat".into()),
                    token_env: None,
                };
                save_new_credential_scoped("another-project", &spec, "private-project-token", true)
                    .unwrap();
                let mut prompts = 0;
                guided_group_setup(
                    &root,
                    &project,
                    &BTreeSet::new(),
                    &mut scripted(&[]),
                    &mut |_| {
                        prompts += 1;
                        Ok("new-default-token".into())
                    },
                )
                .unwrap();
                assert_eq!(
                    prompts, 1,
                    "another project's scoped token must not be auto-selected"
                );
                let store = auth::load().unwrap();
                assert_ne!(
                    store.default_for_host("github.com").unwrap().0,
                    "another-project"
                );
                assert_eq!(
                    auth::credential("another-project").unwrap().token,
                    "private-project-token"
                );
                assert!(store.projects.get(&key).is_none_or(|p| p.is_empty()));
            }
            "typo" => {
                let error = format!(
                    "{:#}",
                    setup_with_prompt(&root, &project, &[], &mut scripted(&[]), &mut |_| panic!(
                        "invalid group must not ask for token"
                    ))
                    .unwrap_err()
                );
                assert!(error.contains("unknown repository `appp`"), "{error}");
            }
            // Successful probes must not flip group coverage to missing.
            "status-check" => {
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "gh1".into(),
                    CredentialSpec {
                        provider: "github".into(),
                        host: "github.com".into(),
                        username: None,
                        token_type: Some("fine_grained_pat".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                // A second GitHub credential keeps the host default-less, so
                // the ungrouped `pub` repository's ambient allowance — not an
                // implicit default — is what status exercises.
                store.credentials.insert(
                    "gh2".into(),
                    CredentialSpec {
                        provider: "github".into(),
                        host: "github.com".into(),
                        username: None,
                        token_type: Some("classic_pat".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.credentials.insert(
                    "bb1".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: None,
                        token_type: Some("access_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.projects.insert(
                    key,
                    [
                        ("api".to_string(), "gh1".to_string()),
                        ("web".to_string(), "gh1".to_string()),
                        ("bb".to_string(), "bb1".to_string()),
                        ("bb2".to_string(), "bb1".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                );
                auth::save(&store).unwrap();
                status_project_with(&root, &project, true, false, &mut |_, _| Ok(())).unwrap();
                // The child's own test harness appends its summary after the
                // JSON document; sentinel the document so the parent can
                // slice exactly what status printed.
                println!("<<GROUP-STATUS-JSON>>");
                status_project_with(&root, &project, true, true, &mut |_, _| Ok(())).unwrap();
                println!("<<GROUP-STATUS-JSON-END>>");
            }
            // Already-classified Bitbucket credentials still get their email
            // repaired: an API token missing it (or carrying a username that
            // is not a valid email) is asked for the real one, an access
            // token carrying a stale one offers to clear it.
            "classified" => {
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "api-tok".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: None,
                        token_type: Some("atlassian_api_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.credentials.insert(
                    "acc-tok".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: Some("stale@example.com".into()),
                        token_type: Some("access_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.credentials.insert(
                    "bad-email".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: Some("git-handle-not-email".into()),
                        token_type: Some("atlassian_api_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                auth::save(&store).unwrap();
                for name in ["api-tok", "acc-tok", "bad-email"] {
                    let spec = &store.credentials[name];
                    let mut ask = scripted(match name {
                        "api-tok" => &["dev@example.com"],
                        "acc-tok" => &["y"],
                        // First answer is rejected (no @), so the validator
                        // loop asks again for a real address.
                        _ => &["still-not-an-email", "fixed@example.org"],
                    });
                    let kind = ensure_bitbucket_token_type(name, spec, &mut ask).unwrap();
                    reconcile_bitbucket_username(name, &kind, spec.username.as_deref(), &mut ask)
                        .unwrap();
                }
                let store = auth::load().unwrap();
                assert_eq!(
                    store.credentials["api-tok"].username.as_deref(),
                    Some("dev@example.com")
                );
                assert_eq!(store.credentials["acc-tok"].username, None);
                assert_eq!(
                    store.credentials["bad-email"].username.as_deref(),
                    Some("fixed@example.org")
                );
            }
            // Status fails clearly on invalid group declarations instead of
            // rendering coverage over a collapsed map.
            "status-invalid" => {
                let error = format!(
                    "{:#}",
                    status_project(&root, &project, true, false).unwrap_err()
                );
                assert!(error.contains("unknown repository `appp`"), "{error}");
                let path = root.join(".knit/projects/tools.project.json");
                let mut valid = serde_json::to_value(&project).unwrap();
                // Off-host group: the repository sits on another forge host.
                valid["auth"]["groups"][0]["repos"] = json!(["api", "web"]);
                valid["auth"]["groups"][0]["host"] = json!("ghe.example.com");
                std::fs::write(&path, serde_json::to_vec(&valid).unwrap()).unwrap();
                let offhost: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let error = format!(
                    "{:#}",
                    status_project(&root, &offhost, true, false).unwrap_err()
                );
                assert!(error.contains("is for host"), "{error}");
                // Duplicate references across groups must not collapse.
                valid["auth"]["groups"][0]["host"] = json!("github.com");
                valid["auth"]["groups"][3]["repos"] = json!(["api"]);
                std::fs::write(&path, serde_json::to_vec(&valid).unwrap()).unwrap();
                let duplicate: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let error = format!(
                    "{:#}",
                    status_project(&root, &duplicate, true, false).unwrap_err()
                );
                assert!(error.contains("more than one auth group"), "{error}");
            }
            // An ungrouped public repository cloned with ambient access: a
            // recorded allowance for its exact remote keeps status green and
            // probeable without counting it as a linked credential, and a
            // URL change revokes the allowance.
            "status-ambient" => {
                let path = root.join(".knit/projects/tools.project.json");
                let mut with_pub = serde_json::to_value(&project).unwrap();
                with_pub["repos"].as_array_mut().unwrap().push(json!({
                    "id":"pub","path":"pub",
                    "remote":"https://github.com/org/pub.git","baseBranch":"main"}));
                std::fs::write(&path, serde_json::to_vec(&with_pub).unwrap()).unwrap();
                let project: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "gh1".into(),
                    CredentialSpec {
                        provider: "github".into(),
                        host: "github.com".into(),
                        username: None,
                        token_type: Some("fine_grained_pat".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                // A second GitHub credential keeps the host default-less, so
                // the ungrouped `pub` repository's ambient allowance — not an
                // implicit default — is what status exercises.
                store.credentials.insert(
                    "gh2".into(),
                    CredentialSpec {
                        provider: "github".into(),
                        host: "github.com".into(),
                        username: None,
                        token_type: Some("classic_pat".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.credentials.insert(
                    "bb1".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: None,
                        token_type: Some("access_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.projects.insert(
                    key.clone(),
                    [
                        ("api".to_string(), "gh1".to_string()),
                        ("web".to_string(), "gh1".to_string()),
                        ("bb".to_string(), "bb1".to_string()),
                        ("bb2".to_string(), "bb1".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                );
                store.ambient.insert(
                    key.clone(),
                    BTreeMap::from([("pub".to_string(), "github.com/org/pub".to_string())]),
                );
                auth::save(&store).unwrap();
                println!("<<AMBIENT-HUMAN>>");
                status_project_with(&root, &project, false, false, &mut |_, _| Ok(())).unwrap();
                println!("<<AMBIENT-HUMAN-END>>");
                let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
                let recorder = probed.clone();
                status_project_with(&root, &project, true, false, &mut move |_, remote| {
                    recorder.borrow_mut().push(remote.to_string());
                    Ok(())
                })
                .unwrap();
                println!(
                    "<<AMBIENT-PROBES>>{}<<AMBIENT-PROBES-END>>",
                    probed.borrow().join(",")
                );
                println!("<<AMBIENT-STATUS-JSON>>");
                status_project_with(&root, &project, true, true, &mut |_, _| Ok(())).unwrap();
                println!("<<AMBIENT-STATUS-JSON-END>>");
                // URL change: the recorded allowance no longer matches, so
                // the repository fails closed again.
                let mut moved = with_pub.clone();
                moved["repos"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|repo| repo["id"] == "pub")
                    .unwrap()["remote"] = json!("https://github.com/org/pub-renamed.git");
                std::fs::write(&path, serde_json::to_vec(&moved).unwrap()).unwrap();
                let moved: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let error = format!(
                    "{:#}",
                    status_project_with(&root, &moved, true, false, &mut |_, _| Ok(()))
                        .unwrap_err()
                );
                assert!(
                    error.contains("Some repositories need attention"),
                    "{error}"
                );
                println!("<<AMBIENT-REVOKED>>");
                // Declaring a group over the previously-ambient repository
                // makes the recorded allowance stale: status must demand the
                // group's credential again instead of silently keeping
                // ambient access.
                let mut regrouped = with_pub.clone();
                regrouped["auth"]["groups"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|group| group["id"] == "gh-work")
                    .unwrap()["repos"] = json!(["api", "web", "pub"]);
                std::fs::write(&path, serde_json::to_vec(&regrouped).unwrap()).unwrap();
                let regrouped: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                println!("<<AMBIENT-REGROUPED-HUMAN>>");
                status_project_with(&root, &regrouped, false, false, &mut |_, _| Ok(())).unwrap();
                println!("<<AMBIENT-REGROUPED-HUMAN-END>>");
                let error = format!(
                    "{:#}",
                    status_project_with(&root, &regrouped, true, false, &mut |_, _| Ok(()))
                        .unwrap_err()
                );
                assert!(
                    error.contains("Some repositories need attention"),
                    "{error}"
                );
                println!("<<AMBIENT-REGROUPED-DONE>>");
            }
            // The inferred fallback resolves Bitbucket's actual token mode
            // and account email (so Git gets x-bitbucket-api-token-auth),
            // while GitHub's opaque token stays honestly unclassified.
            "inferred-bitbucket" => {
                let mut ungrouped = project.clone();
                ungrouped.auth = None;
                repair_with_prompt(
                    &root,
                    &ungrouped,
                    &[
                        (
                            "api".to_string(),
                            "https://github.com/org/api.git".to_string(),
                        ),
                        (
                            "bb".to_string(),
                            "https://bitbucket.org/team/backend.git".to_string(),
                        ),
                    ],
                    &mut scripted(&["atlassian_api_token", "dev@example.org"]),
                    &mut |_prompt| Ok("inferred-secret".to_string()),
                )
                .unwrap();

                let store = auth::load().unwrap();
                let bb = &store.credentials["bitbucket.org"];
                assert_eq!(bb.token_type.as_deref(), Some("atlassian_api_token"));
                assert_eq!(bb.username.as_deref(), Some("dev@example.org"));
                assert_eq!(
                    auth::ResolvedCredential {
                        name: "bitbucket.org".into(),
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: bb.username.clone().unwrap(),
                        token_type: bb.token_type.clone(),
                        token: String::new(),
                    }
                    .git_username(),
                    "x-bitbucket-api-token-auth"
                );
                let gh = &store.credentials["github.com"];
                assert_eq!(gh.token_type, None);
                assert_eq!(store.defaults["github.com"], "github.com");
                assert_eq!(store.defaults["bitbucket.org"], "bitbucket.org");
                assert!(!store.projects.contains_key(&key));
            }
            // Replacing an environment-backed credential's rejected token
            // creates a new local credential: no environment reference, the
            // kind chosen afresh (the old classification was rejected with
            // the token), and the Atlassian email validated.
            "env-repair" => {
                // Narrow the project to the one Bitbucket group so the
                // scripted run exercises the repair path directly.
                let path = root.join(".knit/projects/tools.project.json");
                let bb_only = json!({
                    "schemaVersion":"0.1", "kind":"KnitProject", "id":"tools",
                    "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
                    "repos":[{"id":"bb","path":"bb","remote":"https://bitbucket.org/team/backend.git","baseBranch":"main"}],
                    "auth": {"groups": [{
                        "id": "bb-cloud", "name": "Bitbucket API", "provider": "bitbucket",
                        "host": "bitbucket.org", "repos": ["bb"],
                        "tokenTypes": ["atlassian_api_token"]
                    }]}
                });
                std::fs::write(&path, serde_json::to_vec(&bb_only).unwrap()).unwrap();
                let project: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "bb-env".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: None,
                        token_type: Some("atlassian_api_token".into()),
                        token_env: Some("KNIT_WIZARD_TOKEN".into()),
                    },
                );
                store.projects.insert(
                    key.clone(),
                    BTreeMap::from([("bb".to_string(), "bb-env".to_string())]),
                );
                auth::save(&store).unwrap();
                let repair = BTreeSet::from(["bb".to_string()]);
                guided_group_setup(
                    &root,
                    &project,
                    &repair,
                    &mut scripted(&["atlassian_api_token", "dev@example.org"]),
                    &mut |_prompt| Ok("replacement-secret".to_string()),
                )
                .unwrap();
                let store = auth::load().unwrap();
                let new_name = store.projects[&key]["bb"].clone();
                assert_ne!(new_name, "bb-env", "failing repo must be rebound");
                let new_spec = &store.credentials[&new_name];
                assert_eq!(
                    new_spec.token_env, None,
                    "replacement must not ride the env var"
                );
                assert_eq!(new_spec.token_type.as_deref(), Some("atlassian_api_token"));
                assert_eq!(new_spec.username.as_deref(), Some("dev@example.org"));
                assert_eq!(
                    auth::credential(&new_name).unwrap().token,
                    "replacement-secret"
                );
                assert_eq!(
                    store.credentials["bb-env"].token_env.as_deref(),
                    Some("KNIT_WIZARD_TOKEN"),
                    "the env-backed credential keeps its reference"
                );
                assert_eq!(
                    store.credentials["bb-env"].token_type.as_deref(),
                    Some("atlassian_api_token"),
                    "the env-backed credential keeps its classification"
                );
            }
            // A rejected Bitbucket credential's replacement token is
            // classified afresh instead of inheriting the rejected
            // credential's metadata.
            "repair-replace" => {
                let project = bitbucket_repair_project(&root);
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "bb-mis".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: Some("dev@example.org".into()),
                        token_type: Some("atlassian_api_token".into()),
                        token_env: None,
                    },
                );
                auth::save_credential(&store, "bb-mis", Some("original-secret")).unwrap();
                let mut store = auth::load().unwrap();
                store
                    .defaults
                    .insert("bitbucket.org".into(), "bb-mis".into());
                store.projects.insert(
                    key.clone(),
                    BTreeMap::from([
                        ("bb".to_string(), "bb-mis".to_string()),
                        ("bb2".to_string(), "bb-mis".to_string()),
                    ]),
                );
                auth::save(&store).unwrap();
                guided_group_setup(
                    &root,
                    &project,
                    &BTreeSet::from(["bb".to_string()]),
                    &mut scripted(&["access_token"]),
                    &mut |_prompt| Ok("replacement-secret".to_string()),
                )
                .unwrap();
                let store = auth::load().unwrap();
                let new_name = store.projects[&key]["bb"].clone();
                assert_ne!(new_name, "bb-mis");
                let spec = &store.credentials[&new_name];
                assert_eq!(
                    spec.token_type.as_deref(),
                    Some("access_token"),
                    "replacement kind must be chosen afresh, not inherited"
                );
                assert_eq!(spec.username, None);
                assert_eq!(spec.token_env, None);
                assert_eq!(
                    auth::credential(&new_name).unwrap().token,
                    "replacement-secret"
                );
                assert_eq!(auth::credential("bb-mis").unwrap().token, "original-secret");
                assert_eq!(
                    store.credentials["bb-mis"].token_type.as_deref(),
                    Some("atlassian_api_token")
                );
                assert_eq!(
                    store.defaults.get("bitbucket.org").map(String::as_str),
                    Some("bb-mis")
                );
                assert_eq!(store.projects[&key]["bb2"], "bb-mis");
            }
            // Leaving the replacement prompt empty changes nothing: the
            // original keeps its classification, token, default, and both
            // repository bindings, and the run reports that no credential
            // changes were made.
            "repair-decline" => {
                let project = bitbucket_repair_project(&root);
                let mut store = auth::load().unwrap();
                store.credentials.insert(
                    "bb-mis".into(),
                    CredentialSpec {
                        provider: "bitbucket".into(),
                        host: "bitbucket.org".into(),
                        username: Some("dev@example.org".into()),
                        token_type: Some("atlassian_api_token".into()),
                        token_env: None,
                    },
                );
                auth::save_credential(&store, "bb-mis", Some("original-secret")).unwrap();
                let mut store = auth::load().unwrap();
                store
                    .defaults
                    .insert("bitbucket.org".into(), "bb-mis".into());
                store.projects.insert(
                    key.clone(),
                    BTreeMap::from([
                        ("bb".to_string(), "bb-mis".to_string()),
                        ("bb2".to_string(), "bb-mis".to_string()),
                    ]),
                );
                auth::save(&store).unwrap();
                guided_group_setup(
                    &root,
                    &project,
                    &BTreeSet::from(["bb".to_string()]),
                    &mut scripted(&[]),
                    &mut |_prompt| Ok(String::new()),
                )
                .unwrap();
                let store = auth::load().unwrap();
                assert_eq!(
                    store.credentials.len(),
                    1,
                    "no new credential may be created by a declined repair"
                );
                let original = &store.credentials["bb-mis"];
                assert_eq!(original.token_type.as_deref(), Some("atlassian_api_token"));
                assert_eq!(original.username.as_deref(), Some("dev@example.org"));
                assert_eq!(original.token_env, None);
                assert_eq!(auth::credential("bb-mis").unwrap().token, "original-secret");
                assert!(store.scoped_credentials.is_empty());
                assert_eq!(
                    store.defaults.get("bitbucket.org").map(String::as_str),
                    Some("bb-mis")
                );
                assert_eq!(store.projects[&key]["bb"], "bb-mis");
                assert_eq!(store.projects[&key]["bb2"], "bb-mis");
            }
            other => panic!("unknown mode {other}"),
        }
    }

    #[test]
    fn project_setup_preserves_scope_defaults_and_group_guidance() {
        for mode in ["two-forges", "scoped", "mixed-case"] {
            let (stdout, stderr, ok) = run_grouped_child(mode, false);
            assert!(ok, "{mode}: {stdout}\n{stderr}");
            assert!(stdout.contains("Permissions: contents:read, pull_requests:write"));
            assert!(stdout.contains(
                "Create a token at: https://github.com/settings/personal-access-tokens/new"
            ));
            assert!(stdout
                .contains("Instructions: Create the token in the org, scoped to the two repos."));
            assert!(stdout.contains("Skipping group `out-scope`"));
        }
    }

    #[test]
    fn clone_does_not_reuse_another_projects_scoped_token() {
        let (stdout, stderr, ok) = run_grouped_child("scoped-reuse", false);
        assert!(ok, "{stdout}\n{stderr}");
    }

    #[test]
    fn grouped_setup_rejects_broken_group_config_clearly() {
        let (stdout, stderr, ok) = run_grouped_child("typo", true);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    }

    #[test]
    fn grouped_setup_repairs_already_classified_bitbucket_credentials() {
        let (stdout, stderr, ok) = run_grouped_child("classified", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        for expected in [
            "Atlassian account email for this API token: ",
            "Recorded the account email for `api-tok`.",
            "must not use one. Clear the email?",
            "Cleared the account email from `acc-tok`.",
        ] {
            assert!(
                stdout.contains(expected),
                "missing {expected:?} in:\n{stdout}"
            );
        }
    }

    #[test]
    fn grouped_status_fails_clearly_on_invalid_group_declarations() {
        let (stdout, stderr, ok) = run_grouped_child("status-invalid", true);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    }

    #[test]
    fn grouped_status_check_success_keeps_group_coverage_linked() {
        let (stdout, stderr, ok) = run_grouped_child("status-check", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("Git read access confirmed"), "{stdout}");
        assert!(
            stdout.contains("Auth group gh-work (github @ github.com): 2/2 repository(ies) linked"),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "Auth group bb-cloud (bitbucket @ bitbucket.org): 1/1 repository(ies) linked"
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "Auth group bb-second (bitbucket @ bitbucket.org): 1/1 repository(ies) linked"
            ),
            "{stdout}"
        );
        // The regression this guards: coverage must not report a linked
        // repository as missing after a successful probe rewrote the status
        // line. (`out-scope` reporting `extra` is the correct kind of
        // missing: a repo this workspace never cloned.)
        assert!(
            !stdout.contains("gh-work (github @ github.com): 1/2"),
            "{stdout}"
        );
        assert!(
            !stdout.contains("bb-cloud (bitbucket @ bitbucket.org): 0/1"),
            "{stdout}"
        );
        let json_start =
            stdout.find("<<GROUP-STATUS-JSON>>").unwrap() + "<<GROUP-STATUS-JSON>>".len();
        let json_end = stdout.find("<<GROUP-STATUS-JSON-END>>").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(stdout[json_start..json_end].trim()).unwrap();
        let groups = json["authGroups"].as_array().unwrap();
        assert_eq!(groups[0]["linked"], 2);
        assert_eq!(groups[0]["missing"], serde_json::json!([]));
        assert_eq!(groups[2]["linked"], 1);
        assert_eq!(groups[3]["missing"], serde_json::json!(["extra"]));
        assert_eq!(json["reposWithoutAuthGroup"], serde_json::json!([]));
    }

    #[test]
    fn grouped_status_recognizes_recorded_ambient_allowances() {
        let (stdout, stderr, ok) = run_grouped_child("status-ambient", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        let human_start = stdout.find("<<AMBIENT-HUMAN>>").unwrap() + "<<AMBIENT-HUMAN>>".len();
        let human_end = stdout.find("<<AMBIENT-HUMAN-END>>").unwrap();
        let human = &stdout[human_start..human_end];
        assert!(
            human.contains("pub\t—\tambient Git access (recorded for this remote)"),
            "ambient row missing in:\n{human}"
        );
        assert!(!human.contains("pub\t—\tneeds credential assignment"));
        let probes_start = stdout.find("<<AMBIENT-PROBES>>").unwrap() + "<<AMBIENT-PROBES>>".len();
        let probes_end = stdout.find("<<AMBIENT-PROBES-END>>").unwrap();
        let probes = &stdout[probes_start..probes_end];
        assert!(
            probes.contains("https://github.com/org/pub.git"),
            "ambient repo was not probed by --check: {probes}"
        );
        let json_start =
            stdout.find("<<AMBIENT-STATUS-JSON>>").unwrap() + "<<AMBIENT-STATUS-JSON>>".len();
        let json_end = stdout.find("<<AMBIENT-STATUS-JSON-END>>").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(stdout[json_start..json_end].trim()).unwrap();
        let pub_row = json["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["repo"] == "pub")
            .unwrap();
        assert_eq!(pub_row["ambient"], serde_json::json!(true));
        assert_eq!(pub_row["credential"], serde_json::json!(null));
        // No false linked counts: the grouped repositories keep exactly the
        // links they had, and the ambient repo stays a non-link.
        let groups = json["authGroups"].as_array().unwrap();
        assert_eq!(groups[0]["linked"], 2);
        assert_eq!(groups[1]["linked"], 1);
        assert_eq!(json["reposWithoutAuthGroup"], serde_json::json!(["pub"]));
        // The URL change revoked the allowance in the same child run.
        assert!(stdout.contains("<<AMBIENT-REVOKED>>"), "{stdout}");
        // Declaring a group over the ambient repository stales the
        // allowance: the row demands the group's credential and --check
        // fails until one is assigned.
        let regrouped_start = stdout
            .find("<<AMBIENT-REGROUPED-HUMAN>>")
            .map(|at| at + "<<AMBIENT-REGROUPED-HUMAN>>".len())
            .unwrap();
        let regrouped_end = stdout.find("<<AMBIENT-REGROUPED-HUMAN-END>>").unwrap();
        let regrouped = &stdout[regrouped_start..regrouped_end];
        assert!(
            regrouped.contains("pub\t—\tneeds credential assignment (auth group `gh-work`)"),
            "declared member must not keep ambient access:\n{regrouped}"
        );
        assert!(
            regrouped.contains("Auth group gh-work (github @ github.com): 2/3 repository(ies) linked; missing: pub"),
            "{regrouped}"
        );
        assert!(!regrouped.contains("ambient Git access"), "{regrouped}");
        assert!(stdout.contains("<<AMBIENT-REGROUPED-DONE>>"), "{stdout}");
    }

    #[test]
    fn inferred_fallback_resolves_bitbucket_mode_and_email() {
        let (stdout, stderr, ok) = run_grouped_child("inferred-bitbucket", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains("Which kind of Bitbucket token is it? (name/number): "),
            "{stdout}"
        );
        assert!(
            stdout.contains("Atlassian account email for this API token: "),
            "{stdout}"
        );
        // GitHub's opaque token is never asked for a kind.
        assert!(!stdout.contains("Which kind of GitHub"), "{stdout}");
    }

    #[test]
    fn env_backed_replacement_becomes_new_local_credential() {
        let (stdout, stderr, ok) = run_grouped_child("env-repair", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains(
                "Git authentication failed while credential `bb-env` was selected for bb."
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains("Which kind of Bitbucket token is it? (name/number): "),
            "the replacement token's kind must be chosen afresh:\n{stdout}"
        );
    }

    #[test]
    fn guided_bitbucket_replacement_token_is_classified_anew() {
        let (stdout, stderr, ok) = run_grouped_child("repair-replace", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains(
                "Git authentication failed while credential `bb-mis` was selected for bb."
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains("Saved credentials override ordinary Git credential helpers and SSH"),
            "{stdout}"
        );
        assert!(
            stdout.contains("does not establish that repository access is missing"),
            "{stdout}"
        );
        assert!(
            stdout.contains("Which kind of Bitbucket token is it? (name/number): "),
            "{stdout}"
        );
        assert!(
            !stdout.contains("Recorded `bitbucket.org-bb-cloud` as a `atlassian_api_token` token"),
            "the rejected classification must not be inherited:\n{stdout}"
        );
    }

    #[test]
    fn guided_bitbucket_declined_repair_changes_nothing() {
        let (stdout, stderr, ok) = run_grouped_child("repair-decline", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains(
                "Git authentication failed while credential `bb-mis` was selected for bb."
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains("No credential changes were made."),
            "{stdout}"
        );
        assert!(
            !stdout.contains("Every group is already linked"),
            "a declined repair is not an already-linked setup:\n{stdout}"
        );
        // Neither the reused nor the replacement secret ever reaches the
        // transcript; the token prompts stay hidden in real runs and the
        // mocks never echo values.
        assert!(!stdout.contains("original-secret"), "{stdout}");
        assert!(!stdout.contains("replacement-secret"), "{stdout}");
    }
}
