use crate::commands::agents::write_project_agents_md;
use crate::commands::base::validate_configured_base;
use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::{short_sha, slugify};
use crate::model::{
    CheckoutMode, KnitConfig, KnitProject, ProjectRepoEntry, ProjectRunCommand, PROJECT_CONFIG_FILE,
};
use crate::output as out;
use crate::store::{
    acquire_named_lock, find_knit_root, load_config, project_path, read_json, save_config,
    views_path, write_json,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

pub fn init_project(name: &str, agents: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).unwrap_or(cwd);
    let project_id = slugify(name);
    let knit_dir = root.join(".knit");
    let project_dir = knit_dir.join("projects");
    fs::create_dir_all(&project_dir).context("failed to create .knit/projects")?;
    fs::create_dir_all(knit_dir.join("bundles")).context("failed to create .knit/bundles")?;
    fs::create_dir_all(knit_dir.join("worktrees")).context("failed to create .knit/worktrees")?;

    let path = project_path(&root, &project_id);
    if path.exists() {
        if agents {
            let project: KnitProject = read_json(&path)?;
            let agents_path = write_project_agents_md(&root, &project)?;
            println!(
                "{} {}",
                out::heading("Project AGENTS.md:"),
                out::path(agents_path.display())
            );
            return Ok(());
        }
        bail!("Project {} already exists.", out::path(path.display()));
    }

    let project = KnitProject::new(project_id.clone(), now_iso());
    write_json(&path, &project)?;

    let mut config = if root.join(".knit/config.json").exists() {
        load_config(&root)?
    } else {
        KnitConfig::new_project(project_id.clone())
    };
    config.active_project = Some(project_id.clone());
    save_config(&root, &config)?;

    println!("{} {}", out::heading("Project:"), out::repo(&project_id));
    println!("{} {}", out::heading("Path:"), out::path(path.display()));
    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

pub fn add_project_repo(
    repo_id: &str,
    repo_path: &Path,
    base: Option<&str>,
    observe: bool,
    agents: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root =
        find_knit_root(&cwd).context("No Knit project found. Run `knit init <name>` first.")?;
    let config = load_config(&root)?;
    let project_id = config
        .active_project
        .as_deref()
        .context("No active Knit project. Run `knit init <name>` first.")?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, project_id);
    let mut project: KnitProject = read_json(&path)?;
    let (repo, base_source) = resolve_project_repo(repo_id, repo_path, base, observe)?;

    let resolved_base = repo.base_branch.clone();
    if let Some(existing) = project
        .repos
        .iter_mut()
        .find(|existing| existing.id == repo.id)
    {
        // Re-adding refreshes location and base; publishing preferences set
        // with `knit project set-draft` are not this command's to reset.
        let publish = existing.publish.take();
        *existing = repo.clone();
        existing.publish = publish;
        println!("{} {}", out::movement("updated"), out::repo(&repo.id));
    } else {
        println!("{} {}", out::movement("added"), out::repo(&repo.id));
        project.repos.push(repo);
    }

    project.updated_at = now_iso();
    write_json(&path, &project)?;
    // Repository registration: plain Git in the checkout resolves saved Knit
    // credentials from here on. The explicit workspace/project context pins
    // the helper even for checkouts outside the workspace, where the checkout
    // location cannot reveal the project. Nothing is installed until a
    // credential actually selects this checkout's forge URL, and a setup
    // failure never undoes the registration — it is reported with the
    // recovery path.
    if let Err(error) = crate::auth_git::install_for_project(repo_path, &root, &project) {
        println!(
            "{} plain-Git credential helper not set up for this checkout: {error:#}; run `knit auth status` to repair",
            out::warn("Warning:")
        );
    }
    println!(
        "{} {} ({})",
        out::heading("Base branch:"),
        out::branch(&resolved_base),
        out::muted(base_source)
    );
    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

/// Change only one project repo's configured base. Existing bundles retain
/// their pinned baseBranch/baseSha and are reported so the user can migrate an
/// untouched checkout deliberately instead of silently rewriting its diff and
/// review target.
pub fn set_project_repo_base(
    project_name: Option<&str>,
    repo_id: &str,
    base_branch: &str,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project_name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass --project <name> or run `knit init <name>`.")?;
    let repo_id = slugify(repo_id);
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let repo = project
        .repos
        .iter_mut()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("Project `{project_id}` has no repo `{repo_id}`."))?;

    let validation = validate_configured_base(Path::new(&repo.path), base_branch)
        .with_context(|| format!("{repo_id}: cannot use `{base_branch}` as its configured base"))?;
    let previous = repo.base_branch.clone();
    let pinned = open_bundles_tracking_repo(&root, &project_id, &repo_id)?;
    repo.base_branch = base_branch.trim().to_string();
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    let movement = format!("-> {}", base_branch.trim());
    println!(
        "{} {} {} {} ({}, {})",
        out::heading("Project base:"),
        out::repo(&repo_id),
        out::branch(&previous),
        out::movement(&movement),
        out::muted(validation.source_ref),
        out::sha(short_sha(&validation.sha))
    );

    if !pinned.is_empty() {
        println!(
            "{} existing bundles remain pinned to their recorded bases:",
            out::warn("Note:")
        );
        for (bundle_id, base, sha) in &pinned {
            println!(
                "  {} {} {}",
                out::node(bundle_id),
                out::branch(base),
                sha.as_deref()
                    .map(short_sha)
                    .map(out::sha)
                    .unwrap_or_else(|| out::muted("unrecorded"))
            );
        }
        println!(
            "  For an untouched repo checkout, recreate it safely with `knit --bundle <bundle> bundle remove {} --delete-branch`, then `knit --bundle <bundle> bundle add {}`.",
            repo_id, repo_id
        );
    }
    Ok(())
}

