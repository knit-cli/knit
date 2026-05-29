use crate::checkout::checkout_dir;
use crate::model::{
    KnitProject, ProjectRuntime, ProjectRuntimeDatabase, ProjectRuntimePorts, RepoEntry,
};
use crate::output as out;
use crate::store::{load_active_bundle, project_path, read_json, write_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const RUNTIME_KIND_DOCKER_COMPOSE: &str = "docker-compose";

pub fn try_handle(
    name: Option<&str>,
    raw_args: &[OsString],
) -> Result<bool> {
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
        Some(_) => return Ok(false),
    }
}

fn run_up(active: &ActiveBundle, project: &KnitProject, runtime: &ProjectRuntime) -> Result<()> {
    if runtime.kind != RUNTIME_KIND_DOCKER_COMPOSE {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }

    let stack_repo = resolve_stack_repo(active, runtime)?;
    let stack_checkout = checkout_dir(active, stack_repo)
        .with_context(|| format!("{} has no checkout. Run `knit worktree` first.", stack_repo.id))?;

    let database = runtime.database.clone().unwrap_or_default();
    ensure_database_reachable(&database)?;

    let ports = allocate_ports(&active.root, &active.bundle.id, runtime.ports.clone())?;
    let plan = build_plan(active, project, runtime, stack_repo, &stack_checkout, &ports)?;
    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;

    let compose_path = run_dir.join("docker-compose.yml");
    fs::write(&compose_path, plan.compose_yaml).context("failed to write generated compose file")?;

    let state = RuntimeRunState {
        bundle_id: active.bundle.id.clone(),
        stack_repo: stack_repo.id.clone(),
        backend_port: ports.backend,
        frontend_port: ports.frontend,
        database_port: database.port,
        compose_file: compose_path
            .strip_prefix(&active.root)
            .unwrap_or(&compose_path)
            .display()
            .to_string(),
        profile_path: runtime.profile_path.clone(),
        started_at: crate::time::now_iso(),
    };
    write_json(&run_dir.join("state.json"), &state)?;

    println!(
        "{} {}",
        out::heading("Runtime up:"),
        out::repo(&active.bundle.id)
    );
    println!("{} {}", out::muted("Compose:"), out::path(compose_path.display()));
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

    let status = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_path)
        .args(["up", "--build", "-d"])
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
    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    let compose_path = run_dir.join("docker-compose.yml");
    if !compose_path.exists() {
        bail!("No runtime compose file found for bundle `{}`.", active.bundle.id);
    }

    let state: RuntimeRunState = read_json(&run_dir.join("state.json"))?;
    let stack_checkout = active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == state.stack_repo)
        .and_then(|repo| checkout_dir(active, repo))
        .context("stack repo checkout is missing")?;

    let status = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_path)
        .args(["down"])
        .current_dir(&stack_checkout)
        .status()
        .context("failed to run docker compose down")?;

    if !status.success() {
        bail!("docker compose down exited with status {status}");
    }

    println!("{} {}", out::heading("Runtime down:"), out::repo(&active.bundle.id));
    Ok(())
}

fn run_status(active: &ActiveBundle) -> Result<()> {
    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    let state_path = run_dir.join("state.json");
    if !state_path.exists() {
        bail!("No runtime state found for bundle `{}`. Run `knit run up` first.", active.bundle.id);
    }

    let state: RuntimeRunState = read_json(&state_path)?;
    println!("{} {}", out::heading("Bundle:"), out::repo(&state.bundle_id));
    println!("{} http://localhost:{}", out::heading("Backend:"), state.backend_port);
    println!("{} http://localhost:{}", out::heading("Frontend:"), state.frontend_port);
    println!(
        "{} localhost:{} ({})",
        out::heading("Database:"),
        state.database_port,
        state.database_port
    );
    if let Some(profile) = &state.profile_path {
        println!(
            "{} http://localhost:{}{}",
            out::heading("Profile:"),
            state.frontend_port,
            profile
        );
    }
    println!("{} {}", out::muted("Compose:"), state.compose_file);
    Ok(())
}

fn build_plan(
    active: &ActiveBundle,
    project: &KnitProject,
    runtime: &ProjectRuntime,
    _stack_repo: &RepoEntry,
    stack_checkout: &Path,
    ports: &AllocatedPorts,
) -> Result<RuntimePlan> {
    let workspace_root = &active.root;
    let database = runtime.database.clone().unwrap_or_default();
    let frontend_repo_id = runtime
        .frontend_repo
        .clone()
        .unwrap_or_else(|| "knithub-frontend".to_string());
    let frontend_repo = active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == frontend_repo_id)
        .with_context(|| format!("frontend repo `{frontend_repo_id}` is not in this bundle"))?;
    let frontend_checkout = checkout_dir(active, frontend_repo)
        .with_context(|| format!("{} has no checkout.", frontend_repo.id))?;

    let gloss_repo_id = runtime
        .gloss_web_ui_repo
        .clone()
        .unwrap_or_else(|| "gloss-web-ui".to_string());
    let gloss_path = project
        .repos
        .iter()
        .find(|repo| repo.id == gloss_repo_id)
        .map(|repo| PathBuf::from(&repo.path))
        .unwrap_or_else(|| workspace_root.join("gloss-web-ui"));

    let in_worktree = is_worktree_checkout(stack_checkout);
    let stack_rel = relative_path(workspace_root, stack_checkout)?;
    let frontend_rel = relative_path(workspace_root, &frontend_checkout)?;
    let gloss_rel = relative_path(workspace_root, &gloss_path)?;

    let profile_path = runtime.profile_path.clone().unwrap_or_else(|| "/app/profile".to_string());
    let project_name = format!("knit-run-{}", active.bundle.id);

    let compose_yaml = if in_worktree {
        let dockerfile = runtime
            .worktree_dockerfile
            .clone()
            .unwrap_or_else(|| "Dockerfile.worktree".to_string());
        let dockerfile_rel = format!("{stack_rel}/{dockerfile}");
        generate_worktree_compose(
            &project_name,
            workspace_root,
            &stack_rel,
            &dockerfile_rel,
            &frontend_rel,
            &gloss_rel,
            ports,
            &database,
            &profile_path,
        )
    } else {
        let compose_file = runtime.compose_file.clone();
        let dockerfile_rel = format!("{stack_rel}/{compose_file}");
        generate_main_compose(
            &project_name,
            workspace_root,
            &stack_rel,
            &dockerfile_rel,
            &frontend_rel,
            &gloss_rel,
            ports,
            &database,
            &profile_path,
        )
    };

    Ok(RuntimePlan { compose_yaml })
}

