//! `knit run up|status|down` — disposable per-bundle runtime instances.
//!
//! Knit's runtime primitive is environment injection, not stack generation.
//! Knit knows the checkout topology (paths, revs), allocates a per-bundle
//! namespace (compose project name, host ports, database identity), and
//! exposes all of it as `KNIT_*` environment variables. The stack repo owns a
//! compose file written against those variables; `knit run up` is sugar for
//! `docker compose -f <stack>/<composeFile> -p knit-run-<bundle> up` with the
//! contract injected. Service topology, build args, and app env live in the
//! repo's compose file, versioned with the stack they describe.
//!
//! The environment contract:
//!
//! - `KNIT_ROOT` / `KNIT_BUNDLE` — workspace root and bundle id
//! - `COMPOSE_PROJECT_NAME` — `knit-run-<bundle>` (also passed as `-p`)
//! - `KNIT_CHECKOUT_<REPO>` — absolute path of each repo's resolved checkout
//!   (bundle worktree when tracked, source path otherwise)
//! - `KNIT_SRC_<REPO>` — the same path relative to `KNIT_ROOT`, for build
//!   contexts rooted at the workspace
//! - `KNIT_REV_<REPO>` — HEAD revision of that checkout
//! - `KNIT_PORT_BACKEND` / `KNIT_PORT_FRONTEND` — allocated host ports
//! - `KNIT_DB_MODE` / `KNIT_DB_HOST` / `KNIT_DB_PORT` / `KNIT_DB_NAME` /
//!   `KNIT_DB_HOST_PORT` — resolved database identity
//!
//! Repo ids are uppercased with non-alphanumerics mapped to `_`
//! (`gloss-web-ui` -> `KNIT_CHECKOUT_GLOSS_WEB_UI`). In `bundle` database
//! mode Knit activates the `bundle-db` compose profile so the stack's
//! profile-gated database service starts; in `shared` mode it stays off.

use crate::checkout::checkout_dir;
use crate::git::rev_parse;
use crate::model::{
    DatabaseMode, KnitProject, ProjectRuntime, ProjectRuntimeDatabase, ProjectRuntimePorts,
    RepoEntry,
};
use crate::output as out;
use crate::store::{load_active_bundle, project_path, read_json, write_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const RUNTIME_KIND_DOCKER_COMPOSE: &str = "docker-compose";
const BUNDLE_DB_PROFILE: &str = "bundle-db";

pub fn try_handle(name: Option<&str>, raw_args: &[OsString]) -> Result<bool> {
    if !raw_args.is_empty() {
        return Ok(false);
    }

    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active)?;
    let Some(runtime) = project.runtime.clone() else {
        return Ok(false);
    };

    match name {
        None | Some("up") => run_up(&active, &project, &runtime).map(|_| true),
        Some("down") => run_down(&active).map(|_| true),
        Some("status") => run_status(&active).map(|_| true),
        Some(_) => Ok(false),
    }
}

