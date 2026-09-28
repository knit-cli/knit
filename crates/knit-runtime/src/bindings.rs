//! Explicit endpoint bindings: named consumer keys pinned to named target
//! endpoints, rewired to the bundle's allocated ports.
//!
//! Automatic port rewiring (transform phase 1 and the cross-stack wiring of
//! phase 2) is heuristic and bails out when duplicate source host ports
//! make a reference's destination ambiguous. A `runtime.bindings` entry
//! resolves that ambiguity by name: this consumer repo's service, this
//! environment key or build arg, holds a single loopback-host endpoint
//! (`localhost:<port>`, `127.0.0.1:<port>`, `host.docker.internal:<port>`)
//! that belongs to that target repo's service.
//!
//! The module has two halves, split so `up` can interleave them with the
//! existing phases:
//!
//! - [`capture_binding_values`] runs on the RESOLVED compose configs,
//!   before any port rewriting. It validates every active binding — the
//!   key exists, is a string, and holds exactly one supported endpoint to
//!   the target's source published port — and records the ORIGINAL value.
//!   Bindings whose consumer repo is not part of the run are skipped
//!   (narrow bundles legitimately lack sibling repos); an active consumer
//!   whose target is missing fails here, before anything starts.
//! - [`apply_binding_values`] runs on the FINAL configs, after automatic
//!   cross-stack wiring. Each bound value is rebuilt from the captured
//!   original with only the port replaced by the target's allocated host
//!   port, so automatic rewrites in between can neither cascade remaps
//!   through a bound key nor collide with the consumer's own ports.
//!
//! Both halves share [`transform::port_reference_spans`] — the exact
//! reference scanner the automatic rewriter and the collector use — so a
//! binding, the collector, and the rewriter can never disagree about what
//! an endpoint reference is.
//!
//! Diagnostics name repos, services, and keys, never environment values:
//! bound values are connection strings that routinely carry credentials.

use crate::config::{RuntimeBinding, RuntimeEndpoint};
use crate::transform::{self, ServicePort};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One published port of a `(repo, service)` endpoint, as the registry
/// sees it: the allocated host port, the container-side port, and the
/// source host port it replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegistryPort {
    pub host: u16,
    pub container: Option<u16>,
    pub source_host: Option<u16>,
}

/// The complete published-endpoint registry of a run, keyed by
/// `(repo, service)`. Built once from every stack — resolved configs at
/// capture time, allocated [`ServicePort`]s at apply time — so bindings
/// resolve names no matter which stack publishes them.
pub(crate) struct EndpointRegistry {
    ports: BTreeMap<(String, String), Vec<RegistryPort>>,
}

impl EndpointRegistry {
    /// Registry of the endpoints each resolved compose config publishes,
    /// before allocation: host and source host are the same published port.
    pub(crate) fn from_configs(configs: &[(String, &Value)]) -> Self {
        let mut ports: BTreeMap<(String, String), Vec<RegistryPort>> = BTreeMap::new();
        for (repo, config) in configs {
            let Some(services) = config.get("services").and_then(Value::as_object) else {
                continue;
            };
            for (service, entry) in services {
                let Some(published) = entry.get("ports").and_then(Value::as_array) else {
                    continue;
                };
                for port in published {
                    let Some(found) = published_host_port(port) else {
                        continue;
                    };
                    ports
                        .entry((repo.clone(), service.clone()))
                        .or_default()
                        .push(RegistryPort {
                            host: found.published,
                            container: found.container,
                            source_host: Some(found.published),
                        });
                }
            }
        }
        Self { ports }
    }

    /// Registry of the endpoints each stack finally publishes, after
    /// allocation.
    pub(crate) fn from_service_ports(stacks: &[(String, &[ServicePort])]) -> Self {
        let mut ports: BTreeMap<(String, String), Vec<RegistryPort>> = BTreeMap::new();
        for (repo, service_ports) in stacks {
            for port in *service_ports {
                ports
                    .entry((repo.clone(), port.service.clone()))
                    .or_default()
                    .push(RegistryPort {
                        host: port.host,
                        container: port.container,
                        source_host: port.source_host,
                    });
            }
        }
        Self { ports }
    }

