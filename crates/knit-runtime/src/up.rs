//! `knit run up`: build and start the per-bundle stacks. Owns stack
//! preparation for both modes (transform and contract), cross-stack port
//! wiring, host-port allocation across live runtimes, the `KNIT_*`
//! environment contract, and database resolution.

use crate::bindings::{self, BindingLocation, EndpointRegistry};
use crate::config::{DatabaseMode, ProjectRuntime, ProjectRuntimeDatabase, RuntimeMode};
use crate::database::{self, SharedDatabaseAttachment};
use crate::envfile;
use crate::plan::StackPlan;
use crate::state::{
    compose_project_name, frontend_port, runtime_run_dir, RuntimeRunState, RuntimeStackState,
    StateDatabase,
};
use crate::support::{env_var_suffix, out, read_json, rev_parse, write_json};
use crate::transform::{self, ServicePort};
use crate::RuntimeContext;
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
        /// Per-service `env_file` references from the `--no-env-resolution`
        /// pass, used to keep env-file values (secrets included) out of the
        /// generated file. `None` when the local compose lacks the flag —
        /// values then stay inlined, as before.
        env_files: Option<BTreeMap<String, Vec<envfile::EnvFileRef>>>,
        /// Endpoint references (`localhost:<port>` and friends) captured
        /// from the resolved compose before port rewriting, so phase 2 can
        /// tell authored references apart from values knit itself wrote.
        references: Vec<transform::PortReference>,
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

#[derive(Clone)]
struct ReusableStack {
    ports: Vec<ServicePort>,
    database: Option<StateDatabase>,
}

