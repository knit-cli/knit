//! `knit run up`: build and start the per-bundle stacks. Owns stack
//! preparation for both modes (transform and contract), cross-stack port
//! wiring, host-port allocation across live runtimes, the `KNIT_*`
//! environment contract, and database resolution.

use super::state::{
    compose_project_name, frontend_port, runtime_run_dir, RuntimeRunState, RuntimeStackState,
    StateDatabase,
};
use super::transform::{self, ServicePort};
use super::StackPlan;
use crate::checkout::checkout_dir;
use crate::git::rev_parse;
use crate::model::{
    DatabaseMode, KnitProject, ProjectRuntime, ProjectRuntimeDatabase, RuntimeMode,
};
use crate::output as out;
use crate::store::{read_json, write_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const BUNDLE_DB_PROFILE: &str = "bundle-db";

/// A stack prepared for launch but not yet started.
enum Prepared {
    Transform {
        config: Value,
        port_map: Vec<(u16, u16)>,
    },
    Contract {
        profiles: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

struct Ready {
    ports: Vec<ServicePort>,
    database: Option<StateDatabase>,
    prepared: Prepared,
}

/// Start every planned stack: prepare all of them first (so cross-stack port
/// wiring sees every port map), print the full plan, then `docker compose up`
/// each stack in bundle order. Run state is recorded only after every stack
/// starts, so a failed `up` leaves no phantom state — `knit run down` still
/// cleans up by derived project names.
pub(super) fn run_up_stacks(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    runtime: &ProjectRuntime,
    plans: Vec<StackPlan<'_>>,
) -> Result<()> {
    let multi = plans.len() > 1;
    let mut taken = load_used_ports(&active.root)?;
    let step = runtime.ports.clone().unwrap_or_default().step.max(1);

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

    // Shared-database reachability gates contract stacks; check it once.
    let database_config = runtime.database.clone().unwrap_or_default();
    if let Some(contract) = plans.iter().find(|plan| plan.mode == RuntimeMode::Contract) {
        if database_config.mode == DatabaseMode::Shared {
            ensure_shared_database_reachable(&database_config, &contract.checkout)?;
        }
    }

    // Phase 1: prepare every stack without starting docker.
    let mut ready: Vec<Ready> = Vec::new();
    for plan in &plans {
        match plan.mode {
            RuntimeMode::Transform => {
                let source_dir = PathBuf::from(&plan.repo.path);
                let mut config = transform::resolve_compose_config(&plan.compose, &source_dir)?;
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
                let (ports, port_map) =
                    transform::transform_compose(&mut config, &repo_map, &mut allocate)?;
                ready.push(Ready {
                    ports,
                    database: None,
                    prepared: Prepared::Transform { config, port_map },
                });
            }
            RuntimeMode::Contract => {
                let resolved = resolve_database(&database_config, &active.bundle.id, &mut taken);
                let ports_config = runtime.ports.clone().unwrap_or_default();
                let service_ports = allocate_service_ports(
                    &taken,
                    &ports_config.service_bases(),
                    ports_config.step.max(1),
                )?;
                taken.extend(service_ports.values().copied());
                let env = runtime_env(
                    active,
                    project,
                    &plan.project_name,
                    &service_ports,
                    &resolved,
                );
                let profiles = if resolved.mode == DatabaseMode::Bundle {
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
                if resolved.mode == DatabaseMode::Bundle {
                    ports.push(ServicePort {
                        service: "db".to_string(),
                        host: resolved.host_port,
                        container: Some(5432),
                    });
                }
                let database = Some(StateDatabase {
                    mode: resolved.mode,
                    port: resolved.host_port,
                    name: resolved.name.clone(),
                });
                ready.push(Ready {
                    ports,
                    database,
                    prepared: Prepared::Contract { profiles, env },
                });
            }
        }
    }

    // Phase 2: cross-stack wiring. References to a SIBLING stack's old
    // published port are rewritten to its new bundle port, so stacks find
    // each other's bundle instances instead of the dev ones. A stack's own
    // ports were already rewritten in phase 1 and win; old ports that are
    // ambiguous across siblings are left alone.
    if multi {
        let all_maps: Vec<Vec<(u16, u16)>> = ready
            .iter()
            .map(|entry| match &entry.prepared {
                Prepared::Transform { port_map, .. } => port_map.clone(),
                Prepared::Contract { .. } => Vec::new(),
            })
            .collect();
        for (index, entry) in ready.iter_mut().enumerate() {
            let Prepared::Transform { config, port_map } = &mut entry.prepared else {
                continue;
            };
            let own: BTreeSet<u16> = port_map.iter().map(|(old, _)| *old).collect();
            let mut candidates: BTreeMap<u16, BTreeSet<u16>> = BTreeMap::new();
            for (other_index, map) in all_maps.iter().enumerate() {
                if other_index == index {
                    continue;
                }
                for (old, new) in map {
                    if !own.contains(old) {
                        candidates.entry(*old).or_default().insert(*new);
                    }
                }
            }
            let cross: Vec<(u16, u16)> = candidates
                .into_iter()
                .filter_map(|(old, news)| {
                    (news.len() == 1).then(|| (old, news.into_iter().next().unwrap()))
                })
                .collect();
            transform::rewrite_extra_port_references(config, &cross);
        }
    }

    // Phase 3: write generated files, print the full plan, then start every
    // stack in bundle order.
    let run_dir = runtime_run_dir(&active.root, &active.bundle.id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;

    println!(
        "{} {}",
        out::heading("Runtime up:"),
        out::repo(&active.bundle.id)
    );
    let mut stack_states: Vec<RuntimeStackState> = Vec::new();
    for (plan, entry) in plans.iter().zip(&ready) {
        let compose_file: PathBuf = match &entry.prepared {
            Prepared::Transform { config, .. } => {
                // JSON is valid YAML, so the generated file stays a compose file.
                let generated = if multi {
                    run_dir.join(format!("docker-compose.{}.yml", plan.repo.id))
                } else {
                    run_dir.join("docker-compose.yml")
                };
                fs::write(&generated, serde_json::to_string_pretty(config)?)
                    .context("failed to write generated compose file")?;
                generated
            }
            Prepared::Contract { .. } => plan.compose.clone(),
        };
        if multi {
            println!(
                "{} {} ({})",
                out::heading("Stack:"),
                out::repo(&plan.repo.id),
                out::muted(&plan.project_name)
            );
        }
        println!(
            "{} {}",
            out::muted("Compose:"),
            out::path(compose_file.display())
        );
        for port in &entry.ports {
            println!(
                "{} {} localhost:{}",
                out::muted("Port:"),
                port.service,
                port.host
            );
        }
        let (profiles, env) = match &entry.prepared {
            Prepared::Contract { profiles, env } => (profiles.clone(), env.clone()),
            Prepared::Transform { .. } => (Vec::new(), BTreeMap::new()),
        };
        stack_states.push(RuntimeStackState {
            repo: plan.repo.id.clone(),
            project_name: plan.project_name.clone(),
            mode: plan.mode,
            compose_file: compose_file
                .strip_prefix(&active.root)
                .unwrap_or(&compose_file)
                .display()
                .to_string(),
            ports: entry.ports.clone(),
            profiles,
            env,
            database: entry.database.clone(),
        });
    }
    let all_ports: Vec<ServicePort> = stack_states
        .iter()
        .flat_map(|stack| stack.ports.clone())
        .collect();
    if let (Some(profile), Some(frontend)) =
        (runtime.profile_path.as_deref(), frontend_port(&all_ports))
    {
        println!(
            "{} http://localhost:{}{}",
            out::heading("Open:"),
            frontend,
            profile
        );
    }

    for (plan, stack) in plans.iter().zip(&stack_states) {
        let mut command = Command::new("docker");
        command.args(["compose", "-f"]);
        command.arg(active.root.join(&stack.compose_file));
        command.args(["-p", &plan.project_name]);
        for profile in &stack.profiles {
            command.args(["--profile", profile]);
        }
        let status = command
            .args(["up", "--build", "-d"])
            .envs(&stack.env)
            .current_dir(&plan.checkout)
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
    }

    // Recorded only after every stack starts so a failed `up` does not leave
    // phantom run state.
    let first = &stack_states[0];
    let state = RuntimeRunState {
        bundle_id: active.bundle.id.clone(),
        stack_repo: first.repo.clone(),
        mode: first.mode,
        ports: all_ports,
        database: stack_states.iter().find_map(|stack| stack.database.clone()),
        compose_file: first.compose_file.clone(),
        profiles: first.profiles.clone(),
        env: first.env.clone(),
        profile_path: runtime.profile_path.clone(),
        started_at: crate::time::now_iso(),
        stacks: stack_states,
    };
    write_json(&run_dir.join("state.json"), &state)?;
    Ok(())
}

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

fn allocate_service_ports(
    used: &BTreeSet<u16>,
    bases: &BTreeMap<String, u16>,
    step: u16,
) -> Result<BTreeMap<String, u16>> {
    allocate_service_ports_with(used, bases, step, port_available)
}

fn allocate_service_ports_with(
    used: &BTreeSet<u16>,
    bases: &BTreeMap<String, u16>,
    step: u16,
    mut available: impl FnMut(u16) -> bool,
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
                || !available(port)
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
        let alive = running.contains(&compose_project_name(&bundle_id))
            || state
                .stacks
                .iter()
                .any(|stack| running.contains(&stack.project_name));
        if alive {
            for port in &state.ports {
                used.insert(port.host);
            }
            if let Some(database) = &state.database {
                used.insert(database.port);
            }
            for stack in &state.stacks {
                for port in &stack.ports {
                    used.insert(port.host);
                }
                if let Some(database) = &stack.database {
                    used.insert(database.port);
                }
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
    if TcpListener::bind(("0.0.0.0", port)).is_err() {
        return false;
    }
    TcpListener::bind(("127.0.0.1", port)).is_ok()
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

fn resolve_database(
    database: &ProjectRuntimeDatabase,
    bundle_id: &str,
    taken: &mut BTreeSet<u16>,
) -> ResolvedDatabase {
    if database.mode == DatabaseMode::Bundle {
        let template = database
            .name_template
            .as_deref()
            .unwrap_or("app_{bundleId}");
        let name = template.replace("{bundleId}", bundle_id);
        // Multiple bundle-db stacks in one run each need their own host port.
        let base = database.port_base.unwrap_or(5437);
        let mut host_port = base;
        while taken.contains(&host_port) && host_port < 65000 {
            host_port = host_port.saturating_add(1);
        }
        taken.insert(host_port);
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
    use crate::model::{ChangeGroup, RepoEntry};
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
    fn resolve_database_bundle_mode_names_per_bundle() {
        let database = ProjectRuntimeDatabase {
            mode: DatabaseMode::Bundle,
            ..Default::default()
        };
        let mut taken = BTreeSet::new();
        let resolved = resolve_database(&database, "venue-capacity", &mut taken);
        assert_eq!(resolved.name, "app_venue-capacity");
        assert_eq!(resolved.host, "db");
        assert_eq!(resolved.port, 5432);
        assert_eq!(resolved.host_port, 5437);
        // A second bundle-db stack in the same run steps past the taken port.
        let second = resolve_database(&database, "venue-capacity", &mut taken);
        assert_eq!(second.host_port, 5438);
    }

    #[test]
    fn allocate_service_ports_steps_pools_together() {
        let bases = BTreeMap::from([
            ("backend".to_string(), 4001u16),
            ("frontend".to_string(), 5174u16),
        ]);
        let used = BTreeSet::from([4001u16, 5174u16]);
        let allocated = allocate_service_ports_with(&used, &bases, 10, |_| true).unwrap();
        assert_eq!(allocated["backend"] - 4001, allocated["frontend"] - 5174);
        assert!(allocated["backend"] >= 4011);
    }
}