    /// Resolve a binding target to its single published port entry. The
    /// container-port disambiguator is mandatory when the service
    /// publishes more than one host port.
    fn resolve(&self, target: &RuntimeEndpoint) -> Result<RegistryPort> {
        let entries = self
            .ports
            .get(&(target.repo.clone(), target.service.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "target repo `{}` service `{}` publishes no host ports",
                    target.repo,
                    target.service
                )
            })?;
        let matched: Vec<RegistryPort> = match target.port {
            Some(container) => entries
                .iter()
                .copied()
                .filter(|entry| entry.container == Some(container))
                .collect(),
            None => {
                if entries.len() > 1 {
                    bail!(
                        "target repo `{}` service `{}` publishes multiple host ports; set \
                         `target.port` to the container port to bind to",
                        target.repo,
                        target.service
                    );
                }
                entries.clone()
            }
        };
        match matched.as_slice() {
            [one] => Ok(*one),
            [] => bail!(
                "target repo `{}` service `{}` publishes no container port {}",
                target.repo,
                target.service,
                target.port.unwrap_or_default()
            ),
            _ => bail!(
                "target repo `{}` service `{}` publishes container port {} on multiple host \
                 ports; the binding cannot tell them apart",
                target.repo,
                target.service,
                target.port.unwrap_or_default()
            ),
        }
    }
}

/// The published host port of one compose `ports` entry, long or short
/// syntax, with its container-side port when the entry states one.
struct FoundPort {
    published: u16,
    container: Option<u16>,
}

fn published_host_port(entry: &Value) -> Option<FoundPort> {
    match entry {
        Value::Object(port) => {
            let published = match port.get("published")? {
                Value::String(text) => text.parse::<u16>().ok()?,
                Value::Number(number) => number.as_u64().and_then(|n| u16::try_from(n).ok())?,
                _ => return None,
            };
            let container = port
                .get("target")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok());
            Some(FoundPort {
                published,
                container,
            })
        }
        // Short syntax "HOST:CONTAINER[/proto]".
        Value::String(text) => {
            let (host, rest) = text.split_once(':')?;
            let published = host.parse::<u16>().ok()?;
            let container = rest
                .split('/')
                .next()
                .and_then(|part| part.parse::<u16>().ok());
            Some(FoundPort {
                published,
                container,
            })
        }
        _ => None,
    }
}

/// Where a bound value lives: the consumer repo, service, and key. Used
/// both for diagnostics and so `up` can exclude bound keys from the
/// ambiguity guard — a bound key's destination is decided here, not by
/// cross-stack wiring.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BindingLocation {
    pub repo: String,
    pub service: String,
    /// `"environment"` or `"build args"`, matching the reference
    /// collector's field labels.
    pub field: &'static str,
    pub key: String,
}

impl BindingLocation {
    /// Human label for diagnostics: names only, never values.
    fn describe(&self) -> String {
        match self.field {
            "environment" => format!(
                "repo `{}` service `{}` environment key `{}`",
                self.repo, self.service, self.key
            ),
            _ => format!(
                "repo `{}` service `{}` build arg `{}`",
                self.repo, self.service, self.key
            ),
        }
    }
}

/// The target side a captured binding must land on.
#[derive(Clone)]
struct BindingTarget {
    repo: String,
    service: String,
    /// Container-port disambiguator from the binding, if given.
    port: Option<u16>,
    /// The source (pre-allocation) host port the bound value references.
    source_host: u16,
}

/// One binding captured against the resolved configs: where the value
/// lives, where it must land, and the ORIGINAL value text. The original is
/// deliberately private and the type deliberately not `Debug`, so no
/// diagnostic path can echo a bound value (credentials live there).
pub(crate) struct CapturedBinding {
    location: BindingLocation,
    target: BindingTarget,
    original: String,
}

/// All bindings captured for a run.
pub(crate) struct CapturedBindings(Vec<CapturedBinding>);

impl CapturedBindings {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every bound location, for the caller's ambiguity-guard exclusion:
    /// these keys' destinations are decided by the bindings, not by
    /// cross-stack wiring.
    pub(crate) fn locations(&self) -> impl Iterator<Item = &BindingLocation> {
        self.0.iter().map(|binding| &binding.location)
    }
}

/// One applied binding, for the caller's plan report: where the value
/// lives, which target it landed on, and the allocated host port. Names
/// and ports only, never values.
#[derive(Debug)]
pub(crate) struct AppliedBinding {
    pub location: BindingLocation,
    pub target_repo: String,
    pub target_service: String,
    pub port: u16,
}