fn run_up(active: &ActiveBundle, project: &KnitProject, runtime: &ProjectRuntime) -> Result<()> {
    if runtime.kind != RUNTIME_KIND_DOCKER_COMPOSE {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }

    let stack_repo = resolve_stack_repo(active, runtime)?;
    let stack_checkout = checkout_dir(active, stack_repo).with_context(|| {
        format!(
            "{} has no checkout. Run `knit bundle worktree` first.",
            stack_repo.id
        )
    })?;

    let compose_path = stack_checkout.join(&runtime.compose_file);
    if !compose_path.exists() {
        bail!(
            "Runtime compose file not found: {}. The stack repo `{}` must provide `{}` written against Knit's runtime environment contract (KNIT_CHECKOUT_<repo>, KNIT_REV_<repo>, KNIT_PORT_*, KNIT_DB_*).",
            compose_path.display(),
            stack_repo.id,
            runtime.compose_file
        );
    }

    let database = runtime.database.clone().unwrap_or_default();
    let resolved_database = resolve_database(&database, &active.bundle.id);
    if resolved_database.mode == DatabaseMode::Shared {
        ensure_shared_database_reachable(&database, &stack_checkout)?;
    }

    let ports = allocate_ports(&active.root, runtime.ports.clone())?;
    let project_name = compose_project_name(&active.bundle.id);
    let env = runtime_env(
        active,
        project,
        &project_name,
        &ports,
        &resolved_database,
    );
    let profiles = if resolved_database.mode == DatabaseMode::Bundle {
        vec![BUNDLE_DB_PROFILE.to_string()]
    } else {
        Vec::new()
    };

    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;
    let state = RuntimeRunState {
        bundle_id: active.bundle.id.clone(),
        stack_repo: stack_repo.id.clone(),
        backend_port: ports.backend,
        frontend_port: ports.frontend,
        database_port: resolved_database.host_port,
        database_mode: resolved_database.mode,
        database_name: resolved_database.name.clone(),
        compose_file: compose_path
            .strip_prefix(&active.root)
            .unwrap_or(&compose_path)
            .display()
            .to_string(),
        profiles: profiles.clone(),
        env: env.clone(),
        profile_path: runtime.profile_path.clone(),
        started_at: crate::time::now_iso(),
    };
    write_json(&run_dir.join("state.json"), &state)?;

    println!(
        "{} {}",
        out::heading("Runtime up:"),
        out::repo(&active.bundle.id)
    );
    println!(
        "{} {}",
        out::muted("Compose:"),
        out::path(compose_path.display())
    );
    println!(
        "{} backend http://localhost:{}  frontend http://localhost:{}",
        out::muted("Ports:"),
        ports.backend,
        ports.frontend
    );
    if let Some(profile) = &runtime.profile_path {
        println!(
            "{} http://localhost:{}{}",
            out::heading("Open:"),
            ports.frontend,
            profile
        );
    }

    let mut command = Command::new("docker");
    command
        .args(["compose", "-f"])
        .arg(&compose_path)
        .args(["-p", &project_name]);
    for profile in &profiles {
        command.args(["--profile", profile]);
    }
    let status = command
        .args(["up", "--build", "-d"])
        .envs(&env)
        .current_dir(&stack_checkout)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run docker compose")?;

    if !status.success() {
        bail!("docker compose exited with status {status}");
    }

    Ok(())
}

fn run_down(active: &ActiveBundle) -> Result<()> {
    let state = load_runtime_state(active)?;
    let project_name = compose_project_name(&active.bundle.id);

    // Project-scoped `down` resolves containers by compose label, so it works
    // even after the bundle worktree (and its compose file) is gone.
    let mut command = Command::new("docker");
    command.args(["compose", "-p", &project_name]);
    for profile in &state.profiles {
        command.args(["--profile", profile]);
    }
    let status = command
        .args(["down", "--remove-orphans"])
        .status()
        .context("failed to run docker compose down")?;

    if !status.success() {
        bail!("docker compose down exited with status {status}");
    }

    println!(
        "{} {}",
        out::heading("Runtime down:"),
        out::repo(&active.bundle.id)
    );
    Ok(())
}

