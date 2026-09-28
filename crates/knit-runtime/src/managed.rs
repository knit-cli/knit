//! Restricted host-engine execution. Workspace documents are data, never authority.
use crate::{
    EngineView, RuntimeContext,
    config::{DatabaseMode, ProjectRuntime},
    plan::StackPlan,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
#[path = "managed_images.rs"]
mod images;

const OWNER: &str = "io.knit.runtime.owner";
const SCOPE: &str = "io.knit.runtime.scope";
const PROJECT: &str = "io.knit.runtime.project";
const REPO: &str = "io.knit.runtime.repo";
const PORTS: &str = "io.knit.runtime.ports";

fn digest(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())[..32].to_string()
}
fn scope(ctx: &RuntimeContext) -> String {
    digest(&[
        ctx.engine.as_ref().unwrap().owner.as_deref().unwrap_or(""),
        &ctx.root.to_string_lossy(),
        &ctx.bundle_id,
    ])
}
fn project(ctx: &RuntimeContext, repo: &str) -> String {
    format!("knit-{}", digest(&[&scope(ctx), repo]))
}
fn alias(project: &str, service: &str) -> String {
    format!("preview-{}", digest(&[project, service]))
}
fn labels(ctx: &RuntimeContext, project: &str, repo: &str) -> Value {
    json!({OWNER: ctx.engine.as_ref().unwrap().owner, SCOPE: scope(ctx), PROJECT: project, REPO: repo,
        "io.knit.runtime.bundle": ctx.bundle_id})
}
fn within(path: &Path, root: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("Managed runtime path must exist: {}", path.display()))?;
    let root = fs::canonicalize(root)?;
    if !canonical.starts_with(root) {
        bail!(
            "Managed runtime path is outside workspace volume: {}",
            path.display()
        );
    }
    Ok(canonical)
}
fn allow(value: &Value, keys: &[&str], at: &str) -> Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("Expected object at {at}"))?;
    for key in object.keys() {
        if !keys.contains(&key.as_str()) {
            bail!(
                "Managed runtime forbids {at}.{key}; remove this option from the Compose workload."
            );
        }
    }
    Ok(())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Expected {key} string"))
}