/// Set whether `knit publish create` opens one project repo's review as a
/// draft. `false` removes the setting, so the repo publishes exactly as it did
/// before it was set.
pub fn set_project_repo_draft(
    project_name: Option<&str>,
    repo_id: &str,
    draft: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project_name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass --project <name> or run `knit init <name>`.")?;
    let repo_id = slugify(repo_id);
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let repo = project
        .repos
        .iter_mut()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("Project `{project_id}` has no repo `{repo_id}`."))?;

    repo.publish = draft.then_some(crate::model::ProjectRepoPublish { draft: true });
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    let opens = if draft {
        "opens as a draft"
    } else {
        "opens ready for review"
    };
    println!(
        "{} {} {}",
        out::heading("Project publish:"),
        out::repo(&repo_id),
        out::movement(opens)
    );
    Ok(())
}

/// Set the project or per-repo title/body policy `knit publish create` reads.
/// Draft settings in the same policy are left alone.
pub fn set_project_publish(
    project_name: Option<&str>,
    repo_id: Option<&str>,
    title: Option<&str>,
    body_file: Option<&str>,
    body_fallback: Option<&str>,
    clear: bool,
) -> Result<()> {
    if !clear && title.is_none() && body_file.is_none() && body_fallback.is_none() {
        bail!("Pass --title, --body-file, --body-fallback, or --clear.");
    }
    if title.or(body_file).is_some_and(|v| v.trim().is_empty()) {
        bail!("--title and --body-file need a non-empty value; use --clear to remove them.");
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project_name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass --project <name> or run `knit init <name>`.")?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let mut policy = project.publish.take().unwrap_or_default();
    let mut settings = Vec::new();
    let scope = if let Some(repo_id) = repo_id {
        let repo_id = slugify(repo_id);
        if !project.repos.iter().any(|repo| repo.id == repo_id) {
            bail!("Project `{project_id}` has no repo `{repo_id}`.");
        }
        if body_fallback.is_some() {
            bail!("--body-fallback is project-wide; omit the repo.");
        }
        let entry = policy.repos.entry(repo_id.clone()).or_default();
        if clear {
            entry.title = None;
            entry.body_file = None;
        }
        entry.title = title.map(str::to_owned).or(entry.title.take());
        entry.body_file = body_file.map(str::to_owned).or(entry.body_file.take());
        settings.extend(entry.title.as_ref().map(|t| format!("title \"{t}\"")));
        settings.extend(entry.body_file.as_ref().map(|f| format!("body file {f}")));
        if entry.draft.is_none()
            && entry.title.is_none()
            && entry.body_file.is_none()
            && entry.extensions.is_empty()
        {
            policy.repos.remove(&repo_id);
        }
        repo_id
    } else {
        if clear {
            policy.title = None;
            policy.body = None;
        }
        if let Some(title) = title {
            policy.title = Some(
                serde_json::from_value(title.into()).with_context(|| {
                    format!("--title without a repo must be commit-group, bundle-title, or file, not `{title}`")
                })?,
            );
        }
        if body_file.is_some() || body_fallback.is_some() {
            let body = policy.body.get_or_insert_with(Default::default);
            body.file = body_file.map(str::to_owned).or(body.file.take());
            if let Some(fallback) = body_fallback {
                body.fallback = Some(serde_json::from_value(fallback.into())?);
            }
        }
        let name = |value: serde_json::Value| value.as_str().unwrap_or_default().to_owned();
        settings.extend(
            policy
                .title
                .map(|t| format!("title {}", name(serde_json::json!(t)))),
        );
        if let Some(body) = &policy.body {
            settings.extend(body.file.as_ref().map(|f| format!("body file {f}")));
            settings.extend(
                body.fallback
                    .map(|f| format!("body fallback {}", name(serde_json::json!(f)))),
            );
        }
        "project".to_string()
    };
    let unset = policy.draft.is_none()
        && policy.title.is_none()
        && policy.body.is_none()
        && policy.repos.is_empty()
        && policy.extensions.is_empty();
    project.publish = (!unset).then_some(policy);
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    let summary = if settings.is_empty() {
        "uses default titles and bodies".to_string()
    } else {
        settings.join(", ")
    };
    println!(
        "{} {} {}",
        out::heading("Project publish:"),
        out::repo(&scope),
        out::movement(&summary)
    );
    Ok(())
}

pub fn refresh_project_agents(name: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass a project name or run `knit init <name>`.")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let agents_path = write_project_agents_md(&root, &project)?;
    println!(
        "{} {}",
        out::heading("Project AGENTS.md:"),
        out::path(agents_path.display())
    );
    Ok(())
}

pub fn list_projects() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let active = load_config(&root)?.active_project;
    let dir = root.join(".knit/projects");
    if !dir.exists() {
        println!("{}", out::muted("No projects."));
        return Ok(());
    }

    let mut entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        let project: KnitProject = read_json(&path)?;
        let marker = if active.as_deref() == Some(project.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{} {} {} repo(s)",
            marker,
            out::repo(&project.id),
            project.repos.len()
        );
    }
    Ok(())
}