/// Start every planned stack: prepare all of them first (so cross-stack port
/// wiring sees every port map), print the full plan, then `docker compose up`
/// each stack in bundle order. Run state is recorded only after every stack
/// starts, so a failed `up` leaves no phantom state — `knit run down` still
/// cleans up by derived project names.
pub(crate) fn run_up_stacks(
    ctx: &RuntimeContext,
    runtime: &ProjectRuntime,
    plans: Vec<StackPlan>,
) -> Result<()> {
    if runtime.startup_timeout_seconds == 0 {
        bail!("runtime.startupTimeoutSeconds must be greater than zero");
    }
    if let Some(database) = &runtime.database {
        for repo in &database.repos {
            if !ctx.repos.iter().any(|known| &known.id == repo)
                && !ctx.extra_checkouts.iter().any(|(known, _)| known == repo)
            {
                bail!("runtime.database.repos names unknown repository `{repo}`");
            }
        }
    }
    bindings::validate_bindings(&runtime.bindings)?;
    for binding in &runtime.bindings {
        for repo in [&binding.repo, &binding.target.repo] {
            if !ctx.repos.iter().any(|known| &known.id == repo)
                && !ctx.extra_checkouts.iter().any(|(known, _)| known == repo)
            {
                bail!("runtime.bindings names unknown repository `{repo}`");
            }
        }
        if let Some(consumer) = plans.iter().find(|plan| plan.repo.id == binding.repo) {
            if consumer.mode == RuntimeMode::Contract
                || plans.iter().any(|plan| {
                    plan.repo.id == binding.target.repo && plan.mode == RuntimeMode::Contract
                })
            {
                bail!("runtime.bindings requires transform-mode stacks; binding from repo `{}` to `{}` includes a contract stack", binding.repo, binding.target.repo);
            }
        }
    }
    let multi = plans.len() > 1;
    let running = running_compose_projects();
    let previous_state = load_allocations(&runtime_run_dir(&ctx.root, &ctx.bundle_id));
    // Other live bundles reserve their ports. This bundle's recorded ports
    // are handled separately as preferred allocations: when its compose
    // project is already running, Docker itself owns those listeners and a
    // repeated `up` must be allowed to keep them.
    let mut taken = load_used_ports(&ctx.root, &ctx.bundle_id, &running)?;
    let step = runtime.ports.clone().unwrap_or_default().step.max(1);

    let repo_map: Vec<(PathBuf, PathBuf)> = ctx
        .repos
        .iter()
        .filter_map(|repo| {
            let checkout = repo.checkout.clone()?;
            let source = crate::support::canonicalize(&repo.source_path).ok()?;
            (source != checkout).then_some((source, checkout))
        })
        .collect();

    // Shared-database reachability gates contract stacks and transform stacks
    // that strip their db service; check it once. Only an explicitly
    // configured database gates: the default database block is shared-mode
    // too, but a project without one has no dev database to reach (its
    // stacks run whatever db services their compose files define).
    let database_configured = runtime.database.is_some();
    let database_config = runtime.database.clone().unwrap_or_default();
    let mut shared_db_checked = false;
    for contract in plans
        .iter()
        .filter(|plan| plan.mode == RuntimeMode::Contract)
    {
        let uses_database = fs::read_to_string(&contract.compose)?.contains("${KNIT_DB_");
        if uses_database
            && !database_config.repos.is_empty()
            && !database_config.repos.contains(&contract.repo.id)
        {
            bail!("Contract stack `{}` references KNIT_DB_* but is excluded by runtime.database.repos", contract.repo.id);
        }
        if uses_database
            && database_configured
            && database_config.mode == DatabaseMode::Shared
            && !shared_db_checked
        {
            ensure_shared_database_reachable(&database_config, &contract.checkout)?;
            shared_db_checked = true;
        }
    }

    // Phase 1: prepare every stack without starting docker.
    let mut ready: Vec<Ready> = Vec::new();
    let mut original_configs: Vec<(String, Value)> = Vec::new();
    for plan in &plans {
        let reusable = previous_state
            .as_ref()
            .and_then(|state| reusable_stack(state, plan));
        let reuse_bound_ports = reusable.is_some() && running.contains(&plan.project_name);
        match plan.mode {
            RuntimeMode::Transform => {
                let source_dir = plan.repo.source_path.clone();
                let mut config = transform::resolve_compose_config(&plan.compose, &source_dir)?;
                // A second, unresolved pass records which environment values
                // come from env files, so the generated file can reference
                // them in place instead of carrying copies.
                let env_files =
                    transform::resolve_compose_config_no_env(&plan.compose, &source_dir)
                        .map(|unresolved| envfile::service_env_files(&unresolved))
                        .ok();
                // Remove shared database services before allocating their ports.
                // Keep authored database references until endpoint rewriting is
                // finished so an unrelated source port cannot remap the shared DB.
                let mut database: Option<StateDatabase> = None;
                let attachment = database::shared_database_attachment(
                    &plan.repo.id,
                    &database_config,
                    &database_config.repos,
                    &config,
                    multi,
                );
                if database_config.mode == DatabaseMode::Shared {
                    if let SharedDatabaseAttachment::Keep(Some(reason)) = &attachment {
                        println!("{} {reason}", out::heading("Database:"));
                    }
                }
                if database_config.mode == DatabaseMode::Shared
                    && attachment == SharedDatabaseAttachment::Attach
                {
                    if let Some(service) = &database_config.service {
                        let container_port = database_config.container_port.unwrap_or(5432);
                        if transform::strip_shared_database(
                            &mut config,
                            service,
                            service,
                            container_port,
                            container_port,
                        ) {
                            if !shared_db_checked {
                                ensure_shared_database_reachable(&database_config, &plan.checkout)?;
                                shared_db_checked = true;
                            }
                            database = Some(StateDatabase {
                                mode: DatabaseMode::Shared,
                                port: database_config.port,
                                name: database_config.name.clone(),
                            });
                        }
                    }
                }
                // Only remaining services can consume sibling endpoints. Capture
                // references before port rewriting changes their authored ports.
                let references = transform::collect_port_references(&config);
                original_configs.push((plan.repo.id.clone(), config.clone()));
                let mut preferred = reusable
                    .as_ref()
                    .map(|stack| stack.ports.clone())
                    .unwrap_or_default();
                let mut allocate =
                    |service: &str, old: u16, container: Option<u16>| -> Result<u16> {
                        if let Some(index) = preferred
                            .iter()
                            .position(|port| port.service == service && port.container == container)
                        {
                            let candidate = preferred.remove(index).host;
                            if can_reuse_recorded_port(candidate, &taken, reuse_bound_ports) {
                                taken.insert(candidate);
                                return Ok(candidate);
                            }
                        }
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
                    transform::prepare_compose(&mut config, &repo_map, &mut allocate)?;
                ready.push(Ready {
                    ports,
                    database,
                    prepared: Prepared::Transform {
                        config,
                        port_map,
                        env_files,
                        references,
                    },
                });
            }
            RuntimeMode::Contract => {
                let preferred_database = reusable
                    .as_ref()
                    .and_then(|stack| stack.database.as_ref())
                    .map(|database| database.port);
                let resolved = resolve_database(
                    &database_config,
                    &ctx.bundle_id,
                    &mut taken,
                    preferred_database,
                    reuse_bound_ports,
                );
                let ports_config = runtime.ports.clone().unwrap_or_default();
                let bases = contract_port_bases(&plan.compose, runtime.ports.as_ref())?;
                let preferred_ports: BTreeMap<String, u16> = reusable
                    .as_ref()
                    .map(|stack| {
                        stack
                            .ports
                            .iter()
                            .map(|port| (port.service.clone(), port.host))
                            .collect()
                    })
                    .unwrap_or_default();
                let service_ports = allocate_service_ports(
                    &taken,
                    &bases,
                    ports_config.step.max(1),
                    &preferred_ports,
                    reuse_bound_ports,
                )?;
                taken.extend(service_ports.values().copied());
                let env = runtime_env(ctx, &plan.project_name, &service_ports, &resolved);
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
                        source_host: bases.get(service).copied(),
                    })
                    .collect();
                if resolved.mode == DatabaseMode::Bundle {
                    ports.push(ServicePort {
                        service: "db".to_string(),
                        host: resolved.host_port,
                        container: Some(5432),
                        source_host: None,
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

    // Capture explicit dependencies before any textual port rewrite. Automatic
    // own/sibling mappings are applied together exactly once to original values.
    let captured = bindings::capture_binding_values(
        &runtime.bindings,
        &original_configs
            .iter()
            .map(|(repo, config)| (repo.clone(), config))
            .collect::<Vec<_>>(),
    )?;
    let bound_locations: BTreeSet<BindingLocation> = captured.locations().cloned().collect();
    {
        // Per stack: (repo id, service, old host port, new host port) for
        // every published port. Transform stacks only: contract stacks run
        // as-is and never join cross-stack rewriting.
        let all_wires: Vec<Vec<(String, String, u16, u16)>> = ready
            .iter()
            .zip(plans.iter())
            .map(|(entry, plan)| match &entry.prepared {
                Prepared::Transform { port_map, .. } => port_map
                    .iter()
                    .zip(entry.ports.iter())
                    .map(|((old, new), port)| {
                        (plan.repo.id.clone(), port.service.clone(), *old, *new)
                    })
                    .collect(),
                Prepared::Contract { .. } => Vec::new(),
            })
            .collect();
        let mut violations: Vec<String> = Vec::new();
        for (index, entry) in ready.iter_mut().enumerate() {
            let Prepared::Transform {
                config,
                port_map,
                references,
                ..
            } = &mut entry.prepared
            else {
                continue;
            };
            let own: BTreeSet<u16> = port_map.iter().map(|(old, _)| *old).collect();
            let mut siblings: Vec<(String, String, u16, u16)> = Vec::new();
            for (other_index, wires) in all_wires.iter().enumerate() {
                if other_index != index {
                    siblings.extend(wires.iter().cloned());
                }
            }
            let unbound: Vec<_> = references
                .iter()
                .filter(|reference| {
                    !bound_locations.contains(&BindingLocation {
                        repo: plans[index].repo.id.clone(),
                        service: reference.service.clone(),
                        field: reference.field,
                        key: reference.key.clone(),
                    })
                })
                .cloned()
                .collect();
            let (mut cross, found) =
                wire_cross_stack_ports(&plans[index].repo.id, &unbound, &own, &siblings);
            cross.extend(port_map.iter().copied());
            transform::rewrite_extra_port_references(config, &cross);
            violations.extend(found);
        }
        if !violations.is_empty() {
            let mut message = String::from(
                "Ambiguous cross-stack port references; no application stacks were started:\n",
            );
            for violation in &violations {
                message.push_str(violation);
                message.push('\n');
            }
            message.push_str(
                "Declare the intended provider repo and service in runtime.bindings \
                 for each consumer key. Knit then supplies the allocated port for \
                 every bundle without changing Compose source ports.",
            );
            bail!("{message}");
        }
    }

    if !captured.is_empty() {
        let registry = EndpointRegistry::from_service_ports(
            &plans
                .iter()
                .zip(&ready)
                .map(|(plan, entry)| (plan.repo.id.clone(), entry.ports.as_slice()))
                .collect::<Vec<_>>(),
        );
        let mut configs: Vec<_> = plans
            .iter()
            .zip(ready.iter_mut())
            .filter_map(|(plan, entry)| match &mut entry.prepared {
                Prepared::Transform { config, .. } => Some((plan.repo.id.clone(), config)),
                Prepared::Contract { .. } => None,
            })
            .collect();
        for binding in bindings::apply_binding_values(&captured, &registry, &mut configs)? {
            println!(
                "Binding: {}/{} {} {} -> {}/{} localhost:{}",
                binding.location.repo,
                binding.location.service,
                binding.location.field,
                binding.location.key,
                binding.target_repo,
                binding.target_service,
                binding.port
            );
        }
    }

    for entry in &mut ready {
        if entry
            .database
            .as_ref()
            .is_some_and(|db| db.mode == DatabaseMode::Shared)
        {
            if let (Prepared::Transform { config, .. }, Some(service)) =
                (&mut entry.prepared, &database_config.service)
            {
                transform::rewrite_shared_database_references(
                    config,
                    service,
                    &database_config.host,
                    database_config.port,
                    database_config.container_port.unwrap_or(5432),
                );
            }
        }
    }

    // Phase 3: write generated files, print the full plan, then start every
    // stack in bundle order.
    let run_dir = runtime_run_dir(&ctx.root, &ctx.bundle_id);
    fs::create_dir_all(&run_dir).context("failed to create runtime run directory")?;

    println!(
        "{} {}",
        out::heading("Runtime up:"),
        out::repo(&ctx.bundle_id)
    );
    let mut stack_states: Vec<RuntimeStackState> = Vec::new();
    let mut any_transform_remap = false;
    for (plan, entry) in plans.iter().zip(ready.iter_mut()) {
        let mut env_notes: Vec<String> = Vec::new();
        let compose_file: PathBuf = match &mut entry.prepared {
            Prepared::Transform {
                config, env_files, ..
            } => {
                // Env-file values are referenced in place, not copied: the
                // resolved config would otherwise persist secrets from the
                // repo's env files inside this generated artifact.
                match env_files {
                    Some(refs) => {
                        envfile::detach_env_files(config, refs, &|path| path.display().to_string());
                        let mut seen = BTreeSet::new();
                        for reference in refs.values().flatten() {
                            if seen.insert(reference.path.clone()) {
                                env_notes.push(format!(
                                    "{} {} {}",
                                    out::muted("Env file:"),
                                    out::path(reference.path.display()),
                                    out::muted("(referenced, not copied)")
                                ));
                            }
                        }
                    }
                    None => env_notes.push(out::muted(
                        "Env files: inlined (this docker compose lacks --no-env-resolution)",
                    )),
                }
                // JSON is valid YAML, so the generated file stays a compose file.
                let generated = if multi {
                    run_dir.join(format!("docker-compose.{}.yml", plan.repo.id))
                } else {
                    run_dir.join("docker-compose.yml")
                };
                fs::write(&generated, serde_json::to_string_pretty(config)?)
                    .context("failed to write generated compose file")?;
                // Values the detach could not prove came from an env file
                // (and knit's own rewrites of env-file values) may still be
                // sensitive: keep the artifact owner-only.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&generated, fs::Permissions::from_mode(0o600));
                }
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
        for note in &env_notes {
            println!("{note}");
        }
        for port in &entry.ports {
            match port.source_host.filter(|source| *source != port.host) {
                Some(source) => {
                    if plan.mode == RuntimeMode::Transform {
                        any_transform_remap = true;
                    }
                    println!(
                        "{} {} localhost:{} {}",
                        out::muted("Port:"),
                        port.service,
                        port.host,
                        out::muted(format!("(source {source})"))
                    );
                }
                None => println!(
                    "{} {} localhost:{}",
                    out::muted("Port:"),
                    port.service,
                    port.host
                ),
            }
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
                .strip_prefix(&ctx.root)
                .unwrap_or(&compose_file)
                .display()
                .to_string(),
            ports: entry.ports.clone(),
            profiles,
            env,
            database: entry.database.clone(),
        });
    }
    if any_transform_remap {
        println!(
            "{}",
            out::muted(
                "Note: port references declared in the compose files were rewritten to the bundle ports; ports hardcoded in app source still point at the source ports."
            )
        );
    }
    let all_ports: Vec<ServicePort> = stack_states
        .iter()
        .flat_map(|stack| stack.ports.clone())
        .collect();

    // Persist allocations before launching: retries reuse ports even when
    // readiness fails. This file is not a successful-run record.
    let first = &stack_states[0];
    let mut state = RuntimeRunState {
        bundle_id: ctx.bundle_id.clone(),
        stack_repo: first.repo.clone(),
        mode: first.mode,
        ports: all_ports.clone(),
        database: stack_states.iter().find_map(|stack| stack.database.clone()),
        compose_file: first.compose_file.clone(),
        profiles: first.profiles.clone(),
        env: first.env.clone(),
        profile_path: runtime.profile_path.clone(),
        started_at: String::new(),
        stacks: stack_states.clone(),
    };
    write_json(&run_dir.join("allocations.json"), &state)?;

    for (plan, stack) in plans.iter().zip(&stack_states) {
        let mut command = Command::new("docker");
        command.args(["compose", "-f"]);
        command.arg(ctx.root.join(&stack.compose_file));
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
            let hint = if stack.mode == RuntimeMode::Transform {
                " If the lifted compose shape is wrong for this repo, run `knit run eject` to materialize an editable docker-compose.knit.yml and adjust it."
            } else {
                ""
            };
            bail!(
                "docker compose exited with status {status}. Clean up partial containers with `knit run down`.{hint}"
            );
        }
    }

    for (plan, stack) in plans.iter().zip(&stack_states) {
        let compose_file = ctx.root.join(&stack.compose_file);
        let report =
            crate::readiness::verify_stack_startup(&crate::readiness::StackStartupCheck {
                repo: &plan.repo.id,
                project_name: &plan.project_name,
                compose_file: &compose_file,
                profiles: &stack.profiles,
                env: &stack.env,
                checkout: &plan.checkout,
                timeout_seconds: runtime.startup_timeout_seconds,
            })?;
        println!("Ready: {} — {}", plan.repo.id, report.summary());
    }
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

    // App code that hardcodes a source port fails silently against a dead
    // port — or reaches the dev stack when it is still running. Surface the
    // source ports something is actually listening on.
    let mut busy: BTreeMap<u16, String> = BTreeMap::new();
    for stack in &stack_states {
        if stack.mode != RuntimeMode::Transform {
            continue;
        }
        for port in &stack.ports {
            let Some(source) = port.source_host.filter(|source| *source != port.host) else {
                continue;
            };
            if !busy.contains_key(&source)
                && TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], source)),
                    Duration::from_millis(250),
                )
                .is_ok()
            {
                busy.insert(source, port.service.clone());
            }
        }
    }
    for (source, service) in &busy {
        println!(
            "{} source port {source} ({service}) is in use by another process — app code that hardcodes it reaches that stack, not this bundle",
            out::warn("Warn:")
        );
    }

    state.started_at = crate::support::now_iso();
    write_json(&run_dir.join("state.json"), &state)?;
    Ok(())
}