/// Every Docker invocation starts with a private HOME/config and a fixed executable,
/// socket, plugin search path and environment. No workspace .docker or .env controls it.
struct Docker {
    home: TempDir,
    executable: PathBuf,
}
impl Docker {
    fn new(ctx: &RuntimeContext) -> Result<Self> {
        let engine = ctx
            .engine
            .as_ref()
            .context("Managed runtime requires engine environment")?;
        if engine.owner.as_deref().is_none_or(str::is_empty)
            || engine.network.is_empty()
            || engine.volume.is_empty()
        {
            bail!("Managed runtime requires trusted owner, network and volume");
        }
        within(&ctx.root, &engine.mount)?;
        let home = tempfile::Builder::new()
            .prefix("knit-engine-")
            .tempdir_in("/tmp")?;
        fs::write(home.path().join("config.json"), "{}")?;
        let executable = [
            "/usr/local/bin/docker",
            "/usr/bin/docker",
            "/opt/homebrew/bin/docker",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .context("Trusted Docker executable not installed")?;
        Ok(Self { home, executable })
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.executable);
        cmd.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", self.home.path())
            .env("DOCKER_CONFIG", self.home.path())
            .env("DOCKER_HOST", "unix:///var/run/docker.sock")
            .env("COMPOSE_DISABLE_ENV_FILE", "1")
            .env("COMPOSE_BAKE", "false")
            .current_dir(self.home.path());
        cmd
    }
    fn output(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .output()
            .context("Failed to execute trusted Docker")?;
        if !out.status.success() {
            bail!(
                "Docker {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    }
    fn inspect(&self, kind: &str, id: &str) -> Result<Value> {
        let values: Value = serde_json::from_str(&self.output(&[kind, "inspect", id])?)?;
        values
            .get(0)
            .cloned()
            .context("Docker inspect returned no resource")
    }
    fn compose(&self, file: &Path, project: &str) -> Command {
        let mut cmd = self.command();
        cmd.args([
            "compose",
            "--env-file",
            "/dev/null",
            "--project-name",
            project,
            "--file",
        ])
        .arg(file);
        cmd
    }

    /// Bound read-only startup probes without allowing a full pipe to stall
    /// the deadline. Compose and its plugins share the child's process group.
    fn output_until(&self, mut command: Command, deadline: Instant) -> Result<String> {
        let mut stdout = tempfile::tempfile_in(self.home.path())?;
        let mut stderr = tempfile::tempfile_in(self.home.path())?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .context("Failed to run managed startup probe")?;
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    stderr.rewind()?;
                    let mut error = Vec::new();
                    stderr.read_to_end(&mut error)?;
                    bail!(
                        "Managed startup probe failed: {}",
                        String::from_utf8_lossy(&error)
                            .chars()
                            .take(1200)
                            .collect::<String>()
                    );
                }
                stdout.rewind()?;
                let mut output = Vec::new();
                stdout.read_to_end(&mut output)?;
                return Ok(String::from_utf8(output)?);
            }
            if Instant::now() >= deadline {
                #[cfg(unix)]
                let _ = Command::new("/bin/kill")
                    .args(["-9", &format!("-{}", child.id())])
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                bail!("Managed startup probe exceeded runtime.startupTimeoutSeconds");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn verify_managed_startup(
    docker: &Docker,
    ctx: &RuntimeContext,
    project: &str,
    repo: &str,
    snapshot: &Path,
    document: &Value,
    timeout_seconds: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let services = document["services"]
        .as_object()
        .context("Missing managed services")?;
    let completed: BTreeSet<&str> = services
        .values()
        .filter_map(|service| service.get("depends_on").and_then(Value::as_object))
        .flat_map(|dependencies| dependencies.iter())
        .filter(|(_, dependency)| dependency["condition"] == "service_completed_successfully")
        .map(|(name, _)| name.as_str())
        .collect();
    loop {
        let mut ps = docker.compose(snapshot, project);
        ps.args(["ps", "--all", "--orphans=false", "--format", "json"]);
        let output = docker.output_until(ps, deadline)?;
        let entries: Vec<Value> = if output.trim().is_empty() {
            vec![]
        } else if let Ok(Value::Array(entries)) = serde_json::from_str(output.trim()) {
            entries
        } else {
            output
                .lines()
                .map(serde_json::from_str)
                .collect::<std::result::Result<_, _>>()
                .context("Invalid managed Compose status")?
        };
        let mut seen = BTreeSet::new();
        let mut pending = Vec::new();
        for entry in entries {
            let Some(name) = entry["Service"].as_str() else {
                continue;
            };
            if !services.contains_key(name)
                || entry["Labels"]
                    .as_str()
                    .unwrap_or("")
                    .contains("com.docker.compose.oneoff=True")
            {
                continue;
            }
            let id = entry["ID"]
                .as_str()
                .context("Compose status lacks container ID")?;
            let mut inspect = docker.command();
            inspect.args(["container", "inspect", id]);
            let container: Value = serde_json::from_str(&docker.output_until(inspect, deadline)?)?;
            let container = container
                .get(0)
                .context("Container inspect returned no resource")?;
            if !owned(ctx, resource_labels("container", container))
                || container["Config"]["Labels"]["com.docker.compose.project"] != project
                || container["Config"]["Labels"]["com.docker.compose.service"] != name
            {
                bail!("Managed startup found an unowned container for {repo}/{name}");
            }
            if !seen.insert(name.to_string()) {
                bail!("Managed startup found duplicate containers for {repo}/{name}");
            }
            let state = &container["State"];
            match state["Status"].as_str().unwrap_or("") {
                "running" if completed.contains(name) => {
                    pending.push(format!("{name}: job still running"))
                }
                "running" => {
                    if let Some(note) = pending_health(container, repo, name)? {
                        pending.push(note);
                    }
                }
                "exited" if completed.contains(name) && state["ExitCode"] == 0 => {}
                "exited" => bail!(
                    "Managed startup failed for {repo}/{name}: exited ({})",
                    state["ExitCode"]
                ),
                "created" => pending.push(format!("{name}: created")),
                other => bail!("Managed startup failed for {repo}/{name}: {other}"),
            }
        }
        for name in services.keys() {
            if !seen.contains(name) {
                pending.push(format!("{name}: no container found"));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "Managed startup timed out for {repo} after {timeout_seconds}s: {}",
                pending.join(", ")
            );
        }
        std::thread::sleep(
            Duration::from_millis(500).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn pending_health(container: &Value, repo: &str, name: &str) -> Result<Option<String>> {
    // Docker inspect reports the effective healthcheck, including one inherited
    // from the image. Config can precede the first State.Health update.
    let configured = container["Config"]["Healthcheck"].is_object()
        && container["Config"]["Healthcheck"]["Test"] != "NONE"
        && container["Config"]["Healthcheck"]["Test"][0] != "NONE";
    let state = &container["State"];
    if !configured && !state["Health"].is_object() {
        return Ok(None);
    }
    match state["Health"]["Status"].as_str() {
        Some("healthy") => Ok(None),
        Some("unhealthy") => bail!("Managed startup failed for {repo}/{name}: unhealthy"),
        _ => Ok(Some(format!("{name}: health check starting"))),
    }
}

fn sanitize(mut doc: Value, ctx: &RuntimeContext, project: &str, repo: &str) -> Result<Value> {
    let engine = ctx.engine.as_ref().unwrap();
    allow(
        &doc,
        &["name", "services", "volumes", "networks"],
        "compose",
    )?;
    // Compose 5 emits this empty IPAM object even when the source declares no
    // networks. It carries no engine configuration; custom IPAM still fails
    // the allowlist below, including explicitly selected default drivers.
    if let Some(networks) = doc.get_mut("networks").and_then(Value::as_object_mut) {
        for entry in networks.values_mut() {
            if entry
                .get("ipam")
                .is_some_and(|ipam| ipam.as_object().is_some_and(|map| map.is_empty()))
            {
                entry.as_object_mut().unwrap().remove("ipam");
            }
        }
    }
    for (kind, allowed) in [
        ("volumes", vec!["name", "labels"]),
        ("networks", vec!["name", "labels", "internal", "attachable"]),
    ] {
        if let Some(entries) = doc.get(kind) {
            for (name, entry) in entries.as_object().context("Expected resource map")? {
                allow(entry, &allowed, &format!("{kind}.{name}"))?;
                // Explicit names cannot claim another project's resources. Always replaced below.
            }
        }
    }
    let mut volumes = doc.get("volumes").cloned().unwrap_or(json!({}));
    if volumes
        .as_object()
        .unwrap()
        .keys()
        .any(|name| name == "knit_workspace" || name.starts_with("knit_image_"))
    {
        bail!("Volume name knit_workspace is reserved for trusted runtime mounts");
    }
    for (name, entry) in volumes.as_object_mut().unwrap() {
        *entry = json!({"name": format!("{project}-v-{}", digest(&[name])), "labels": labels(ctx, project, repo)});
    }
    let services = doc
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .context("Compose requires services")?;
    if services.is_empty() {
        bail!("Compose has no enabled services");
    }
    for (name, service) in services {
        allow(
            service,
            &[
                "image",
                "build",
                "command",
                "entrypoint",
                "environment",
                "working_dir",
                "user",
                "init",
                "stdin_open",
                "tty",
                "restart",
                "stop_signal",
                "stop_grace_period",
                "healthcheck",
                "depends_on",
                "ports",
                "expose",
                "volumes",
                "networks",
                "labels",
                "profiles",
                "pull_policy",
                "read_only",
                "hostname",
                "domainname",
            ],
            &format!("services.{name}"),
        )?;
        if let Some(networks) = service.get("networks") {
            for (_, settings) in networks
                .as_object()
                .context("Expected resolved networks map")?
            {
                if !settings.is_null() {
                    allow(settings, &[], "service network settings")?;
                }
            }
        }
        if let Some(build) = service.get_mut("build") {
            allow(
                build,
                &[
                    "context",
                    "dockerfile",
                    "dockerfile_inline",
                    "args",
                    "target",
                    "labels",
                    "no_cache",
                    "pull",
                ],
                "build",
            )?;
            let context = within(Path::new(text(build, "context")?), &engine.mount)?;
            let source =
                if let Some(inline) = build.get("dockerfile_inline").and_then(Value::as_str) {
                    if build.get("dockerfile").is_some() {
                        bail!("Choose only one Dockerfile source");
                    }
                    inline.to_string()
                } else {
                    let file = build
                        .get("dockerfile")
                        .and_then(Value::as_str)
                        .unwrap_or("Dockerfile");
                    fs::read_to_string(within(&context.join(file), &context)?)?
                };
            let frozen = images::rewrite_dockerfile(&source, build.get("args"), |source| {
                Ok(source.to_string())
            })?;
            build.as_object_mut().unwrap().remove("dockerfile");
            build["dockerfile_inline"] = json!(frozen);
            build["context"] = json!(context);
            build["labels"] = labels(ctx, project, repo);
            build["pull"] = json!(true);
            build["no_cache"] = json!(true);
            service["image"] = json!(format!("{project}-{}:runtime", digest(&[name])));
            service["pull_policy"] = json!("build");
        } else {
            service["image"] = json!(images::registry_reference(text(service, "image")?)?);
            service["pull_policy"] = json!("always");
        }
        let mut ports = Vec::new();
        if let Some(entries) = service.get("ports") {
            for entry in entries
                .as_array()
                .context("Expected resolved ports array")?
            {
                allow(
                    entry,
                    &[
                        "target",
                        "published",
                        "host_ip",
                        "protocol",
                        "mode",
                        "name",
                        "app_protocol",
                    ],
                    "port",
                )?;
                let target = entry
                    .get("target")
                    .and_then(Value::as_u64)
                    .filter(|p| *p > 0 && *p <= 65535)
                    .context("Invalid target port")?;
                let protocol = entry
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("tcp");
                if !["tcp", "udp"].contains(&protocol) {
                    bail!("Unsupported port protocol");
                }
                ports.push(json!({"service": name, "host": target, "container": target, "targetPort": target, "protocol": protocol}));
            }
        }
        if let Some(entries) = service.get_mut("volumes") {
            for entry in entries
                .as_array_mut()
                .context("Expected resolved volume array")?
            {
                allow(
                    entry,
                    &[
                        "type",
                        "source",
                        "target",
                        "read_only",
                        "bind",
                        "volume",
                        "consistency",
                    ],
                    "mount",
                )?;
                let target = text(entry, "target")?.to_string();
                let read_only = entry
                    .get("read_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match text(entry, "type")? {
                    "bind" => {
                        if let Some(bind) = entry.get("bind") {
                            allow(bind, &["create_host_path"], "bind")?;
                        }
                        let source = within(Path::new(text(entry, "source")?), &engine.mount)?;
                        let mount = fs::canonicalize(&engine.mount)?;
                        let subpath = source.strip_prefix(mount)?.to_string_lossy().to_string();
                        if subpath.is_empty() {
                            bail!(
                                "Mounting the entire workspace volume is not allowed; select a subdirectory"
                            );
                        }
                        *entry = json!({"type":"volume", "source":"knit_workspace", "target":target, "read_only":read_only, "volume":{"subpath":subpath, "nocopy":true}});
                    }
                    "volume" => {
                        let source = text(entry, "source")?;
                        if volumes.get(source).is_none() {
                            bail!("Volume {source} must be declared and project-owned");
                        }
                        if let Some(opts) = entry.get("volume") {
                            allow(opts, &["nocopy"], "volume mount")?;
                        }
                        entry["volume"] = json!({"nocopy":true});
                    }
                    other => bail!("Managed runtime forbids mount type {other}"),
                }
            }
        }
        let mut owned_labels = labels(ctx, project, repo);
        owned_labels[PORTS] = json!(serde_json::to_string(&ports)?);
        service["labels"] = owned_labels;
        let obj = service.as_object_mut().unwrap();
        obj.remove("ports");
        obj.remove("profiles");
        obj.insert("networks".into(), json!({"default":{}}));
        obj.insert("security_opt".into(), json!(["no-new-privileges:true"]));
        obj.insert("cap_drop".into(), json!(["ALL"]));
        // Docker's ordinary application capability subset; no admin/network/raw access.
        obj.insert(
            "cap_add".into(),
            json!([
                "CHOWN",
                "DAC_OVERRIDE",
                "FOWNER",
                "SETGID",
                "SETUID",
                "NET_BIND_SERVICE"
            ]),
        );
        obj.insert("pids_limit".into(), json!(512));
        obj.insert("mem_limit".into(), json!("2g"));
        obj.insert("cpus".into(), json!(2));
        obj.insert(
            "logging".into(),
            json!({"driver":"json-file", "options":{"max-size":"10m", "max-file":"3"}}),
        );
    }
    volumes["knit_workspace"] = json!({"external":true, "name":engine.volume});
    doc["volumes"] = volumes;
    doc["networks"] = json!({"default":{"name":format!("{project}-default"), "labels":labels(ctx, project, repo)}});
    doc["name"] = json!(project);
    Ok(doc)
}

fn escape_interpolation(value: &mut Value) {
    match value {
        Value::String(s) => *s = s.replace('$', "$$"),
        Value::Array(values) => values.iter_mut().for_each(escape_interpolation),
        Value::Object(map) => map.values_mut().for_each(escape_interpolation),
        _ => {}
    }
}

pub(crate) fn up(
    ctx: &RuntimeContext,
    runtime: &ProjectRuntime,
    plans: Vec<StackPlan>,
) -> Result<()> {
    let docker = Docker::new(ctx)?;
    up_with_docker(ctx, runtime, plans, &docker)
}

fn up_with_docker(
    ctx: &RuntimeContext,
    runtime: &ProjectRuntime,
    plans: Vec<StackPlan>,
    docker: &Docker,
) -> Result<()> {
    if runtime.startup_timeout_seconds == 0 {
        bail!("runtime.startupTimeoutSeconds must be greater than zero");
    }
    if !runtime.bindings.is_empty() {
        bail!(
            "Managed runtime forbids runtime.bindings: host-port endpoint wiring is unavailable; use service DNS on the project network"
        );
    }
    if runtime.ports.is_some() {
        bail!(
            "Managed runtime forbids runtime.ports: applications cannot allocate or publish host ports"
        );
    }
    if runtime
        .database
        .as_ref()
        .is_some_and(|db| db.mode == DatabaseMode::Shared)
    {
        bail!(
            "Managed runtime forbids shared databases and host.docker.internal; use a bundle database service"
        );
    }
    let engine = ctx.engine.as_ref().unwrap();
    verify_preview_network(docker, engine)?;
    let mut env = BTreeMap::from([
        ("KNIT_ROOT".to_string(), ctx.root.display().to_string()),
        ("KNIT_BUNDLE".into(), ctx.bundle_id.clone()),
    ]);
    for repo in &ctx.repos {
        if let Some(checkout) = &repo.checkout {
            let checkout = within(checkout, &engine.mount)?;
            let suffix = crate::support::env_var_suffix(&repo.id);
            env.insert(
                format!("KNIT_CHECKOUT_{suffix}"),
                checkout.display().to_string(),
            );
            env.insert(
                format!("KNIT_SRC_{suffix}"),
                checkout
                    .strip_prefix(&ctx.root)
                    .unwrap_or(&checkout)
                    .display()
                    .to_string(),
            );
        }
    }
    let mut profiles = Vec::new();
    if let Some(db) = &runtime.database {
        if db.mode == DatabaseMode::Bundle {
            profiles.push("bundle-db");
        }
        env.insert("KNIT_DB_MODE".into(), db.mode.to_string());
        env.insert(
            "KNIT_DB_HOST".into(),
            db.service.clone().unwrap_or("db".into()),
        );
        env.insert("KNIT_DB_PORT".into(), 5432.to_string());
        env.insert(
            "KNIT_DB_NAME".into(),
            db.name_template
                .as_ref()
                .map(|s| s.replace("{bundle}", &ctx.bundle_id))
                .unwrap_or(db.name.clone()),
        );
    }
    // Validate and freeze EVERY stack before the first workload mutation.
    let mut snapshots = Vec::new();
    for plan in plans {
        let compose = within(&plan.compose, &engine.mount)?;
        reject_host_port_contract(&compose)?;
        let project = project(ctx, &plan.repo.id);
        let mut cmd = docker.compose(&compose, &project);
        cmd.args(["--project-directory"])
            .arg(within(&plan.checkout, &engine.mount)?);
        for profile in &profiles {
            cmd.args(["--profile", profile]);
        }
        let output = cmd
            .envs(&env)
            .args(["config", "--format", "json"])
            .output()?;
        if !output.status.success() {
            bail!(
                "Compose resolution failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut resolved: Value = serde_json::from_slice(&output.stdout)?;
        if serde_json::to_string(&resolved)?.contains("host.docker.internal") {
            bail!(
                "Managed runtime forbids host.docker.internal; use service DNS on the project network"
            );
        }
        if !crate::transform::collect_port_references(&resolved).is_empty() {
            bail!(
                "Managed runtime forbids loopback host-port references in service environment or build args; use service DNS and container ports"
            );
        }
        if plan.mode == crate::config::RuntimeMode::Transform {
            remap_workspace_paths(&mut resolved, ctx)?;
        }
        let sanitized = sanitize(resolved, ctx, &project, &plan.repo.id)?;
        check_project_ownership(docker, ctx, &project)?;
        check_named_resources(docker, ctx, &sanitized)?;
        snapshots.push((project, plan.repo.id, sanitized));
    }
    // Only after every document passes policy may we contact registries/build.
    // Container creation waits until *all* resulting image metadata is checked.
    let mut executable = Vec::new();
    let mut pulled = BTreeMap::new();
    for (project, repo, mut document) in snapshots {
        let mut image_metadata = BTreeMap::new();
        for (name, service) in document["services"].as_object_mut().unwrap() {
            if let Some(build) = service.get_mut("build") {
                let source = text(build, "dockerfile_inline")?.to_string();
                let rewritten = images::rewrite_dockerfile(&source, build.get("args"), |source| {
                    let (reference, metadata) = pull_registry_image(docker, source, &mut pulled)?;
                    if metadata["Config"]["OnBuild"]
                        .as_array()
                        .is_some_and(|hooks| !hooks.is_empty())
                    {
                        bail!("Managed builds forbid base images with ONBUILD triggers: {source}");
                    }
                    Ok(reference)
                })?;
                build["dockerfile_inline"] = json!(rewritten);
            } else {
                let (_, metadata) =
                    pull_registry_image(docker, text(service, "image")?, &mut pulled)?;
                service["image"] = json!(text(&metadata, "Id")?);
                service["pull_policy"] = json!("never");
                image_metadata.insert(name.clone(), metadata);
            }
        }
        let snapshot = docker.home.path().join(format!("{project}.build.json"));
        write_snapshot(&snapshot, &document)?;
        if document["services"]
            .as_object()
            .unwrap()
            .values()
            .any(|service| service.get("build").is_some())
        {
            let result = docker
                .compose(&snapshot, &project)
                .args(["build", "--pull", "--no-cache"])
                .status()?;
            if !result.success() {
                bail!("Managed Compose build failed for {project}");
            }
        }
        for (name, service) in document["services"].as_object_mut().unwrap() {
            if service.get("build").is_none() {
                continue;
            }
            let metadata = docker.inspect("image", text(service, "image")?)?;
            if !owned(ctx, resource_labels("image", &metadata)) {
                bail!("Build output is not owned by this runtime");
            }
            // Freeze the exact authorized build result, too. No rebuild or tag
            // lookup may occur between metadata inspection and container create.
            service["image"] = json!(text(&metadata, "Id")?);
            service["pull_policy"] = json!("never");
            service.as_object_mut().unwrap().remove("build");
            image_metadata.insert(name.clone(), metadata);
        }
        add_declared_image_volumes(&mut document, ctx, &project, &repo, &image_metadata)?;
        check_named_resources(docker, ctx, &document)?;
        let snapshot = docker.home.path().join(format!("{project}.run.json"));
        write_snapshot(&snapshot, &document)?;
        executable.push((project, repo, snapshot, document));
    }
    for (project, _, snapshot, _) in &executable {
        check_project_ownership(docker, ctx, project)?;
        let result = docker
            .compose(snapshot, project)
            .args([
                "up",
                "--detach",
                "--no-build",
                "--pull",
                "never",
                "--remove-orphans",
            ])
            .status()?;
        if !result.success() {
            bail!("Managed Compose startup failed for {project}");
        }
        // Compose automatically adds the service name as a network alias. Connect
        // the preview network explicitly so stacks never share a `web` alias.
        for id in list(
            docker,
            "container",
            &format!("label=com.docker.compose.project={project}"),
        )? {
            let container = docker.inspect("container", &id)?;
            let labels = resource_labels("container", &container);
            if !owned(ctx, labels) {
                bail!("Refusing to attach an unowned container");
            }
            let service = text(labels, "com.docker.compose.service")?;
            if container["NetworkSettings"]["Networks"]
                .get(&engine.network)
                .is_none()
            {
                docker.output(&[
                    "network",
                    "connect",
                    "--alias",
                    &alias(project, service),
                    &engine.network,
                    &id,
                ])?;
            }
        }
    }
    for (project, repo, snapshot, document) in &executable {
        verify_managed_startup(
            docker,
            ctx,
            project,
            repo,
            snapshot,
            document,
            runtime.startup_timeout_seconds,
        )?;
    }
    Ok(())
}

fn reject_host_port_contract(compose: &Path) -> Result<()> {
    let source = fs::read_to_string(compose)?;
    if [
        "${KNIT_PORT_",
        "$KNIT_PORT_",
        "${KNIT_DB_HOST_PORT",
        "$KNIT_DB_HOST_PORT",
    ]
    .iter()
    .any(|variable| source.contains(variable))
    {
        bail!(
            "Managed runtime forbids KNIT_PORT_* and KNIT_DB_HOST_PORT contract variables: host ports are unavailable; use service DNS and container ports"
        );
    }
    Ok(())
}

fn write_snapshot(path: &Path, document: &Value) -> Result<()> {
    let mut escaped = document.clone();
    escape_interpolation(&mut escaped);
    fs::write(path, serde_json::to_vec(&escaped)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o400))?;
    }
    Ok(())
}

/// Translate source checkouts before policy validation. Both ends are checked
/// against the trusted volume, and nested repos win over their parents.
fn remap_workspace_paths(document: &mut Value, ctx: &RuntimeContext) -> Result<()> {
    let mount = &ctx.engine.as_ref().unwrap().mount;
    let mut mappings = Vec::new();
    for repo in &ctx.repos {
        if let Some(checkout) = &repo.checkout {
            mappings.push((within(&repo.source_path, mount)?, within(checkout, mount)?));
        }
    }
    // The shared transform understands context-relative Dockerfile/build-arg
    // paths and nested repository precedence. Its identity allocator only
    // preserves Compose's declared port until sanitize removes publication.
    if document["services"]
        .as_object()
        .context("Expected services map")?
        .values()
        .any(|service| service.get("container_name").is_some())
    {
        bail!("Managed runtime forbids services.*.container_name");
    }
    for service in document["services"].as_object().unwrap().values() {
        if let Some(context) = service["build"]["context"].as_str() {
            within(Path::new(context), mount)?;
        }
        if let Some(volumes) = service["volumes"].as_array() {
            for volume in volumes {
                if volume["type"] == "bind" {
                    within(Path::new(text(volume, "source")?), mount)?;
                }
            }
        }
    }
    crate::transform::prepare_compose(document, &mappings, &mut |_, old, _| Ok(old))?;
    Ok(())
}

fn pull_registry_image(
    docker: &Docker,
    source: &str,
    cache: &mut BTreeMap<String, (String, Value)>,
) -> Result<(String, Value)> {
    let source = images::registry_reference(source)?;
    if let Some(result) = cache.get(&source) {
        return Ok(result.clone());
    }
    // No host cache fallback and no workspace credential helpers. A failed pull
    // is an authorization failure even if the host has an image with this name.
    docker.output(&["image", "pull", &source])?;
    let metadata = docker.inspect("image", &source)?;
    let name = source.split('@').next().unwrap();
    let slash = name.rfind('/').unwrap_or(0);
    let repository = match name.rfind(':') {
        Some(colon) if colon > slash => &name[..colon],
        _ => name,
    };
    let reference = metadata["RepoDigests"]
        .as_array()
        .context("Registry pull returned no content digest")?
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|digest| images::registry_reference(digest).ok())
        .find(|digest| {
            digest.starts_with(&format!("{repository}@sha256:"))
                && source.split_once('@').is_none_or(|(_, requested)| {
                    digest
                        .split_once('@')
                        .is_some_and(|(_, actual)| actual == requested)
                })
        })
        .context("Registry pull did not authorize a digest for the requested repository")?;
    if source.contains('@')
        && source.split_once('@').unwrap().1 != reference.split_once('@').unwrap().1
    {
        bail!("Registry digest does not match the requested image");
    }
    let result = (reference, metadata);
    cache.insert(source, result.clone());
    Ok(result)
}

fn add_declared_image_volumes(
    document: &mut Value,
    ctx: &RuntimeContext,
    project: &str,
    repo: &str,
    metadata: &BTreeMap<String, Value>,
) -> Result<()> {
    let mut declarations = Vec::new();
    for (service_name, service) in document["services"].as_object_mut().unwrap() {
        let image = metadata
            .get(service_name)
            .context("Missing authorized image metadata")?;
        let Some(volumes) = image["Config"].get("Volumes").filter(|v| !v.is_null()) else {
            continue;
        };
        for target in volumes
            .as_object()
            .context("Image volumes must be a map")?
            .keys()
        {
            if !Path::new(target).is_absolute()
                || Path::new(target)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("Image declares an invalid volume target: {target}");
            }
            let mounts = service
                .as_object_mut()
                .unwrap()
                .entry("volumes")
                .or_insert(json!([]))
                .as_array_mut()
                .context("Expected mounts array")?;
            if mounts
                .iter()
                .any(|mount| mount["target"].as_str() == Some(target))
            {
                continue;
            }
            let key = format!("knit_image_{}", digest(&[service_name, target]));
            mounts.push(json!({"type":"volume","source":key,"target":target}));
            declarations.push((key,json!({"name":format!("{project}-v-{}",digest(&["image",service_name,target])),"labels":labels(ctx,project,repo)})));
        }
    }
    for (key, value) in declarations {
        document["volumes"][key] = value;
    }
    Ok(())
}

fn resource_labels<'a>(kind: &str, resource: &'a Value) -> &'a Value {
    if kind == "container" || kind == "image" {
        &resource["Config"]["Labels"]
    } else {
        &resource["Labels"]
    }
}
fn owned(ctx: &RuntimeContext, labels: &Value) -> bool {
    labels[OWNER].as_str() == ctx.engine.as_ref().unwrap().owner.as_deref()
        && labels[SCOPE].as_str() == Some(scope(ctx).as_str())
        && labels[REPO]
            .as_str()
            .is_some_and(|repo| labels[PROJECT].as_str() == Some(project(ctx, repo).as_str()))
}
/// Match the ordinary IPv4 bridge topology qualified by the broker. Metadata
/// checks do not replace its behavioral isolation gate. Cleanup intentionally
/// does not call this validator so unsafe owned networks remain removable.
fn validate_network_topology(resource: &Value) -> Result<()> {
    for (field, expected) in [
        ("Scope", json!("local")),
        ("Driver", json!("bridge")),
        ("EnableIPv6", json!(false)),
        ("Internal", json!(false)),
        ("Options", json!({})),
    ] {
        if resource.get(field) != Some(&expected) {
            bail!(
                "Managed network topology requires {field}={expected}; recreate this network with the broker-qualified default IPv4 bridge settings"
            );
        }
    }
    Ok(())
}

fn verify_preview_network(docker: &Docker, engine: &EngineView) -> Result<String> {
    let resource = docker.inspect("network", &engine.network)?;
    if resource["Labels"][OWNER].as_str() != engine.owner.as_deref() {
        bail!("Preview network does not belong to the trusted workspace owner");
    }
    validate_network_topology(&resource)?;
    Ok(text(&resource, "Id")?.to_string())
}
fn list(docker: &Docker, kind: &str, filter: &str) -> Result<Vec<String>> {
    let mut args = vec![kind, "ls", "-q", "--filter", filter];
    if kind == "container" {
        args.push("--all");
    }
    Ok(docker
        .output(&args)?
        .lines()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}
fn check_project_ownership(docker: &Docker, ctx: &RuntimeContext, project: &str) -> Result<()> {
    for kind in ["container", "network", "volume", "image"] {
        for id in list(
            docker,
            kind,
            &format!("label=com.docker.compose.project={project}"),
        )? {
            let resource = docker.inspect(kind, &id)?;
            if !owned(ctx, resource_labels(kind, &resource)) {
                bail!("Refusing to replace unowned {kind} {id}");
            }
            if kind == "network" {
                validate_network_topology(&resource)?;
            }
        }
    }
    Ok(())
}
fn check_named_resources(docker: &Docker, ctx: &RuntimeContext, doc: &Value) -> Result<()> {
    for (kind, resources) in [("volume", "volumes"), ("network", "networks")] {
        for entry in doc[resources]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
        {
            if entry["external"] == true {
                continue;
            }
            let name = text(entry, "name")?;
            for id in list(docker, kind, &format!("name=^{name}$"))? {
                let resource = docker.inspect(kind, &id)?;
                if !owned(ctx, resource_labels(kind, &resource)) {
                    bail!("Refusing to reuse unowned {kind} {name}");
                }
                if kind == "network" {
                    validate_network_topology(&resource)?;
                }
            }
        }
    }
    for service in doc["services"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        if service.get("build").is_none() {
            continue;
        }
        let name = text(service, "image")?;
        for id in list(docker, "image", &format!("reference={name}"))? {
            let resource = docker.inspect("image", &id)?;
            if !owned(ctx, resource_labels("image", &resource)) {
                bail!("Refusing to replace unowned build image {name}");
            }
        }
    }
    Ok(())
}
/// A content-addressed build can have several tags. Remove only tags in the
/// verified project's generated namespace, never force-delete the shared ID.
fn remove_owned_image(docker: &Docker, ctx: &RuntimeContext, resource: &Value) -> Result<()> {
    let image_labels = resource_labels("image", resource);
    if !owned(ctx, image_labels) {
        bail!("Refusing to remove an unowned image");
    }
    let id = text(resource, "Id")?;
    let project = text(image_labels, PROJECT)?;
    let tags: std::collections::BTreeSet<&str> = resource["RepoTags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if tags.is_empty() {
        // Untagged intermediates have no tag through which to reclaim them.
        // Keep digest-referenced images and let Docker protect container use.
        if resource["RepoDigests"].as_array().is_none_or(Vec::is_empty) {
            docker.output(&["image", "rm", id])?;
        }
        return Ok(());
    }
    for tag in tags {
        let suffix = tag
            .strip_prefix(&format!("{project}-"))
            .and_then(|tag| tag.strip_suffix(":runtime"));
        if !suffix.is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            continue;
        }
        // A tag can be moved independently of its original image listing.
        let current = docker.inspect("image", tag)?;
        if current["Id"].as_str() != Some(id) || !owned(ctx, resource_labels("image", &current)) {
            bail!("Image tag changed ownership during cleanup: {tag}");
        }
        docker.output(&["image", "rm", tag])?;
    }
    Ok(())
}

pub(crate) fn down(ctx: &RuntimeContext, purge: bool) -> Result<()> {
    let docker = Docker::new(ctx)?;
    down_with_docker(ctx, purge, &docker)
}

fn down_with_docker(ctx: &RuntimeContext, purge: bool, docker: &Docker) -> Result<()> {
    for kind in ["container", "network", "volume", "image"] {
        if !purge && ["volume", "image"].contains(&kind) {
            continue;
        }
        for id in list(docker, kind, &format!("label={SCOPE}={}", scope(ctx)))? {
            let resource = docker.inspect(kind, &id)?;
            if !owned(ctx, resource_labels(kind, &resource)) {
                bail!("Refusing to remove unowned {kind} {id}");
            }
            if kind == "image" {
                remove_owned_image(docker, ctx, &resource)?;
            } else if kind == "container" {
                docker.output(&[kind, "rm", "--force", &id])?;
            } else {
                docker.output(&[kind, "rm", &id])?;
            }
        }
    }
    Ok(())
}
pub(crate) fn status(ctx: &RuntimeContext) -> Result<()> {
    let docker = Docker::new(ctx)?;
    let network_id = verify_preview_network(&docker, ctx.engine.as_ref().unwrap())?;
    let mut stacks: BTreeMap<String, Value> = BTreeMap::new();
    for id in list(
        &docker,
        "container",
        &format!("label={SCOPE}={}", scope(ctx)),
    )? {
        let container = docker.inspect("container", &id)?;
        let labels = resource_labels("container", &container);
        if !owned(ctx, labels) {
            continue;
        }
        let project = text(labels, PROJECT)?;
        if labels["com.docker.compose.project"].as_str() != Some(project) {
            continue;
        }
        let service = text(labels, "com.docker.compose.service")?;
        let running = container["State"]["Running"] == true;
        let stack = stacks.entry(project.into()).or_insert_with(
            || json!({"repo":labels[REPO], "projectName":project, "services":[], "ports":[]}),
        );
        stack["services"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":service,"state": if running {"running"} else {"stopped"}}));
        stack["ports"]
            .as_array_mut()
            .unwrap()
            .extend(verified_ports(ctx, &container, &network_id)?);
    }
    let stacks: Vec<Value> = stacks.into_values().collect();
    let running = stacks.iter().any(|s| {
        s["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["state"] == "running")
    });
    println!(
        "{}",
        json!({"bundleId":ctx.bundle_id,"recorded":!stacks.is_empty(),"running":running,"stacks":stacks})
    );
    Ok(())
}

/// Only engine-observed ownership and current network membership authorize an endpoint.
fn verified_ports(ctx: &RuntimeContext, container: &Value, network_id: &str) -> Result<Vec<Value>> {
    let labels = resource_labels("container", container);
    if !owned(ctx, labels) || container["State"]["Running"] != true {
        return Ok(vec![]);
    }
    let project = text(labels, PROJECT)?;
    if labels["com.docker.compose.project"].as_str() != Some(project) {
        return Ok(vec![]);
    }
    let service = text(labels, "com.docker.compose.service")?;
    let network = &container["NetworkSettings"]["Networks"][&ctx.engine.as_ref().unwrap().network];
    let endpoint = alias(project, service);
    if network["NetworkID"].as_str() != Some(network_id)
        || !network["Aliases"]
            .as_array()
            .is_some_and(|aliases| aliases.iter().any(|a| a == &endpoint))
    {
        return Ok(vec![]);
    }
    let ports: Vec<Value> = serde_json::from_str(labels[PORTS].as_str().unwrap_or("[]"))?;
    Ok(ports
        .into_iter()
        .filter_map(|mut port| {
            let target = port["container"].as_u64()?;
            if port["service"] != service
                || port["protocol"] != "tcp"
                || !(1..=65535).contains(&target)
            {
                return None;
            }
            port["targetHost"] = json!(endpoint);
            port["targetPort"] = json!(target);
            Some(port)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(root: &Path, owner: &str) -> RuntimeContext {
        RuntimeContext {
            root: root.into(),
            bundle_id: "demo".into(),
            repos: vec![],
            extra_checkouts: vec![],
            engine: Some(EngineView {
                volume: "workspace-fixture".into(),
                mount: root.into(),
                owner: Some(owner.into()),
                network: "preview-fixture".into(),
            }),
        }
    }
    fn basic() -> Value {
        json!({"services":{"web":{"image":"alpine:3", "ports":[{"target":8080,"published":"18080","protocol":"tcp"}],"networks":{"default":null}}}, "networks":{"default":{"name":"untrusted-default"}}})
    }
    #[test]
    fn managed_contract_rejects_host_port_variables() {
        let root = tempfile::tempdir().unwrap();
        let compose = root.path().join("compose.yml");
        for variable in ["${KNIT_PORT_WEB:-8080}", "${KNIT_DB_HOST_PORT:-5432}"] {
            fs::write(
                &compose,
                format!("services:\n  web:\n    ports: [\"{variable}:8080\"]\n"),
            )
            .unwrap();
            assert!(
                reject_host_port_contract(&compose)
                    .unwrap_err()
                    .to_string()
                    .contains("host ports are unavailable")
            );
        }
        fs::write(
            &compose,
            "services:\n  web:\n    image: example.test/web:1\n",
        )
        .unwrap();
        reject_host_port_contract(&compose).unwrap();
    }
    #[test]
    fn inherited_image_healthcheck_blocks_managed_success() {
        let starting = json!({"Config":{"Healthcheck":{"Test":["CMD","check"]}},"State":{"Status":"running","Health":{"Status":"starting"}}});
        assert_eq!(
            pending_health(&starting, "app", "web").unwrap().as_deref(),
            Some("web: health check starting")
        );
        let before_first_update =
            json!({"Config":{"Healthcheck":{"Test":["CMD","check"]}},"State":{"Status":"running"}});
        assert_eq!(
            pending_health(&before_first_update, "app", "web")
                .unwrap()
                .as_deref(),
            Some("web: health check starting")
        );
        let unhealthy = json!({"Config":{"Healthcheck":{"Test":["CMD","check"]}},"State":{"Status":"running","Health":{"Status":"unhealthy"}}});
        assert!(
            pending_health(&unhealthy, "app", "web")
                .unwrap_err()
                .to_string()
                .contains("unhealthy")
        );
        let healthy = json!({"Config":{"Healthcheck":{"Test":["CMD","check"]}},"State":{"Status":"running","Health":{"Status":"healthy"}}});
        assert!(pending_health(&healthy, "app", "web").unwrap().is_none());
        let disabled =
            json!({"Config":{"Healthcheck":{"Test":["NONE"]}},"State":{"Status":"running"}});
        assert!(pending_health(&disabled, "app", "web").unwrap().is_none());
    }
    #[test]
    fn managed_host_port_configuration_fails_before_docker_mutation() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), "owner-a");
        let docker = Docker {
            home: tempfile::tempdir().unwrap(),
            executable: "/missing/docker".into(),
        };
        let mut runtime = ProjectRuntime::default();
        runtime.startup_timeout_seconds = 0;
        assert!(
            up_with_docker(&ctx, &runtime, vec![], &docker)
                .unwrap_err()
                .to_string()
                .contains("startupTimeoutSeconds")
        );
        runtime.startup_timeout_seconds = 1;
        runtime.bindings = serde_json::from_value(json!([{"repo":"app","service":"web","environment":"URL","target":{"repo":"app","service":"web"}}])).unwrap();
        assert!(
            up_with_docker(&ctx, &runtime, vec![], &docker)
                .unwrap_err()
                .to_string()
                .contains("runtime.bindings")
        );
        runtime.bindings.clear();
        runtime.ports = Some(Default::default());
        assert!(
            up_with_docker(&ctx, &runtime, vec![], &docker)
                .unwrap_err()
                .to_string()
                .contains("runtime.ports")
        );
        runtime.ports = None;
        runtime.database = Some(Default::default());
        assert!(
            up_with_docker(&ctx, &runtime, vec![], &docker)
                .unwrap_err()
                .to_string()
                .contains("shared databases")
        );
    }
    #[test]
    fn hostile_service_and_build_options_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "owner-a");
        for key in [
            "privileged",
            "network_mode",
            "pid",
            "ipc",
            "uts",
            "userns_mode",
            "devices",
            "device_cgroup_rules",
            "cap_add",
            "security_opt",
            "sysctls",
            "volumes_from",
            "use_api_socket",
            "post_start",
            "pre_stop",
            "provider",
            "develop",
            "deploy",
            "logging",
            "cgroup_parent",
            "unknown_future_option",
        ] {
            let mut doc = basic();
            doc["services"]["web"][key] = json!(true);
            assert!(
                sanitize(doc, &ctx, "project", "app")
                    .unwrap_err()
                    .to_string()
                    .contains(key),
                "{key}"
            );
        }
        for key in [
            "network",
            "ssh",
            "secrets",
            "additional_contexts",
            "entitlements",
            "privileged",
            "cache_from",
            "cache_to",
            "tags",
            "extra_hosts",
        ] {
            let mut doc = basic();
            doc["services"]["web"]["build"] = json!({"context":dir.path(),key:[]});
            assert!(
                sanitize(doc, &ctx, "project", "app")
                    .unwrap_err()
                    .to_string()
                    .contains(key),
                "{key}"
            );
        }
    }
    #[test]
    fn outside_symlink_parent_binds_and_external_resources_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "owner-a");
        for source in [
            outside.path().to_path_buf(),
            dir.path().join(".."),
            PathBuf::from("/var/run/docker.sock"),
        ] {
            let mut doc = basic();
            doc["services"]["web"]["volumes"] =
                json!([{"type":"bind","source":source,"target":"/data"}]);
            assert!(sanitize(doc, &ctx, "project", "app").is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert!(within(&dir.path().join("link"), dir.path()).is_err());
        }
        for kind in ["volumes", "networks"] {
            for key in ["external", "driver", "driver_opts"] {
                let mut doc = basic();
                doc[kind] = json!({"custom":{key:true}});
                assert!(sanitize(doc, &ctx, "project", "app").is_err());
            }
        }
    }
    #[test]
    fn safe_config_has_no_host_ports_and_owns_resources_including_profile_services() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("app")).unwrap();
        fs::write(dir.path().join("app/Dockerfile"), "FROM scratch").unwrap();
        let ctx = context(dir.path(), "owner-a");
        let project = project(&ctx, "app");
        let mut doc = basic();
        doc["services"]["web"]["build"] =
            json!({"context":dir.path().join("app"),"dockerfile":"Dockerfile"});
        doc["services"]["web"]["volumes"] = json!([{"type":"bind","source":dir.path().join("app"),"target":"/app","bind":{"create_host_path":true}}]);
        doc["services"]["db"] = json!({"image":"postgres:17","profiles":["bundle-db"],"volumes":[{"type":"volume","source":"data","target":"/data","volume":{}}]});
        doc["volumes"] = json!({"data":{"name":"shared-data"}});
        let safe = sanitize(doc, &ctx, &project, "app").unwrap();
        for service in safe["services"].as_object().unwrap().values() {
            assert!(service.get("ports").is_none());
            assert!(service.get("profiles").is_none());
            assert!(owned(&ctx, &service["labels"]));
            assert_eq!(service["networks"], json!({"default":{}}));
            assert_eq!(service["security_opt"], json!(["no-new-privileges:true"]));
        }
        assert_eq!(
            safe["services"]["web"]["volumes"][0]["volume"]["subpath"],
            "app"
        );
        assert!(
            safe["services"]["web"]["image"]
                .as_str()
                .unwrap()
                .starts_with(&project)
        );
        assert!(owned(&ctx, &safe["services"]["web"]["build"]["labels"]));
        assert!(owned(&ctx, &safe["volumes"]["data"]["labels"]));
        assert!(owned(&ctx, &safe["networks"]["default"]["labels"]));
    }
    #[test]
    fn identities_and_forged_resource_labels_cannot_cross_owners_or_projects() {
        let dir = tempfile::tempdir().unwrap();
        let a = context(dir.path(), "owner-a");
        let b = context(dir.path(), "owner-b");
        assert_ne!(project(&a, "app"), project(&b, "app"));
        assert_ne!(
            alias(&project(&a, "app"), "web"),
            alias(&project(&a, "other"), "web")
        );
        let mut forged = labels(&a, &project(&a, "app"), "app");
        assert!(owned(&a, &forged));
        assert!(!owned(&b, &forged));
        forged[PROJECT] = json!(project(&b, "app"));
        assert!(!owned(&a, &forged));
        // Editable state is neither read nor used for resource authorization.
        fs::create_dir_all(dir.path().join(".knit/runtime-runs/demo")).unwrap();
        fs::write(
            dir.path().join(".knit/runtime-runs/demo/state.json"),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert_eq!(
            project(&a, "app"),
            project(&context(dir.path(), "owner-a"), "app")
        );
    }
    #[test]
    fn docker_command_has_only_trusted_environment_and_snapshot_file() {
        let dir = tempfile::tempdir().unwrap();
        let docker = Docker {
            home: dir,
            executable: "/usr/bin/docker".into(),
        };
        let snapshot = docker.home.path().join("snapshot.json");
        let cmd = docker.compose(&snapshot, "knit-synthetic");
        let env: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.unwrap().to_string_lossy().to_string(),
                )
            })
            .collect();
        assert_eq!(env["DOCKER_HOST"], "unix:///var/run/docker.sock");
        assert_eq!(env["COMPOSE_DISABLE_ENV_FILE"], "1");
        assert_eq!(env["HOME"], env["DOCKER_CONFIG"]);
        assert_eq!(cmd.get_current_dir(), Some(docker.home.path()));
        let args: Vec<_> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert_eq!(args.iter().filter(|s| *s == "--file").count(), 1);
        assert_eq!(args.last().unwrap(), snapshot.to_str().unwrap());
        let mut doc = json!({"environment":{"LITERAL":"${DOCKER_HOST} $VALUE $$"}});
        escape_interpolation(&mut doc);
        assert_eq!(
            doc["environment"]["LITERAL"],
            "$${DOCKER_HOST} $$VALUE $$$$"
        );
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_is_executed_alone_after_original_changes_and_hostile_config_never_starts() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = context(root.path(), "owner-a");
        let original = root.path().join("compose.json");
        fs::write(&original, serde_json::to_vec(&basic()).unwrap()).unwrap();
        let script = root.path().join("docker-fake");
        let log = root.path().join("calls.log");
        let captured = root.path().join("executed.json");
        let resolved = root.path().join("resolved.json");
        let started = root.path().join("started.json");
        let inspected = root.path().join("inspected.json");
        fs::write(&resolved, serde_json::to_vec(&basic()).unwrap()).unwrap();
        fs::write(
            &started,
            r#"[{"ID":"container-fixture","Service":"web","State":"running"}]"#,
        )
        .unwrap();
        let project_name = project(&ctx, "app");
        let mut container_labels = labels(&ctx, &project_name, "app");
        container_labels["com.docker.compose.project"] = json!(project_name);
        container_labels["com.docker.compose.service"] = json!("web");
        fs::write(
            &inspected,
            serde_json::to_vec(
                &json!([{"Config":{"Labels":container_labels},"State":{"Status":"running"}}]),
            )
            .unwrap(),
        )
        .unwrap();
        let script_text = format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{log}'
