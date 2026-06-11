//! `knit run up|status|down` — disposable per-bundle runtime instances.
//!
//! The goal: take the docker shape the repos already run on their main
//! branches and lift a parallel instance of that same shape — different
//! ports, isolated compose project, and the bundle's code substituted in.
//!
//! Two modes, picked per compose file:
//!
//! - **Transform** (default): the stack repo's own compose file is resolved
//!   with `docker compose config` and rewritten — paths into tracked repos
//!   remapped to bundle worktrees, published host ports reallocated, port
//!   references inside env values rewritten. No authoring required; see
//!   [`transform`].
//! - **Contract**: a `KNIT_*`-aware compose file is run as-is with the
//!   contract injected: `KNIT_ROOT`/`KNIT_BUNDLE`,
//!   `KNIT_CHECKOUT_<REPO>`/`KNIT_SRC_<REPO>`/`KNIT_REV_<REPO>` per repo,
//!   `KNIT_PORT_<SERVICE>` per configured port pool (backend/frontend by
//!   default), and `KNIT_DB_*`. Repo and service ids are uppercased with
//!   non-alphanumerics mapped to `_`. In `bundle` database mode the
//!   `bundle-db` compose profile is activated.
//!
//! Contract mode is chosen by the `runtime.mode` project setting, the
//! `docker-compose.knit.yml` filename, or `${KNIT_*}` references in the
//! compose file; everything else is lifted by transformation.
//!
//! Either way the stack runs as compose project `knit-run-<bundle>`, so
//! networks and named volumes are isolated per bundle, and `down`/`status`
//! resolve containers by label and survive worktree deletion.

mod transform;

use crate::checkout::checkout_dir;
use crate::git::rev_parse;
use crate::model::{
    DatabaseMode, KnitProject, ProjectRuntime, ProjectRuntimeDatabase, RepoEntry, RuntimeMode,
};
use crate::output as out;
use crate::store::{load_active_bundle, project_path, read_json, write_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use transform::ServicePort;

const RUNTIME_KIND_DOCKER_COMPOSE: &str = "docker-compose";
const BUNDLE_DB_PROFILE: &str = "bundle-db";
const CONTRACT_COMPOSE_CANDIDATES: [&str; 1] = ["docker-compose.knit.yml"];
const PLAIN_COMPOSE_CANDIDATES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
    "compose.yml",
];

pub fn try_handle(name: &str) -> Result<bool> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active).ok();
    let runtime = project.as_ref().and_then(|p| p.runtime.clone());

    match name {
        "up" => {
            let Some((runtime, stack_repo)) = resolve_runtime(&active, runtime)? else {
                return Ok(false);
            };
            run_up(&active, project.as_ref(), &runtime, stack_repo).map(|_| true)
        }
        "down" => {
            if !runtime_applies(&active, runtime)? {
                return Ok(false);
            }
            run_down(&active).map(|_| true)
        }
        "status" => {
            if !runtime_applies(&active, runtime)? {
                return Ok(false);
            }
            run_status(&active).map(|_| true)
        }
        _ => Ok(false),
    }
}

/// Whether `down`/`status` should handle this bundle: a configured runtime,
/// recorded run state, or a detectable stack repo (so cleanup works even when
/// a failed `up` never recorded state).
fn runtime_applies(active: &ActiveBundle, runtime: Option<ProjectRuntime>) -> Result<bool> {
    if runtime.is_some() || has_state(active) {
        return Ok(true);
    }
    Ok(resolve_runtime(active, None).unwrap_or(None).is_some())
}