fn generate_worktree_compose(
    project_name: &str,
    workspace_root: &Path,
    knithub_src: &str,
    dockerfile: &str,
    frontend_src: &str,
    gloss_src: &str,
    ports: &AllocatedPorts,
    database: &ProjectRuntimeDatabase,
    profile_path: &str,
) -> String {
    let workspace = workspace_root.display();
    let frontend_context = workspace_root.join(frontend_src).display().to_string();
    let gloss_context = workspace_root.join(gloss_src).display().to_string();
    format!(
        r#"name: {project_name}

services:
  backend:
    container_name: {project_name}-backend
    build:
      context: {workspace}
      dockerfile: {dockerfile}
      args:
        KNITHUB_SRC: {knithub_src}
    extra_hosts:
      - "host.docker.internal:host-gateway"
    environment:
      DATABASE_HOST: {db_host}
      DATABASE_PORT: "{db_port}"
      DATABASE_NAME: {db_name}
      DATABASE_USER: postgres
      DATABASE_PASSWORD: postgres
      PHX_BIND_IP: 0.0.0.0
      PORT: "4000"
      KNITHUB_ALLOWED_ORIGINS: http://localhost:{frontend_port}
      KNITHUB_FRONTEND_URL: http://localhost:{frontend_port}{profile_path}
      KNITHUB_KNIT_BIN: /usr/local/bin/knit
      GH_TOKEN: ${{GH_TOKEN:-}}
      KNITHUB_SEJ_LOCAL_WORKSPACE_ROOT: {workspace}
    ports:
      - "{backend_port}:4000"
    volumes:
      - {workspace}:{workspace}
      - ${{HOME}}/.config/gh:/root/.config/gh:ro

  frontend:
    container_name: {project_name}-frontend
    build:
      context: {frontend_context}
      dockerfile: Dockerfile
      additional_contexts:
        gloss-web-ui: {gloss_context}
    environment:
      VITE_KNITHUB_API_URL: http://localhost:{backend_port}
      VITE_KNITHUB_USE_MOCKS: "false"
    ports:
      - "{frontend_port}:5173"
    depends_on:
      - backend
"#,
        project_name = project_name,
        workspace = workspace,
        dockerfile = dockerfile,
        knithub_src = knithub_src,
        db_host = database.host,
        db_port = database.port,
        db_name = database.name,
        frontend_port = ports.frontend,
        backend_port = ports.backend,
        profile_path = profile_path,
        frontend_context = frontend_context,
        gloss_context = gloss_context,
    )
}

fn generate_main_compose(
    project_name: &str,
    workspace_root: &Path,
    knithub_src: &str,
    _compose_file: &str,
    frontend_src: &str,
    gloss_src: &str,
    ports: &AllocatedPorts,
    database: &ProjectRuntimeDatabase,
    profile_path: &str,
) -> String {
    generate_worktree_compose(
        project_name,
        workspace_root,
        knithub_src,
        &format!("{knithub_src}/Dockerfile"),
        frontend_src,
        gloss_src,
        ports,
        database,
        profile_path,
    )
}

fn allocate_ports(
    root: &Path,
    _bundle_id: &str,
    config: Option<ProjectRuntimePorts>,
) -> Result<AllocatedPorts> {
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
            return Ok(AllocatedPorts {
                backend,
                frontend,
            });
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

    for entry in fs::read_dir(&runs_dir).context("failed to read runtime runs directory")? {
        let entry = entry?;
        let state_path = entry.path().join("state.json");
        if !state_path.exists() {
            continue;
        }
        let state: RuntimeRunState = read_json(&state_path)?;
        used.insert(state.backend_port);
        used.insert(state.frontend_port);
    }

    Ok(used)
}

fn port_available(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn ensure_database_reachable(database: &ProjectRuntimeDatabase) -> Result<()> {
    let addr = format!("127.0.0.1:{}", database.port);
    if TcpStream::connect(&addr).is_err() {
        bail!(
            "Could not connect to the main dev database on localhost:{}. Start it first with `docker compose up -d db` from the main stack repo.",
            database.port
        );
    }
    Ok(())
}

fn resolve_stack_repo<'a>(active: &'a ActiveBundle, runtime: &ProjectRuntime) -> Result<&'a RepoEntry> {
    let stack_repo_id = runtime
        .stack_repo
        .as_deref()
        .unwrap_or("knithub");
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

fn is_worktree_checkout(path: &Path) -> bool {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|window| window[0] == ".knit" && window[1] == "worktrees")
}

fn relative_path(base: &Path, target: &Path) -> Result<String> {
    let base = base
        .canonicalize()
        .unwrap_or_else(|_| base.to_path_buf());
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
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
    compose_file: String,
    profile_path: Option<String>,
    started_at: String,
}

struct RuntimePlan {
    compose_yaml: String,
}

struct AllocatedPorts {
    backend: u16,
    frontend: u16,
}
