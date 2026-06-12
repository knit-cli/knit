//! Compose transformation: lift the docker shape the repos already run on
//! their main branches into a disposable per-bundle instance.
//!
//! Input is the stack repo's own compose file (the one developers already
//! use), resolved by `docker compose config` against the *source* repo
//! location so relative paths land in source-space. The transform then:
//!
//! - rewrites every path that resolves inside a tracked repo's source
//!   checkout (build contexts, additional contexts, dockerfiles, build args,
//!   bind-mount sources) to that repo's bundle worktree checkout — "main
//!   everywhere, except the repos this bundle changes"
//! - reallocates every published host port to a free one (container side
//!   untouched) and rewrites textual references to the old host ports inside
//!   environment values and build args (`localhost:5173` -> `localhost:5183`)
//! - strips `container_name` and the top-level `name` so the result runs as
//!   an isolated compose project with its own networks and volumes
//!
//! The result is the same composed shape with different ports and the new
//! code. Repos that need precise control can instead commit a compose file
//! written against the `KNIT_*` env contract; see the parent module.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// One published port of the transformed stack.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    pub service: String,
    pub host: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<u16>,
}

/// Transform a resolved compose config (the JSON output of `docker compose
/// config --format json`) in place. `repo_map` maps canonical source repo
/// paths to bundle checkouts; `allocate` maps an original published host
/// port to a fresh free one.
pub fn transform_compose(
    config: &mut Value,
    repo_map: &[(PathBuf, PathBuf)],
    allocate: &mut dyn FnMut(u16) -> Result<u16>,
) -> Result<Vec<ServicePort>> {
    if let Some(top) = config.as_object_mut() {
        top.remove("name");
    }

    let Some(services) = config
        .get_mut("services")
        .and_then(|services| services.as_object_mut())
    else {
        bail!("compose config has no services");
    };

    let mut ports = Vec::new();
    let mut port_map: Vec<(u16, u16)> = Vec::new();

    for (service_name, service) in services.iter_mut() {
        let Some(service) = service.as_object_mut() else {
            continue;
        };
        service.remove("container_name");

        if let Some(build) = service.get_mut("build").and_then(Value::as_object_mut) {
            transform_build(build, repo_map)?;
        }

        if let Some(volumes) = service.get_mut("volumes").and_then(Value::as_array_mut) {
            for volume in volumes {
                transform_volume(volume, repo_map);
            }
        }

        if let Some(entries) = service.get_mut("ports").and_then(Value::as_array_mut) {
            for entry in entries {
                if let Some(mapping) = transform_port(entry, allocate)? {
                    port_map.push((mapping.0, mapping.1));
                    ports.push(ServicePort {
                        service: service_name.clone(),
                        host: mapping.1,
                        container: mapping.2,
                    });
                }
            }
        }
    }

    // Second pass: rewrite textual references to remapped host ports in app
    // configuration, now that the full port map is known (services commonly
    // reference each other's published ports, e.g. CORS origins).
    for service in services.values_mut() {
        let Some(service) = service.as_object_mut() else {
            continue;
        };
        if let Some(environment) = service
            .get_mut("environment")
            .and_then(Value::as_object_mut)
        {
            rewrite_port_references(environment, &port_map);
        }
        if let Some(args) = service
            .get_mut("build")
            .and_then(Value::as_object_mut)
            .and_then(|build| build.get_mut("args"))
            .and_then(Value::as_object_mut)
        {
            rewrite_port_references(args, &port_map);
        }
    }

    Ok(ports)
}

fn transform_build(build: &mut Map<String, Value>, repo_map: &[(PathBuf, PathBuf)]) -> Result<()> {
    let original_context = build
        .get("context")
        .and_then(Value::as_str)
        .map(PathBuf::from);

    if let Some(context) = &original_context {
        if let Some(remapped) = remap_path(context, repo_map) {
            build.insert(
                "context".to_string(),
                Value::String(remapped.display().to_string()),
            );
        }
    }

    if let Some(contexts) = build
        .get_mut("additional_contexts")
        .and_then(Value::as_object_mut)
    {
        for value in contexts.values_mut() {
            if let Some(path) = value.as_str() {
                if let Some(remapped) = remap_path(Path::new(path), repo_map) {
                    *value = Value::String(remapped.display().to_string());
                }
            }
        }
    }

    // Dockerfiles and build args are resolved against the *original* context;
    // when they land inside a tracked repo they are re-expressed relative to
    // the context so the result stays inside the build context.
    let Some(context) = original_context else {
        return Ok(());
    };

    if let Some(dockerfile) = build.get("dockerfile").and_then(Value::as_str) {
        if let Some(rewritten) = remap_context_relative(&context, dockerfile, repo_map) {
            build.insert("dockerfile".to_string(), Value::String(rewritten));
        }
    }

    if let Some(args) = build.get_mut("args").and_then(Value::as_object_mut) {
        for value in args.values_mut() {
            if let Some(text) = value.as_str() {
                if let Some(rewritten) = remap_context_relative(&context, text, repo_map) {
                    *value = Value::String(rewritten);
                }
            }
        }
    }

    Ok(())
}