fn run_status(active: &ActiveBundle) -> Result<()> {
    let state = load_runtime_state(active)?;
    let project_name = compose_project_name(&active.bundle.id);
    let services = compose_service_states(&project_name);
    let any_running = services.iter().any(|(_, state)| state == "running");

    println!("{} {}", out::heading("Bundle:"), out::repo(&state.bundle_id));
    if services.is_empty() {
        println!("{} {}", out::heading("Services:"), out::muted("none running"));
    } else {
        for (service, service_state) in &services {
            println!("{} {}", out::heading(format!("{service}:")), service_state);
        }
    }
    println!(
        "{} backend http://localhost:{}  frontend http://localhost:{}",
        out::heading("Ports:"),
        state.backend_port,
        state.frontend_port
    );
    println!(
        "{} {} localhost:{} ({})",
        out::heading("Database:"),
        database_status_label(&state, &services),
        state.database_port,
        state.database_name
    );
    if let Some(profile) = &state.profile_path {
        if any_running {
            println!(
                "{} http://localhost:{}{}",
                out::heading("Profile:"),
                state.frontend_port,
                profile
            );
        } else {
            println!(
                "{} http://localhost:{}{} {}",
                out::heading("Profile:"),
                state.frontend_port,
                profile,
                out::muted("(stack stopped)")
            );
        }
    }
    println!("{} {}", out::muted("Compose:"), state.compose_file);
    if !any_running {
        println!(
            "{} Runtime is stopped. Run `knit run up` from a stack worktree checkout.",
            out::heading("Next:")
        );
    }
    Ok(())
}

/// Build the `KNIT_*` environment contract for a runtime: bundle identity,
/// per-repo checkout paths and revisions, allocated ports, and the resolved
/// database. Covers every project repo plus any ad-hoc bundle repos; a repo
/// tracked in the bundle resolves to its bundle checkout, anything else to
/// its source path.
fn runtime_env(
    active: &ActiveBundle,
    project: &KnitProject,
    project_name: &str,
    ports: &AllocatedPorts,
    database: &ResolvedDatabase,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("KNIT_ROOT".to_string(), active.root.display().to_string());
    env.insert("KNIT_BUNDLE".to_string(), active.bundle.id.clone());
    env.insert("COMPOSE_PROJECT_NAME".to_string(), project_name.to_string());
    env.insert("KNIT_PORT_BACKEND".to_string(), ports.backend.to_string());
    env.insert("KNIT_PORT_FRONTEND".to_string(), ports.frontend.to_string());
    env.insert("KNIT_DB_MODE".to_string(), database.mode.to_string());
    env.insert("KNIT_DB_HOST".to_string(), database.host.clone());
    env.insert("KNIT_DB_PORT".to_string(), database.port.to_string());
    env.insert("KNIT_DB_NAME".to_string(), database.name.clone());
    env.insert(
        "KNIT_DB_HOST_PORT".to_string(),
        database.host_port.to_string(),
    );

    let mut checkouts: BTreeMap<String, PathBuf> = BTreeMap::new();
    for repo in &project.repos {
        checkouts.insert(repo.id.clone(), PathBuf::from(&repo.path));
    }
    for repo in &active.bundle.repos {
        if let Some(checkout) = checkout_dir(active, repo) {
            checkouts.insert(repo.id.clone(), checkout);
        }
    }

    for (repo_id, checkout) in checkouts {
        let suffix = env_var_suffix(&repo_id);
        env.insert(
            format!("KNIT_CHECKOUT_{suffix}"),
            checkout.display().to_string(),
        );
        if let Ok(relative) = relative_path(&active.root, &checkout) {
            env.insert(format!("KNIT_SRC_{suffix}"), relative);
        }
        env.insert(
            format!("KNIT_REV_{suffix}"),
            rev_parse(&checkout, "HEAD").unwrap_or_else(|_| "unknown".to_string()),
        );
    }

    env
}