case "$*" in
  'image inspect '*) printf '%s' '[{{"Id":"sha256:public-fixture","RepoDigests":["alpine@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],"Config":{{"Volumes":{{"/config":{{}}}}}}}}]';;
  *'network inspect'*) printf '%s' '[{{"Id":"network-fixture","Driver":"bridge","Scope":"local","EnableIPv6":false,"Internal":false,"Options":{{}},"Labels":{{"io.knit.runtime.owner":"owner-a"}}}}]';;
  *'config --format json'*) cat '{resolved}'; printf '%s' '{{"services":{{"web":{{"privileged":true}}}}}}' > '{original}';;
  *'ps --all --orphans=false --format json'*) cat '{started}';;
  'container inspect '*) cat '{inspected}';;
  *'up --detach'*)
    previous=''
    for argument in "$@"; do
      if [ "$previous" = '--file' ]; then cp "$argument" '{captured}'; fi
      previous="$argument"
    done;;
esac
"#,
            log = log.display(),
            resolved = resolved.display(),
            started = started.display(),
            inspected = inspected.display(),
            original = original.display(),
            captured = captured.display()
        );
        fs::write(&script, script_text).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = Docker {
            home: tempfile::tempdir_in("/tmp").unwrap(),
            executable: script,
        };
        let plans = || {
            vec![StackPlan {
                repo: crate::RuntimeRepo {
                    id: "app".into(),
                    source_path: root.path().into(),
                    checkout: Some(root.path().into()),
                },
                checkout: root.path().into(),
                compose: original.clone(),
                mode: crate::config::RuntimeMode::Contract,
                project_name: "untrusted".into(),
            }]
        };
        let runtime = ProjectRuntime {
            database: Some(crate::config::ProjectRuntimeDatabase {
                mode: DatabaseMode::Bundle,
                ..Default::default()
            }),
            ..Default::default()
        };
        up_with_docker(&ctx, &runtime, plans(), &docker).unwrap();
        let executed: Value = serde_json::from_slice(&fs::read(&captured).unwrap()).unwrap();
        assert!(executed["services"]["web"].get("privileged").is_none());
        assert!(executed["services"]["web"].get("ports").is_none());
        assert_eq!(
            executed["services"]["web"]["image"],
            "sha256:public-fixture"
        );
        assert_eq!(executed["services"]["web"]["pull_policy"], "never");
        let volume_key = executed["services"]["web"]["volumes"][0]["source"]
            .as_str()
            .unwrap();
        assert_eq!(
            executed["services"]["web"]["volumes"][0]["target"],
            "/config"
        );
        assert!(owned(&ctx, &executed["volumes"][volume_key]["labels"]));
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls
                .lines()
                .any(|line| line.contains("--profile bundle-db")
                    && line.contains("config --format json"))
        );
        let up = calls.lines().find(|l| l.contains("up --detach")).unwrap();
        assert!(!up.contains(original.to_str().unwrap()));
        assert!(!up.contains("untrusted"));
        fs::remove_file(&captured).unwrap();
        fs::write(
            &resolved,
            r#"{"services":{"web":{"image":"alpine","privileged":true}}}"#,
        )
        .unwrap();
        assert!(
            up_with_docker(&ctx, &ProjectRuntime::default(), plans(), &docker)
                .unwrap_err()
                .to_string()
                .contains("privileged")
        );
        assert!(!captured.exists());
    }
    #[test]
    fn stopped_forged_or_wrong_network_containers_never_yield_preview_targets() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "owner-a");
        let project = project(&ctx, "app");
        let mut labels = labels(&ctx, &project, "app");
        labels["com.docker.compose.project"] = json!(project);
        labels["com.docker.compose.service"] = json!("web");
        labels[PORTS] =
            json!(r#"[{"service":"web","host":8080,"container":8080,"protocol":"tcp"}]"#);
        let running = json!({"Config":{"Labels":labels},"State":{"Running":true},"NetworkSettings":{"Networks":{"preview-fixture":{"NetworkID":"network-fixture","Aliases":[alias(&project,"web")]}}}});
        let ports = verified_ports(&ctx, &running, "network-fixture").unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0]["targetPort"], 8080);
        let mut stopped = running.clone();
        stopped["State"]["Running"] = json!(false);
        assert!(
            verified_ports(&ctx, &stopped, "network-fixture")
                .unwrap()
                .is_empty()
        );
        assert!(
            verified_ports(&ctx, &running, "replaced-network")
                .unwrap()
                .is_empty()
        );
        let mut forged = running.clone();
        forged["Config"]["Labels"][OWNER] = json!("owner-b");
        assert!(
            verified_ports(&ctx, &forged, "network-fixture")
                .unwrap()
                .is_empty()
        );
        let mut missing = running;
        missing["NetworkSettings"]["Networks"]["preview-fixture"]["Aliases"] = json!(["web"]);
        assert!(
            verified_ports(&ctx, &missing, "network-fixture")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn compose_five_synthesized_empty_ipam_is_normalized_but_custom_ipam_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "owner-a");
        let mut resolved = basic();
        // Exact default network shape reported by official Compose 5.5.1.
        resolved["networks"] = json!({"default":{"name":"sample_default","ipam":{}}});
        let safe = sanitize(resolved.clone(), &ctx, &project(&ctx, "app"), "app").unwrap();
        assert!(safe["networks"]["default"].get("ipam").is_none());
        assert_ne!(safe["networks"]["default"]["name"], "sample_default");
        for ipam in [
            json!({"driver":"default"}),
            json!({"config":[]}),
            json!({"config":[{"subnet":"192.0.2.0/24"}]}),
            json!({"options":{}}),
            json!(null),
            json!([]),
        ] {
            resolved["networks"]["default"]["ipam"] = ipam;
            let error = sanitize(resolved.clone(), &ctx, "project", "app").unwrap_err();
            assert!(
                error.to_string().contains("networks.default.ipam"),
                "{error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn purge_deduplicates_image_ids_removes_owned_tags_and_preserves_foreign_references() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = context(root.path(), "owner-a");
        let project = project(&ctx, "app");
        let first = format!("{project}-{}:runtime", digest(&["web"]));
        let second = format!("{project}-{}:runtime", digest(&["worker"]));
        let foreign = "foreign-project/application:keep";
        let resource = root.path().join("resource.json");
        let log = root.path().join("removed.log");
        let script = root.path().join("docker-fake");
        let image = json!({"Id":"sha256:fixture", "Config":{"Labels":labels(&ctx,&project,"app")},
            "RepoTags":[first, second, foreign], "RepoDigests":[]});
        fs::write(&resource, serde_json::to_vec(&json!([image])).unwrap()).unwrap();
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
set -eu
case "$1 $2" in
  'image ls') printf 'fixture\nfixture\n';;
  'image inspect') cat '{resource}';;
  'image rm')
    printf '%s\n' "$*" >> '{log}'
    # This fake reproduces Docker's multiply-tagged image-ID conflict.
    case "$3" in fixture|sha256:fixture|--force) exit 23;; esac;;
