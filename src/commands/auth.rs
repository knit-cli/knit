//! CLI-owned forge setup; no hosted account or desktop application is required.
use crate::auth::{self, CredentialSpec};
use crate::cli::AuthCommand;
use crate::model::{KnitProject, ProjectRepoEntry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;

pub fn run(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Setup { project, repos } => setup(project.as_deref(), &repos),
        AuthCommand::Add {
            name,
            provider,
            host,
            username,
            token_env,
            token_stdin,
            replace,
        } => {
            let spec = CredentialSpec {
                host: host.unwrap_or_else(|| default_host(&provider).into()),
                provider,
                username,
                token_env,
            };
            add(&name, spec, token_stdin, replace)
        }
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
                println!(
                    "{name}\t{}\t{}\t{count} repository assignments\t{}",
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

fn add(name: &str, spec: CredentialSpec, token_stdin: bool, replace: bool) -> Result<()> {
    validate_name(name)?;
    auth::validate_spec(&spec)?;
    let mut store = auth::load()?;
    if store.credentials.contains_key(name) && !replace {
        bail!("Credential `{name}` exists. Use --replace to rotate it for every assigned project.");
    }
    if let Some(old) = store.credentials.get(name) {
        if old.host != spec.host || old.provider != spec.provider {
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
        if old.host != spec.host || old.provider != spec.provider {
            bail!("A replacement must keep the existing provider and host.");
        }
    }
    let is_env = spec.token_env.is_some();
    store.credentials.insert(name.into(), spec);
    auth::save_credential(&store, name, token.as_deref())?;
    println!("Saved `{name}` in your personal Knit credential store{}. Repository access has not been checked.",
        if is_env { " as an environment reference" } else { " (private file, not encrypted)" });
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
    let repos = selected_repos(&project, ids)?;
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
        if host != spec.host {
            bail!(
                "Credential `{name}` is for {}, but repository `{}` is on {host}.",
                spec.host,
                repo.id
            );
        }
    }
    let key = auth::project_key(&root, &project.id)?;
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
            if !linked {
                missing.push(repo.id.clone());
            }
            format!(
                "{host}/{path} → {}{}",
                name.map(String::as_str).unwrap_or("unassigned"),
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
    // Status can select a project different from the workspace fallback.
    auth::set_project_override(Some(project.id.clone()));
    let store = auth::load()?;
    let key = auth::project_key(&root, &project.id)?;
    let bindings = store.projects.get(&key);
    let explicit = bindings.is_some_and(|p| !p.is_empty());
    let mut failed = false;
    let mut rows = Vec::new();
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
        let mut state = if unsupported {
            failed = true;
            "unsupported forge remote; correct host/protocol/path".to_string()
        } else if target.is_none() {
            "local / no forge remote".to_string()
        } else if let Some(name) = name {
            match auth::credential(name) {
                Ok(c) if target.as_ref().is_some_and(|(h, _)| *h == c.host) => {
                    "configured (unchecked)".into()
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
        } else if explicit {
            failed = true;
            "needs credential assignment".into()
        } else {
            "existing Git/forge authentication".into()
        };
        if check && target.is_some() && (!explicit || state == "configured (unchecked)") {
            // Use the project root and explicit remote, including when checkout does not yet exist.
            match probe_read_access(&root, repo.remote.as_deref().unwrap()) {
                Ok(()) => {
                    state = "Git read access confirmed; API/write permissions unchecked".into()
                }
                Err(_) => {
                    failed = true;
                    state = "Git read access failed; check credential and repository access".into();
                }
            }
        }
        rows.push(json!({"repo": repo.id, "host": target.as_ref().map(|(h,_)| h), "credential": name, "status": state}));
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"project": project.id, "explicit": explicit, "repositories": rows})
            )?
        );
    } else {
        println!("Project: {}", project.id);
        for row in &rows {
            println!(
                "  {}\t{}\t{}",
                row["repo"].as_str().unwrap(),
                row["credential"].as_str().unwrap_or("—"),
                row["status"].as_str().unwrap()
            );
        }
        if !explicit {
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
}