/// Remap a path expressed relative to a build context (or absolute). Returns
/// the rewritten value preserving the original flavor: relative inputs stay
/// relative to the context, absolute inputs stay absolute.
fn remap_context_relative(
    context: &Path,
    value: &str,
    repo_map: &[(PathBuf, PathBuf)],
) -> Option<String> {
    let candidate = PathBuf::from(value);
    if candidate.is_absolute() {
        return remap_path(&candidate, repo_map).map(|path| path.display().to_string());
    }
    let resolved = context.join(&candidate);
    let remapped = remap_path(&resolved, repo_map)?;
    Some(relative_between(context, &remapped).unwrap_or_else(|| remapped.display().to_string()))
}

fn transform_volume(volume: &mut Value, repo_map: &[(PathBuf, PathBuf)]) {
    match volume {
        Value::Object(entry) => {
            let is_bind = entry.get("type").and_then(Value::as_str) == Some("bind");
            if !is_bind {
                return;
            }
            if let Some(source) = entry.get("source").and_then(Value::as_str) {
                if let Some(remapped) = remap_path(Path::new(source), repo_map) {
                    entry.insert(
                        "source".to_string(),
                        Value::String(remapped.display().to_string()),
                    );
                }
            }
        }
        Value::String(text) => {
            if let Some((source, rest)) = text.split_once(':') {
                if let Some(remapped) = remap_path(Path::new(source), repo_map) {
                    *text = format!("{}:{rest}", remapped.display());
                }
            }
        }
        _ => {}
    }
}

/// Reallocate one published port entry. Returns `(old_host, new_host,
/// container)` when the entry published a host port.
fn transform_port(
    entry: &mut Value,
    allocate: &mut dyn FnMut(u16) -> Result<u16>,
) -> Result<Option<(u16, u16, Option<u16>)>> {
    match entry {
        Value::Object(port) => {
            let Some(published) = port.get("published") else {
                return Ok(None);
            };
            let old = match published {
                Value::String(text) => text.parse::<u16>().ok(),
                Value::Number(number) => number.as_u64().and_then(|n| u16::try_from(n).ok()),
                _ => None,
            };
            let Some(old) = old else {
                return Ok(None);
            };
            let new = allocate(old)?;
            port.insert("published".to_string(), Value::String(new.to_string()));
            let container = port
                .get("target")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok());
            Ok(Some((old, new, container)))
        }
        Value::String(text) => {
            // Short syntax "HOST:CONTAINER[/proto]".
            let Some((host, rest)) = text.split_once(':') else {
                return Ok(None);
            };
            let Ok(old) = host.parse::<u16>() else {
                return Ok(None);
            };
            let new = allocate(old)?;
            let container = rest
                .split('/')
                .next()
                .and_then(|part| part.parse::<u16>().ok());
            *entry = Value::String(format!("{new}:{rest}"));
            Ok(Some((old, new, container)))
        }
        _ => Ok(None),
    }
}

/// Rewrite `localhost:<old>`-style references to remapped host ports inside
/// a string map (environment or build args). Heuristic by design: host ports
/// shifted by the transform are otherwise invisible to app config.
fn rewrite_port_references(values: &mut Map<String, Value>, port_map: &[(u16, u16)]) {
    for value in values.values_mut() {
        let Some(text) = value.as_str() else {
            continue;
        };
        let mut rewritten = text.to_string();
        for (old, new) in port_map {
            for host in ["localhost", "127.0.0.1", "host.docker.internal"] {
                rewritten = rewritten.replace(&format!("{host}:{old}"), &format!("{host}:{new}"));
            }
        }
        if rewritten != text {
            *value = Value::String(rewritten);
        }
    }
}

