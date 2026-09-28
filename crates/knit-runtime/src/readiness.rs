//! Bounded startup verification for one compose stack, run only AFTER every
//! stack's ordinary `up --build -d` has launched. A detached `up` reports
//! success the moment containers are created, so an API that exits a second
//! later — or never passes its healthcheck — still looks like a healthy
//! start. This module is the gate between "detached start returned" and
//! "the run is declared successful".
//!
//! It never runs `up` again: a second `up` — even `--no-build --wait` — can
//! recreate or rerun one-shot services, restarting a migration job that
//! already completed. Verification is read-only:
//!
//! 1. `docker compose config --format json` (same file, project, profiles,
//!    environment, and working directory as the launch) resolves the
//!    stack's own declaration of what startup means: which services are
//!    expected (Compose resolves the active set itself — profile flags,
//!    `COMPOSE_PROFILES`, and `.env` included — so every service in the
//!    output is active and expected, whatever profiles it lists), how many
//!    replicas each runs, which of them define healthchecks, and which are
//!    intentional one-shot jobs — a service some other service awaits with
//!    `depends_on` condition `service_completed_successfully`, the one
//!    Compose-native signal that exiting is its success state. `scale: 0`
//!    services are expected of nothing.
//! 2. `docker compose ps --all --orphans=false --format json` is polled on
//!    a hard deadline for that expected set. A service is ready when it is
//!    running and — when it declares a healthcheck — healthy, or, for a
//!    declared job, when it has exited(0). Exited(non-zero), restarting,
//!    unhealthy, paused, or never-created services fail the run, and so
//!    does a service running fewer containers than its declared replicas.
//!    An undeclared service exiting(0) fails too: only the dependency
//!    condition makes exit(0) intentional, so an API that just quits
//!    cannot pass as a finished job.
//!
//! The whole verification — model resolution and every poll — runs under
//! one budget that starts before the first subprocess is spawned. Each
//! subprocess runs in its own process group with piped output received
//! through bounded channels and the process polled with `try_wait`, so a
//! wedged `docker compose` — or a plugin process holding its pipes after
//! it dies — is killed or abandoned at the deadline instead of hanging
//! the run; full pipes cannot deadlock it.
//! A fast-finishing job never ends the wait early while another service's
//! healthcheck is still starting.
//!
//! Operational contract: containers are never touched (no stop, no down,
//! no log dump — service logs may carry secrets), so a failed start leaves
//! the exact containers in place for the user to inspect themselves. Error
//! text never includes injected environment values; only service names,
//! states, exit codes, and a bounded tail of Compose's own stderr pass
//! through.
//!
//! The check runs per stack after ALL stacks launched so a consumer that
//! waits on a sibling stack's published endpoint cannot deadlock the wait
//! inside a single stack's startup.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How much of a failing command's stderr to surface. Compose errors that
/// matter ("service X is unhealthy") are short; build and pull spam is not.
const STDERR_TAIL_CHARS: usize = 1200;

/// Poll cadence for the structured `ps` verdict and the bounded subprocess
/// wait. Not an arbitrary settle delay: every poll re-reads real container
/// or process states under a hard deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Everything needed to address one already-launched stack, exactly as
/// phase 3 launched it. `compose_file` must be absolute (the commands run
/// with `checkout` as their cwd, matching the launch).
pub(crate) struct StackStartupCheck<'a> {
    /// Repo id of the stack, for error reporting only.
    pub(crate) repo: &'a str,
    /// The stack's isolated compose project name.
    pub(crate) project_name: &'a str,
    /// Compose file the stack runs: the generated transform file or the
    /// repo's contract file.
    pub(crate) compose_file: &'a Path,
    /// Compose profiles activated for this stack (e.g. `bundle-db`).
    pub(crate) profiles: &'a [String],
    /// Environment the stack was launched with (contract `KNIT_*` values).
    /// Injected into every command here, never printed.
    pub(crate) env: &'a BTreeMap<String, String>,
    /// Stack checkout, the cwd Compose ran in (`.env` resolution context).
    pub(crate) checkout: &'a Path,
    /// Bounded startup budget in seconds; the caller validates it is > 0.
    pub(crate) timeout_seconds: u64,
}

/// Final per-service states of a verified stack, sorted by service name —
/// for example `("api", "running (healthy)")`, `("migrate-job", "exited (0)")`.
pub(crate) struct StackStartupReport {
    pub(crate) services: Vec<(String, String)>,
}