pub fn show_project(name: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass a project name or run `knit init <name>`.")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let text = serde_json::to_string_pretty(&project).context("failed to serialize project")?;
    println!("{text}");
    Ok(())
}

pub fn remove_project(name: &str, force: bool) -> Result<()> {
    if !force {
        bail!("Removing a project requires --force.");
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = slugify(name);
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    if !path.exists() {
        bail!("No Knit project named `{project_id}` found.");
    }

    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    // The project's saved views are meaningless without it; leaving the file
    // behind would resurrect stale views if a project with the same id is
    // created later.
    let views_file = views_path(&root, &project_id);
    if views_file.exists() {
        fs::remove_file(&views_file)
            .with_context(|| format!("failed to remove {}", views_file.display()))?;
        println!(
            "{} {}",
            out::heading("Removed views:"),
            out::path(views_file.display())
        );
    }
    let mut config = load_config(&root)?;
    if config.active_project.as_deref() == Some(project_id.as_str()) {
        config.active_project = None;
        save_config(&root, &config)?;
    }
    println!(
        "{} {}",
        out::heading("Removed project:"),
        out::repo(project_id)
    );
    Ok(())
}

/// Remove specific repo ids from a project template, leaving the template and
/// every repo checkout on disk in place. This is bookkeeping only: it stops the
/// repo appearing in future bundle starts and lets `knit project push --prune`
/// drop it from the sync remote. Refuses any repo that an open bundle tracks.
pub fn remove_project_repos(name: &str, repos: &[String]) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = slugify(name);
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    if !path.exists() {
        bail!("No Knit project named `{project_id}` found.");
    }
    let mut project: KnitProject = read_json(&path)?;

    let targets: Vec<String> = repos.iter().map(|repo| slugify(repo)).collect();
    for target in &targets {
        if !project.repos.iter().any(|repo| &repo.id == target) {
            bail!("Project `{project_id}` has no repo `{target}`.");
        }
    }

    // An open bundle that tracks the repo still needs it; archived, closed, and
    // deleted bundles do not block removal.
    let blocking = open_bundles_tracking_repos(&root, &targets)?;
    if !blocking.is_empty() {
        bail!(
            "Cannot remove repo(s) tracked by open bundle(s):\n{}",
            blocking
                .iter()
                .map(|(bundle, repo)| format!("  {repo} tracked by {bundle}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    project.repos.retain(|repo| !targets.contains(&repo.id));
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    // Maintain saved views at the removal point so they never reference repos
    // the project can no longer select.
    let pruned =
        crate::commands::view::prune_removed_repos_from_views(&root, &project_id, &targets)?;

    for target in &targets {
        println!(
            "{} {} {}",
            out::heading("Removed repo:"),
            out::repo(target),
            out::muted(format!("from project {project_id}"))
        );
    }
    for target in &pruned {
        println!(
            "{} {} {} {}",
            out::heading("Views:"),
            out::movement("pruned"),
            out::repo(target),
            out::muted("from saved view(s)")
        );
    }
    Ok(())
}

/// Pairs of `(bundle_id, repo_id)` for every open bundle that tracks one of the
/// given repo ids.
fn open_bundles_tracking_repos(root: &Path, repos: &[String]) -> Result<Vec<(String, String)>> {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut blocking = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read bundle directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bundle: crate::model::ChangeGroup = read_json(&path)?;
        if !crate::store::bundle_is_open(&bundle) {
            continue;
        }
        for repo in &bundle.repos {
            if repos.contains(&repo.id) {
                blocking.push((bundle.id.clone(), repo.id.clone()));
            }
        }
    }
    blocking.sort();
    Ok(blocking)
}

fn open_bundles_tracking_repo(
    root: &Path,
    project_id: &str,
    repo_id: &str,
) -> Result<Vec<(String, String, Option<String>)>> {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut pinned = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read bundle directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bundle: crate::model::ChangeGroup = read_json(&path)?;
        if !crate::store::bundle_is_open(&bundle)
            || bundle.project_id.as_deref() != Some(project_id)
        {
            continue;
        }
        if let Some(repo) = bundle.repos.iter().find(|repo| repo.id == repo_id) {
            pinned.push((
                bundle.id.clone(),
                repo.base_branch.clone(),
                repo.base_sha.clone(),
            ));
        }
    }
    pinned.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(pinned)
}

pub fn set_project_run_command(
    name: &str,
    repos: &[String],
    cwd: Option<&Path>,
    env: &[String],
    command: &[OsString],
) -> Result<()> {
    if command.is_empty() {
        bail!("Pass a command after the name, for example `knit project command set dev -- docker compose up`.");
    }
    let cwd_root = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd_root).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let command_name = slugify(name);
    let env = parse_env(env)?;
    let command = command
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let cwd = cwd.map(|path| path.to_string_lossy().to_string());

    project.commands.insert(
        command_name.clone(),
        ProjectRunCommand {
            repos: repos.iter().map(|repo| slugify(repo)).collect(),
            cwd,
            command,
            env,
        },
    );
    project.updated_at = now_iso();
    write_json(&path, &project)?;
    println!(
        "{} {}",
        out::heading("Project command:"),
        out::repo(command_name)
    );
    Ok(())
}

pub fn list_project_run_commands() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    if project.commands.is_empty() {
        println!("{}", out::muted("No project commands."));
        return Ok(());
    }

    for (name, command) in project.commands {
        let repo_label = if command.repos.is_empty() {
            "(select at run time)".to_string()
        } else {
            command.repos.join(",")
        };
        println!(
            "{} {} {}",
            out::repo(name),
            out::muted(repo_label),
            command.command.join(" ")
        );
    }
    Ok(())
}