/// Resolve the runtime config and stack repo for `up`. With no `runtime`
/// block in the project, fall back to auto-detection: a single bundle repo
/// with a compose file makes the runtime work with zero configuration.
fn resolve_runtime(
    active: &ActiveBundle,
    runtime: Option<ProjectRuntime>,
) -> Result<Option<(ProjectRuntime, &RepoEntry)>> {
    if let Some(stack_repo_id) = runtime.as_ref().and_then(|r| r.stack_repo.clone()) {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == stack_repo_id)
            .with_context(|| {
                format!("stack repo `{stack_repo_id}` is not tracked in this bundle")
            })?;
        return Ok(Some((runtime.unwrap(), repo)));
    }

    let mut detected = Vec::new();
    for repo in &active.bundle.repos {
        let Some(checkout) = checkout_dir(active, repo) else {
            continue;
        };
        let has_compose = CONTRACT_COMPOSE_CANDIDATES
            .iter()
            .chain(PLAIN_COMPOSE_CANDIDATES.iter())
            .any(|candidate| checkout.join(candidate).exists());
        if has_compose {
            detected.push(repo);
        }
    }

    match detected.len() {
        0 => Ok(None),
        1 => Ok(Some((runtime.unwrap_or_default(), detected[0]))),
        _ => bail!(
            "Multiple bundle repos have compose files ({}). Set `runtime.stackRepo` in the project to pick one.",
            detected
                .iter()
                .map(|repo| repo.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn run_up(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    runtime: &ProjectRuntime,
    stack_repo: &RepoEntry,
) -> Result<()> {
    if runtime.kind != RUNTIME_KIND_DOCKER_COMPOSE {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }

    let stack_checkout = checkout_dir(active, stack_repo).with_context(|| {
        format!(
            "{} has no checkout. Run `knit bundle worktree` first.",
            stack_repo.id
        )
    })?;

    let compose_path = find_compose_file(&stack_checkout, runtime)?;
    let mode = detect_mode(runtime, &compose_path)?;

    match mode {
        RuntimeMode::Contract => run_up_contract(
            active,
            project,
            runtime,
            stack_repo,
            &stack_checkout,
            &compose_path,
        ),
        RuntimeMode::Transform => {
            run_up_transform(active, runtime, stack_repo, &stack_checkout, &compose_path)
        }
    }
}

/// Pick the compose file: explicit `composeFile` config wins, then the
/// contract file, then the repo's own compose file.
fn find_compose_file(stack_checkout: &Path, runtime: &ProjectRuntime) -> Result<PathBuf> {
    if let Some(configured) = &runtime.compose_file {
        let path = stack_checkout.join(configured);
        if !path.exists() {
            bail!(
                "Runtime compose file not found: {}. Configured as `composeFile` in the project runtime.",
                path.display()
            );
        }
        return Ok(path);
    }
    for candidate in CONTRACT_COMPOSE_CANDIDATES
        .iter()
        .chain(PLAIN_COMPOSE_CANDIDATES.iter())
    {
        let path = stack_checkout.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "No compose file found in {}. Add a docker-compose.yml to the stack repo (lifted automatically) or a docker-compose.knit.yml written against the KNIT_* contract.",
        stack_checkout.display()
    );
}

/// Explicit `runtime.mode` config wins; the contract filename or `${KNIT_*}`
/// variable references opt into the contract; anything else is lifted by
/// transformation.
fn detect_mode(runtime: &ProjectRuntime, compose_path: &Path) -> Result<RuntimeMode> {
    if let Some(mode) = runtime.mode {
        return Ok(mode);
    }
    if compose_path
        .file_name()
        .is_some_and(|name| CONTRACT_COMPOSE_CANDIDATES.iter().any(|c| name == *c))
    {
        return Ok(RuntimeMode::Contract);
    }
    let text = fs::read_to_string(compose_path)
        .with_context(|| format!("failed to read {}", compose_path.display()))?;
    if text.contains("${KNIT_") || text.contains("$KNIT_") {
        Ok(RuntimeMode::Contract)
    } else {
        Ok(RuntimeMode::Transform)
    }
}

/// Transform mode: resolve the repo's compose file in source-space, rewrite
/// it into the bundle namespace, and run the generated file.
fn run_up_transform(
    active: &ActiveBundle,
    runtime: &ProjectRuntime,
    stack_repo: &RepoEntry,
    stack_checkout: &Path,
    compose_path: &Path,
) -> Result<()> {
    let source_dir = PathBuf::from(&stack_repo.path);
    let mut config = transform::resolve_compose_config(compose_path, &source_dir)?;

    let repo_map: Vec<(PathBuf, PathBuf)> = active
        .bundle
        .repos
        .iter()
        .filter_map(|repo| {
            let checkout = checkout_dir(active, repo)?;
            let source = crate::paths::canonicalize(Path::new(&repo.path)).ok()?;
            (source != checkout).then_some((source, checkout))
        })
        .collect();

    let step = runtime.ports.clone().unwrap_or_default().step.max(1);
    let mut taken = load_used_ports(&active.root)?;
    let mut allocate = |old: u16| -> Result<u16> {
        let mut candidate = old.saturating_add(step);
        loop {
            if !taken.contains(&candidate) && port_available(candidate) {
                taken.insert(candidate);
                return Ok(candidate);
            }
            candidate = candidate.saturating_add(step);
            if candidate > 65000 {
                bail!("Could not find a free runtime port for {old}.");
            }
        }
    };

    let ports = transform::transform_compose(&mut config, &repo_map, &mut allocate)?;

    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;
    // JSON is valid YAML, so the generated file can stay a compose file.
    let generated = run_dir.join("docker-compose.yml");
    fs::write(&generated, serde_json::to_string_pretty(&config)?)
        .context("failed to write generated compose file")?;

    let project_name = compose_project_name(&active.bundle.id);
    print_up_summary(active, &generated, &ports, runtime.profile_path.as_deref());

    let status = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&generated)
        .args(["-p", &project_name, "up", "--build", "-d"])
        .current_dir(stack_checkout)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run docker compose")?;
    if !status.success() {
        bail!(
            "docker compose exited with status {status}. Clean up partial containers with `knit run down`."
        );
    }

    // Recorded only after a successful start so a failed `up` does not leave
    // phantom run state.
    let state = RuntimeRunState {
        bundle_id: active.bundle.id.clone(),
        stack_repo: stack_repo.id.clone(),
        mode: RuntimeMode::Transform,
        ports,
        database: None,
        compose_file: generated
            .strip_prefix(&active.root)
            .unwrap_or(&generated)
            .display()
            .to_string(),
        profiles: Vec::new(),
        env: BTreeMap::new(),
        profile_path: runtime.profile_path.clone(),
        started_at: crate::time::now_iso(),
    };
    write_json(&run_dir.join("state.json"), &state)?;
    Ok(())
}

/// Contract mode: inject the `KNIT_*` environment into the repo-owned
/// compose file and run it in place.
fn run_up_contract(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    runtime: &ProjectRuntime,
    stack_repo: &RepoEntry,
    stack_checkout: &Path,
    compose_path: &Path,
) -> Result<()> {
    let database = runtime.database.clone().unwrap_or_default();
    let resolved_database = resolve_database(&database, &active.bundle.id);
    if resolved_database.mode == DatabaseMode::Shared {
        ensure_shared_database_reachable(&database, stack_checkout)?;
    }

    let ports_config = runtime.ports.clone().unwrap_or_default();
    let used = load_used_ports(&active.root)?;
    let service_ports = allocate_service_ports(
        &used,
        &ports_config.service_bases(),
        ports_config.step.max(1),
    )?;

    let project_name = compose_project_name(&active.bundle.id);
    let env = runtime_env(
        active,
        project,
        &project_name,
        &service_ports,
        &resolved_database,
    );
    let profiles = if resolved_database.mode == DatabaseMode::Bundle {
        vec![BUNDLE_DB_PROFILE.to_string()]
    } else {
        Vec::new()
    };

    let mut ports: Vec<ServicePort> = service_ports
        .iter()
        .map(|(service, port)| ServicePort {
            service: service.clone(),
            host: *port,
            container: None,
        })
        .collect();
    if resolved_database.mode == DatabaseMode::Bundle {
        ports.push(ServicePort {
            service: "db".to_string(),
            host: resolved_database.host_port,
            container: Some(5432),
        });
    }

    print_up_summary(
        active,
        compose_path,
        &ports,
        runtime.profile_path.as_deref(),
    );

    let mut command = Command::new("docker");
    command
        .args(["compose", "-f"])
        .arg(compose_path)
        .args(["-p", &project_name]);
    for profile in &profiles {
        command.args(["--profile", profile]);
    }
    let status = command
        .args(["up", "--build", "-d"])
        .envs(&env)
        .current_dir(stack_checkout)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run docker compose")?;
    if !status.success() {
        bail!(
            "docker compose exited with status {status}. Clean up partial containers with `knit run down`."
        );
    }

    // Recorded only after a successful start so a failed `up` does not leave
    // phantom run state.
    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;
    let state = RuntimeRunState {
        bundle_id: active.bundle.id.clone(),
        stack_repo: stack_repo.id.clone(),
        mode: RuntimeMode::Contract,
        ports,
        database: Some(StateDatabase {
            mode: resolved_database.mode,
            port: resolved_database.host_port,
            name: resolved_database.name.clone(),
        }),
        compose_file: compose_path
            .strip_prefix(&active.root)
            .unwrap_or(compose_path)
            .display()
            .to_string(),
        profiles,
        env,
        profile_path: runtime.profile_path.clone(),
        started_at: crate::time::now_iso(),
    };
    write_json(&run_dir.join("state.json"), &state)?;
    Ok(())
}

fn print_up_summary(
    active: &ActiveBundle,
    compose_path: &Path,
    ports: &[ServicePort],
    profile_path: Option<&str>,
) {
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
    for port in ports {
        println!(
            "{} {} localhost:{}",
            out::muted("Port:"),
            port.service,
            port.host
        );
    }
    if let (Some(profile), Some(frontend)) = (profile_path, frontend_port(ports)) {
        println!(
            "{} http://localhost:{}{}",
            out::heading("Open:"),
            frontend,
            profile
        );
    }
}

/// The port to attach UI URLs to: a service named `frontend`, else anything
/// front-ish, else the first published port.
fn frontend_port(ports: &[ServicePort]) -> Option<u16> {
    ports
        .iter()
        .find(|port| port.service == "frontend")
        .or_else(|| ports.iter().find(|port| port.service.contains("front")))
        .or_else(|| ports.first())
        .map(|port| port.host)
}

fn run_down(active: &ActiveBundle) -> Result<()> {
    // State may be missing when an `up` failed before recording it; down
    // still works because containers are resolved by compose label.
    let profiles = load_runtime_state(active)
        .map(|state| state.profiles)
        .unwrap_or_default();
    let project_name = compose_project_name(&active.bundle.id);

    // Project-scoped `down` resolves containers by compose label, so it works
    // even after the bundle worktree (and its compose file) is gone.
    let mut command = Command::new("docker");
    command.args(["compose", "-p", &project_name]);
    for profile in &profiles {
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
    // State may be missing when an `up` failed before recording it; still
    // report containers resolved by compose label so cleanup is visible.
    let state = load_runtime_state(active).ok();
    let project_name = compose_project_name(&active.bundle.id);
    let services = compose_service_states(&project_name);
    let any_running = services.iter().any(|(_, state)| state == "running");

    println!(
        "{} {}",
        out::heading("Bundle:"),
        out::repo(&active.bundle.id)
    );
    if services.is_empty() {
        println!(
            "{} {}",
            out::heading("Services:"),
            out::muted("none running")
        );
    } else {
        for (service, service_state) in &services {
            println!("{} {}", out::heading(format!("{service}:")), service_state);
        }
    }

    let Some(state) = state else {
        println!(
            "{} No recorded runtime state. Run `knit run up`{}.",
            out::heading("Next:"),
            if services.is_empty() {
                ""
            } else {
                ", or `knit run down` to clean up the containers above"
            }
        );
        return Ok(());
    };

    for port in &state.ports {
        println!(
            "{} {} localhost:{}",
            out::muted("Port:"),
            port.service,
            port.host
        );
    }
    if let Some(database) = &state.database {
        println!(
            "{} {} localhost:{} ({})",
            out::heading("Database:"),
            database_status_label(database, &services),
            database.port,
            database.name
        );
    }
    if let (Some(profile), Some(frontend)) = (&state.profile_path, frontend_port(&state.ports)) {
        if any_running {
            println!(
                "{} http://localhost:{}{}",
                out::heading("Profile:"),
                frontend,
                profile
            );
        } else {
            println!(
                "{} http://localhost:{}{} {}",
                out::heading("Profile:"),
                frontend,
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
    project: Option<&KnitProject>,
    project_name: &str,
    service_ports: &BTreeMap<String, u16>,
    database: &ResolvedDatabase,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("KNIT_ROOT".to_string(), active.root.display().to_string());
    env.insert("KNIT_BUNDLE".to_string(), active.bundle.id.clone());
    env.insert("COMPOSE_PROJECT_NAME".to_string(), project_name.to_string());
    for (service, port) in service_ports {
        env.insert(
            format!("KNIT_PORT_{}", env_var_suffix(service)),
            port.to_string(),
        );
    }
    env.insert("KNIT_DB_MODE".to_string(), database.mode.to_string());
    env.insert("KNIT_DB_HOST".to_string(), database.host.clone());
    env.insert("KNIT_DB_PORT".to_string(), database.port.to_string());
    env.insert("KNIT_DB_NAME".to_string(), database.name.clone());
    env.insert(
        "KNIT_DB_HOST_PORT".to_string(),
        database.host_port.to_string(),
    );

    let mut checkouts: BTreeMap<String, PathBuf> = BTreeMap::new();
    if let Some(project) = project {
        for repo in &project.repos {
            checkouts.insert(repo.id.clone(), PathBuf::from(&repo.path));
        }
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

fn has_state(active: &ActiveBundle) -> bool {
    runtime_run_dir(&active.root, &active.bundle.id)
        .join("state.json")
        .exists()
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

/// Allocate one free host port per contract-mode service pool, stepping all
/// bases together so paired stacks stay recognizable.
fn allocate_service_ports(
    used: &BTreeSet<u16>,
    bases: &BTreeMap<String, u16>,
    step: u16,
) -> Result<BTreeMap<String, u16>> {
    if bases.is_empty() {
        bail!("The project runtime defines no service port pools.");
    }
    let mut offset = 0u16;
    loop {
        let mut allocated = BTreeMap::new();
        for (service, base) in bases {
            let port = base.saturating_add(offset);
            if used.contains(&port)
                || allocated.values().any(|taken| *taken == port)
                || !port_available(port)
            {
                allocated.clear();
                break;
            }
            allocated.insert(service.clone(), port);
        }
        if allocated.len() == bases.len() {
            return Ok(allocated);
        }
        offset = offset.saturating_add(step);
        if bases
            .values()
            .any(|base| base.saturating_add(offset) > 65000)
        {
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
        let Ok(state) = read_json::<RuntimeRunState>(&state_path) else {
            continue;
        };
        if running.contains(&compose_project_name(&bundle_id)) {
            for port in &state.ports {
                used.insert(port.host);
            }
            if let Some(database) = &state.database {
                used.insert(database.port);
            }
        }
    }

    Ok(used)
}

/// Names of compose projects with running containers. Empty when docker is
/// unavailable, which makes their recorded ports eligible for reuse.
fn running_compose_projects() -> BTreeSet<String> {
    let Ok(output) = Command::new("docker")
        .args(["compose", "ls", "-q"])
        .output()
    else {
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
            .unwrap_or("app_{bundleId}");
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

fn database_status_label(database: &StateDatabase, services: &[(String, String)]) -> &'static str {
    if database.mode == DatabaseMode::Bundle {
        let running = services
            .iter()
            .any(|(service, service_state)| service == "db" && service_state == "running");
        if running {
            "running"
        } else {
            "stopped"
        }
    } else if TcpStream::connect(format!("127.0.0.1:{}", database.port)).is_ok() {
        "reachable"
    } else {
        "unreachable"
    }
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
    #[serde(default)]
    mode: RuntimeMode,
    #[serde(default)]
    ports: Vec<ServicePort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    database: Option<StateDatabase>,
    /// Workspace-relative path of the compose file this run executed
    /// (generated file in transform mode, repo file in contract mode).
    compose_file: String,
    /// Compose profiles activated by this run (e.g. `bundle-db`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
    /// The injected environment contract (contract mode), recorded so the
    /// same compose file can be driven manually for debugging.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    profile_path: Option<String>,
    started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateDatabase {
    #[serde(default)]
    mode: DatabaseMode,
    port: u16,
    name: String,
}

struct ResolvedDatabase {
    mode: DatabaseMode,
    host: String,
    port: u16,
    name: String,
    host_port: u16,
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

        let database = ResolvedDatabase {
            mode: DatabaseMode::Shared,
            host: "host.docker.internal".to_string(),
            port: 5436,
            name: "knithub_dev".to_string(),
            host_port: 5436,
        };

        let service_ports = BTreeMap::from([
            ("backend".to_string(), 4011u16),
            ("frontend".to_string(), 5184u16),
        ]);
        let env = runtime_env(
            &active,
            Some(&project),
            "knit-run-demo",
            &service_ports,
            &database,
        );

        assert_eq!(env.get("KNIT_BUNDLE").unwrap(), "demo");
        assert_eq!(env.get("COMPOSE_PROJECT_NAME").unwrap(), "knit-run-demo");
        assert_eq!(env.get("KNIT_PORT_BACKEND").unwrap(), "4011");
        assert_eq!(env.get("KNIT_PORT_FRONTEND").unwrap(), "5184");
        assert_eq!(env.get("KNIT_DB_MODE").unwrap(), "shared");
        assert_eq!(env.get("KNIT_DB_NAME").unwrap(), "knithub_dev");
        assert_eq!(env.get("KNIT_DB_HOST_PORT").unwrap(), "5436");
        // Bundle repo resolves to its checkout; project-only repo to its path.
        assert!(env
            .get("KNIT_CHECKOUT_KNITHUB")
            .unwrap()
            .ends_with("knithub"));
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
        let array =
            r#"[{"Service":"backend","State":"running"},{"Service":"db","State":"exited"}]"#;
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
        assert_eq!(resolved.name, "app_venue-capacity");
        assert_eq!(resolved.host, "db");
        assert_eq!(resolved.port, 5432);
        assert_eq!(resolved.host_port, 5437);
    }

    #[test]
    fn detect_mode_sniffs_contract_variables() {
        let dir =
            std::env::temp_dir().join(format!("knit-runtime-mode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = ProjectRuntime::default();

        let contract = dir.join("contract.yml");
        std::fs::write(
            &contract,
            "services:\n  b:\n    ports: [\"${KNIT_PORT_BACKEND}:4000\"]\n",
        )
        .unwrap();
        let plain = dir.join("plain.yml");
        std::fs::write(&plain, "services:\n  b:\n    ports: [\"4000:4000\"]\n").unwrap();
        // A bare KNIT_ mention (e.g. in a comment) is not a contract opt-in.
        let comment = dir.join("comment.yml");
        std::fs::write(
            &comment,
            "# managed via KNIT_ tooling\nservices:\n  b:\n    ports: [\"4000:4000\"]\n",
        )
        .unwrap();
        let named = dir.join(CONTRACT_COMPOSE_CANDIDATES[0]);
        std::fs::write(&named, "services:\n  b:\n    ports: [\"4000:4000\"]\n").unwrap();

        assert_eq!(
            detect_mode(&runtime, &contract).unwrap(),
            RuntimeMode::Contract
        );
        assert_eq!(
            detect_mode(&runtime, &plain).unwrap(),
            RuntimeMode::Transform
        );
        assert_eq!(
            detect_mode(&runtime, &comment).unwrap(),
            RuntimeMode::Transform
        );
        assert_eq!(
            detect_mode(&runtime, &named).unwrap(),
            RuntimeMode::Contract
        );

        // Explicit config wins over detection.
        let forced = ProjectRuntime {
            mode: Some(RuntimeMode::Contract),
            ..Default::default()
        };
        assert_eq!(detect_mode(&forced, &plain).unwrap(), RuntimeMode::Contract);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn allocate_service_ports_steps_pools_together() {
        let bases = BTreeMap::from([
            ("backend".to_string(), 4001u16),
            ("frontend".to_string(), 5174u16),
        ]);
        let used = BTreeSet::from([4001u16, 5174u16]);
        let allocated = allocate_service_ports(&used, &bases, 10).unwrap();
        assert_eq!(allocated["backend"] - 4001, allocated["frontend"] - 5174);
        assert!(allocated["backend"] >= 4011);
    }

    #[test]
    fn frontend_port_prefers_frontend_service() {
        let ports = vec![
            ServicePort {
                service: "db".into(),
                host: 5446,
                container: Some(5432),
            },
            ServicePort {
                service: "web-frontend".into(),
                host: 5184,
                container: Some(5173),
            },
        ];
        assert_eq!(frontend_port(&ports), Some(5184));
        assert_eq!(frontend_port(&[]), None);
    }
}