impl StackStartupReport {
    /// One-line success summary, e.g. `api running (healthy), db running`.
    pub(crate) fn summary(&self) -> String {
        self.services
            .iter()
            .map(|(service, status)| format!("{service} {status}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Verify one launched stack became genuinely up: resolve what the stack
/// itself expects (read-only `compose config`), then poll structured `ps`
/// under a hard deadline until every expected service is running/healthy —
/// or exited(0) for declared completed jobs. Fails — with exact repo and
/// per-service statuses — on exited(non-zero), restarting, unhealthy,
/// paused, undeclared exits, short replica counts, and expected services
/// that never appeared. Never starts, stops, recreates, or removes
/// anything, so failed starts keep their containers for diagnosis.
pub(crate) fn verify_stack_startup(check: &StackStartupCheck<'_>) -> Result<StackStartupReport> {
    let timeout = startup_timeout(check.timeout_seconds);
    // The budget bounds subprocesses too, so it starts before the first
    // one is spawned: a wedged `docker compose` cannot outlive it.
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let model = load_stack_model(check, deadline)?;
    loop {
        let statuses = inspect_services(check, deadline)?;
        let verdict = evaluate(&model, &statuses);
        if verdict.is_ready() {
            return Ok(StackStartupReport {
                services: verdict.ready,
            });
        }
        // A terminal container state settles nothing by waiting; a missing
        // or still-starting service might, so only those reach the deadline.
        if !verdict.failed.is_empty() || Instant::now() >= deadline {
            bail!("{}", startup_failure_message(check, timeout, &verdict));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// What the stack's resolved compose model expects of one service.
struct ServiceExpectation {
    /// Some other service awaits this one with
    /// `depends_on: {condition: service_completed_successfully}`, so
    /// exit(0) is its success state — the only exited state that ever
    /// counts as ready.
    completed_job: bool,
    /// The service declares a healthcheck; running alone is not enough.
    healthchecked: bool,
    /// How many containers the service runs (`scale`, or
    /// `deploy.replicas`; 1 by default). Fewer live containers than this
    /// is not ready.
    replicas: u64,
}

/// The expected services of a stack, from its resolved compose model.
struct StackModel {
    expected: BTreeMap<String, ServiceExpectation>,
}

/// Resolve the stack's model with `docker compose config`: the same
/// file, project, profiles, environment, and cwd the stack launched with,
/// so interpolation sees exactly what the launch saw. Bounded by the
/// startup deadline.
fn load_stack_model(check: &StackStartupCheck<'_>, deadline: Instant) -> Result<StackModel> {
    let output = run_compose_bounded(check, &config_args(), deadline)?;
    if !output.status.success() {
        bail!(
            "docker compose config exited with status {}. {}",
            output.status,
            bounded_tail(&stderr_text(&output))
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let config: Value =
        serde_json::from_str(text.trim()).context("failed to parse docker compose config")?;
    let model = parse_stack_model(&config);
    if model.expected.is_empty() {
        bail!(
            "docker compose config resolved no services for stack `{}` (compose project `{}`). \
Check the compose file and its profile configuration.",
            check.repo,
            check.project_name
        );
    }
    Ok(model)
}

/// Derive the expected services from resolved `docker compose config`
/// output. Compose has already resolved the active service set — profile
/// flags, inherited `COMPOSE_PROFILES`, and `.env` included — so every
/// service in the output is active and expected, whatever `profiles` field
/// it still carries; only `scale: 0` services (and `deploy.replicas: 0`)
/// are expected of nothing. A `healthcheck` block (unless disabled) makes
/// running insufficient, and services awaited with
/// `service_completed_successfully` are completed jobs.
fn parse_stack_model(config: &Value) -> StackModel {
    let services = field(config, "services").and_then(Value::as_object);
    let mut completed_jobs: BTreeSet<String> = BTreeSet::new();
    if let Some(services) = services {
        for service in services.values() {
            collect_completed_jobs(field(service, "depends_on"), &mut completed_jobs);
        }
    }
    let mut expected = BTreeMap::new();
    if let Some(services) = services {
        for (name, service) in services {
            let replicas = service_scale(service).unwrap_or(1);
            if replicas == 0 {
                continue;
            }
            expected.insert(
                name.clone(),
                ServiceExpectation {
                    completed_job: completed_jobs.contains(name),
                    healthchecked: declares_healthcheck(service),
                    replicas,
                },
            );
        }
    }
    StackModel { expected }
}

/// Collect dependency names awaited as completed jobs. Compose resolves
/// `depends_on` into a map of `{name: {condition, ...}}`; the list forms
/// (plain names, or `{Name, Condition}` objects) are accepted defensively.
fn collect_completed_jobs(depends_on: Option<&Value>, completed: &mut BTreeSet<String>) {
    let Some(depends_on) = depends_on else {
        return;
    };
    if let Some(mapping) = depends_on.as_object() {
        for (name, dependency) in mapping {
            if condition_of(dependency) == Some("service_completed_successfully") {
                completed.insert(name.clone());
            }
        }
    } else if let Some(entries) = depends_on.as_array() {
        for entry in entries {
            if let Some(name) = field(entry, "Name")
                .or_else(|| field(entry, "name"))
                .and_then(Value::as_str)
            {
                if condition_of(entry) == Some("service_completed_successfully") {
                    completed.insert(name.to_string());
                }
            }
        }
    }
}

fn condition_of(dependency: &Value) -> Option<&str> {
    dependency
        .get("condition")
        .or_else(|| dependency.get("Condition"))
        .and_then(Value::as_str)
}

/// A service's declared container count: the `scale` key or
/// `deploy.replicas`. Absent means one.
fn service_scale(service: &Value) -> Option<u64> {
    if let Some(scale) = field(service, "scale").and_then(Value::as_u64) {
        return Some(scale);
    }
    field(service, "deploy")
        .and_then(|deploy| field(deploy, "replicas"))
        .and_then(Value::as_u64)
}

/// Whether the service runs a Docker healthcheck: a `healthcheck` block
/// that is neither `disable: true` nor a `NONE` test.
fn declares_healthcheck(service: &Value) -> bool {
    let Some(healthcheck) = field(service, "healthcheck") else {
        return false;
    };
    if !healthcheck.is_object() {
        return false;
    }
    if field(healthcheck, "disable").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let none_test = match field(healthcheck, "test") {
        Some(Value::Array(parts)) => parts.first().and_then(Value::as_str) == Some("NONE"),
        Some(Value::String(test)) => test == "NONE",
        _ => false,
    };
    !none_test
}

/// Arguments of the model resolution pass: the stack's resolved
/// configuration as structured JSON. Pure resolution, no side effects.
fn config_args() -> Vec<String> {
    ["config", "--format", "json"]
        .iter()
        .map(|argument| argument.to_string())
        .collect()
}

/// Arguments of the inspection pass: every container of the project,
/// including exited ones, as structured JSON. Orphans stay out of the
/// verdict — they are not this stack's services.
fn ps_args() -> Vec<String> {
    ["ps", "--all", "--orphans=false", "--format", "json"]
        .iter()
        .map(|argument| argument.to_string())
        .collect()
}

/// A `docker compose` command addressing exactly the launched stack: same
/// file, project, profiles, environment, and working directory as phase
/// 3's `up`.
fn compose_command(check: &StackStartupCheck<'_>, args: &[String]) -> Command {
    let mut command = Command::new("docker");
    command.args(["compose", "-f"]);
    command.arg(check.compose_file);
    command.args(["-p", check.project_name]);
    for profile in check.profiles {
        command.args(["--profile", profile]);
    }
    command.args(args);
    command.envs(check.env);
    command.current_dir(check.checkout);
    command.stdin(Stdio::null());
    command
}

/// Run one read-only compose subcommand under the startup deadline and
/// capture its output.
fn run_compose_bounded(
    check: &StackStartupCheck<'_>,
    args: &[String],
    deadline: Instant,
) -> Result<Output> {
    let what = format!(
        "docker compose {}",
        args.first().map(String::as_str).unwrap_or("")
    );
    let mut command = compose_command(check, args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Own process group, so a wedged Compose — and any plugin process it
    // spawned — can be killed as one unit at the deadline.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to run {what}"))?;
    wait_bounded(child, &what, deadline)
}

/// Wait for a subprocess under the deadline: stdout and stderr are drained
/// on their own reader threads whose results are received with a bounded
/// `recv_timeout` (a full pipe must not wedge the wait, and neither may a
/// descendant process still holding the pipe after the direct child is
/// gone), the process is polled with `try_wait`, and one still running at
/// the deadline is killed together with its process group and reaped. A
/// plain `.output()` can hang forever on a wedged docker; this cannot
/// outlive the startup budget.
fn wait_bounded(mut child: Child, what: &str, deadline: Instant) -> Result<Output> {
    let pid = child.id();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    // Never joined: a reader whose pipe a descendant holds open would block
    // a join forever. Its result arrives through the channel or not at all.
    let _stdout_reader = thread::spawn(move || {
        let _ = stdout_tx.send(read_all(stdout_pipe));
    });
    let _stderr_reader = thread::spawn(move || {
        let _ = stderr_tx.send(read_all(stderr_pipe));
    });
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait for {what}"))?
        {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(POLL_INTERVAL);
    };
    match status {
        Some(status) => {
            // The direct child is reaped; if capturing its output runs out
            // of budget or fails on the pipe, the CLI's plugin processes
            // may still be alive holding the pipes — kill their group
            // before propagating.
            let captured = (|| -> Result<(Vec<u8>, Vec<u8>)> {
                let stdout = recv_bounded(&stdout_rx, deadline)
                    .with_context(|| {
                        format!("failed to capture {what} stdout within the startup budget")
                    })?
                    .context(format!("failed to read {what} stdout"))?;
                let stderr = recv_bounded(&stderr_rx, deadline)
                    .with_context(|| {
                        format!("failed to capture {what} stderr within the startup budget")
                    })?
                    .context(format!("failed to read {what} stderr"))?;
                Ok((stdout, stderr))
            })();
            match captured {
                Ok((stdout, stderr)) => Ok(Output {
                    status,
                    stdout,
                    stderr,
                }),
                Err(error) => {
                    kill_group(pid);
                    Err(error)
                }
            }
        }
        None => {
            // Kill the whole process group first (best effort — the
            // Compose CLI's plugin processes hold the pipes), then the
            // direct child, and reap it. The readers are abandoned: their
            // pipes may never reach EOF.
            kill_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{what} did not finish within the startup budget and was terminated. \
Containers were left in place for diagnosis; clean up with `knit run down`."
            );
        }
    }
}

/// Receive one reader thread's result under the remaining budget. `None`
/// means no result arrived in time — a capture failure, not empty output.
fn recv_bounded<T>(receiver: &mpsc::Receiver<T>, deadline: Instant) -> Option<T> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

/// Best-effort kill of the child's whole process group: spawning with
/// `process_group(0)` made the child its own leader, so its pid names the
/// group. Uses the `kill` utility to avoid a libc dependency; failures are
/// ignored — the direct child is killed right after regardless.
#[cfg(unix)]
fn kill_group(child_pid: u32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{child_pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_group(_child_pid: u32) {}

/// Drain an optional pipe to EOF. Read errors propagate to the caller as
/// io errors — only the error text, never the bytes already captured.
fn read_all<R: Read>(mut pipe: Option<R>) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        pipe.read_to_end(&mut buffer)?;
    }
    Ok(buffer)
}

/// Structured service states of the stack from `docker compose ps`,
/// bounded by the startup deadline.
fn inspect_services(
    check: &StackStartupCheck<'_>,
    deadline: Instant,
) -> Result<Vec<ServiceStatus>> {
    let output = run_compose_bounded(check, &ps_args(), deadline)?;
    if !output.status.success() {
        bail!(
            "docker compose ps exited with status {}. {}",
            output.status,
            bounded_tail(&stderr_text(&output))
        );
    }
    Ok(parse_ps_services(&String::from_utf8_lossy(&output.stdout)))
}

#[derive(Debug, Clone, PartialEq)]
struct ServiceStatus {
    service: String,
    state: String,
    exit_code: Option<i64>,
    health: Option<String>,
}

/// Parse `docker compose ps --format json` output: a JSON array in some
/// Compose versions, one JSON object per line in others, a bare object for
/// a single container, and empty text when the project has no containers.
/// `docker compose run` one-off containers are skipped: they are not
/// services of the stack. Malformed output yields fewer (or zero)
/// services, which the expected-set verdict then reports as missing —
/// never as success.
fn parse_ps_services(text: &str) -> Vec<ServiceStatus> {
    let trimmed = text.trim();
    let entries: Vec<Value> = if trimmed.is_empty() {
        Vec::new()
    } else if let Ok(Value::Array(values)) = serde_json::from_str(trimmed) {
        values
    } else {
        trimmed
            .lines()
            .filter_map(|line| serde_json::from_str(line.trim()).ok())
            .collect()
    };
    entries.iter().filter_map(parse_service_status).collect()
}

/// One `ps` entry into a [`ServiceStatus`]. Compose emits Go-cased keys
/// (`Service`, `State`, `ExitCode`, `Health`); `Health` is an empty string,
/// not absent, when the service defines no healthcheck.
fn parse_service_status(entry: &Value) -> Option<ServiceStatus> {
    let service = field(entry, "Service")?.as_str()?.to_string();
    let state = field(entry, "State")?.as_str()?.to_ascii_lowercase();
    let exit_code = field(entry, "ExitCode").and_then(Value::as_i64);
    let health = field(entry, "Health")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|health| !health.is_empty() && *health != "none")
        .map(str::to_ascii_lowercase);
    if labels(entry).any(|label| label == "com.docker.compose.oneoff=True") {
        return None;
    }
    Some(ServiceStatus {
        service,
        state,
        exit_code,
        health,
    })
}

/// Case-insensitive entry field: Go-cased first, lowercase second.
fn field<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
    entry
        .get(key)
        .or_else(|| entry.get(key.to_ascii_lowercase()))
}

/// The comma-separated `Labels` string of a `ps` entry, split into labels.
fn labels(entry: &Value) -> impl Iterator<Item = &str> {
    field(entry, "Labels")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
}

enum Classification {
    /// Serving, or a completed job that finished successfully.
    Ready(String),
    /// Not yet up; only a failure once the startup budget is spent.
    Pending(String),
    /// Cannot become ready on its own anymore.
    Failed(String),
}

/// Classify one container against what the stack's model expects of its
/// service. Exit(0) is success only for services the stack declares as
/// completed jobs; anything else that exited, restart-loops, or reports
/// unhealthy has failed, and a healthchecked service that only runs is not
/// done yet.
fn classify(status: &ServiceStatus, expectation: &ServiceExpectation) -> Classification {
    match status.state.as_str() {
        "running" => {
            if expectation.completed_job {
                return Classification::Pending("job still running".to_string());
            }
            match status.health.as_deref() {
                Some("healthy") => Classification::Ready("running (healthy)".to_string()),
                Some("unhealthy") => Classification::Failed("unhealthy".to_string()),
                Some("starting") => Classification::Pending("health check starting".to_string()),
                Some(other) => Classification::Pending(format!("health {other}")),
                None if expectation.healthchecked => {
                    Classification::Pending("health status not reported".to_string())
                }
                None => Classification::Ready("running".to_string()),
            }
        }
        "exited" => match status.exit_code {
            Some(0) if expectation.completed_job => Classification::Ready("exited (0)".to_string()),
            Some(0) => Classification::Failed(
                "exited (0) without being a declared completed job; intentional one-shots \
need a dependent's `depends_on: condition: service_completed_successfully`"
                    .to_string(),
            ),
            Some(code) => Classification::Failed(format!("exited (code {code})")),
            None => Classification::Failed("exited".to_string()),
        },
        "restarting" => Classification::Failed("restarting (crash loop)".to_string()),
        "created" => Classification::Pending("created but not started".to_string()),
        other => Classification::Failed(other.to_string()),
    }
}

struct Verdict {
    failed: Vec<(String, String)>,
    pending: Vec<(String, String)>,
    ready: Vec<(String, String)>,
}

impl Verdict {
    fn is_ready(&self) -> bool {
        self.failed.is_empty() && self.pending.is_empty()
    }
}

/// Judge the live container states against the expected service set. Every
/// expected service must be present, fully replicated, and classified
/// ready — a service with no container at all (and an empty or malformed
/// `ps` in general) stays pending, so it can only ever fail the run, never
/// pass it. A service running fewer containers than its declared replica
/// count is not ready yet, but a container that already failed outranks
/// the shortage. A service with more containers than declared is only as
/// good as its worst container.
fn evaluate(model: &StackModel, statuses: &[ServiceStatus]) -> Verdict {
    let mut grouped: BTreeMap<&str, Vec<&ServiceStatus>> = BTreeMap::new();
    for status in statuses {
        grouped
            .entry(status.service.as_str())
            .or_default()
            .push(status);
    }
    let mut verdict = Verdict {
        failed: Vec::new(),
        pending: Vec::new(),
        ready: Vec::new(),
    };
    for (service, expectation) in &model.expected {
        let Some(containers) = grouped.get(service.as_str()) else {
            verdict
                .pending
                .push((service.clone(), "no container found".to_string()));
            continue;
        };
        let mut failed: Option<String> = None;
        let mut pending: Option<String> = None;
        let mut labels: Vec<String> = Vec::new();
        for container in containers {
            match classify(container, expectation) {
                Classification::Ready(label) => labels.push(label),
                Classification::Pending(reason) => {
                    if pending.is_none() {
                        pending = Some(reason);
                    }
                }
                Classification::Failed(reason) => {
                    if failed.is_none() {
                        failed = Some(reason);
                    }
                }
            }
        }
        if let Some(reason) = failed {
            verdict.failed.push((service.clone(), reason));
        } else if (containers.len() as u64) < expectation.replicas {
            verdict.pending.push((
                service.clone(),
                format!(
                    "only {} of {} containers present",
                    containers.len(),
                    expectation.replicas
                ),
            ));
        } else if let Some(reason) = pending {
            verdict.pending.push((service.clone(), reason));
        } else {
            labels.sort();
            labels.dedup();
            verdict.ready.push((service.clone(), labels.join(" / ")));
        }
    }
    verdict
}

/// The failure a user sees when a stack did not become ready: exact repo,
/// compose project, every expected service's status, and a bounded tail of
/// Compose's own error text when a helper command failed. No environment
/// values, no container logs. Containers are left in place, and the
/// message says how to inspect and clean them up.
fn startup_failure_message(
    check: &StackStartupCheck<'_>,
    timeout: u64,
    verdict: &Verdict,
) -> String {
    let mut message = format!(
        "Stack `{}` (compose project `{}`) did not become ready within {timeout}s:\n",
        check.repo, check.project_name
    );
    for (service, reason) in &verdict.failed {
        message.push_str(&format!("  - {service}: {reason}\n"));
    }
    for (service, reason) in &verdict.pending {
        message.push_str(&format!("  - {service}: not ready ({reason})\n"));
    }
    if !verdict.ready.is_empty() {
        let ready = verdict
            .ready
            .iter()
            .map(|(service, label)| format!("{service} {label}"))
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!("  Already ready: {ready}\n"));
    }
    message.push_str(&format!(
        "Containers were left in place for diagnosis; their logs may contain secrets, so they are not shown here. \
Inspect with `docker compose -p {} ps -a` and `docker compose -p {} logs <service>`, then clean up with `knit run down`.",
        check.project_name, check.project_name
    ));
    message
}

/// The startup budget, floored at one second. The caller validates the
/// configured value; the floor only stops a zero from expiring instantly.
fn startup_timeout(configured: u64) -> u64 {
    configured.max(1)
}

/// A bounded tail of a command's stderr: recent Compose errors name the
/// failing container last, and the tail stays free of any value knit
/// injected (those never appear in Compose's own messages).
fn bounded_tail(text: &str) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= STDERR_TAIL_CHARS {
        return trimmed.to_string();
    }
    let tail: String = trimmed.chars().skip(count - STDERR_TAIL_CHARS).collect();
    format!("…{tail}")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        service: &str,
        state: &str,
        exit_code: Option<i64>,
        health: Option<&str>,
    ) -> ServiceStatus {
        ServiceStatus {
            service: service.to_string(),
            state: state.to_string(),
            exit_code,
            health: health.map(str::to_string),
        }
    }

    /// A minimal model builder: `(service, completed_job, healthchecked,
    /// replicas)`.
    fn model(entries: &[(&str, bool, bool, u64)]) -> StackModel {
        StackModel {
            expected: entries
                .iter()
                .map(|(service, completed_job, healthchecked, replicas)| {
                    (
                        service.to_string(),
                        ServiceExpectation {
                            completed_job: *completed_job,
                            healthchecked: *healthchecked,
                            replicas: *replicas,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn parse_reads_array_shape() {
        let array = r#"[
            {"Service":"api","State":"running","ExitCode":0,"Health":"healthy","Labels":"com.docker.compose.oneoff=False"},
            {"Service":"migrate-job","State":"exited","ExitCode":0,"Health":"","Labels":""}
        ]"#;
        assert_eq!(
            parse_ps_services(array),
            vec![
                status("api", "running", Some(0), Some("healthy")),
                status("migrate-job", "exited", Some(0), None),
            ]
        );
    }

    #[test]
    fn parse_reads_ndjson_shape() {
        let ndjson = "{\"Service\":\"api\",\"State\":\"running\",\"ExitCode\":0,\"Health\":\"\"}\n\
                      {\"Service\":\"db\",\"State\":\"exited\",\"ExitCode\":1,\"Health\":\"\"}\n";
        assert_eq!(
            parse_ps_services(ndjson),
            vec![
                status("api", "running", Some(0), None),
                status("db", "exited", Some(1), None),
            ]
        );
    }

    #[test]
    fn parse_reads_single_object_and_lowercase_keys() {
        let single =
            "{\"service\":\"api\",\"state\":\"restarting\",\"exitcode\":0,\"health\":\"none\"}";
        assert_eq!(
            parse_ps_services(single),
            vec![status("api", "restarting", Some(0), None)]
        );
    }

    #[test]
    fn parse_skips_malformed_lines_and_one_off_containers() {
        let text = "not json at all\n\
                    {\"Service\":\"api\",\"State\":\"running\",\"ExitCode\":0}\n\
                    {\"Service\":\"leftover\",\"State\":\"exited\",\"ExitCode\":5,\"Labels\":\"com.docker.compose.oneoff=True\"}\n\
                    {\"Name\":\"keyless\"}\n";
        assert_eq!(
            parse_ps_services(text),
            vec![status("api", "running", Some(0), None)]
        );
    }

    #[test]
    fn parse_accepts_empty_output() {
        assert!(parse_ps_services("").is_empty());
        assert!(parse_ps_services("\n").is_empty());
        assert!(parse_ps_services("[]\n").is_empty());
    }

    fn demo_config() -> Value {
        serde_json::json!({
            "name": "demo-stack",
            "services": {
                "api": {
                    "image": "demo-api",
                    "healthcheck": {"test": ["CMD", "true"], "interval": "2s"},
                    "depends_on": {
                        "migrate-job": {"condition": "service_completed_successfully", "required": true},
                        "db": {"condition": "service_healthy", "required": true}
                    }
                },
                "migrate-job": {"image": "demo-migrate"},
                "init-job": {"image": "demo-init"},
                "legacy-api": {
                    "image": "demo-legacy",
                    "depends_on": [
                        {"Name": "init-job", "Condition": "service_completed_successfully"}
                    ]
                },
                "seed": {"image": "demo-seed", "depends_on": ["db"]},
                "db": {"image": "demo-db"},
                "tools": {"image": "demo-tools", "profiles": ["tools"]},
                "zeroed": {"image": "demo-zero", "scale": 0},
                "replica-zeroed": {"image": "demo-zero", "deploy": {"replicas": 0}},
                "pair": {"image": "demo-pair", "scale": 2},
                "fleet": {"image": "demo-fleet", "deploy": {"replicas": 3}},
                "no-hc": {"image": "demo-x", "healthcheck": {"disable": true}},
                "none-hc": {"image": "demo-x", "healthcheck": {"test": ["NONE"]}}
            }
        })
    }

    #[test]
    fn model_trusts_resolved_services_and_excludes_only_zero_scales() {
        let model = parse_stack_model(&demo_config());
        // Compose already resolved the active set: a retained service is
        // expected even though it still lists profiles.
        assert!(model.expected.contains_key("tools"));
        assert!(!model.expected.contains_key("zeroed"));
        assert!(!model.expected.contains_key("replica-zeroed"));
        assert!(model.expected.contains_key("api"));
    }

    #[test]
    fn model_derives_completed_jobs_from_dependency_conditions() {
        let model = parse_stack_model(&demo_config());
        // Only service_completed_successfully marks a job — healthy and
        // started conditions and the plain short form do not.
        assert!(model.expected["migrate-job"].completed_job);
        assert!(model.expected["init-job"].completed_job);
        assert!(!model.expected["db"].completed_job);
        assert!(!model.expected["api"].completed_job);
        assert!(!model.expected["seed"].completed_job);
    }

    #[test]
    fn model_preserves_expected_replica_counts() {
        let model = parse_stack_model(&demo_config());
        assert_eq!(model.expected["api"].replicas, 1);
        assert_eq!(model.expected["pair"].replicas, 2);
        assert_eq!(model.expected["fleet"].replicas, 3);
    }

    #[test]
    fn model_marks_healthchecks_unless_disabled() {
        let model = parse_stack_model(&demo_config());
        assert!(model.expected["api"].healthchecked);
        assert!(!model.expected["db"].healthchecked);
        assert!(!model.expected["no-hc"].healthchecked);
        assert!(!model.expected["none-hc"].healthchecked);
    }

    #[test]
    fn running_and_healthy_services_are_ready() {
        let model = model(&[("api", false, true, 1), ("db", false, false, 1)]);
        let verdict = evaluate(
            &model,
            &[
                status("api", "running", Some(0), Some("healthy")),
                status("db", "running", Some(0), None),
            ],
        );
        assert!(verdict.is_ready());
        assert_eq!(
            verdict.ready,
            vec![
                ("api".to_string(), "running (healthy)".to_string()),
                ("db".to_string(), "running".to_string()),
            ]
        );
    }

    #[test]
    fn healthchecked_service_that_only_runs_is_not_ready() {
        let model = model(&[("api", false, true, 1)]);
        let verdict = evaluate(&model, &[status("api", "running", Some(0), None)]);
        assert!(!verdict.is_ready());
        assert_eq!(
            verdict.pending,
            vec![("api".to_string(), "health status not reported".to_string())]
        );
    }

    #[test]
    fn health_starting_is_pending_and_unhealthy_fails() {
        let model = model(&[("api", false, true, 1)]);
        let verdict = evaluate(
            &model,
            &[status("api", "running", Some(0), Some("starting"))],
        );
        assert!(!verdict.is_ready());
        assert!(verdict.failed.is_empty());
        assert_eq!(
            verdict.pending,
            vec![("api".to_string(), "health check starting".to_string())]
        );
        let verdict = evaluate(
            &model,
            &[status("api", "running", Some(0), Some("unhealthy"))],
        );
        assert_eq!(
            verdict.failed,
            vec![("api".to_string(), "unhealthy".to_string())]
        );
    }

    #[test]
    fn declared_completed_job_exit_zero_is_ready() {
        let model = model(&[("migrate-job", true, false, 1), ("api", false, true, 1)]);
        let verdict = evaluate(
            &model,
            &[
                status("migrate-job", "exited", Some(0), None),
                status("api", "running", Some(0), Some("healthy")),
            ],
        );
        assert!(verdict.is_ready());
        assert!(verdict
            .ready
            .contains(&("migrate-job".to_string(), "exited (0)".to_string())));
    }

    #[test]
    fn declared_completed_job_still_running_is_pending() {
        let model = model(&[("migrate-job", true, false, 1)]);
        let verdict = evaluate(&model, &[status("migrate-job", "running", Some(0), None)]);
        assert!(!verdict.is_ready());
        assert!(verdict.failed.is_empty());
        assert_eq!(
            verdict.pending,
            vec![("migrate-job".to_string(), "job still running".to_string())]
        );
    }

    #[test]
    fn declared_completed_job_exit_nonzero_fails() {
        let model = model(&[("migrate-job", true, false, 1)]);
        let verdict = evaluate(&model, &[status("migrate-job", "exited", Some(1), None)]);
        assert_eq!(
            verdict.failed,
            vec![("migrate-job".to_string(), "exited (code 1)".to_string())]
        );
    }

    #[test]
    fn undeclared_exit_zero_fails() {
        let model = model(&[("api", false, true, 1)]);
        let verdict = evaluate(&model, &[status("api", "exited", Some(0), None)]);
        let reason = &verdict.failed[0].1;
        assert!(reason.contains("a declared completed job"), "{reason}");
        assert!(
            reason.contains("service_completed_successfully"),
            "{reason}"
        );
    }

    #[test]
    fn exited_nonzero_restarting_and_unknown_states_fail() {
        let model = model(&[
            ("api", false, false, 1),
            ("db", false, false, 1),
            ("worker", false, false, 1),
        ]);
        let verdict = evaluate(
            &model,
            &[
                status("api", "exited", Some(1), None),
                status("db", "restarting", Some(1), None),
                status("worker", "paused", Some(0), None),
            ],
        );
        assert_eq!(
            verdict.failed,
            vec![
                ("api".to_string(), "exited (code 1)".to_string()),
                ("db".to_string(), "restarting (crash loop)".to_string()),
                ("worker".to_string(), "paused".to_string()),
            ]
        );
    }

    #[test]
    fn created_is_pending() {
        let model = model(&[("worker", false, false, 1)]);
        let verdict = evaluate(&model, &[status("worker", "created", None, None)]);
        assert!(verdict.failed.is_empty());
        assert_eq!(
            verdict.pending,
            vec![("worker".to_string(), "created but not started".to_string())]
        );
    }

    #[test]
    fn missing_expected_service_is_pending_never_success() {
        let model = model(&[("api", false, false, 1), ("db", false, false, 1)]);
        let verdict = evaluate(&model, &[status("api", "running", Some(0), None)]);
        assert!(!verdict.is_ready());
        assert_eq!(
            verdict.pending,
            vec![("db".to_string(), "no container found".to_string())]
        );
    }

    #[test]
    fn empty_or_malformed_ps_is_never_success() {
        let model = model(&[("api", false, false, 1)]);
        assert!(!evaluate(&model, &[]).is_ready());
        assert!(!evaluate(&model, &parse_ps_services("not json")).is_ready());
        assert!(!evaluate(&model, &parse_ps_services("")).is_ready());
    }

    #[test]
    fn fewer_containers_than_replicas_is_pending() {
        let model = model(&[("api", false, false, 2)]);
        let verdict = evaluate(&model, &[status("api", "running", Some(0), None)]);
        assert!(!verdict.is_ready());
        assert!(verdict.failed.is_empty());
        assert_eq!(
            verdict.pending,
            vec![(
                "api".to_string(),
                "only 1 of 2 containers present".to_string()
            )]
        );
    }

    #[test]
    fn full_replica_set_is_ready_and_extras_cannot_rescue_a_failure() {
        let model = model(&[("api", false, false, 2)]);
        let verdict = evaluate(
            &model,
            &[
                status("api", "running", Some(0), None),
                status("api", "running", Some(0), None),
            ],
        );
        assert!(verdict.is_ready());
        // A third container that failed fails the service even though the
        // declared pair is running.
        let verdict = evaluate(
            &model,
            &[
                status("api", "running", Some(0), None),
                status("api", "running", Some(0), None),
                status("api", "exited", Some(1), None),
            ],
        );
        assert_eq!(
            verdict.failed,
            vec![("api".to_string(), "exited (code 1)".to_string())]
        );
    }

    #[test]
    fn short_replica_set_with_a_failed_container_fails() {
        let model = model(&[("api", false, false, 2)]);
        let verdict = evaluate(
            &model,
            &[
                status("api", "running", Some(0), None),
                status("api", "exited", Some(1), None),
            ],
        );
        assert_eq!(
            verdict.failed,
            vec![("api".to_string(), "exited (code 1)".to_string())]
        );
    }

    fn check<'a>() -> StackStartupCheck<'a> {
        static NO_ENV: std::sync::LazyLock<BTreeMap<String, String>> =
            std::sync::LazyLock::new(BTreeMap::new);
        StackStartupCheck {
            repo: "demo",
            project_name: "demo-stack",
            compose_file: Path::new("/tmp/demo/docker-compose.yml"),
            profiles: &[],
            env: &NO_ENV,
            checkout: Path::new("/tmp/demo"),
            timeout_seconds: 120,
        }
    }

    #[test]
    fn failure_message_reports_repo_statuses_and_diagnosis_safely() {
        let model = model(&[
            ("api", false, true, 1),
            ("db", false, false, 1),
            ("migrate-job", true, false, 1),
        ]);
        let verdict = evaluate(
            &model,
            &[
                status("api", "exited", Some(1), None),
                status("db", "running", Some(0), Some("starting")),
                status("migrate-job", "exited", Some(0), None),
            ],
        );
        let message = startup_failure_message(&check(), 120, &verdict);
        assert!(message.contains("Stack `demo`"), "{message}");
        assert!(message.contains("`demo-stack`"), "{message}");
        assert!(message.contains("api: exited (code 1)"), "{message}");
        assert!(
            message.contains("db: not ready (health check starting)"),
            "{message}"
        );
        assert!(
            message.contains("Already ready: migrate-job exited (0)"),
            "{message}"
        );
        assert!(
            message.contains("docker compose -p demo-stack ps -a"),
            "{message}"
        );
        assert!(message.contains("knit run down"), "{message}");
        assert!(message.contains("secrets"), "{message}");
    }

    #[test]
    fn bounded_tail_keeps_short_text_and_trims_long_text_from_the_front() {
        assert_eq!(bounded_tail("  fine \n"), "fine");
        let long = "x".repeat(STDERR_TAIL_CHARS + 500);
        let tail = bounded_tail(&long);
        assert!(tail.starts_with('…'), "{tail}");
        assert_eq!(tail.chars().count(), STDERR_TAIL_CHARS + 1);
    }

    #[test]
    fn config_and_ps_args_are_read_only() {
        assert_eq!(config_args(), vec!["config", "--format", "json"]);
        assert_eq!(
            ps_args(),
            vec!["ps", "--all", "--orphans=false", "--format", "json"]
        );
    }

    #[test]
    fn startup_timeout_floors_at_one_second() {
        assert_eq!(startup_timeout(120), 120);
        assert_eq!(startup_timeout(0), 1);
    }

    #[test]
    fn report_summary_lists_every_service() {
        let report = StackStartupReport {
            services: vec![
                ("api".to_string(), "running (healthy)".to_string()),
                ("migrate-job".to_string(), "exited (0)".to_string()),
            ],
        };
        assert_eq!(
            report.summary(),
            "api running (healthy), migrate-job exited (0)"
        );
    }

    #[cfg(unix)]
    fn spawn_probe(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    fn spawn_probe_grouped(script: &str) -> Child {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.process_group(0);
        command.spawn().unwrap()
    }

    /// Whether the process group still has living members, via the
    /// `kill -0` existence probe (no libc dependency).
    #[cfg(unix)]
    fn group_alive(group_id: u32) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(format!("-{group_id}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn bounded_wait_captures_a_finished_process() {
        let child = spawn_probe("echo out; echo err >&2; exit 3");
        let output = wait_bounded(child, "probe", Instant::now() + Duration::from_secs(30))
            .expect("finished process must not hit the deadline");
        assert!(!output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_wait_kills_a_stuck_process_at_the_deadline() {
        let child = spawn_probe("sleep 30");
        let error = wait_bounded(child, "probe", Instant::now())
            .expect_err("a process stuck past the deadline must fail bounded");
        let message = error.to_string();
        assert!(
            message.contains("did not finish within the startup budget"),
            "{message}"
        );
        assert!(message.contains("probe"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_wait_returns_and_kills_group_when_a_descendant_holds_stdout() {
        // The direct child exits and is reaped, but its backgrounded
        // descendant inherits the stdout pipe and keeps it open: reading
        // without a bound would hang for the descendant's lifetime — and
        // the descendant itself must not outlive the failed capture.
        let child = spawn_probe_grouped("sleep 30 & exit 0");
        let group_id = child.id();
        let error = wait_bounded(child, "probe", Instant::now() + Duration::from_secs(2))
            .expect_err("a descendant holding the pipe must fail bounded, not hang");
        let message = error.to_string();
        assert!(
            message.contains("failed to capture probe stdout within the startup budget"),
            "{message}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(group_id) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !group_alive(group_id),
            "the capture timeout must kill the descendant's process group"
        );
    }
}