esac
"#,
                resource = resource.display(),
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = Docker {
            home: tempfile::tempdir_in("/tmp").unwrap(),
            executable: script,
        };
        down_with_docker(&ctx, true, &docker).unwrap();
        let removed = fs::read_to_string(&log).unwrap();
        let expected: std::collections::BTreeSet<_> =
            [format!("image rm {first}"), format!("image rm {second}")]
                .into_iter()
                .collect();
        assert_eq!(
            removed
                .lines()
                .map(str::to_string)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        assert_eq!(
            removed.lines().count(),
            2,
            "duplicate image IDs must not repeat removals"
        );
        assert!(!removed.contains(foreign));
        // A subsequent purge may still discover labels on the preserved image;
        // no foreign tag is ever treated as an owner-authorized removal target.
        let mut preserved = image.clone();
        preserved["RepoTags"] = json!([foreign]);
        fs::write(&resource, serde_json::to_vec(&json!([preserved])).unwrap()).unwrap();
        down_with_docker(&ctx, true, &docker).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), removed);
        // Matching query results are not authorization when inspection disagrees.
        let mut forged = image;
        forged["Config"]["Labels"][OWNER] = json!("owner-b");
        fs::write(&resource, serde_json::to_vec(&json!([forged])).unwrap()).unwrap();
        assert!(
            down_with_docker(&ctx, true, &docker)
                .unwrap_err()
                .to_string()
                .contains("unowned")
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), removed);
    }
    #[test]
    fn transform_remaps_nested_sources_and_freezes_the_bundle_dockerfile() {
        let root = tempfile::tempdir().unwrap();
        for path in ["source/app/nested", "bundle/app", "bundle/nested"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        fs::write(
            root.path().join("source/app/Dockerfile"),
            "FROM knit-foreign:runtime",
        )
        .unwrap();
        fs::write(
            root.path().join("source/app/nested/Dockerfile"),
            "FROM knit-other:runtime",
        )
        .unwrap();
        fs::write(
            root.path().join("bundle/app/Dockerfile"),
            "FROM alpine:3\nRUN echo bundle-app",
        )
        .unwrap();
        fs::write(
            root.path().join("bundle/nested/Dockerfile"),
            "FROM scratch\nLABEL fixture=bundle-nested",
        )
        .unwrap();
        let mut ctx = context(root.path(), "owner-a");
        ctx.repos = vec![
            crate::RuntimeRepo {
                id: "app".into(),
                source_path: root.path().join("source/app"),
                checkout: Some(root.path().join("bundle/app")),
            },
            crate::RuntimeRepo {
                id: "nested".into(),
                source_path: root.path().join("source/app/nested"),
                checkout: Some(root.path().join("bundle/nested")),
            },
        ];
        let mut doc = json!({"services":{
            "app":{"build":{"context":root.path().join("source/app"),"dockerfile":"Dockerfile"}},
            "nested":{"build":{"context":root.path().join("source/app/nested"),"dockerfile":"Dockerfile"},
                "volumes":[{"type":"bind","source":root.path().join("source/app/nested"),"target":"/app"}]}
        }});
        remap_workspace_paths(&mut doc, &ctx).unwrap();
        assert_eq!(
            doc["services"]["nested"]["build"]["context"],
            json!(fs::canonicalize(root.path().join("bundle/nested")).unwrap())
        );
        let safe = sanitize(doc, &ctx, &project(&ctx, "app"), "app").unwrap();
        assert_eq!(
            safe["services"]["nested"]["volumes"][0]["volume"]["subpath"],
            "bundle/nested"
        );
        assert!(
            safe["services"]["app"]["build"]["dockerfile_inline"]
                .as_str()
                .unwrap()
                .contains("bundle-app")
        );
        assert!(
            safe["services"]["nested"]["build"]["dockerfile_inline"]
                .as_str()
                .unwrap()
                .contains("bundle-nested")
        );
        fs::write(
            root.path().join("bundle/app/Dockerfile"),
            "FROM knit-foreign:runtime",
        )
        .unwrap();
        assert!(!safe.to_string().contains("knit-foreign"));
        assert!(safe["services"]["app"]["build"].get("dockerfile").is_none());
        assert_eq!(safe["services"]["app"]["build"]["pull"], true);
        assert_eq!(safe["services"]["app"]["build"]["no_cache"], true);
        let outside = tempfile::tempdir().unwrap();
        let mut hostile = json!({"services":{"web":{"volumes":[{"type":"bind","source":outside.path(),"target":"/host"}]}}});
        assert!(remap_workspace_paths(&mut hostile, &ctx).is_err());
    }

    #[test]
    fn image_declared_volumes_are_explicit_stable_owned_and_do_not_replace_user_mounts() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), "owner-a");
        let project = project(&ctx, "app");
        let doc = json!({"services":{"web":{"image":"caddy:2","volumes":[{"type":"volume","source":"data","target":"/data"}]}},"volumes":{"data":{}}});
        let mut safe = sanitize(doc, &ctx, &project, "app").unwrap();
        let metadata = BTreeMap::from([(
            "web".to_string(),
            json!({"Config":{"Volumes":{"/config":{},"/data":{}}}}),
        )]);
        add_declared_image_volumes(&mut safe, &ctx, &project, "app", &metadata).unwrap();
        let mounts = safe["services"]["web"]["volumes"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0]["source"], "data");
        let key = mounts[1]["source"].as_str().unwrap().to_string();
        assert_eq!(mounts[1]["target"], "/config");
        assert!(owned(&ctx, &safe["volumes"][&key]["labels"]));
        let first = safe.clone();
        add_declared_image_volumes(&mut safe, &ctx, &project, "app", &metadata).unwrap();
        assert_eq!(first, safe);
        let other = context(root.path(), "owner-b");
        assert!(!owned(&other, &safe["volumes"][&key]["labels"]));
    }

    #[cfg(unix)]
    #[test]
    fn failed_registry_pull_cannot_fall_back_to_a_cached_private_canary() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let script = root.path().join("docker-fake");
        let log = root.path().join("calls.log");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