/// Static validation of binding configuration, independent of any compose
/// data: consumer and target names are non-empty, each binding selects
/// exactly one non-empty `environment`/`buildArg` key, the target's
/// container-port disambiguator is never 0, and no two bindings write the
/// same location (they would race). Runs for every binding, including ones
/// whose consumer repo is absent from a narrow bundle.
pub(crate) fn validate_bindings(bindings: &[RuntimeBinding]) -> Result<()> {
    let mut seen: BTreeSet<(String, String, &'static str, String)> = BTreeSet::new();
    for binding in bindings {
        if binding.repo.is_empty() {
            bail!("runtime binding `repo` must not be empty");
        }
        if binding.service.is_empty() {
            bail!(
                "runtime binding for repo `{}` has an empty `service`",
                binding.repo
            );
        }
        let selection = match (&binding.environment, &binding.build_arg) {
            (Some(_), Some(_)) => {
                bail!(
                    "runtime binding for repo `{}` service `{}` selects both `environment` and \
                     `buildArg`; exactly one is allowed",
                    binding.repo,
                    binding.service
                );
            }
            (None, None) => {
                bail!(
                    "runtime binding for repo `{}` service `{}` selects neither `environment` \
                     nor `buildArg`; exactly one is required",
                    binding.repo,
                    binding.service
                );
            }
            (Some(key), None) => {
                if key.is_empty() {
                    bail!(
                        "runtime binding for repo `{}` service `{}` selects an empty \
                         `environment` key name",
                        binding.repo,
                        binding.service
                    );
                }
                ("environment", key.as_str())
            }
            (None, Some(key)) => {
                if key.is_empty() {
                    bail!(
                        "runtime binding for repo `{}` service `{}` selects an empty `buildArg` \
                         key name",
                        binding.repo,
                        binding.service
                    );
                }
                ("build args", key.as_str())
            }
        };
        if binding.target.repo.is_empty() {
            bail!(
                "runtime binding for repo `{}` service `{}` has a target with an empty `repo`",
                binding.repo,
                binding.service
            );
        }
        if binding.target.service.is_empty() {
            bail!(
                "runtime binding for repo `{}` service `{}` has a target with an empty \
                 `service`",
                binding.repo,
                binding.service
            );
        }
        if binding.target.port == Some(0) {
            bail!(
                "runtime binding for repo `{}` service `{}` declares `target.port` 0; \
                 container ports start at 1",
                binding.repo,
                binding.service
            );
        }
        if !seen.insert((
            binding.repo.clone(),
            binding.service.clone(),
            selection.0,
            selection.1.to_string(),
        )) {
            bail!(
                "runtime bindings for repo `{}` service `{}` define `{}` more than once",
                binding.repo,
                binding.service,
                selection.1
            );
        }
    }
    Ok(())
}

/// Capture the bound values from the RESOLVED compose configs, before any
/// port rewriting. For every binding whose consumer repo is part of the
/// run, the bound key must exist, be a string, and hold exactly one
/// supported endpoint reference — to the target's source published port.
/// Bindings whose consumer repo is absent are skipped: a narrow bundle
/// legitimately lacks sibling repos. Any active binding that cannot be
/// satisfied is an error here, before any stack starts.
pub(crate) fn capture_binding_values(
    bindings: &[RuntimeBinding],
    configs: &[(String, &Value)],
) -> Result<CapturedBindings> {
    validate_bindings(bindings)?;
    let registry = EndpointRegistry::from_configs(configs);
    let present: BTreeMap<&str, &Value> = configs
        .iter()
        .map(|(repo, config)| (repo.as_str(), *config))
        .collect();

    let mut captured = Vec::new();
    for binding in bindings {
        let Some(config) = present.get(binding.repo.as_str()) else {
            // Absent consumer repo: skip for narrow bundles.
            continue;
        };
        let (field, key) = match (&binding.environment, &binding.build_arg) {
            (Some(key), None) => ("environment", key.clone()),
            (None, Some(key)) => ("build args", key.clone()),
            // validate_bindings rejected every other combination.
            _ => unreachable!("binding selection validated above"),
        };
        let location = BindingLocation {
            repo: binding.repo.clone(),
            service: binding.service.clone(),
            field,
            key: key.clone(),
        };
        let context = format!("endpoint binding for {}: ", location.describe());

        let Some(value) = bound_value(config, &binding.service, field, &key) else {
            bail!("{context}the service does not define that key");
        };
        let Some(text) = value.as_str() else {
            bail!("{context}the value is not a string");
        };

        let target_port = registry
            .resolve(&binding.target)
            .map_err(|error| anyhow!("{context}{error}"))?;
        let source_host = target_port.source_host.unwrap_or(target_port.host);

        let spans = transform::port_reference_spans(text);
        let ports: BTreeSet<u16> = spans.iter().map(|span| span.port).collect();
        match ports.len() {
            0 => bail!(
                "{context}holds no supported endpoint reference (expected one of \
                 localhost:<port>, 127.0.0.1:<port>, host.docker.internal:<port>)"
            ),
            1 if ports.contains(&source_host) => {}
            1 => bail!(
                "{context}references source host port {}, but target repo `{}` service `{}` \
                 publishes source host port {}",
                ports.iter().next().copied().unwrap_or_default(),
                binding.target.repo,
                binding.target.service,
                source_host
            ),
            _ => bail!(
                "{context}references multiple endpoint ports ({}); a bound key must hold a \
                 single endpoint",
                ports
                    .iter()
                    .map(|port| port.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }

        captured.push(CapturedBinding {
            location,
            target: BindingTarget {
                repo: binding.target.repo.clone(),
                service: binding.target.service.clone(),
                port: binding.target.port,
                source_host,
            },
            original: text.to_string(),
        });
    }
    Ok(CapturedBindings(captured))
}

/// Apply captured bindings to the FINAL configs, after automatic
/// cross-stack wiring. Each bound value is rebuilt from the captured
/// ORIGINAL with only the port digits replaced by the target's allocated
/// host port — scheme, host, path, query, and userinfo are preserved
/// verbatim — so rewrites that happened in between are overwritten rather
/// than compounded. Fails (before anything starts) when an active
/// consumer's target is missing from the final registry or a bound
/// location no longer exists. Returns one entry per applied binding for
/// the caller's plan report.
pub(crate) fn apply_binding_values(
    captured: &CapturedBindings,
    registry: &EndpointRegistry,
    configs: &mut [(String, &mut Value)],
) -> Result<Vec<AppliedBinding>> {
    let mut report = Vec::new();
    for binding in &captured.0 {
        let context = format!("endpoint binding for {}: ", binding.location.describe());
        let final_port = registry
            .resolve(&RuntimeEndpoint {
                repo: binding.target.repo.clone(),
                service: binding.target.service.clone(),
                port: binding.target.port,
            })
            .map_err(|error| anyhow!("{context}{error}"))?;

        let Some((_, config)) = configs
            .iter_mut()
            .find(|(repo, _)| *repo == binding.location.repo)
        else {
            bail!(
                "{context}repo `{}` is no longer part of the run",
                binding.location.repo
            );
        };
        let Some(slot) = bound_value_mut(
            config,
            &binding.location.service,
            binding.location.field,
            &binding.location.key,
        ) else {
            bail!("{context}the service no longer defines that key");
        };
        let remap = BTreeMap::from([(binding.target.source_host, final_port.host)]);
        let rewritten = transform::rewrite_port_text(&binding.original, &remap)
            .ok_or_else(|| anyhow!("{context}no endpoint reference to rewrite"))?;
        *slot = Value::String(rewritten);
        report.push(AppliedBinding {
            location: binding.location.clone(),
            target_repo: binding.target.repo.clone(),
            target_service: binding.target.service.clone(),
            port: final_port.host,
        });
    }
    Ok(report)
}

/// Read one bound value out of a resolved compose config.
fn bound_value<'a>(
    config: &'a Value,
    service: &str,
    field: &'static str,
    key: &str,
) -> Option<&'a Value> {
    let service = config.get("services")?.get(service)?;
    match field {
        "environment" => service.get("environment")?.get(key),
        _ => service.get("build")?.get("args")?.get(key),
    }
}

/// Mutable variant of [`bound_value`], for writing the final value in.
fn bound_value_mut<'a>(
    config: &'a mut Value,
    service: &str,
    field: &'static str,
    key: &str,
) -> Option<&'a mut Value> {
    let service = config.get_mut("services")?.get_mut(service)?;
    match field {
        "environment" => service.get_mut("environment")?.get_mut(key),
        _ => service.get_mut("build")?.get_mut("args")?.get_mut(key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProjectRuntime, RuntimeEndpoint};
    use serde_json::json;

    fn env_binding(
        repo: &str,
        service: &str,
        key: &str,
        target: RuntimeEndpoint,
    ) -> RuntimeBinding {
        RuntimeBinding {
            repo: repo.to_string(),
            service: service.to_string(),
            environment: Some(key.to_string()),
            build_arg: None,
            target,
        }
    }

    fn endpoint(repo: &str, service: &str, port: Option<u16>) -> RuntimeEndpoint {
        RuntimeEndpoint {
            repo: repo.to_string(),
            service: service.to_string(),
            port,
        }
    }

    fn binding_config(repo: &str) -> (String, Value) {
        let config = match repo {
            "web" => json!({
                "services": {
                    "app": {
                        "environment": {
                            "APP_API_URL": "http://localhost:8000",
                            "SEARCH_URL": "http://127.0.0.1:8100/search?q=demo",
                            "DB_URL": "postgres://demo-user:demo-pass@127.0.0.1:8000/demo?pool=true",
                            "OTHER_HOST": "http://api.localhost:8000",
                            "COUNT": 7
                        },
                        "build": {"args": {"API_ORIGIN": "http://localhost:8000"}}
                    }
                }
            }),
            "api-a" => json!({
                "services": {
                    "api": {
                        "environment": {"LISTEN": "0.0.0.0"},
                        "ports": [{"target": 8000, "published": "8000"}]
                    }
                }
            }),
            "api-b" => json!({
                "services": {
                    "api": {
                        "environment": {"LISTEN": "0.0.0.0"},
                        "ports": [{"target": 8000, "published": "8000"}]
                    }
                }
            }),
            "edge" => json!({
                "services": {
                    "hub": {
                        "ports": [
                            {"target": 8000, "published": "8000"},
                            {"target": 8100, "published": "8100"}
                        ]
                    }
                }
            }),
            "worker" => json!({
                "services": {
                    "jobs": {
                        "environment": {"QUEUE_URL": "http://localhost:8000"},
                        "ports": ["9000:9000"]
                    }
                }
            }),
            _ => json!({"services": {}}),
        };
        (repo.to_string(), config)
    }

    fn resolved(repo: &str) -> (String, Value) {
        binding_config(repo)
    }

    fn capture_single(binding: RuntimeBinding, repos: &[&str]) -> Result<CapturedBindings> {
        let owned: Vec<(String, Value)> = repos.iter().map(|repo| resolved(repo)).collect();
        let configs: Vec<(String, &Value)> = owned
            .iter()
            .map(|(repo, config)| (repo.clone(), config))
            .collect();
        capture_binding_values(std::slice::from_ref(&binding), &configs)
    }

    /// Failure text without the success side's `Debug` bound:
    /// `CapturedBindings` deliberately has no `Debug` (it holds original
    /// bound values), so `expect_err` is not available for captures.
    fn error_text<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected the call to fail"),
            Err(error) => error.to_string(),
        }
    }

    fn capture_error(binding: RuntimeBinding, repos: &[&str]) -> String {
        error_text(capture_single(binding, repos))
    }

    #[test]
    fn validate_rejects_dual_and_missing_selection_and_duplicates() {
        let both = RuntimeBinding {
            repo: "web".into(),
            service: "app".into(),
            environment: Some("APP_API_URL".into()),
            build_arg: Some("API_ORIGIN".into()),
            target: endpoint("api-a", "api", None),
        };
        let error = validate_bindings(&[both]).unwrap_err().to_string();
        assert!(
            error.contains("both `environment` and `buildArg`"),
            "{error}"
        );

        let mut neither = env_binding("web", "app", "APP_API_URL", endpoint("api-a", "api", None));
        neither.environment = None;
        let error = validate_bindings(&[neither]).unwrap_err().to_string();
        assert!(error.contains("neither"), "{error}");

        let first = env_binding("web", "app", "APP_API_URL", endpoint("api-a", "api", None));
        let second = env_binding("web", "app", "APP_API_URL", endpoint("api-b", "api", None));
        let error = validate_bindings(&[first, second]).unwrap_err().to_string();
        assert!(error.contains("more than once"), "{error}");

        // An environment key and a build arg on the same service are
        // different locations and both validate.
        let mut split = env_binding("web", "app", "APP_API_URL", endpoint("api-a", "api", None));
        split.build_arg = Some("API_ORIGIN".into());
        split.environment = None;
        assert!(validate_bindings(&[
            env_binding("web", "app", "APP_API_URL", endpoint("api-a", "api", None)),
            split
        ])
        .is_ok());
    }

    #[test]
    fn validate_rejects_empty_names_and_zero_target_port() {
        let rejects = |binding: RuntimeBinding, expected: &str| {
            let error = validate_bindings(std::slice::from_ref(&binding))
                .expect_err("validation should fail")
                .to_string();
            assert!(error.contains(expected), "{error} lacked `{expected}`");
        };

        // Empty consumer repo.
        let mut binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", None));
        binding.repo = String::new();
        rejects(binding, "`repo` must not be empty");

        // Empty consumer service.
        let mut binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", None));
        binding.service = String::new();
        rejects(binding, "empty `service`");

        // Empty environment key name.
        let binding = env_binding("web", "app", "", endpoint("api", "api", None));
        rejects(binding, "empty `environment` key name");

        // Empty build arg key name.
        let mut binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", None));
        binding.environment = None;
        binding.build_arg = Some(String::new());
        rejects(binding, "empty `buildArg` key name");

        // Empty target repo and target service.
        let mut binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", None));
        binding.target.repo = String::new();
        rejects(binding, "target with an empty `repo`");
        let mut binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", None));
        binding.target.service = String::new();
        rejects(binding, "target with an empty `service`");

        // Zero container port: ports start at 1.
        let binding = env_binding("web", "app", "APP_API_URL", endpoint("api", "api", Some(0)));
        rejects(binding, "`target.port` 0");
    }

    #[test]
    fn duplicate_source_ports_across_services_bind_to_the_named_target() {
        // api-a and api-b both publish source host port 8000; the web
        // consumer points at localhost:8000. Automatic wiring cannot tell
        // the siblings apart; the binding names api-b, so api-b's
        // allocation wins — even if automatic rewrites already moved the
        // value somewhere else in between.
        let binding = env_binding("web", "app", "APP_API_URL", endpoint("api-b", "api", None));
        let captured = capture_single(binding, &["web", "api-a", "api-b"]).unwrap();
        assert_eq!(captured.locations().count(), 1);

        let mut config = resolved("web");
        // Simulate the intervening automatic rewrites: phase 1 mapped the
        // consumer's own 8000-shaped reference to api-a's port.
        config.1["services"]["app"]["environment"]["APP_API_URL"] = json!("http://localhost:9010");

        let registry = EndpointRegistry::from_service_ports(&[
            (
                "api-a".to_string(),
                &[ServicePort {
                    service: "api".to_string(),
                    host: 9010,
                    container: Some(8000),
                    source_host: Some(8000),
                }],
            ),
            (
                "api-b".to_string(),
                &[ServicePort {
                    service: "api".to_string(),
                    host: 9020,
                    container: Some(8000),
                    source_host: Some(8000),
                }],
            ),
        ]);
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        let report = apply_binding_values(&captured, &registry, &mut configs).unwrap();

        assert_eq!(
            config.1["services"]["app"]["environment"]["APP_API_URL"],
            "http://localhost:9020"
        );
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].port, 9020);
        assert_eq!(report[0].target_repo, "api-b");
        assert_eq!(report[0].location.key, "APP_API_URL");
    }

    #[test]
    fn capture_rejects_missing_target_missing_key_and_non_string() {
        // Active consumer, target repo absent from the run.
        let binding = env_binding(
            "web",
            "app",
            "APP_API_URL",
            endpoint("cache", "store", None),
        );
        let error = capture_error(binding, &["web"]);
        assert!(error.contains("`cache`"), "{error}");
        assert!(error.contains("publishes no host ports"), "{error}");

        // Key missing on the consumer service.
        let binding = env_binding("web", "app", "MISSING_KEY", endpoint("api-a", "api", None));
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("`MISSING_KEY`"), "{error}");
        assert!(error.contains("does not define that key"), "{error}");

        // Build args are located separately from environment keys.
        let mut binding = env_binding("web", "app", "API_ORIGIN", endpoint("api-a", "api", None));
        binding.environment = None;
        binding.build_arg = Some("NO_SUCH_ARG".into());
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("`NO_SUCH_ARG`"), "{error}");

        // Non-string values cannot hold references.
        let mut binding = env_binding("web", "app", "COUNT", endpoint("api-a", "api", None));
        binding.environment = Some("COUNT".into());
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("not a string"), "{error}");

        // Consumer service missing from an active repo.
        let binding = env_binding("web", "nope", "APP_API_URL", endpoint("api-a", "api", None));
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("`nope`"), "{error}");
    }

    #[test]
    fn capture_rejects_missing_nonmatching_and_ambiguous_references() {
        // No supported endpoint reference at all — an unrelated host
        // prefix is not one (exact-token matcher).
        let binding = env_binding("web", "app", "OTHER_HOST", endpoint("api-a", "api", None));
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("no supported endpoint reference"), "{error}");
        assert!(error.contains("`OTHER_HOST`"), "{error}");

        // Reference to a different port than the target publishes.
        let binding = env_binding("web", "app", "SEARCH_URL", endpoint("api-a", "api", None));
        let error = capture_error(binding, &["web", "api-a"]);
        assert!(error.contains("8100"), "{error}");
        assert!(error.contains("source host port 8000"), "{error}");

        // Multiple distinct endpoint ports in one bound value: ambiguous.
        let config = resolved("web");
        let mut with_pair = config.1.clone();
        with_pair["services"]["app"]["environment"]["PAIR"] =
            json!("http://localhost:8000 and http://localhost:8100");
        let api_a = resolved("api-a");
        let configs = vec![
            ("web".to_string(), &with_pair),
            ("api-a".to_string(), &api_a.1),
        ];
        let binding = env_binding("web", "app", "PAIR", endpoint("api-a", "api", None));
        let error = error_text(capture_binding_values(
            std::slice::from_ref(&binding),
            &configs,
        ));
        assert!(error.contains("multiple endpoint ports"), "{error}");
        assert!(error.contains("8000, 8100"), "{error}");
    }

    #[test]
    fn target_with_multiple_published_ports_requires_the_disambiguator() {
        // No disambiguator: ambiguous.
        let binding = env_binding("web", "app", "APP_API_URL", endpoint("edge", "hub", None));
        let error = capture_error(binding, &["web", "edge"]);
        assert!(error.contains("multiple host ports"), "{error}");
        assert!(error.contains("`target.port`"), "{error}");

        // Disambiguator selects the second published port.
        let binding = env_binding(
            "web",
            "app",
            "SEARCH_URL",
            endpoint("edge", "hub", Some(8100)),
        );
        let captured = capture_single(binding, &["web", "edge"]).unwrap();
        let registry = EndpointRegistry::from_service_ports(&[(
            "edge".to_string(),
            &[
                ServicePort {
                    service: "hub".to_string(),
                    host: 9000,
                    container: Some(8000),
                    source_host: Some(8000),
                },
                ServicePort {
                    service: "hub".to_string(),
                    host: 9100,
                    container: Some(8100),
                    source_host: Some(8100),
                },
            ],
        )]);
        let mut config = resolved("web");
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        let report = apply_binding_values(&captured, &registry, &mut configs).unwrap();
        assert_eq!(
            config.1["services"]["app"]["environment"]["SEARCH_URL"],
            "http://127.0.0.1:9100/search?q=demo"
        );
        assert_eq!(report[0].port, 9100);

        // Unknown container port: error naming it.
        let binding = env_binding(
            "web",
            "app",
            "APP_API_URL",
            endpoint("edge", "hub", Some(9999)),
        );
        let error = capture_error(binding, &["web", "edge"]);
        assert!(error.contains("9999"), "{error}");
    }

    #[test]
    fn bound_values_preserve_path_query_and_credentials() {
        // Only the port digits change; scheme, host, userinfo, path, and
        // query survive verbatim.
        let binding = env_binding("web", "app", "DB_URL", endpoint("api-b", "api", None));
        let captured = capture_single(binding, &["web", "api-a", "api-b"]).unwrap();
        let registry = EndpointRegistry::from_service_ports(&[(
            "api-b".to_string(),
            &[ServicePort {
                service: "api".to_string(),
                host: 9020,
                container: Some(8000),
                source_host: Some(8000),
            }],
        )]);
        let mut config = resolved("web");
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        apply_binding_values(&captured, &registry, &mut configs).unwrap();
        assert_eq!(
            config.1["services"]["app"]["environment"]["DB_URL"],
            "postgres://demo-user:demo-pass@127.0.0.1:9020/demo?pool=true"
        );
    }

    #[test]
    fn apply_rewrites_from_the_original_avoiding_chains_and_own_port_collisions() {
        // The target's allocated port collides with a port the consumer's
        // own stack publishes (and auto-rewrites): applying from the
        // captured original means the collision can never compound into a
        // second remap of an already-rewritten value.
        let binding = env_binding(
            "worker",
            "jobs",
            "QUEUE_URL",
            endpoint("api-b", "api", None),
        );
        let captured = capture_single(binding, &["worker", "api-b"]).unwrap();

        let mut config = resolved("worker");
        // Simulate the worst-case intermediate state: automatic wiring
        // already chained the value onto the consumer's own reallocated
        // port.
        config.1["services"]["jobs"]["environment"]["QUEUE_URL"] = json!("http://localhost:9020");

        let registry = EndpointRegistry::from_service_ports(&[
            (
                "worker".to_string(),
                &[ServicePort {
                    service: "jobs".to_string(),
                    host: 9030,
                    container: Some(9000),
                    source_host: Some(9000),
                }],
            ),
            (
                "api-b".to_string(),
                &[ServicePort {
                    service: "api".to_string(),
                    // Collides with the consumer's own old published port.
                    host: 9000,
                    container: Some(8000),
                    source_host: Some(8000),
                }],
            ),
        ]);
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        apply_binding_values(&captured, &registry, &mut configs).unwrap();
        // The bound key lands exactly on the target's allocation, built
        // from the original — not from the chained intermediate.
        assert_eq!(
            config.1["services"]["jobs"]["environment"]["QUEUE_URL"],
            "http://localhost:9000"
        );
    }

    #[test]
    fn apply_fails_when_the_final_target_or_location_disappears() {
        let binding = env_binding("web", "app", "APP_API_URL", endpoint("api-b", "api", None));
        let captured = capture_single(binding, &["web", "api-b"]).unwrap();

        // Final registry without the target: error before startup.
        let empty = EndpointRegistry::from_service_ports(&[]);
        let mut config = resolved("web");
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        let error = apply_binding_values(&captured, &empty, &mut configs)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`api-b`"), "{error}");

        // Final config without the bound location: error naming it.
        let registry = EndpointRegistry::from_service_ports(&[(
            "api-b".to_string(),
            &[ServicePort {
                service: "api".to_string(),
                host: 9020,
                container: Some(8000),
                source_host: Some(8000),
            }],
        )]);
        let mut stripped = json!({"services": {"app": {"environment": {}}}});
        let mut configs = vec![("web".to_string(), &mut stripped)];
        let error = apply_binding_values(&captured, &registry, &mut configs)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`APP_API_URL`"), "{error}");
    }

    #[test]
    fn absent_consumer_repo_is_skipped_for_narrow_bundles() {
        let binding = env_binding("web", "app", "APP_API_URL", endpoint("api-b", "api", None));
        let captured = capture_single(binding, &["api-b"]).unwrap();
        assert!(captured.is_empty());

        let mut config = resolved("web");
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        let registry = EndpointRegistry::from_service_ports(&[]);
        let report = apply_binding_values(&captured, &registry, &mut configs).unwrap();
        assert!(report.is_empty());
    }

    #[test]
    fn build_arg_bindings_apply_after_cross_wiring() {
        let mut binding = env_binding("web", "app", "", endpoint("api-a", "api", None));
        binding.environment = None;
        binding.build_arg = Some("API_ORIGIN".into());
        let captured = capture_single(binding, &["web", "api-a"]).unwrap();
        let registry = EndpointRegistry::from_service_ports(&[(
            "api-a".to_string(),
            &[ServicePort {
                service: "api".to_string(),
                host: 9010,
                container: Some(8000),
                source_host: Some(8000),
            }],
        )]);
        let mut config = resolved("web");
        let mut configs = vec![(config.0.clone(), &mut config.1)];
        let report = apply_binding_values(&captured, &registry, &mut configs).unwrap();
        assert_eq!(
            config.1["services"]["app"]["build"]["args"]["API_ORIGIN"],
            "http://localhost:9010"
        );
        assert_eq!(report[0].location.field, "build args");
    }

    #[test]
    fn project_runtime_bindings_deserialize_and_default_empty() {
        // Wire-level shape: camelCase `buildArg`, target port optional,
        // and a runtime without bindings round-trips without the key.
        let runtime: ProjectRuntime = serde_json::from_value(json!({
            "bindings": [
                {"repo": "web", "service": "app", "buildArg": "API_ORIGIN",
                 "target": {"repo": "api", "service": "api", "port": 8000}}
            ]
        }))
        .unwrap();
        assert_eq!(runtime.bindings.len(), 1);
        assert_eq!(runtime.bindings[0].build_arg.as_deref(), Some("API_ORIGIN"));
        assert_eq!(runtime.bindings[0].environment, None);
        assert_eq!(runtime.bindings[0].target.port, Some(8000));
        assert_eq!(runtime.startup_timeout_seconds, 120);

        let value = serde_json::to_value(ProjectRuntime::default()).unwrap();
        assert!(value.get("bindings").is_none());
        assert!(value.get("startupTimeoutSeconds").is_none());

        let database = serde_json::to_value(runtime_database_with_repos(&["api", "web"])).unwrap();
        assert_eq!(database["repos"], json!(["api", "web"]));
        let roundtrip: crate::config::ProjectRuntimeDatabase =
            serde_json::from_value(database).unwrap();
        assert_eq!(roundtrip.repos, vec!["api".to_string(), "web".to_string()]);
    }

    fn runtime_database_with_repos(repos: &[&str]) -> crate::config::ProjectRuntimeDatabase {
        crate::config::ProjectRuntimeDatabase {
            repos: repos.iter().map(|repo| repo.to_string()).collect(),
            ..Default::default()
        }
    }
}