/// Cross-stack wiring for one transform stack. Sibling wires whose old host
/// port the stack does not publish itself (`own`) become rewrite candidates:
/// exactly one distinct sibling allocation rewrites the stack's references;
/// more than one is ambiguous and unrewritable, so every authored reference
/// to such a port is reported — the wiring would otherwise silently leave it
/// on the dead dev port. Returns the unambiguous `(old, new)` rewrite map
/// and violation descriptions naming the referencing repo/service/key, the
/// original port, and each candidate sibling destination. Never includes
/// environment values.
fn wire_cross_stack_ports(
    repo: &str,
    references: &[transform::PortReference],
    own: &BTreeSet<u16>,
    siblings: &[(String, String, u16, u16)],
) -> (Vec<(u16, u16)>, Vec<String>) {
    let mut candidates: BTreeMap<u16, BTreeSet<u16>> = BTreeMap::new();
    let mut detail: BTreeMap<u16, BTreeSet<(String, String, u16)>> = BTreeMap::new();
    for (sibling_repo, service, old, new) in siblings {
        if own.contains(old) {
            continue;
        }
        candidates.entry(*old).or_default().insert(*new);
        detail
            .entry(*old)
            .or_default()
            .insert((sibling_repo.clone(), service.clone(), *new));
    }
    let cross: Vec<(u16, u16)> = candidates
        .iter()
        .filter(|(_, news)| news.len() == 1)
        .map(|(old, news)| (*old, *news.iter().next().unwrap()))
        .collect();
    let mut violations = Vec::new();
    for reference in references {
        if candidates
            .get(&reference.port)
            .is_none_or(|news| news.len() <= 1)
        {
            continue;
        }
        let mut lines = String::new();
        for (sibling_repo, service, new) in &detail[&reference.port] {
            lines.push_str(&format!(
                "\n  - repo `{sibling_repo}` service `{service}` -> localhost:{new}"
            ));
        }
        violations.push(format!(
            "- repo `{repo}` service `{}` {} key `{}` references `{}:{}`, but {} candidate destinations publish source host port {}:{lines}",
            reference.service,
            reference.field,
            reference.key,
            reference.host,
            reference.port,
            candidates[&reference.port].len(),
            reference.port,
        ));
    }
    (cross, violations)
}

