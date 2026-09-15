//! CLI-owned forge setup; no hosted account or desktop application is required.
use crate::auth::{self, CredentialSpec};
use crate::cli::AuthCommand;
use crate::model::{KnitProject, ProjectAuthGroup, ProjectRepoEntry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Read, Write};
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
/// the first token saved for a forge becomes its default automatically.
fn global_defaults_wizard() -> Result<()> {
    println!(
        "Personal forge tokens — one default token per forge, used for every project, clone, fetch, and push on that forge unless a project overrides it."
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
        println!("  Enter when done");
        let choice = prompt("Forge (1-4, or Enter to finish): ")?;
        if choice.is_empty() {
            println!("Done. Tokens are saved in your personal Knit store; nothing was synced.");
            return Ok(());
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
                let token = read_hidden(&format!("New token for {host} (hidden): "))?;
                let token = token.trim();
                if token.is_empty() {
                    println!("Kept the current token.");
                    continue;
                }
                // A deliberate update rotates the default token in place:
                // same name, same default, new secret. A pasted replacement
                // always becomes a local secret, so an environment reference
                // is cleared first — the two token sources never compete.
                let _lock = auth::lock()?;
                let mut store = auth::load()?;
                if let Some(spec) = store.credentials.get_mut(&name) {
                    spec.token_env = None;
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
                let token = read_hidden(&format!("Token for {host} (hidden): "))?;
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

/// The project-scoped wizard: for each forge this project uses, choose
/// between the shared default token (Enter — clears this project's overrides
/// so inheritance works) and a project-only token (saved as a new local
/// credential bound to just this project's repositories on that forge; the
/// global default and its secret are never touched).
fn project_wizard(project_name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, project) = auth::project_context(&cwd, Some(project_name))?;
    let key = auth::project_key(&root, &project.id)?;
    println!(
        "Project `{}` — per forge, use the shared default token or give this project its own. Other projects are never touched.",
        project.id
    );
    // One entry per forge host the project actually uses.
    let mut by_host: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for repo in &project.repos {
        if let Some(remote) = repo.remote.as_deref() {
            if let Ok((host, _)) = auth::remote_target(remote) {
                by_host.entry(host).or_default().push(repo.id.clone());
            }
        }
    }
    if by_host.is_empty() {
        println!("This project has no forge repositories.");
        return Ok(());
    }
    for (host, repos) in by_host {
        let store = auth::load()?;
        let default = store.default_for_host(&host);
        let overrides: BTreeSet<&String> = repos
            .iter()
            .filter_map(|repo| store.projects.get(&key).and_then(|b| b.get(repo.as_str())))
            .collect();
        let override_note = if let [only] = overrides.iter().collect::<Vec<_>>()[..] {
            format!("project token `{}` on {}", only, repos.join(", "))
        } else if overrides.is_empty() {
            String::new()
        } else {
            format!(
                "project tokens {}",
                overrides
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        match &default {
            Some((name, source)) => {
                let suffix = if override_note.is_empty() {
                    String::new()
                } else {
                    format!("; {override_note}")
                };
                println!(
                    "\n{host} ({}): {} (default token){}{}",
                    repos.join(", "),
                    name,
                    match source {
                        auth::DefaultSource::Chosen => "",
                        auth::DefaultSource::Inherited => {
                            " — the only one saved for this forge"
                        }
                    },
                    suffix
                )
            }
            None => println!(
                "\n{host} ({}): no default token saved{}",
                repos.join(", "),
                override_note
            ),
        }
        let answer = prompt(
            "Use the default token (Enter), `t` for a project-only token, or `s` to skip: ",
        )?;
        match answer.trim() {
            "" => {
                let Some((_, _)) = &default else {
                    println!("No default token for {host}; skipped.");
                    continue;
                };
                // Inherit the default: clear this project's overrides for
                // these repositories so the default applies.
                let _lock = auth::lock()?;
                let mut store = auth::load()?;
                let bindings = store.projects.entry(key.clone()).or_default();
                let cleared: Vec<String> = repos
                    .iter()
                    .filter(|repo| bindings.remove((*repo).as_str()).is_some())
                    .cloned()
                    .collect();
                if store.projects.get(&key).is_some_and(|b| b.is_empty()) {
                    store.projects.remove(&key);
                }
                auth::save(&store)?;
                if cleared.is_empty() {
                    println!("Already using the default token for {host}.");
                } else {
                    println!("Cleared project overrides for {} — the default token for {host} now applies.", cleared.join(", "));
                }
            }
            "t" | "T" => {
                let provider = default
                    .as_ref()
                    .and_then(|(name, _)| store.credentials.get(name))
                    .map(|spec| spec.provider.clone())
                    .unwrap_or_else(|| provider_for_host(&host).to_string());
                println!("{}", permission_help(&provider));
                let token_type = if provider == "bitbucket" {
                    Some(ask_bitbucket_token_type(&mut prompt)?)
                } else {
                    None
                };
                let username = match &token_type {
                    Some(kind) => bitbucket_username_for_token_type(kind, &mut prompt)?,
                    None => None,
                };
                let token = read_hidden(&format!("Project token for {host} (hidden): "))?;
                let token = token.trim();
                if token.is_empty() {
                    println!("Skipped {host}.");
                    continue;
                }
                let spec = CredentialSpec {
                    provider,
                    host: host.clone(),
                    username,
                    token_type,
                    token_env: None,
                };
                let name = unique_credential_name(&format!(
                    "{host}-{}",
                    sanitize_credential_name(&project.id)
                ))?;
                save_new_credential_scoped(&name, &spec, token, true)?;
                assign_in(&root, &project, &repos, &name)?;
                println!(
                    "`{name}` is used for {} in this project only; the default token for {host} is untouched.",
                    repos.join(", ")
                );
            }
            _ => println!("Skipped {host}."),
        }
    }
    println!("Done.");
    Ok(())
}

/// The provider a host belongs to by its well-known name; unknown hosts
/// default to GitHub like the rest of Knit's host detection.
fn provider_for_host(host: &str) -> &str {
    for (_, provider, known) in WIZARD_FORGES {
        if known == host {
            return provider;
        }
    }
    "github"
}

pub fn run(command: AuthCommand) -> Result<()> {
    match command {
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
            operation,
        } => crate::auth_git::credential_helper(&credential, &host, &path, operation),
    }
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
        let token = if token_stdin {
            let mut token = String::new();
            io::stdin()
                .take(65537)
                .read_to_string(&mut token)
                .context("Could not read token from stdin")?;
            if token.len() > 65536 {
                bail!("Token input is too large.");
            }
            token
        } else {
            require_terminal()?;
            rpassword::prompt_password("Token (hidden): ").context("Could not read token")?
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
fn repo_selection(input: &str, eligible: &[String]) -> Result<Vec<String>> {
    if input == "all" {
        return Ok(eligible.to_vec());
    }
    let mut selected = Vec::new();
    for item in input
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        let id = eligible
            .iter()
            .find(|id| id.as_str() == item)
            .or_else(|| {
                item.parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| eligible.get(n))
            })
            .with_context(|| {
                format!(
                    "Unknown or ineligible repository `{item}`. Use displayed IDs/numbers, or all."
                )
            })?;
        if !selected.contains(id) {
            selected.push(id.clone());
        }
    }
    if selected.is_empty() {
        bail!("Choose at least one repository, or Enter to cancel.");
    }
    Ok(selected)
}

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
    setup_with_prompt(&root, &project, ids, &mut prompt)
}

fn setup_with_prompt(
    root: &Path,
    project: &KnitProject,
    ids: &[String],
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    match project.auth.as_ref().filter(|auth| !auth.groups.is_empty()) {
        Some(_) => {
            // Malformed groups are a configuration error, not a reason to
            // quietly drop the project's requirements. Pending membership
            // references (out of clone scope, or not yet cloned) validate
            // against the recorded full membership; typos still fail.
            let pending = auth::load_known_pending_repos(root, &project.id);
            auth::validate_project_auth_with_pending(project, &pending)
                .context("Cannot run grouped auth setup")?;
            group_setup_with_prompt(root, project, ids, ask)
        }
        // Absent or empty groups are the legacy shape: no recommendation,
        // so the unconfigured wizard stays exactly as it was.
        None => legacy_setup_with_prompt(root, project, ids, ask),
    }
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
            if email.contains('@') && !email.contains(char::is_whitespace) {
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
        .is_some_and(|email| email.contains('@'))
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
/// repository/project/workspace access token must not carry one.
fn reconcile_bitbucket_username(
    name: &str,
    token_type: &str,
    has_username: bool,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    if token_type == "atlassian_api_token" {
        if has_username {
            return Ok(());
        }
        let Some(email) = bitbucket_username_for_token_type(token_type, ask)? else {
            return Ok(());
        };
        save_credential_username(name, Some(&email))?;
        println!("Recorded the account email for `{name}`.");
    } else if token_type == "access_token" && has_username {
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

/// Select a credential for one group: a compatible saved one, or a new one.
/// Returns the chosen credential name, or None when the group is skipped.
fn choose_group_credential(
    group: &ProjectAuthGroup,
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<Option<String>> {
    loop {
        let store = auth::load()?;
        let compatible: Vec<&String> = store
            .credentials
            .iter()
            .filter(|(_, spec)| credential_matches_group(spec, group))
            .map(|(name, _)| name)
            .collect();
        if compatible.is_empty() {
            println!(
                "\nNo saved credential matches {} @ {}.",
                group.provider, group.host
            );
        } else {
            println!(
                "\nSaved credentials compatible with {} @ {}:",
                group.provider, group.host
            );
            for (i, name) in compatible.iter().enumerate() {
                let spec = &store.credentials[*name];
                println!(
                    "  {}. {name} ({})",
                    i + 1,
                    spec.token_type
                        .as_deref()
                        .unwrap_or("token type unclassified")
                );
            }
        }
        let choice =
            ask("Credential for this group (name/number), 'new', or Enter to skip it for now: ")?;
        if choice.is_empty() {
            return Ok(None);
        }
        if choice == "new" {
            let token_type = choose_group_token_type(group, ask)?;
            println!("{}", permission_help(&group.provider));
            let name = loop {
                let name = ask("Name for this credential: ")?;
                if name.is_empty() {
                    println!("Choose a name.");
                    continue;
                }
                break name;
            };
            let username = if group.provider == "bitbucket" {
                bitbucket_username_for_token_type(&token_type, ask)?
            } else {
                None
            };
            let env = ask(
                "Token environment variable, or Enter to save a token in a private local file: ",
            )?;
            let spec = CredentialSpec {
                provider: group.provider.clone(),
                // Group hosts are case-insensitive; store the canonical
                // lowercase form so later exact host comparisons hold.
                host: group.host.to_ascii_lowercase(),
                username,
                token_type: Some(token_type),
                token_env: (!env.is_empty()).then_some(env),
            };
            match add(&name, spec, false, false) {
                Ok(()) => return Ok(Some(name)),
                Err(error) => {
                    println!("{error}");
                    continue;
                }
            }
        }
        let selected = compatible
            .iter()
            .find(|name| name.as_str() == choice)
            .or_else(|| {
                choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| compatible.get(n))
            });
        let Some(name) = selected else {
            println!("Choose a displayed credential, 'new', or Enter to skip.");
            continue;
        };
        let name = (*name).clone();
        let spec = store.credentials[&name].clone();
        match spec.token_type.clone() {
            Some(token_type) => {
                // Even an already-classified Bitbucket credential may have
                // drifted: an API token without its account email, or an
                // access token still carrying one, authenticates wrongly.
                if group.provider == "bitbucket" {
                    reconcile_bitbucket_username(&name, &token_type, spec.username.is_some(), ask)?;
                }
                if !group.token_types.contains(&token_type) {
                    let confirmed = ask(&format!(
                        "This credential is a `{token_type}` token; the group recommends {}. Use it anyway? [y/N]: ",
                        group.token_types.join(", ")
                    ))?;
                    if !matches!(confirmed.to_ascii_lowercase().as_str(), "y" | "yes") {
                        continue;
                    }
                }
                return Ok(Some(name));
            }
            None => {
                // An old credential with no recorded kind: ask its owner
                // what it truly is — every kind the provider offers, not
                // just the group's recommendation — instead of guessing from
                // the opaque token value. Group fit is checked afterwards.
                println!("Credential `{name}` has no recorded token type.");
                let options: Vec<String> = crate::model::token_types_for_provider(&group.provider)
                    .iter()
                    .map(|t| t.to_string())
                    .collect();
                for (i, token_type) in options.iter().enumerate() {
                    println!("  {}. {token_type}", i + 1);
                }
                let answer = ask("Which kind of token is it? (name/number, Enter to skip): ")?;
                if answer.is_empty() {
                    println!("Using `{name}` without a recorded token type.");
                    return Ok(Some(name));
                }
                let classified = options.iter().find(|t| *t == answer.as_str()).or_else(|| {
                    answer
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|n| options.get(n))
                });
                let Some(classified) = classified else {
                    println!("Choose one of the listed token types.");
                    continue;
                };
                save_credential_token_type(&name, classified)?;
                println!("Recorded `{name}` as a `{classified}` token.");
                if group.provider == "bitbucket" {
                    reconcile_bitbucket_username(&name, classified, spec.username.is_some(), ask)?;
                }
                if !group.token_types.contains(classified) {
                    let confirmed = ask(&format!(
                        "This credential is a `{classified}` token; the group recommends {}. Use it anyway? [y/N]: ",
                        group.token_types.join(", ")
                    ))?;
                    if !matches!(confirmed.to_ascii_lowercase().as_str(), "y" | "yes") {
                        continue;
                    }
                }
                return Ok(Some(name));
            }
        }
    }
}

/// Show the current and proposed repository mapping for one group, then apply
/// it only after an explicit yes. Only repositories present in this
/// workspace (`local`) can change; `--repo` narrows that further, and the
/// full group coverage is still shown.
fn apply_group_mapping(
    root: &Path,
    project: &KnitProject,
    group: &ProjectAuthGroup,
    local: &[String],
    name: &str,
    ids: &[String],
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<bool> {
    let store = auth::load()?;
    let key = auth::project_key(root, &project.id)?;
    let bindings = store.projects.get(&key);
    let mut to_assign = Vec::new();
    println!("\nProposed mapping for group `{}`:", group.id);
    for repo_id in &group.repos {
        let current = bindings
            .and_then(|b| b.get(repo_id))
            .map(String::as_str)
            .unwrap_or("unassigned");
        if !local.contains(repo_id) {
            println!("  {repo_id}: not in this workspace (out of scope or not cloned yet)");
        } else if !ids.is_empty() && !ids.contains(repo_id) {
            println!("  {repo_id}: {current} [outside --repo scope]");
        } else if current == name {
            println!("  {repo_id}: {current} (unchanged)");
        } else {
            println!("  {repo_id}: {current} -> {name}");
            to_assign.push(repo_id.clone());
        }
    }
    if to_assign.is_empty() {
        println!("Nothing to change for this group.");
        return Ok(false);
    }
    let confirmed = ask("Apply this mapping? [y/N]: ")?;
    if matches!(confirmed.to_ascii_lowercase().as_str(), "y" | "yes") {
        assign_in(root, project, &to_assign, name)?;
        return Ok(true);
    }
    println!("Mapping skipped; existing links kept.");
    Ok(false)
}

/// Interactive setup driven by the project's declared auth groups: show each
/// group's requirements once, pick or create one credential for it, and map
/// its repositories only after an explicit acceptance. Groups are projected
/// onto the repositories this workspace actually has: a group whose
/// repositories were all left out of a scoped clone is reported and skipped,
/// never prompted, and a partially local group maps only its local subset.
pub(crate) fn group_setup_with_prompt(
    root: &Path,
    project: &KnitProject,
    ids: &[String],
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let auth_requirements = project
        .auth
        .as_ref()
        .context("Project has no auth requirements")?;
    let local: std::collections::BTreeSet<&str> =
        project.repos.iter().map(|repo| repo.id.as_str()).collect();
    println!(
        "This project defines {} authentication group(s). Groups describe what to create; nothing is assigned until you accept it.",
        auth_requirements.groups.len()
    );
    for group in &auth_requirements.groups {
        let group_local: Vec<String> = group
            .repos
            .iter()
            .filter(|repo| local.contains(repo.as_str()))
            .cloned()
            .collect();
        let absent: Vec<String> = group
            .repos
            .iter()
            .filter(|repo| !local.contains(repo.as_str()))
            .cloned()
            .collect();
        // The host's default credential covers this group with no prompts and
        // no per-repository links: setup guides missing access only.
        if !group_local.is_empty() {
            let store = auth::load()?;
            if let Some((default, source)) = store.default_for_host(&group.host) {
                let provider_matches = store
                    .credentials
                    .get(&default)
                    .is_some_and(|spec| canonical_provider(&spec.provider) == group.provider);
                if provider_matches {
                    println!(
                        "Host default `{default}`{} covers {} — group `{}` needs no links; existing assignments keep winning.",
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
        describe_group(group, &absent);
        let editable: Vec<String> = if ids.is_empty() {
            group_local.clone()
        } else {
            group_local
                .iter()
                .filter(|repo| ids.contains(repo))
                .cloned()
                .collect()
        };
        if editable.is_empty() {
            if group_local.is_empty() {
                println!(
                    "Skipping group `{}`: none of its repositories are in this workspace.",
                    group.id
                );
            } else {
                println!(
                    "Skipping group `{}`: all of its repositories are outside the --repo scope.",
                    group.id
                );
            }
            continue;
        }
        let Some(name) = choose_group_credential(group, ask)? else {
            println!(
                "Skipped group `{}`; its repositories stay as they are.",
                group.id
            );
            continue;
        };
        // The host's default serves the group without per-repository links —
        // switching the default later applies everywhere. Only a genuine
        // alternative gets mapped.
        if auth::load()?
            .default_for_host(&group.host)
            .is_some_and(|(default, _)| default == name)
        {
            println!(
                "No repository links needed: `{name}` is the default credential for {}.",
                group.host
            );
            continue;
        }
        apply_group_mapping(root, project, group, &group_local, &name, ids, ask)?;
    }
    let missing = show_setup_mapping(root, project, ids)?;
    if !missing.is_empty() {
        println!(
            "Setup incomplete: missing links for {}. Saved links have been kept.",
            missing.join(", ")
        );
        return Ok(());
    }
    println!("All forge repositories have credential links; permissions remain unchecked.");
    let check = ask("Check Git read access now? [y/N]: ")?;
    status_project(
        root,
        project,
        matches!(check.to_ascii_lowercase().as_str(), "y" | "yes"),
        false,
    )
}

/// Guided setup for `knit clone` and pull recovery — the tight path. For each
/// group (already projected to the repositories this clone or recovery
/// actually needs), in order:
///
/// * Repositories the group already links keep their links — assignment fills
///   only the unlinked ones, never overwriting a deliberate per-repo override.
/// * A group whose sibling repositories already share one credential is
///   extended to its unlinked repositories without asking.
/// * Otherwise the saved credentials are tiered: same provider and host with
///   a token kind the group declares (or any, when the group declares none;
///   unclassified ones are labeled honestly) are preferred over classified
///   kinds the group does not declare. Exactly one preferred credential —
///   and none created for a different group in this session — is reused with
///   an announcement; anything less clean offers a numbered choice with a
///   new-token option, so distinct groups on one host stay distinct.
/// * A new token is prompted for directly — hidden, once per group, no
///   credential-name choreography — and saved under an automatic name
///   derived from host and group id.
/// * `repair` names repositories whose authentication just failed despite a
///   link; their group does not get skipped but offers one hidden token that
///   rotates the rejected credential in place.
///
/// `knit auth setup` keeps the full wizard for review and edits.
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
                        "Credential `{name}` was used and access was denied for {}.",
                        failing.join(", ")
                    );
                    let token = read_token(&format!(
                        "Replacement token for {} (saved as a new local credential for {}; `{name}` keeps its token — hidden): ",
                        spec.host,
                        failing.join(", ")
                    ))?;
                    let token = token.trim();
                    if !token.is_empty() {
                        let new_spec = CredentialSpec {
                            provider: spec.provider.clone(),
                            host: spec.host.to_ascii_lowercase(),
                            username: spec.username.clone(),
                            token_type: spec.token_type.clone(),
                            // A pasted replacement never rides the old
                            // environment reference.
                            token_env: None,
                        };
                        let new_name = unique_credential_name(&format!(
                            "{}-{}",
                            sanitize_credential_name(&spec.host),
                            sanitize_credential_name(&group.id)
                        ))?;
                        save_new_credential_scoped(&new_name, &new_spec, token, true)?;
                        if spec.provider == "bitbucket" {
                            let kind = ensure_bitbucket_token_type(&new_name, &new_spec, ask)?;
                            reconcile_bitbucket_username(
                                &new_name,
                                &kind,
                                new_spec.username.is_some(),
                                ask,
                            )?;
                        }
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
            .filter(|(_, spec)| credential_matches_group(spec, group))
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
                        if let Some(name) =
                            create_group_credential(group, ask, read_token, &mut |base| {
                                unique_credential_name(base)
                            })?
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
            _ => match create_group_credential(group, ask, read_token, &mut |base| {
                unique_credential_name(base)
            })? {
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
                reconcile_bitbucket_username(&name, &kind, spec.username.is_some(), ask)?;
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
        println!("Every group is already linked; nothing to set up.");
    }
    Ok(())
}

/// Create a brand-new credential for a guided group: one hidden token
/// prompt, an automatic name, no choreography. `None` means the answer was
/// empty and the group is declined for now.
fn create_group_credential(
    group: &ProjectAuthGroup,
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
    unique_name: &mut impl FnMut(&str) -> Result<String>,
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
    let name = unique_name(&auto_credential_name(group))?;
    save_new_credential(&name, &spec, token)?;
    // The first token entered for a host becomes that host's default.
    if promote_host_default(&spec.host, &name)? {
        println!("`{name}` is now the default credential for {}.", spec.host);
    }
    Ok(Some(name))
}

/// Replace a rejected credential's token in place, keeping its name, spec,
/// and every project assignment that already points at it.
/// Promote a just-created credential to its host's default when the host
/// would otherwise have none: the first token entered for a forge becomes
/// that forge's default everywhere. A second token never displaces it.
fn promote_host_default(host: &str, name: &str) -> Result<bool> {
    let _lock = auth::lock()?;
    let mut store = auth::load()?;
    if auth::stage_default_if_absent(&mut store, host, name) {
        auth::save(&store)?;
        Ok(true)
    } else {
        Ok(false)
    }
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
    auth::save_credential(&store, name, Some(token))?;
    println!(
        "Saved `{name}` in your personal Knit credential store (private file, not encrypted). Repository access has not been checked."
    );
    Ok(())
}

/// Infer one group per distinct forge host from repositories that just failed
/// authenticated access, and guide the user through them: a unique compatible
/// saved credential is linked without a prompt, a missing token is asked for
/// once per host, a rejected existing link is rotated in place, and
/// public/SSH access that already worked is never disturbed. Returns the
/// repo ids that ended up linked to a credential. Never invents required
/// permissions: inferred groups carry no permissions and only the provider's
/// known token kinds.
pub(crate) fn guided_inferred_setup(
    root: &Path,
    project: &KnitProject,
    failing: &[(String, String)],
    ask: &mut impl FnMut(&str) -> Result<String>,
    read_token: &mut impl FnMut(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let mut by_target: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (repo_id, remote) in failing {
        if let Ok((host, _)) = auth::remote_target(remote) {
            by_target
                .entry((host, remote.clone()))
                .or_default()
                .push(repo_id.clone());
        }
    }
    // One group per distinct host: (host, repos).
    let mut by_host: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for ((host, _), repo_ids) in &by_target {
        by_host
            .entry(host.clone())
            .or_default()
            .extend(repo_ids.iter().cloned());
    }
    if by_host.is_empty() {
        return Ok(Vec::new());
    }
    let mut groups = Vec::new();
    for (host, repos) in by_host {
        let provider = crate::providers::for_remote(
            failing
                .iter()
                .find(|(_, remote)| {
                    auth::remote_target(remote)
                        .is_ok_and(|(candidate, _)| candidate.eq_ignore_ascii_case(&host))
                })
                .map(|(_, remote)| remote.as_str())
                .unwrap_or_default(),
        )
        .map(|forge| forge.id().to_owned())
        .or_else(|| {
            // Unknown host: the provider decides the token kinds and the Git
            // username scheme, so one question is warranted.
            match host.as_str() {
                "github.com" => Some("github".to_owned()),
                "gitlab.com" => Some("gitlab".to_owned()),
                "bitbucket.org" => Some("bitbucket".to_owned()),
                _ => None,
            }
        });
        let provider = match provider {
            Some(provider) => provider,
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
        groups.push(ProjectAuthGroup {
            id: host.clone(),
            name: host.clone(),
            host,
            // No invented kinds: the token is asked for once, stays
            // unclassified, and its owner can classify it in `knit auth
            // setup` later.
            token_types: Vec::new(),
            provider,
            repos,
            permissions: Vec::new(),
            instructions: None,
            token_url: None,
        });
    }
    let mut setup_project = project.clone();
    setup_project.auth = Some(crate::model::ProjectAuth {
        groups: groups.clone(),
    });
    let repair: BTreeSet<String> = failing.iter().map(|(repo, _)| repo.clone()).collect();
    guided_group_setup(root, &setup_project, &repair, ask, read_token)?;
    // Every failing repository that has a binding now, or whose host gained
    // a default token (the first token entered becomes the host's default
    // with no per-repository links), is worth a retry.
    let linked = linked_repos(root, project)?;
    let store = auth::load()?;
    Ok(failing
        .iter()
        .filter(|(repo, remote)| {
            linked.contains(repo)
                || auth::remote_target(remote)
                    .ok()
                    .and_then(|(host, _)| store.default_for_host(&host).map(|_| ()))
                    .is_some()
        })
        .map(|(repo, _)| repo.clone())
        .collect())
}

/// Repo ids this project has a credential binding for right now.
fn linked_repos(root: &Path, project: &KnitProject) -> Result<Vec<String>> {
    let key = auth::project_key(root, &project.id)?;
    Ok(auth::load()?
        .projects
        .get(&key)
        .map(|bindings| bindings.keys().cloned().collect())
        .unwrap_or_default())
}

/// Interactive grouped setup before the first private git fetch needs a
/// credential: `knit clone` and pull recovery. Only ever runs with a real
/// terminal: without one the caller reports the groups and the scriptable
/// `knit auth add`/`auth use` path instead of hanging on a prompt. The global
/// project selection the caller had in flight is restored afterwards,
/// whatever happens here.
pub(crate) fn clone_group_setup(root: &Path, project: &KnitProject) -> Result<()> {
    require_terminal()?;
    let previous = auth::current_project_override();
    let result = guided_group_setup(
        root,
        project,
        &BTreeSet::new(),
        &mut prompt,
        &mut |message| rpassword::prompt_password(message).context("Could not read token"),
    );
    auth::set_project_override(previous);
    result
}

/// Guided setup for repositories that just failed authenticated access
/// without declared groups (or alongside them): infer one group per failing
/// host, link or create a credential, and report which repositories can now
/// be retried. Terminal-only like the clone setup.
pub(crate) fn inferred_fallback_setup(
    root: &Path,
    project: &KnitProject,
    failing: &[(String, String)],
) -> Result<Vec<String>> {
    require_terminal()?;
    let previous = auth::current_project_override();
    let result = guided_inferred_setup(root, project, failing, &mut prompt, &mut |message| {
        rpassword::prompt_password(message).context("Could not read token")
    });
    auth::set_project_override(previous);
    result
}

/// Guided setup during pull recovery, with the repositories whose
/// authentication just failed: declared groups whose links were rejected get
/// the repair path (rotate the rejected token) instead of being skipped as
/// "already linked".
pub(crate) fn recovery_group_setup(
    root: &Path,
    project: &KnitProject,
    repair: &BTreeSet<String>,
) -> Result<()> {
    require_terminal()?;
    let previous = auth::current_project_override();
    let result = guided_group_setup(root, project, repair, &mut prompt, &mut |message| {
        rpassword::prompt_password(message).context("Could not read token")
    });
    auth::set_project_override(previous);
    result
}

fn legacy_setup_with_prompt(
    root: &Path,
    project: &KnitProject,
    ids: &[String],
    ask: &mut impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let editable = selected_repos(project, ids)?;
    let hosts: Vec<String> = project
        .repos
        .iter()
        .filter_map(|r| {
            r.remote
                .as_deref()
                .and_then(|r| auth::remote_target(r).ok())
                .map(|(h, _)| h)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    println!("Choose a credential, then link the repositories where Knit should use it.\nOne credential can cover several repositories, even across owners; add more when needed.\nLinks do not grant provider permissions. Once linked, unassigned forge repositories require setup.");
    loop {
        let missing = show_setup_mapping(root, project, ids)?;
        let store = auth::load()?;
        for (name, spec) in &store.credentials {
            if hosts.contains(&spec.host) {
                println!(
                    "  Saved credential: {name} ({}, {})",
                    spec.provider, spec.host
                );
            }
        }
        let choice = ask("Credential name (or use NAME), 'new', or 'done' to finish: ")?;
        if choice == "done" {
            if !missing.is_empty() {
                println!(
                    "Setup incomplete: missing links for {}. Saved links have been kept.",
                    missing.join(", ")
                );
                return Ok(());
            }
            println!("All forge repositories have credential links; permissions remain unchecked.");
            let check = ask("Check Git read access now? [y/N]: ")?;
            return status(
                Some(&project.id),
                matches!(check.to_ascii_lowercase().as_str(), "y" | "yes"),
                false,
            );
        }
        let name = if choice == "new" {
            if hosts.is_empty() {
                println!("No supported forge hosts in this project.");
                continue;
            }
            for (i, host) in hosts.iter().enumerate() {
                println!("  {}. {host}", i + 1);
            }
            let host = ask("Host (name or number): ")?;
            let host = match repo_selection(&host, &hosts) {
                Ok(selected) if selected.len() == 1 => selected[0].clone(),
                _ => {
                    println!("Choose one of the project's hosts.");
                    continue;
                }
            };
            let provider = match host.as_str() {
                "github.com" => "github".into(),
                "gitlab.com" => "gitlab".into(),
                "bitbucket.org" => "bitbucket".into(),
                "codeberg.org" => "forgejo".into(),
                _ => ask("Provider (github/gitlab/bitbucket/forgejo): ")?,
            };
            println!("{}", permission_help(&provider));
            let name = ask("Name for this credential: ")?;
            let username = if provider == "bitbucket" {
                let email = ask("Atlassian account email (API token), or Enter for a repository/project/workspace access token: ")?;
                (!email.is_empty()).then_some(email)
            } else {
                None
            };
            let env = ask(
                "Token environment variable, or Enter to save a token in a private local file: ",
            )?;
            let spec = CredentialSpec {
                provider,
                host,
                username,
                token_type: None,
                token_env: (!env.is_empty()).then_some(env),
            };
            if let Err(err) = add(&name, spec, false, false) {
                println!("{err}");
                continue;
            }
            name
        } else {
            choice.strip_prefix("use ").unwrap_or(&choice).to_owned()
        };
        let store = auth::load()?;
        let Some(spec) = store.credentials.get(&name) else {
            println!("Unknown credential `{name}`. Choose a saved name or new.");
            continue;
        };
        let eligible: Vec<String> = editable
            .iter()
            .filter(|r| {
                r.remote
                    .as_deref()
                    .and_then(|r| auth::remote_target(r).ok())
                    .is_some_and(|(h, _)| h == spec.host)
            })
            .map(|r| r.id.clone())
            .collect();
        if eligible.is_empty() {
            println!("No compatible repositories in the editable selection for `{name}`.");
            continue;
        }
        let key = auth::project_key(root, &project.id)?;
        for (i, id) in eligible.iter().enumerate() {
            let current = store.projects.get(&key).and_then(|p| p.get(id));
            println!(
                "  {}. {id} → {}",
                i + 1,
                current.map(String::as_str).unwrap_or("unassigned")
            );
        }
        loop {
            let selection = ask("Link repositories (IDs/numbers, comma or space separated; all; Enter to cancel). Existing links selected will be replaced: ")?;
            if selection.is_empty() {
                break;
            }
            match repo_selection(&selection, &eligible) {
                Ok(repos) => {
                    assign(&name, Some(&project.id), &repos)?;
                    break;
                }
                Err(err) => println!("{err}"),
            }
        }
    }
}

fn permission_help(provider: &str) -> &'static str {
    match provider {
        "github" => "Create a GitHub token with access to the repositories you will link. Choose fine-grained or classic according to the access you need; provider restrictions still apply. For publish/land: Contents and Pull requests read/write, Metadata read. Use a classic token if your access arrangement requires it. Administration is only needed for collaborator management.",
        "bitbucket" => "Use a Bitbucket repository/project/workspace access token, or an Atlassian API token with Bitbucket repository and pull-request permissions. Reading needs read access; publish/land also need repository and pull-request write access. Create API tokens at https://id.atlassian.com/manage-profile/security/api-tokens .",
        "gitlab" => "Create a GitLab token for these repositories. Publishing/landing needs API access and write_repository for HTTPS Git; read-only workflows need read_api and read_repository.",
        _ => "Create a Forgejo token on this host for the selected repositories. Grant repository read access, plus repository write access for publish/land.",
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
        } else if let Some(name) = name {
            match auth::credential(name) {
                Ok(c) if target.as_ref().is_some_and(|(h, _)| *h == c.host) => {
                    let spec = store.credentials.get(name);
                    let provider_ok = match group {
                        Some(group) => spec.is_some_and(|spec| {
                            canonical_provider(&spec.provider) == group.provider
                        }),
                        // Without a group there is no provider expectation.
                        None => true,
                    };
                    if !provider_ok {
                        failed = true;
                        format!(
                            "credential provider mismatch (group `{}` expects {})",
                            group.map(|g| g.id.as_str()).unwrap_or_default(),
                            group.map(|g| g.provider.as_str()).unwrap_or_default()
                        )
                    } else {
                        covered = true;
                        match group.zip(spec) {
                            Some((group, spec)) => match &spec.token_type {
                                Some(token_type) if !group.token_types.contains(token_type) => {
                                    format!(
                                        "configured (unchecked; `{token_type}` not among group `{}` recommended types)",
                                        group.id
                                    )
                                }
                                _ => "configured (unchecked)".into(),
                            },
                            None => "configured (unchecked)".into(),
                        }
                    }
                }
                Ok(_) => {
                    failed = true;
                    "credential host mismatch".into()
                }
                Err(_) => {
                    failed = true;
                    "credential unavailable".into()
                }
            }
        } else if let Some((default_name, source)) = host_default.clone() {
            // The host's default credential serves this repository with no
            // per-repository assignment. Provider expectations and token
            // availability are validated exactly like an explicit row.
            let spec = store.credentials.get(&default_name);
            let provider_ok = match group {
                Some(group) => {
                    spec.is_some_and(|spec| canonical_provider(&spec.provider) == group.provider)
                }
                None => true,
            };
            if !provider_ok {
                failed = true;
                format!(
                    "host default provider mismatch (group `{}` expects {})",
                    group.map(|g| g.id.as_str()).unwrap_or_default(),
                    group.map(|g| g.provider.as_str()).unwrap_or_default()
                )
            } else {
                match auth::credential(&default_name) {
                    Ok(resolved) if target.as_ref().is_some_and(|(h, _)| *h == resolved.host) => {
                        covered = true;
                        match source {
                            auth::DefaultSource::Chosen => {
                                format!("host default `{default_name}` (unchecked)")
                            }
                            auth::DefaultSource::Inherited => {
                                format!(
                                    "host default `{default_name}` (only credential on host, unchecked)"
                                )
                            }
                        }
                    }
                    Ok(_) => {
                        failed = true;
                        "host default credential host mismatch".into()
                    }
                    Err(_) => {
                        failed = true;
                        "host default credential unavailable".into()
                    }
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

    #[test]
    fn selections_validate_the_whole_list() {
        let eligible = vec!["api".into(), "web".into(), "3".into()];
        assert_eq!(
            repo_selection("api, 2 api", &eligible).unwrap(),
            ["api", "web"]
        );
        assert_eq!(repo_selection("all", &eligible).unwrap(), eligible);
        assert_eq!(repo_selection("3", &eligible).unwrap(), ["3"]);
        for invalid in ["api missing", "0", "4", ",", "all api"] {
            assert!(repo_selection(invalid, &eligible).is_err());
        }
    }

    // Run persistence and cwd changes in a child so concurrent library tests
    // cannot see this synthetic project's personal credential store.
    #[test]
    fn credential_first_wizard() {
        if std::env::var_os("KNIT_WIZARD_TEST_CHILD").is_none() {
            let root = std::env::temp_dir().join(format!("knit-wizard-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::auth::tests::credential_first_wizard",
                    "--nocapture",
                ])
                .current_dir(&root)
                .env("KNIT_WIZARD_TEST_CHILD", "1")
                .env("KNIT_HOME", root.join("home"))
                .env_remove("KNIT_BUNDLE")
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(root);
            let stdout = String::from_utf8_lossy(&result.stdout);
            assert!(
                result.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(stdout.contains("Setup incomplete: missing links for bb"));
            assert!(stdout.contains("[outside --repo scope]"));
            assert!(stdout.contains("All forge repositories have credential links"));
            return;
        }
        let root = std::env::current_dir().unwrap();
        std::fs::create_dir_all(root.join(".knit/projects")).unwrap();
        std::fs::write(
            root.join(".knit/config.json"),
            r#"{"schemaVersion":"0.1","activeProject":"tools"}"#,
        )
        .unwrap();
        let project: KnitProject = serde_json::from_value(json!({
            "schemaVersion":"0.1", "kind":"KnitProject", "id":"tools",
            "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
            "repos":[
                {"id":"api","path":root.join("api"),"remote":"https://github.com/one/api.git","baseBranch":"main"},
                {"id":"web","path":root.join("web"),"remote":"git@github.com:two/web.git","baseBranch":"main"},
                {"id":"bb","path":root.join("bb"),"remote":"https://bitbucket.org/team/backend.git","baseBranch":"main"}
            ]
        })).unwrap();
        std::fs::write(
            root.join(".knit/projects/tools.project.json"),
            serde_json::to_vec(&project).unwrap(),
        )
        .unwrap();
        let key = auth::project_key(&root, "tools").unwrap();
        let run = |ids: &[String], inputs: &[&str]| {
            let mut inputs = inputs.iter();
            setup_with_prompt(&root, &project, ids, &mut |_| {
                Ok(inputs.next().expect("unexpected prompt").to_string())
            })
            .unwrap();
            assert!(inputs.next().is_none(), "unused answers");
        };
        run(&[], &["done"]);
        run(
            &[],
            &[
                "new",
                "github.com",
                "shared",
                "SYNTHETIC_TOKEN",
                "all",
                "done",
            ],
        );
        let links = auth::load().unwrap().projects[&key].clone();
        assert_eq!(links["api"], "shared");
        assert_eq!(links["web"], "shared");
        assert!(!links.contains_key("bb"));
        // A partially valid selection must not change any link.
        run(&[], &["shared", "api,bb", "", "done"]);
        assert_eq!(auth::load().unwrap().projects[&key], links);
        // --repo limits both all and explicit IDs; full coverage is still shown.
        run(
            &["web".into()],
            &[
                "new",
                "github.com",
                "second",
                "SYNTHETIC_TOKEN",
                "api",
                "all",
                "done",
            ],
        );
        let links = auth::load().unwrap().projects[&key].clone();
        assert_eq!(links["api"], "shared");
        assert_eq!(links["web"], "second");
        run(
            &[],
            &[
                "new",
                "bitbucket.org",
                "cloud",
                "account@example.com",
                "SYNTHETIC_TOKEN",
                "1",
                "done",
                "n",
            ],
        );
        assert_eq!(auth::load().unwrap().projects[&key]["bb"], "cloud");
        assert_eq!(
            auth::load().unwrap().credentials["cloud"]
                .username
                .as_deref(),
            Some("account@example.com")
        );
        run(&[], &["shared", "1, web", "done", "n"]);
        assert_eq!(auth::load().unwrap().projects[&key]["web"], "shared");

        // A custom host asks for its backend instead of guessing from an owner.
        let mut custom = project.clone();
        custom.repos[2].remote = Some("https://git.example.test/team/backend.git".into());
        std::fs::write(
            root.join(".knit/projects/tools.project.json"),
            serde_json::to_vec(&custom).unwrap(),
        )
        .unwrap();
        let mut inputs = [
            "new",
            "git.example.test",
            "gitlab",
            "self-hosted",
            "SYNTHETIC_TOKEN",
            "all",
            "done",
            "n",
        ]
        .into_iter();
        setup_with_prompt(&root, &custom, &[], &mut |_| {
            Ok(inputs.next().expect("unexpected custom-host prompt").into())
        })
        .unwrap();
        assert!(inputs.next().is_none());
        let store = auth::load().unwrap();
        assert_eq!(store.credentials["self-hosted"].provider, "gitlab");
        assert_eq!(store.projects[&key]["bb"], "self-hosted");
    }

    // ----- Grouped setup -------------------------------------------------

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
            // Two forges end to end: one new credential per group, the
            // known-type mismatch confirmed explicitly, and the group whose
            // repos are all absent never prompting for a credential.
            "two-forges" => {
                setup_with_prompt(
                    &root,
                    &project,
                    &[],
                    &mut scripted(&[
                        "new",
                        "gh1",
                        "KNIT_WIZARD_TOKEN", // gh-work: first token -> default
                        "new",
                        "bb1",
                        "dev@example.com",
                        "KNIT_WIZARD_TOKEN", // bb-cloud: first token -> default
                        // bb-second reuses the bitbucket default unprompted;
                        // the final read-check is declined.
                        "n",
                    ]),
                )
                .unwrap();
                let store = auth::load().unwrap();
                // The first token on each host became its default, and the
                // groups needed no per-repository links at all: switching a
                // default later applies everywhere.
                assert_eq!(store.defaults["github.com"], "gh1");
                assert_eq!(store.defaults["bitbucket.org"], "bb1");
                assert!(store.projects.get(&key).is_none_or(|p| p.is_empty()));
                assert_eq!(
                    store.credentials["gh1"].token_type.as_deref(),
                    Some("fine_grained_pat")
                );
                assert_eq!(
                    store.credentials["bb1"].token_type.as_deref(),
                    Some("atlassian_api_token")
                );
                assert_eq!(
                    store.credentials["bb1"].username.as_deref(),
                    Some("dev@example.com")
                );
            }
            // Opaque legacy credentials are classified by their owner; an
            // Atlassian API-token classification asks for the missing email,
            // an access-token classification offers to clear a stale one.
            "classify" => {
                let mut store = auth::load().unwrap();
                for (name, provider, host, username) in [
                    ("legacy", "github", "github.com", None),
                    // A second GitHub credential keeps the host default-less,
                    // so the group still guides classification instead of
                    // silently reusing an implicit default.
                    ("legacy2", "github", "github.com", None),
                    ("old-api", "bitbucket", "bitbucket.org", None),
                    (
                        "old-access",
                        "bitbucket",
                        "bitbucket.org",
                        Some("stale@example.com"),
                    ),
                ] {
                    store.credentials.insert(
                        name.into(),
                        CredentialSpec {
                            provider: provider.into(),
                            host: host.into(),
                            username: username.map(str::to_owned),
                            token_type: None,
                            token_env: Some("KNIT_WIZARD_TOKEN".into()),
                        },
                    );
                }
                auth::save(&store).unwrap();
                setup_with_prompt(
                    &root,
                    &project,
                    &[],
                    &mut scripted(&[
                        "legacy",
                        "2",
                        "y",
                        "y", // gh: honest classic, use anyway, apply
                        "old-api",
                        "1",
                        "dev@example.com",
                        "y", // bb: atlassian, email recorded, apply
                        "old-access",
                        "2",
                        "y",
                        "y", // bb2: access, clear stale email, apply
                        "n", // final read-check prompt
                    ]),
                )
                .unwrap();
                let store = auth::load().unwrap();
                assert_eq!(
                    store.credentials["legacy"].token_type.as_deref(),
                    Some("classic_pat")
                );
                assert_eq!(
                    store.credentials["old-api"].token_type.as_deref(),
                    Some("atlassian_api_token")
                );
                assert_eq!(
                    store.credentials["old-api"].username.as_deref(),
                    Some("dev@example.com")
                );
                assert_eq!(
                    store.credentials["old-access"].token_type.as_deref(),
                    Some("access_token")
                );
                assert_eq!(store.credentials["old-access"].username, None);
                assert_eq!(store.projects[&key]["api"], "legacy");
                assert_eq!(store.projects[&key]["bb"], "old-api");
                assert_eq!(store.projects[&key]["bb2"], "old-access");
            }
            // --repo limits edits; coverage is still shown for everything.
            "scoped" => {
                setup_with_prompt(
                    &root,
                    &project,
                    &["api".to_string()],
                    &mut scripted(&["new", "scoped", "KNIT_WIZARD_TOKEN"]),
                )
                .unwrap();
                let store = auth::load().unwrap();
                assert_eq!(store.defaults["github.com"], "scoped");
                assert!(store.projects.get(&key).is_none_or(|p| p.is_empty()));
            }
            // A typo in the group config fails loudly; no manual fallback.
            "typo" => {
                let error = format!(
                    "{:#}",
                    setup_with_prompt(&root, &project, &[], &mut scripted(&[])).unwrap_err()
                );
                assert!(error.contains("Cannot run grouped auth setup"), "{error}");
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
            // repaired: an API token missing it is asked for, an access
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
                auth::save(&store).unwrap();
                setup_with_prompt(
                    &root,
                    &project,
                    &[],
                    &mut scripted(&[
                        "", // gh-work skipped
                        "api-tok",
                        "dev@example.com",
                        "y", // bb-cloud: email repair, apply
                        "acc-tok",
                        "y",
                        "y", // bb-second: clear stale email, apply
                        "n", // final read-check prompt
                    ]),
                )
                .unwrap();
                let store = auth::load().unwrap();
                assert_eq!(
                    store.credentials["api-tok"].username.as_deref(),
                    Some("dev@example.com")
                );
                assert_eq!(store.credentials["acc-tok"].username, None);
                assert_eq!(store.projects[&key]["bb"], "api-tok");
                assert_eq!(store.projects[&key]["bb2"], "acc-tok");
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
            // A mixed-case group host normalizes into the stored credential,
            // and mapping succeeds against the lowercase remote host.
            "mixed-case" => {
                let path = root.join(".knit/projects/tools.project.json");
                let mut mixed = serde_json::to_value(&project).unwrap();
                mixed["auth"]["groups"][0]["host"] = json!("GitHub.COM");
                std::fs::write(&path, serde_json::to_vec(&mixed).unwrap()).unwrap();
                let mixed: KnitProject =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                setup_with_prompt(
                    &root,
                    &mixed,
                    &[],
                    &mut scripted(&[
                        "new",
                        "mc",
                        "KNIT_WIZARD_TOKEN", // gh-work: first token -> default
                        "",                  // skip bb-cloud
                        "",                  // skip bb-second
                    ]),
                )
                .unwrap();
                let store = auth::load().unwrap();
                assert_eq!(store.credentials["mc"].host, "github.com");
                assert_eq!(store.defaults["github.com"], "mc");
                assert!(store.projects.get(&key).is_none_or(|p| p.is_empty()));
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
                let linked = guided_inferred_setup(
                    &root,
                    &project,
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
                assert!(linked.contains(&"api".to_string()));
                assert!(linked.contains(&"bb".to_string()));
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
            // kind preserved, and the Atlassian email reconciled.
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
                    &mut scripted(&["dev@example.org"]),
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
                    store.credentials["bb-env"].token_env.as_deref(),
                    Some("KNIT_WIZARD_TOKEN"),
                    "the env-backed credential keeps its reference"
                );
            }
            other => panic!("unknown mode {other}"),
        }
    }

    #[test]
    fn grouped_setup_creates_one_credential_per_forge_and_skips_absent_groups() {
        let (stdout, stderr, ok) = run_grouped_child("two-forges", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        for expected in [
            "Group gh-work: GitHub work token (github @ github.com)",
            "Token type(s): fine_grained_pat",
            "Permissions: contents:read, pull_requests:write",
            "Create a token at: https://github.com/settings/personal-access-tokens/new",
            "Instructions: Create the token in the org, scoped to the two repos.",
            "Not in this workspace (out of clone scope or not cloned yet): extra",
            "Skipping group `out-scope`: none of its repositories are in this workspace.",
            "No repository links needed: `gh1` is the default credential for github.com.",
            "No repository links needed: `bb1` is the default credential for bitbucket.org.",
            "Host default `bb1` covers bitbucket.org — group `bb-second` needs no links",
            "Atlassian account email for this API token:",
            "github.com/org/api → default `gh1`",
        ] {
            assert!(
                stdout.contains(expected),
                "missing {expected:?} in:\n{stdout}"
            );
        }
    }

    #[test]
    fn grouped_setup_classifies_opaque_credentials_and_fixes_bitbucket_email() {
        let (stdout, stderr, ok) = run_grouped_child("classify", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        for expected in [
            "Credential `legacy` has no recorded token type.",
            "Recorded `legacy` as a `classic_pat` token.",
            "the group recommends fine_grained_pat. Use it anyway?",
            "Recorded the account email for `old-api`.",
            "must not use one. Clear the email?",
            "Cleared the account email from `old-access`.",
        ] {
            assert!(
                stdout.contains(expected),
                "missing {expected:?} in:\n{stdout}"
            );
        }
    }

    #[test]
    fn grouped_setup_scopes_edits_but_reports_full_coverage() {
        let (stdout, stderr, ok) = run_grouped_child("scoped", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains("github.com/org/web → default `scoped` [outside --repo scope]"),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "Skipping group `bb-cloud`: all of its repositories are outside the --repo scope."
            ),
            "{stdout}"
        );
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
    fn grouped_setup_normalizes_mixed_case_group_hosts() {
        let (stdout, stderr, ok) = run_grouped_child("mixed-case", false);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            stdout.contains("github.com/org/api → default `mc`"),
            "{stdout}"
        );
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
            stdout.contains("was used and access was denied for bb."),
            "{stdout}"
        );
        assert!(
            stdout.contains("Recorded the account email for `bitbucket.org-bb-cloud`."),
            "{stdout}"
        );
    }
}