/// Map a path that resolves inside a tracked repo's source checkout to the
/// same location inside its bundle checkout. Longest source prefix wins.
fn remap_path(path: &Path, repo_map: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    let canonical = crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<(usize, PathBuf)> = None;
    for (source, checkout) in repo_map {
        if let Ok(suffix) = canonical.strip_prefix(source) {
            let remapped = if suffix.as_os_str().is_empty() {
                checkout.clone()
            } else {
                checkout.join(suffix)
            };
            let depth = source.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, remapped));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Express `target` relative to `base` (both absolute), walking up with `..`
/// as needed. Returns `None` when the paths share no common root.
fn relative_between(base: &Path, target: &Path) -> Option<String> {
    let base = crate::paths::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let base_components: Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();

    let common = base_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return None;
    }

    let mut parts: Vec<String> = vec!["..".to_string(); base_components.len() - common];
    parts.extend(
        target_components[common..]
            .iter()
            .map(|component| component.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        return Some(".".to_string());
    }
    Some(parts.join("/"))
}

/// Resolve the compose file via `docker compose config --format json`,
/// anchored at the source repo so relative paths resolve in source-space.
pub fn resolve_compose_config(compose_file: &Path, project_directory: &Path) -> Result<Value> {
    let output = std::process::Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose_file)
        .arg("--project-directory")
        .arg(project_directory)
        .args(["config", "--format", "json"])
        .output()
        .context("failed to run docker compose config")?;
    if !output.status.success() {
        bail!(
            "docker compose config failed for {}:\n{}",
            compose_file.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("failed to parse docker compose config output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn normalized_path_text(value: impl AsRef<str>) -> String {
        value.as_ref().replace('\\', "/")
    }

    fn json_path_text(value: &Value) -> String {
        normalized_path_text(value.as_str().unwrap())
    }

    fn knithub_like_config(root: &str) -> Value {
        json!({
            "name": "knithub",
            "services": {
                "db": {
                    "container_name": "knithub-db",
                    "image": "postgres:17",
                    "ports": [{"mode": "ingress", "target": 5432, "published": "5436", "protocol": "tcp"}],
                    "volumes": [{"type": "volume", "source": "db-data", "target": "/var/lib/postgresql/data"}]
                },
                "backend": {
                    "container_name": "knithub-backend",
                    "build": {
                        "context": root,
                        "dockerfile": "knithub/Dockerfile.worktree",
                        "args": {"KNITHUB_SRC": "knithub", "KNIT_SRC": "knit", "KNIT_REV": "main"}
                    },
                    "environment": {
                        "DATABASE_PORT": "5432",
                        "KNITHUB_ALLOWED_ORIGINS": "http://localhost:5173",
                        "KNITHUB_FRONTEND_URL": "http://localhost:5173/app/profile"
                    },
                    "ports": [{"mode": "ingress", "target": 4000, "published": "4000", "protocol": "tcp"}],
                    "volumes": [
                        {"type": "bind", "source": root, "target": "/workspace"},
                        {"type": "bind", "source": format!("{root}/knithub/priv"), "target": "/app/priv"}
                    ]
                },
                "frontend": {
                    "build": {
                        "context": format!("{root}/knithub-frontend"),
                        "additional_contexts": {"gloss-web-ui": format!("{root}/gloss-web-ui")}
                    },
                    "environment": {"VITE_KNITHUB_API_URL": "http://localhost:4000"},
                    "ports": [{"mode": "ingress", "target": 5173, "published": "5173", "protocol": "tcp"}]
                }
            },
            "volumes": {"db-data": {}}
        })
    }

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "knit-transform-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
        ));
        for repo in ["knithub", "knithub-frontend", "gloss-web-ui"] {
            std::fs::create_dir_all(root.join(repo)).unwrap();
        }
        std::fs::create_dir_all(root.join("knithub/priv")).unwrap();
        crate::paths::canonicalize(&root).unwrap()
    }

    #[test]
    fn transform_lifts_main_shape_into_bundle_namespace() {
        let root = temp_root();
        let root_str = root.display().to_string();
        let worktrees = root.join(".knit/worktrees/demo");
        std::fs::create_dir_all(worktrees.join("knithub")).unwrap();
        std::fs::create_dir_all(worktrees.join("knithub-frontend")).unwrap();

        let repo_map = vec![
            (root.join("knithub"), worktrees.join("knithub")),
            (
                root.join("knithub-frontend"),
                worktrees.join("knithub-frontend"),
            ),
        ];

        let mut config = knithub_like_config(&root_str);
        let mut next = 0u16;
        let mut allocate = |old: u16| -> Result<u16> {
            next += 1;
            Ok(old + 10 * next)
        };
        let ports = transform_compose(&mut config, &repo_map, &mut allocate).unwrap();

        // Top-level name and container names are stripped.
        assert!(config.get("name").is_none());
        assert!(config["services"]["backend"]
            .get("container_name")
            .is_none());

        // Build paths pointing into tracked repos land in worktrees; the
        // workspace-root context stays.
        let backend_build = &config["services"]["backend"]["build"];
        assert_eq!(
            json_path_text(&backend_build["context"]),
            normalized_path_text(&root_str)
        );
        assert_eq!(
            backend_build["dockerfile"],
            ".knit/worktrees/demo/knithub/Dockerfile.worktree"
        );
        assert_eq!(
            backend_build["args"]["KNITHUB_SRC"],
            ".knit/worktrees/demo/knithub"
        );
        // knit is not in the bundle: stays on main.
        assert_eq!(backend_build["args"]["KNIT_SRC"], "knit");
        assert_eq!(backend_build["args"]["KNIT_REV"], "main");

        let frontend_build = &config["services"]["frontend"]["build"];
        assert_eq!(
            json_path_text(&frontend_build["context"]),
            normalized_path_text(worktrees.join("knithub-frontend").display().to_string())
        );
        // gloss-web-ui is not in the bundle: additional context stays on main.
        assert_eq!(
            json_path_text(&frontend_build["additional_contexts"]["gloss-web-ui"]),
            normalized_path_text(format!("{root_str}/gloss-web-ui"))
        );

        // Bind mounts into tracked repos remap; workspace mount stays.
        let backend_volumes = config["services"]["backend"]["volumes"].as_array().unwrap();
        assert_eq!(
            json_path_text(&backend_volumes[0]["source"]),
            normalized_path_text(&root_str)
        );
        assert_eq!(
            json_path_text(&backend_volumes[1]["source"]),
            normalized_path_text(worktrees.join("knithub").join("priv").display().to_string())
        );
        // Named volumes untouched (project scoping isolates them).
        assert_eq!(config["services"]["db"]["volumes"][0]["source"], "db-data");

        // Every published port is remapped, container ports untouched.
        assert_eq!(ports.len(), 3);
        let by_service: std::collections::BTreeMap<_, _> = ports
            .iter()
            .map(|p| (p.service.clone(), (p.host, p.container)))
            .collect();
        assert_eq!(by_service["db"].1, Some(5432));
        assert_eq!(by_service["backend"].1, Some(4000));
        assert_eq!(by_service["frontend"].1, Some(5173));
        assert_ne!(by_service["backend"].0, 4000);
        assert_ne!(by_service["frontend"].0, 5173);

        // Environment references to old host ports are rewritten, including
        // cross-service references; container-side ports stay.
        let backend_env = &config["services"]["backend"]["environment"];
        let frontend_host = by_service["frontend"].0;
        let backend_host = by_service["backend"].0;
        assert_eq!(
            backend_env["KNITHUB_ALLOWED_ORIGINS"],
            format!("http://localhost:{frontend_host}")
        );
        assert_eq!(
            backend_env["KNITHUB_FRONTEND_URL"],
            format!("http://localhost:{frontend_host}/app/profile")
        );
        assert_eq!(backend_env["DATABASE_PORT"], "5432");
        assert_eq!(
            config["services"]["frontend"]["environment"]["VITE_KNITHUB_API_URL"],
            format!("http://localhost:{backend_host}")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_between_walks_up_and_down() {
        let base = Path::new("/work/knithub");
        assert_eq!(
            relative_between(base, Path::new("/work/.knit/worktrees/x/knithub")).unwrap(),
            "../.knit/worktrees/x/knithub"
        );
        assert_eq!(
            relative_between(Path::new("/work"), Path::new("/work/knit")).unwrap(),
            "knit"
        );
        assert_eq!(
            relative_between(Path::new("/work"), Path::new("/work")).unwrap(),
            "."
        );
    }
}