/// Build the `KNIT_*` environment contract for a runtime: bundle identity,
/// per-repo checkout paths and revisions, allocated ports, and the resolved
/// database. Covers every project repo plus any ad-hoc bundle repos; a repo
/// tracked in the bundle resolves to its bundle checkout, anything else to its
/// source checkout.
fn runtime_env(
    ctx: &RuntimeContext,
    project_name: &str,
    service_ports: &BTreeMap<String, u16>,
    database: &ResolvedDatabase,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("KNIT_ROOT".to_string(), ctx.root.display().to_string());
    env.insert("KNIT_BUNDLE".to_string(), ctx.bundle_id.clone());
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
    for (repo_id, path) in &ctx.extra_checkouts {
        checkouts.insert(repo_id.clone(), path.clone());
    }
    for repo in &ctx.repos {
        if let Some(checkout) = &repo.checkout {
            checkouts.insert(repo.id.clone(), checkout.clone());
        }
    }

    for (repo_id, checkout) in checkouts {
        let suffix = env_var_suffix(&repo_id);
        env.insert(
            format!("KNIT_CHECKOUT_{suffix}"),
            checkout.display().to_string(),
        );
        if let Ok(relative) = relative_path(&ctx.root, &checkout) {
            env.insert(format!("KNIT_SRC_{suffix}"), relative);
        }
        env.insert(
            format!("KNIT_REV_{suffix}"),
            rev_parse(&checkout, "HEAD").unwrap_or_else(|_| "unknown".to_string()),
        );
    }

    env
}