/// Uppercase a repo id into an environment variable suffix, mapping every
/// non-alphanumeric character to `_` (`gloss-web-ui` -> `GLOSS_WEB_UI`).
fn env_var_suffix(repo_id: &str) -> String {
    repo_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn compose_project_name(bundle_id: &str) -> String {
    format!("knit-run-{bundle_id}")
}

fn load_runtime_state(active: &ActiveBundle) -> Result<RuntimeRunState> {
    let state_path = runtime_run_dir(&active.root, &active.bundle.id).join("state.json");
    if !state_path.exists() {
        bail!(
            "No runtime state found for bundle `{}`. Run `knit run up` first.",
            active.bundle.id
        );
    }
    read_json(&state_path)
}

/// `(service, state)` pairs from `docker compose ps` for a compose project,
/// resolved by label so no compose file is needed. Empty when docker is
/// unavailable or nothing is running.
fn compose_service_states(project_name: &str) -> Vec<(String, String)> {
    let Ok(output) = Command::new("docker")
        .args(["compose", "-p", project_name, "ps", "--format", "json"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_compose_ps(&text)
}

/// Parse `docker compose ps --format json` output, which is a JSON array in
/// some compose versions and newline-delimited objects in others.
fn parse_compose_ps(text: &str) -> Vec<(String, String)> {
    let entries: Vec<serde_json::Value> =
        if let Ok(serde_json::Value::Array(values)) = serde_json::from_str(text.trim()) {
            values
        } else {
            text.lines()
                .filter_map(|line| serde_json::from_str(line.trim()).ok())
                .collect()
        };

    entries
        .iter()
        .filter_map(|entry| {
            let service = entry.get("Service")?.as_str()?.to_string();
            let state = entry.get("State")?.as_str()?.to_string();
            Some((service, state))
        })
        .collect()
}

fn allocate_ports(root: &Path, config: Option<ProjectRuntimePorts>) -> Result<AllocatedPorts> {
    let config = config.unwrap_or_default();
    let used = load_used_ports(root)?;
    let mut backend = config.backend_base;
    let mut frontend = config.frontend_base;

    loop {
        if !used.contains(&backend)
            && !used.contains(&frontend)
            && port_available(backend)
            && port_available(frontend)
        {
            return Ok(AllocatedPorts { backend, frontend });
        }
        backend = backend.saturating_add(config.step);
        frontend = frontend.saturating_add(config.step);
        if backend > 65000 || frontend > 65000 {
            bail!("Could not find free runtime ports.");
        }
    }
}

fn load_used_ports(root: &Path) -> Result<BTreeSet<u16>> {
    let mut used = BTreeSet::new();
    let runs_dir = root.join(".knit/runtime-runs");
    if !runs_dir.exists() {
        return Ok(used);
    }

    let running = running_compose_projects();
    for entry in fs::read_dir(&runs_dir).context("failed to read runtime runs directory")? {
        let entry = entry?;
        let bundle_id = entry.file_name().to_string_lossy().into_owned();
        let state_path = entry.path().join("state.json");
        if !state_path.exists() {
            continue;
        }
        let state: RuntimeRunState = read_json(&state_path)?;
        if running.contains(&compose_project_name(&bundle_id)) {
            used.insert(state.backend_port);
            used.insert(state.frontend_port);
            used.insert(state.database_port);
        }
    }

    Ok(used)
}

/// Names of compose projects with running containers. Empty when docker is
/// unavailable, which makes their recorded ports eligible for reuse.
fn running_compose_projects() -> BTreeSet<String> {
    let Ok(output) = Command::new("docker").args(["compose", "ls", "-q"]).output() else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn port_available(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn ensure_shared_database_reachable(
    database: &ProjectRuntimeDatabase,
    stack_checkout: &Path,
) -> Result<()> {
    let addr = format!("127.0.0.1:{}", database.port);
    if TcpStream::connect(&addr).is_ok() {
        return Ok(());
    }

    if let Some(start) = database
        .start_command
        .as_ref()
        .filter(|command| !command.is_empty())
    {
        let _ = Command::new(&start[0])
            .args(&start[1..])
            .current_dir(stack_checkout)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for _ in 0..30 {
            if TcpStream::connect(&addr).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    bail!(
        "Could not connect to the shared dev database on localhost:{}. Start it (or configure `database.startCommand` in the project runtime), or switch the project runtime database mode to `bundle`.",
        database.port
    );
}

fn resolve_database(database: &ProjectRuntimeDatabase, bundle_id: &str) -> ResolvedDatabase {
    if database.mode == DatabaseMode::Bundle {
        let template = database
            .name_template
            .as_deref()
            .unwrap_or("knithub_{bundleId}");
        let name = template.replace("{bundleId}", bundle_id);
        let host_port = database.port_base.unwrap_or(5437);
        ResolvedDatabase {
            mode: DatabaseMode::Bundle,
            host: "db".to_string(),
            port: 5432,
            name,
            host_port,
        }
    } else {
        ResolvedDatabase {
            mode: DatabaseMode::Shared,
            host: database.host.clone(),
            port: database.port,
            name: database.name.clone(),
            host_port: database.port,
        }
    }
}

fn database_status_label(state: &RuntimeRunState, services: &[(String, String)]) -> &'static str {
    if state.database_mode == DatabaseMode::Bundle {
        let running = services
            .iter()
            .any(|(service, service_state)| service == "db" && service_state == "running");
        if running {
            "running"
        } else {
            "stopped"
        }
    } else if TcpStream::connect(format!("127.0.0.1:{}", state.database_port)).is_ok() {
        "reachable"
    } else {
        "unreachable"
    }
}

fn resolve_stack_repo<'a>(
    active: &'a ActiveBundle,
    runtime: &ProjectRuntime,
) -> Result<&'a RepoEntry> {
    let stack_repo_id = runtime.stack_repo.as_deref().unwrap_or("knithub");
    active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == stack_repo_id)
        .with_context(|| format!("stack repo `{stack_repo_id}` is not tracked in this bundle"))
}

fn load_project_for_bundle(active: &ActiveBundle) -> Result<KnitProject> {
    let config = crate::store::load_config(&active.root)?;
    let project_id = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref())
        .context("The resolved bundle is not associated with a Knit project.")?;
    read_json(&project_path(&active.root, project_id))
}

fn runtime_run_dir(root: &Path, bundle_id: &str) -> PathBuf {
    root.join(".knit/runtime-runs").join(bundle_id)
}

fn relative_path(base: &Path, target: &Path) -> Result<String> {
    let base = crate::paths::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let target = crate::paths::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    target
        .strip_prefix(&base)
        .map(|path| path.display().to_string())
        .with_context(|| {
            format!(
                "Could not make `{}` relative to `{}`",
                target.display(),
                base.display()
            )
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeRunState {
    bundle_id: String,
    stack_repo: String,
    backend_port: u16,
    frontend_port: u16,
    database_port: u16,
    #[serde(default)]
    database_mode: DatabaseMode,
    #[serde(default = "default_database_name_state")]
    database_name: String,
    /// Workspace-relative path of the stack repo's compose file.
    compose_file: String,
    /// Compose profiles activated by this run (e.g. `bundle-db`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    /// The injected environment contract, recorded so the same compose file
    /// can be driven manually for debugging.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    profile_path: Option<String>,
    started_at: String,
}

fn default_database_name_state() -> String {
    "knithub_dev".to_string()
}

struct ResolvedDatabase {
    mode: DatabaseMode,
    host: String,
    port: u16,
    name: String,
    host_port: u16,
}

struct AllocatedPorts {
    backend: u16,
    frontend: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChangeGroup;
    use crate::store::ActiveBundle;
    use crate::time::now_iso;

    #[test]
    fn env_var_suffix_uppercases_and_replaces_separators() {
        assert_eq!(env_var_suffix("knithub"), "KNITHUB");
        assert_eq!(env_var_suffix("gloss-web-ui"), "GLOSS_WEB_UI");
        assert_eq!(env_var_suffix("a.b/c"), "A_B_C");
    }

    #[test]
    fn runtime_env_covers_identity_repos_ports_and_database() {
        let root = std::env::temp_dir().join(format!(
            "knit-runtime-env-test-{}-{}",
            std::process::id(),
            now_iso().replace([':', '.'], "")
        ));
        std::fs::create_dir_all(root.join("knithub")).unwrap();
        std::fs::create_dir_all(root.join("gloss-web-ui")).unwrap();

        let mut bundle = ChangeGroup::new("demo".to_string(), "demo".to_string(), now_iso());
        bundle.repos.push(RepoEntry {
            id: "knithub".to_string(),
            path: root.join("knithub").display().to_string(),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: crate::model::CheckoutMode::InPlace,
            base_sha: None,
            feature_branch: Some("knit/demo".to_string()),
            worktree_path: None,
            head_sha: None,
        });
        let active = ActiveBundle::unlocked(
            root.clone(),
            root.join(".knit/bundles/demo.bundle.json"),
            bundle,
        );

        let mut project = KnitProject::new("knit-tools".to_string(), now_iso());
        project.repos.push(crate::model::ProjectRepoEntry {
            id: "gloss-web-ui".to_string(),
            path: root.join("gloss-web-ui").display().to_string(),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: crate::model::CheckoutMode::Worktree,
            include_by_default: false,
        });

        let ports = AllocatedPorts {
            backend: 4011,
            frontend: 5184,
        };
        let database = ResolvedDatabase {
            mode: DatabaseMode::Shared,
            host: "host.docker.internal".to_string(),
            port: 5436,
            name: "knithub_dev".to_string(),
            host_port: 5436,
        };

        let env = runtime_env(&active, &project, "knit-run-demo", &ports, &database);

        assert_eq!(env.get("KNIT_BUNDLE").unwrap(), "demo");
        assert_eq!(env.get("COMPOSE_PROJECT_NAME").unwrap(), "knit-run-demo");
        assert_eq!(env.get("KNIT_PORT_BACKEND").unwrap(), "4011");
        assert_eq!(env.get("KNIT_PORT_FRONTEND").unwrap(), "5184");
        assert_eq!(env.get("KNIT_DB_MODE").unwrap(), "shared");
        assert_eq!(env.get("KNIT_DB_NAME").unwrap(), "knithub_dev");
        assert_eq!(env.get("KNIT_DB_HOST_PORT").unwrap(), "5436");
        // Bundle repo resolves to its checkout; project-only repo to its path.
        assert!(env.get("KNIT_CHECKOUT_KNITHUB").unwrap().ends_with("knithub"));
        assert_eq!(env.get("KNIT_SRC_KNITHUB").unwrap(), "knithub");
        assert!(env
            .get("KNIT_CHECKOUT_GLOSS_WEB_UI")
            .unwrap()
            .ends_with("gloss-web-ui"));
        assert_eq!(env.get("KNIT_SRC_GLOSS_WEB_UI").unwrap(), "gloss-web-ui");
        assert_eq!(env.get("KNIT_REV_KNITHUB").unwrap(), "unknown");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_compose_ps_accepts_array_and_ndjson() {
        let array = r#"[{"Service":"backend","State":"running"},{"Service":"db","State":"exited"}]"#;
        assert_eq!(
            parse_compose_ps(array),
            vec![
                ("backend".to_string(), "running".to_string()),
                ("db".to_string(), "exited".to_string())
            ]
        );
        let ndjson = "{\"Service\":\"frontend\",\"State\":\"running\"}\n{\"Service\":\"backend\",\"State\":\"running\"}\n";
        assert_eq!(
            parse_compose_ps(ndjson),
            vec![
                ("frontend".to_string(), "running".to_string()),
                ("backend".to_string(), "running".to_string())
            ]
        );
    }

    #[test]
    fn resolve_database_bundle_mode_names_per_bundle() {
        let database = ProjectRuntimeDatabase {
            mode: DatabaseMode::Bundle,
            ..Default::default()
        };
        let resolved = resolve_database(&database, "venue-capacity");
        assert_eq!(resolved.name, "knithub_venue-capacity");
        assert_eq!(resolved.host, "db");
        assert_eq!(resolved.port, 5432);
        assert_eq!(resolved.host_port, 5437);
    }
}