pub fn remove_project_run_command(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let command_name = slugify(name);
    if project.commands.remove(&command_name).is_none() {
        bail!("Project command `{command_name}` does not exist.");
    }
    project.updated_at = now_iso();
    write_json(&path, &project)?;
    println!(
        "{} {}",
        out::heading("Removed project command:"),
        out::repo(command_name)
    );
    Ok(())
}

pub fn pull_project_config(name: Option<&str>, repo_id: &str, agents: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = match name {
        Some(name) => slugify(name),
        None => active_project_id(&root)?,
    };
    let path = project_path(&root, &project_id);
    if !path.exists() {
        bail!(
            "Project `{}` does not exist locally. Run `knit init {project_id}` first.",
            out::repo(&project_id)
        );
    }

    let mut project: KnitProject = read_json(&path)?;
    let repo_entry = project
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("repo `{repo_id}` is not listed in project `{}`", project.id))?;
    let bundle_checkout = crate::store::load_active_bundle()
        .ok()
        .filter(|active| {
            active.bundle.project_id.as_deref() == Some(project_id.as_str())
                && active.resolution_source != crate::store::BundleResolutionSource::Config
        })
        .and_then(|active| {
            active
                .bundle
                .repos
                .iter()
                .find(|repo| repo.id == repo_id)
                .and_then(|repo| crate::checkout::checkout_dir(&active, repo))
        });
    let repo_root = bundle_checkout
        .as_deref()
        .unwrap_or_else(|| Path::new(&repo_entry.path));
    let config_path = repo_root.join(PROJECT_CONFIG_FILE);
    if !config_path.exists() {
        bail!(
            "No `{}` found in {}. Commit the project runtime config to the stack repo first.",
            PROJECT_CONFIG_FILE,
            out::path(repo_root.display())
        );
    }

    let incoming: KnitProject = read_json(&config_path)?;
    if incoming.id != project.id {
        bail!(
            "Project id mismatch: workspace has `{}` but `{}` declares `{}`.",
            project.id,
            out::path(config_path.display()),
            incoming.id
        );
    }

    if incoming.requirements.is_some() {
        project.requirements = incoming.requirements;
    }
    if incoming.auth.is_some() {
        // Validate the imported auth groups against this workspace's repos
        // before accepting them; metadata outside auth is preserved as-is.
        let mut candidate = project.clone();
        candidate.auth = incoming.auth.clone();
        crate::auth::validate_project_auth(&candidate).context(
            "Refusing to import invalid project auth requirements from the stack repo config",
        )?;
        project.auth = incoming.auth;
    }
    if incoming.history.is_some() {
        project.history = incoming.history;
    }
    if incoming.runtime.is_some() {
        project.runtime = incoming.runtime;
    }
    if incoming.landing.is_some() {
        project.landing = incoming.landing;
    }
    project.publish = incoming.publish;
    for repo in &mut project.repos {
        if let Some(source) = incoming.repos.iter().find(|source| source.id == repo.id) {
            repo.publish = source.publish.clone();
        }
    }
    for (command_name, command) in incoming.commands {
        project.commands.entry(command_name).or_insert(command);
    }
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    println!(
        "{} {}",
        out::heading("Pulled project config:"),
        out::path(config_path.display())
    );
    println!("{} {}", out::heading("Updated:"), out::path(path.display()));

    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