/// The port pools a contract stack allocates from: `${KNIT_PORT_*}` variables
/// scanned from the compose file (each `:-` default is that pool's base),
/// overlaid by the configured `runtime.ports` pools, which win per service.
/// This is what lets an ejected compose file — whose services are the repo's,
/// not knit's — run with no `ports` config at all. A scanned variable with no
/// default and no configured pool is an error: the allocator would have no
/// base to start from, and compose would silently interpolate an empty port.
fn contract_port_bases(
    compose_file: &Path,
    ports: Option<&crate::config::ProjectRuntimePorts>,
) -> Result<BTreeMap<String, u16>> {
    let text = fs::read_to_string(compose_file)
        .with_context(|| format!("failed to read {}", compose_file.display()))?;
    let scanned = scan_port_variables(&text);

    let mut bases: BTreeMap<String, u16> = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for (suffix, default) in &scanned {
        // Lowercased suffixes are valid service keys that round-trip back to
        // the same `KNIT_PORT_<suffix>` variable in the env contract.
        match default {
            Some(base) => {
                bases.insert(suffix.to_ascii_lowercase(), *base);
            }
            None => missing.push(suffix.clone()),
        }
    }
    if let Some(ports) = ports {
        for (service, base) in ports.service_bases() {
            let key = env_var_suffix(&service).to_ascii_lowercase();
            missing.retain(|suffix| suffix.to_ascii_lowercase() != key);
            bases.insert(key, base);
        }
    }
    if !missing.is_empty() {
        bail!(
            "{} references KNIT_PORT_{} without a `:-<port>` default or a matching `runtime.ports.services` pool, so no base port is known. Add a default (e.g. `${{KNIT_PORT_{}:-8080}}`) or configure the pool.",
            compose_file.display(),
            missing.join(", KNIT_PORT_"),
            missing[0]
        );
    }
    if bases.is_empty() {
        bases = crate::config::ProjectRuntimePorts::default().service_bases();
    }
    Ok(bases)
}

/// Every `${KNIT_PORT_<SUFFIX>}` reference in a compose file, with its
/// `:-<port>` default when one terminates the interpolation. A variable seen
/// both with and without a default keeps the default.
fn scan_port_variables(text: &str) -> BTreeMap<String, Option<u16>> {
    let mut vars: BTreeMap<String, Option<u16>> = BTreeMap::new();
    const MARKER: &str = "${KNIT_PORT_";
    let mut rest = text;
    while let Some(index) = rest.find(MARKER) {
        rest = &rest[index + MARKER.len()..];
        let suffix: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if suffix.is_empty() {
            continue;
        }
        let after = &rest[suffix.len()..];
        let default = after.strip_prefix(":-").and_then(|tail| {
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if tail[digits.len()..].starts_with('}') {
                digits.parse::<u16>().ok()
            } else {
                None
            }
        });
        let entry = vars.entry(suffix).or_insert(None);
        if default.is_some() {
            *entry = default;
        }
    }
    vars
}