if [ "$1 $2" = 'image pull' ]; then exit 1; fi
# A cached image would contain a synthetic canary; inspect must never happen.
printf '%s' '[{{"Id":"sha256:synthetic-private-canary"}}]'
"#,
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = Docker {
            home: tempfile::tempdir_in("/tmp").unwrap(),
            executable: script,
        };
        assert!(
            pull_registry_image(&docker, "private-canary:cached", &mut BTreeMap::new()).is_err()
        );
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "image pull docker.io/library/private-canary:cached\n"
        );
        let ctx = context(root.path(), "owner-a");
        let mut doc = basic();
        doc["services"]["web"]["pull_policy"] = json!("never");
        assert_eq!(
            sanitize(doc, &ctx, "project", "app").unwrap()["services"]["web"]["pull_policy"],
            "always"
        );
    }
    #[cfg(unix)]
    #[test]
    fn builds_use_frozen_registry_sources_then_run_the_inspected_image_without_rebuilding() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = context(root.path(), "owner-a");
        let project = project(&ctx, "app");
        let dockerfile = root.path().join("Dockerfile");
        fs::write(
            &dockerfile,
            "FROM alpine:3\nRUN echo synthetic\nVOLUME /data",
        )
        .unwrap();
        let original = root.path().join("compose.json");
        let resolved = root.path().join("resolved.json");
        let config =
            json!({"services":{"web":{"build":{"context":root.path(),"dockerfile":"Dockerfile"}}}});
        fs::write(&original, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::write(&resolved, serde_json::to_vec(&config).unwrap()).unwrap();
        let output_metadata = root.path().join("output.json");
        fs::write(&output_metadata,serde_json::to_vec(&json!([{"Id":"sha256:built-fixture","Config":{"Labels":labels(&ctx,&project,"app"),"Volumes":{"/data":{}}}}])).unwrap()).unwrap();
        let script = root.path().join("docker-fake");
        let log = root.path().join("calls.log");
        let build_capture = root.path().join("build.json");
        let run_capture = root.path().join("run.json");
        let started = root.path().join("started.json");
        let inspected = root.path().join("inspected.json");
        fs::write(
            &started,
            r#"[{"ID":"container-fixture","Service":"web","State":"running"}]"#,
        )
        .unwrap();
        let mut container_labels = labels(&ctx, &project, "app");
        container_labels["com.docker.compose.project"] = json!(project);
        container_labels["com.docker.compose.service"] = json!("web");
        fs::write(
            &inspected,
            serde_json::to_vec(
                &json!([{"Config":{"Labels":container_labels},"State":{"Status":"running"}}]),
            )
            .unwrap(),
        )
        .unwrap();
        fs::write(&script,format!(r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{log}'
case "$*" in
  'network inspect '*) printf '%s' '[{{"Id":"network-fixture","Driver":"bridge","Scope":"local","EnableIPv6":false,"Internal":false,"Options":{{}},"Labels":{{"io.knit.runtime.owner":"owner-a"}}}}]';;
  *'config --format json') cat '{resolved}';;
  *'ps --all --orphans=false --format json'*) cat '{started}';;
  'container inspect '*) cat '{inspected}';;
  'image pull '*) printf '%s' 'FROM knit-private-canary:runtime' > '{dockerfile}';;
  'image inspect docker.io/'*) printf '%s' '[{{"Id":"sha256:public-fixture","RepoDigests":["alpine@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],"Config":{{}}}}]';;
  'image inspect knit-'*) cat '{output_metadata}';;
  *'build --pull --no-cache'*)
    previous=''
    for argument in "$@"; do
      if [ "$previous" = '--file' ]; then cp "$argument" '{build_capture}'; fi
      previous="$argument"
    done;;
  *'up --detach --no-build --pull never --remove-orphans'*)
    previous=''
    for argument in "$@"; do
      if [ "$previous" = '--file' ]; then cp "$argument" '{run_capture}'; fi
      previous="$argument"
    done;;