pub fn load_project_by_id(root: &Path, project_id: &str) -> Result<KnitProject> {
    read_json(&project_path(root, project_id))
}

fn active_project_id(root: &Path) -> Result<String> {
    load_config(root)?
        .active_project
        .context("No active Knit project. Run `knit init <name>` first.")
}

fn parse_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for value in values {
        let Some((key, variable_value)) = value.split_once('=') else {
            bail!("Environment entries must use KEY=VALUE syntax: {value}");
        };
        if key.trim().is_empty() {
            bail!("Environment variable names cannot be empty.");
        }
        env.insert(key.to_string(), variable_value.to_string());
    }
    Ok(env)
}

fn resolve_project_repo(
    repo_id: &str,
    repo_path: &Path,
    base_override: Option<&str>,
    observe: bool,
) -> Result<(ProjectRepoEntry, String)> {
    let repo_root = git_root(repo_path)?;
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let (base_branch, base_source) = match base_override {
        Some(base) => {
            let validation = validate_configured_base(&repo_root, base)?;
            (
                base.trim().to_string(),
                format!(
                    "explicit; verified at {} {}",
                    validation.source_ref,
                    short_sha(&validation.sha)
                ),
            )
        }
        None => {
            let inference = infer_base_branch(&repo_root, current_branch.as_deref())?;
            (
                inference.branch,
                format!("inferred from {}", inference.source),
            )
        }
    };

    Ok((
        ProjectRepoEntry {
            id: slugify(repo_id),
            path: repo_root.to_string_lossy().to_string(),
            remote,
            base_branch,
            checkout_mode: CheckoutMode::Worktree,
            include_by_default: !observe,
            publish: None,
        },
        base_source,
    ))
}