fn reusable_stack(state: &RuntimeRunState, plan: &StackPlan) -> Option<ReusableStack> {
    if !state.stacks.is_empty() {
        return state
            .stacks
            .iter()
            .find(|stack| {
                stack.repo == plan.repo.id
                    && stack.project_name == plan.project_name
                    && stack.mode == plan.mode
            })
            .map(|stack| ReusableStack {
                ports: stack.ports.clone(),
                database: stack.database.clone(),
            });
    }

    (state.stack_repo == plan.repo.id
        && state.mode == plan.mode
        && plan.project_name == compose_project_name(&state.bundle_id))
    .then(|| ReusableStack {
        ports: state.ports.clone(),
        database: state.database.clone(),
    })
}

fn can_reuse_recorded_port(
    port: u16,
    used_by_other_bundles: &BTreeSet<u16>,
    current_project_running: bool,
) -> bool {
    !used_by_other_bundles.contains(&port) && (current_project_running || port_available(port))
}

/// Allocate one free host port per contract-mode service pool, stepping all
/// pools together so one bundle's ports stay a recognisable cohort.
fn allocate_service_ports(
    used: &BTreeSet<u16>,
    bases: &BTreeMap<String, u16>,
    step: u16,
    preferred: &BTreeMap<String, u16>,
    reuse_bound_ports: bool,
) -> Result<BTreeMap<String, u16>> {
    allocate_service_ports_with(
        used,
        bases,
        step,
        preferred,
        reuse_bound_ports,
        port_available,
    )
}