esac
"#,log=log.display(),resolved=resolved.display(),started=started.display(),inspected=inspected.display(),dockerfile=dockerfile.display(),output_metadata=output_metadata.display(),build_capture=build_capture.display(),run_capture=run_capture.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = Docker {
            home: tempfile::tempdir_in("/tmp").unwrap(),
            executable: script,
        };
        let plan = StackPlan {
            repo: crate::RuntimeRepo {
                id: "app".into(),
                source_path: root.path().into(),
                checkout: Some(root.path().into()),
            },
            checkout: root.path().into(),
            compose: original,
            mode: crate::config::RuntimeMode::Contract,
            project_name: "unused".into(),
        };
        up_with_docker(&ctx, &ProjectRuntime::default(), vec![plan], &docker).unwrap();
        let build: Value = serde_json::from_slice(&fs::read(build_capture).unwrap()).unwrap();
        let source = build["services"]["web"]["build"]["dockerfile_inline"]
            .as_str()
            .unwrap();
        assert!(source.starts_with("FROM docker.io/library/alpine@sha256:"));
        assert!(!source.contains("canary"));
        assert!(fs::read_to_string(dockerfile).unwrap().contains("canary"));
        let run: Value = serde_json::from_slice(&fs::read(run_capture).unwrap()).unwrap();
        assert!(run["services"]["web"].get("build").is_none());
        assert_eq!(run["services"]["web"]["image"], "sha256:built-fixture");
        let volume = run["services"]["web"]["volumes"][0]["source"]
            .as_str()
            .unwrap();
        assert_eq!(run["services"]["web"]["volumes"][0]["target"], "/data");
        assert!(owned(&ctx, &run["volumes"][volume]["labels"]));
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.starts_with("image pull "))
                .count(),
            1
        );
    }
    #[test]
    fn network_topology_matches_only_the_broker_qualified_default_ipv4_bridge() {
        let ordinary = json!({"Scope":"local","Driver":"bridge","EnableIPv6":false,"Internal":false,"Options":{},
            "IPAM":{"Driver":"default","Options":null,"Config":[{"Subnet":"192.0.2.0/24","Gateway":"192.0.2.1"}]}});
        validate_network_topology(&ordinary).unwrap();
        for (field, value) in [
            ("Scope", json!("swarm")),
            ("Driver", json!("overlay")),
            ("Driver", json!("macvlan")),
            ("EnableIPv6", json!(true)),
            ("Internal", json!(true)),
            (
                "Options",
                json!({"com.docker.network.bridge.gateway_mode_ipv4":"routed"}),
            ),
            (
                "Options",
                json!({"com.docker.network.bridge.gateway_mode_ipv4":"nat-unprotected"}),
            ),
            (
                "Options",
                json!({"com.docker.network.bridge.trusted_host_interfaces":"eth0"}),
            ),
            ("Options", json!(null)),
        ] {
            let mut unsafe_network = ordinary.clone();
            unsafe_network[field] = value;
            assert!(
                validate_network_topology(&unsafe_network)
                    .unwrap_err()
                    .to_string()
                    .contains(field)
            );
        }
        for field in ["Scope", "Driver", "EnableIPv6", "Internal", "Options"] {
            let mut incomplete = ordinary.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                validate_network_topology(&incomplete).is_err(),
                "missing {field}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_owned_networks_cannot_be_reused_or_previewed_but_can_be_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = context(root.path(), "owner-a");
        let project = project(&ctx, "app");
        let resource = root.path().join("network.json");
        let removed = root.path().join("removed.log");
        let script = root.path().join("docker-fake");
        fs::write(&resource,serde_json::to_vec(&json!([{"Id":"network-fixture","Scope":"local","Driver":"bridge",
            "EnableIPv6":false,"Internal":false,"Options":{"com.docker.network.bridge.gateway_mode_ipv4":"routed"},
            "Labels":labels(&ctx,&project,"app")}])).unwrap()).unwrap();
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
set -eu
case "$1 $2" in
  'network ls') printf 'network-fixture\n';;
  'network inspect') cat '{resource}';;
  'network rm') printf '%s\n' "$*" >> '{removed}';;
esac
"#,
                resource = resource.display(),
                removed = removed.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = Docker {
            home: tempfile::tempdir_in("/tmp").unwrap(),
            executable: script,
        };
        assert!(
            verify_preview_network(&docker, ctx.engine.as_ref().unwrap())
                .unwrap_err()
                .to_string()
                .contains("Options")
        );
        assert!(
            check_project_ownership(&docker, &ctx, &project)
                .unwrap_err()
                .to_string()
                .contains("Options")
        );
        let document =
            json!({"networks":{"default":{"name":"network-fixture"}},"volumes":{},"services":{}});
        assert!(
            check_named_resources(&docker, &ctx, &document)
                .unwrap_err()
                .to_string()
                .contains("Options")
        );
        assert!(!removed.exists());
        for purge in [false, true] {
            down_with_docker(&ctx, purge, &docker).unwrap();
        }
        assert_eq!(
            fs::read_to_string(removed).unwrap(),
            "network rm network-fixture\nnetwork rm network-fixture\n"
        );
    }
}