fn allocate_service_ports_with(
    used: &BTreeSet<u16>,
    bases: &BTreeMap<String, u16>,
    step: u16,
    preferred: &BTreeMap<String, u16>,
    reuse_bound_ports: bool,
    mut available: impl FnMut(u16) -> bool,
) -> Result<BTreeMap<String, u16>> {
    if bases.is_empty() {
        bail!("The project runtime defines no service port pools.");
    }

    // Contract pools move together by one offset. Preserve that invariant
    // when replaying state, and only accept a complete compatible cohort.
    let reusable = (|| -> Option<BTreeMap<String, u16>> {
        let mut reused = BTreeMap::new();
        let mut common_offset: Option<u16> = None;
        for (service, base) in bases {
            let port = *preferred.get(service)?;
            let offset = port.checked_sub(*base)?;
            if offset % step != 0
                || common_offset.is_some_and(|current| current != offset)
                || used.contains(&port)
                || reused.values().any(|taken| *taken == port)
                || (!reuse_bound_ports && !available(port))
            {
                return None;
            }
            common_offset = Some(offset);
            reused.insert(service.clone(), port);
        }
        Some(reused)
    })();
    if let Some(reused) = reusable {
        return Ok(reused);
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

fn load_allocations(run_dir: &Path) -> Option<RuntimeRunState> {
    read_json(&run_dir.join("allocations.json"))
        .or_else(|_| read_json(&run_dir.join("state.json")))
        .ok()
}

fn load_used_ports(
    root: &Path,
    current_bundle_id: &str,
    running: &BTreeSet<String>,
) -> Result<BTreeSet<u16>> {
    let mut used = BTreeSet::new();
    let runs_dir = root.join(".knit/runtime-runs");
    if !runs_dir.exists() {
        return Ok(used);
    }

    for entry in fs::read_dir(&runs_dir).context("failed to read runtime runs directory")? {
        let entry = entry?;
        let bundle_id = entry.file_name().to_string_lossy().into_owned();
        if bundle_id == current_bundle_id {
            continue;
        }
        let Some(state) = load_allocations(&entry.path()) else {
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
    preferred_host_port: Option<u16>,
    reuse_bound_ports: bool,
) -> ResolvedDatabase {
    if database.mode == DatabaseMode::Bundle {
        let template = database
            .name_template
            .as_deref()
            .unwrap_or("app_{bundleId}");
        let name = template.replace("{bundleId}", bundle_id);
        if let Some(host_port) = preferred_host_port
            .filter(|port| can_reuse_recorded_port(*port, taken, reuse_bound_ports))
        {
            taken.insert(host_port);
            return ResolvedDatabase {
                mode: DatabaseMode::Bundle,
                host: "db".to_string(),
                port: 5432,
                name,
                host_port,
            };
        }

        // Multiple bundle-db stacks in one run each need their own host port.
        // A stale recorded port that is now owned by another process is also
        // skipped instead of being immediately selected again.
        let base = database.port_base.unwrap_or(5437);
        let mut host_port = base;
        while (taken.contains(&host_port)
            || (Some(host_port) == preferred_host_port
                && !reuse_bound_ports
                && !port_available(host_port)))
            && host_port < 65000
        {
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
    let base = crate::support::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let target = crate::support::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
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
    use crate::RuntimeContext;

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
            crate::support::now_iso().replace([':', '.'], "")
        ));
        std::fs::create_dir_all(root.join("knithub")).unwrap();
        std::fs::create_dir_all(root.join("gloss-web-ui")).unwrap();

        let ctx = RuntimeContext {
            root: root.clone(),
            bundle_id: "demo".to_string(),
            repos: vec![crate::RuntimeRepo {
                id: "knithub".to_string(),
                source_path: root.join("knithub"),
                checkout: Some(root.join("knithub")),
            }],
            extra_checkouts: vec![("gloss-web-ui".to_string(), root.join("gloss-web-ui"))],
        };

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
        let env = runtime_env(&ctx, "knit-run-demo", &service_ports, &database);

        assert_eq!(env.get("KNIT_BUNDLE").unwrap(), "demo");
        assert_eq!(env.get("COMPOSE_PROJECT_NAME").unwrap(), "knit-run-demo");
        assert_eq!(env.get("KNIT_PORT_BACKEND").unwrap(), "4011");
        assert_eq!(env.get("KNIT_PORT_FRONTEND").unwrap(), "5184");
        assert_eq!(env.get("KNIT_DB_MODE").unwrap(), "shared");
        assert_eq!(env.get("KNIT_DB_NAME").unwrap(), "knithub_dev");
        assert_eq!(env.get("KNIT_DB_HOST_PORT").unwrap(), "5436");
        // Bundle repo resolves to its checkout; extra repo to its path.
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
        let resolved = resolve_database(&database, "venue-capacity", &mut taken, None, false);
        assert_eq!(resolved.name, "app_venue-capacity");
        assert_eq!(resolved.host, "db");
        assert_eq!(resolved.port, 5432);
        assert_eq!(resolved.host_port, 5437);
        // A second bundle-db stack in the same run steps past the taken port.
        let second = resolve_database(&database, "venue-capacity", &mut taken, None, false);
        assert_eq!(second.host_port, 5438);
    }

    #[test]
    fn resolve_database_reuses_live_projects_recorded_port() {
        let database = ProjectRuntimeDatabase {
            mode: DatabaseMode::Bundle,
            ..Default::default()
        };
        let mut taken = BTreeSet::new();

        let resolved = resolve_database(&database, "venue-capacity", &mut taken, Some(5447), true);

        assert_eq!(resolved.host_port, 5447);
    }

    #[test]
    fn scan_port_variables_reads_suffixes_and_defaults() {
        let text = "services:\n  web:\n    ports:\n      - \"${KNIT_PORT_WEB:-8080}:8080\"\n  api:\n    ports:\n      - \"${KNIT_PORT_API}:4000\"\n    environment:\n      SELF: http://localhost:${KNIT_PORT_API:-4001}\n      BAD: ${KNIT_PORT_ODD:-notaport}\n";
        let vars = scan_port_variables(text);
        assert_eq!(vars.get("WEB"), Some(&Some(8080)));
        // Seen with and without a default: the default wins.
        assert_eq!(vars.get("API"), Some(&Some(4001)));
        // A non-numeric default is no default.
        assert_eq!(vars.get("ODD"), Some(&None));
    }

    #[test]
    fn contract_port_bases_merges_scanned_and_configured_pools() {
        let dir = std::env::temp_dir().join(format!(
            "knit-contract-bases-test-{}-{}",
            std::process::id(),
            crate::support::now_iso().replace([':', '.'], "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.knit.yml");

        // Scanned defaults alone are enough: no ports config needed.
        std::fs::write(
            &compose,
            "services:\n  web:\n    ports: [\"${KNIT_PORT_WEB:-8080}:8080\"]\n  worker:\n    ports: [\"${KNIT_PORT_WORKER:-9000}:9000\"]\n",
        )
        .unwrap();
        let bases = contract_port_bases(&compose, None).unwrap();
        assert_eq!(bases["web"], 8080);
        assert_eq!(bases["worker"], 9000);

        // Configured pools win over scanned defaults and cover defaultless
        // variables.
        std::fs::write(
            &compose,
            "services:\n  web:\n    ports: [\"${KNIT_PORT_WEB:-8080}:8080\"]\n  api:\n    ports: [\"${KNIT_PORT_API}:4000\"]\n",
        )
        .unwrap();
        let ports = crate::config::ProjectRuntimePorts {
            services: BTreeMap::from([("web".to_string(), 8180u16), ("api".to_string(), 4100u16)]),
            ..Default::default()
        };
        let bases = contract_port_bases(&compose, Some(&ports)).unwrap();
        assert_eq!(bases["web"], 8180);
        assert_eq!(bases["api"], 4100);

        // A defaultless variable with no configured pool is an error.
        let error = contract_port_bases(&compose, None).unwrap_err().to_string();
        assert!(error.contains("KNIT_PORT_API"), "{error}");

        // No variables and no config: the default backend/frontend pair.
        std::fs::write(&compose, "services:\n  web:\n    image: nginx\n").unwrap();
        let bases = contract_port_bases(&compose, None).unwrap();
        assert_eq!(bases["backend"], 4001);
        assert_eq!(bases["frontend"], 5174);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn allocate_service_ports_steps_pools_together() {
        let bases = BTreeMap::from([
            ("backend".to_string(), 4001u16),
            ("frontend".to_string(), 5174u16),
        ]);
        let used = BTreeSet::from([4001u16, 5174u16]);
        let allocated =
            allocate_service_ports_with(&used, &bases, 10, &BTreeMap::new(), false, |_| true)
                .unwrap();
        assert_eq!(allocated["backend"] - 4001, allocated["frontend"] - 5174);
        assert!(allocated["backend"] >= 4011);
    }

    #[test]
    fn allocate_service_ports_reuses_live_projects_bound_cohort() {
        let bases = BTreeMap::from([
            ("backend".to_string(), 4001u16),
            ("frontend".to_string(), 5174u16),
        ]);
        let preferred = BTreeMap::from([
            ("backend".to_string(), 4011u16),
            ("frontend".to_string(), 5184u16),
        ]);

        let allocated =
            allocate_service_ports_with(&BTreeSet::new(), &bases, 10, &preferred, true, |_| false)
                .unwrap();

        assert_eq!(allocated, preferred);
    }

    fn reference(
        field: &'static str,
        key: &str,
        host: &'static str,
        port: u16,
    ) -> transform::PortReference {
        transform::PortReference {
            service: "web".to_string(),
            field,
            key: key.to_string(),
            host,
            port,
        }
    }

    fn wire(
        repo: &str,
        references: &[transform::PortReference],
        own: &[u16],
        siblings: &[(String, String, u16, u16)],
    ) -> (Vec<(u16, u16)>, Vec<String>) {
        let own: BTreeSet<u16> = own.iter().copied().collect();
        wire_cross_stack_ports(repo, references, &own, siblings)
    }

    #[test]
    fn ambiguous_sibling_reference_is_rejected_with_candidates() {
        // api-a and api-b both publish source host port 8000; the frontend
        // points at localhost:8000 — no single rewrite can resolve it.
        let siblings = vec![
            ("api-a".to_string(), "api".to_string(), 8000u16, 8010u16),
            ("api-b".to_string(), "api".to_string(), 8000u16, 8020u16),
        ];
        let (cross, violations) = wire(
            "frontend",
            &[reference("environment", "APP_API_URL", "localhost", 8000)],
            &[],
            &siblings,
        );
        assert!(cross.is_empty());
        assert_eq!(violations.len(), 1);
        let message = &violations[0];
        assert!(message.contains("`frontend`"), "{message}");
        assert!(message.contains("`web`"), "{message}");
        assert!(message.contains("`APP_API_URL`"), "{message}");
        assert!(message.contains("localhost:8000"), "{message}");
        assert!(message.contains("`api-a`"), "{message}");
        assert!(message.contains("localhost:8010"), "{message}");
        assert!(message.contains("`api-b`"), "{message}");
        assert!(message.contains("localhost:8020"), "{message}");

        // Build args are consumer references too.
        let (_, violations) = wire(
            "frontend",
            &[reference("build args", "API_ORIGIN", "127.0.0.1", 8000)],
            &[],
            &siblings,
        );
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("`API_ORIGIN`"), "{}", violations[0]);
    }

    #[test]
    fn duplicate_source_ports_without_consumer_reference_stay_allowed() {
        let siblings = vec![
            ("api-a".to_string(), "api".to_string(), 8000u16, 8010u16),
            ("api-b".to_string(), "api".to_string(), 8000u16, 8020u16),
        ];
        let (cross, violations) = wire("frontend", &[], &[], &siblings);
        assert!(cross.is_empty());
        assert!(violations.is_empty());
    }

    #[test]
    fn own_stack_precedence_is_preserved() {
        // The frontend itself publishes 8000: phase 1 already rewrote its
        // references to its own bundle port; sibling 8000s are not
        // candidates and never a violation.
        let siblings = vec![
            ("api-a".to_string(), "api".to_string(), 8000u16, 8010u16),
            ("api-b".to_string(), "api".to_string(), 8000u16, 8020u16),
        ];
        let (cross, violations) = wire(
            "frontend",
            &[reference(
                "environment",
                "SELF_URL",
                "host.docker.internal",
                8000,
            )],
            &[8000],
            &siblings,
        );
        assert!(cross.is_empty());
        assert!(violations.is_empty());
    }

    #[test]
    fn unambiguous_references_still_rewrite() {
        let siblings = vec![
            ("api-a".to_string(), "api".to_string(), 8000u16, 8010u16),
            ("assets".to_string(), "cdn".to_string(), 9000u16, 9010u16),
        ];
        let (cross, violations) = wire(
            "frontend",
            &[
                reference("environment", "APP_API_URL", "localhost", 8000),
                reference("environment", "ASSETS_URL", "localhost", 9000),
            ],
            &[],
            &siblings,
        );
        assert_eq!(cross, vec![(8000, 8010), (9000, 9010)]);
        assert!(violations.is_empty());
    }

    #[test]
    fn port_prefixes_sibling_new_ports_and_unrelated_ports_are_not_violations() {
        let siblings = vec![
            ("api-a".to_string(), "api".to_string(), 8000u16, 8010u16),
            ("api-b".to_string(), "api".to_string(), 8000u16, 8020u16),
        ];
        // References are exact ports: a sibling's NEW bundle port (8010) is
        // not an old source port, 7000 is published by no sibling at all,
        // and a longer digit run than 8000 can never parse as u16 8000.
        for port in [8010u16, 7000] {
            let (cross, violations) = wire(
                "frontend",
                &[reference("environment", "APP_API_URL", "localhost", port)],
                &[],
                &siblings,
            );
            assert!(cross.is_empty());
            assert!(violations.is_empty());
        }
    }
}
