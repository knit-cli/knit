//! Docker outside of docker: the compose override that makes a stack run on
//! an engine this process does not own.
//!
//! When knit runs inside a container whose filesystem the engine knows as a
//! named volume, a bind mount of a path this process can see is meaningless
//! to the engine — it would create an empty directory on the engine host
//! instead. The translation is mechanical and happens once, here: every bind
//! whose source lies under the volume's mount point becomes a subpath mount
//! of that volume with the same target. Compose merges service volumes by
//! target, so the override replaces the bind rather than adding to it.
//!
//! Build contexts are deliberately untouched: the compose client reads them
//! itself and streams them to the engine.

use crate::EngineView;
use anyhow::Result;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Build the override compose document for one resolved stack config, plus
/// the bind sources that could not be translated (they lie outside the
/// volume mount) so the caller can warn about them. Pure: no filesystem
/// writes, and the only filesystem read is canonicalizing bind sources.
pub(crate) fn build_engine_override(
    resolved: &Value,
    engine: &EngineView,
    bundle_id: &str,
) -> Result<(Value, Vec<String>)> {
    let mount_canonical = crate::support::canonicalize(&engine.mount).ok();
    let mut services = Map::new();
    let mut left_alone: Vec<String> = Vec::new();

    if let Some(resolved_services) = resolved.get("services").and_then(Value::as_object) {
        for (name, service) in resolved_services {
            let mut overrides = Map::new();
            overrides.insert(
                "extra_hosts".to_string(),
                Value::Array(vec![Value::String(
                    "host.docker.internal:host-gateway".to_string(),
                )]),
            );

            let mut labels = Map::new();
            labels.insert(
                "io.knit.runtime.bundle".to_string(),
                Value::String(bundle_id.to_string()),
            );
            if let Some(owner) = &engine.owner {
                labels.insert(
                    "io.knit.runtime.owner".to_string(),
                    Value::String(owner.clone()),
                );
            }
            overrides.insert("labels".to_string(), Value::Object(labels));

            let mut volumes: Vec<Value> = Vec::new();
            if let Some(entries) = service.get("volumes").and_then(Value::as_array) {
                for entry in entries {
                    let Some(bind) = parse_bind(entry) else {
                        continue;
                    };
                    let source = resolve_source(&bind.source);
                    match subpath_under_mount(&source, &engine.mount, mount_canonical.as_deref()) {
                        Some(subpath) => {
                            volumes.push(volume_mount(engine, &bind, &subpath));
                        }
                        None => {
                            let display = source.display().to_string();
                            if !left_alone.contains(&display) {
                                left_alone.push(display);
                            }
                        }
                    }
                }
            }
            if !volumes.is_empty() {
                overrides.insert("volumes".to_string(), Value::Array(volumes));
            }

            services.insert(name.clone(), Value::Object(overrides));
        }
    }

    let mut volume_entry = Map::new();
    volume_entry.insert("external".to_string(), Value::Bool(true));
    volume_entry.insert("name".to_string(), Value::String(engine.volume.clone()));
    let mut top_volumes = Map::new();
    top_volumes.insert(engine.volume.clone(), Value::Object(volume_entry));

    let mut document = Map::new();
    document.insert("services".to_string(), Value::Object(services));
    document.insert("volumes".to_string(), Value::Object(top_volumes));
    Ok((Value::Object(document), left_alone))
}

/// The override entry replacing one bind: the same target, served from a
/// subpath of the workspace volume. An empty subpath means the bind pointed
/// at the mount root, which is the whole volume — compose rejects an empty
/// `subpath`, so the `volume` block is dropped entirely.
fn volume_mount(engine: &EngineView, bind: &Bind, subpath: &str) -> Value {
    let mut mount = Map::new();
    mount.insert("type".to_string(), Value::String("volume".to_string()));
    mount.insert("source".to_string(), Value::String(engine.volume.clone()));
    mount.insert("target".to_string(), Value::String(bind.target.clone()));
    if bind.read_only {
        mount.insert("read_only".to_string(), Value::Bool(true));
    }
    if !subpath.is_empty() {
        let mut inner = Map::new();
        inner.insert("subpath".to_string(), Value::String(subpath.to_string()));
        mount.insert("volume".to_string(), Value::Object(inner));
    }
    Value::Object(mount)
}

struct Bind {
    source: String,
    target: String,
    read_only: bool,
}

/// A service `volumes` entry as a bind, in either compose spelling. Named
/// volumes, tmpfs and anything unrecognized are not binds.
fn parse_bind(entry: &Value) -> Option<Bind> {
    match entry {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) != Some("bind") {
                return None;
            }
            Some(Bind {
                source: map.get("source")?.as_str()?.to_string(),
                target: map.get("target")?.as_str()?.to_string(),
                read_only: map
                    .get("read_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }
        Value::String(text) => parse_short_bind(text),
        _ => None,
    }
}

/// `src:target[:opts]`, a bind only when the source is a path rather than a
/// named volume.
fn parse_short_bind(text: &str) -> Option<Bind> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let source = parts[0];
    if !(Path::new(source).is_absolute() || source.starts_with('.')) {
        return None;
    }
    let target = parts[1];
    if target.is_empty() {
        return None;
    }
    Some(Bind {
        source: source.to_string(),
        target: target.to_string(),
        read_only: parts
            .get(2)
            .is_some_and(|options| options.split(',').any(|option| option == "ro")),
    })
}

/// The canonical location of a bind source. A source that does not exist yet
/// (compose creates it on the engine side) keeps its written form.
fn resolve_source(source: &str) -> PathBuf {
    crate::support::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source))
}

/// The source's path relative to the volume mount, or `None` when it lies
/// outside. Both the configured and the canonical mount are tried, since
/// canonicalizing the source may have resolved symlinks the mount path still
/// carries.
fn subpath_under_mount(
    source: &Path,
    mount: &Path,
    mount_canonical: Option<&Path>,
) -> Option<String> {
    let relative = source
        .strip_prefix(mount)
        .ok()
        .or_else(|| source.strip_prefix(mount_canonical?).ok())?;
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn engine() -> EngineView {
        EngineView {
            volume: "svartal-ws-1".to_string(),
            mount: PathBuf::from("/var/lib/svartal-test-mount"),
            owner: Some("ws-1".to_string()),
        }
    }

    #[test]
    fn build_engine_override_maps_binds_under_the_mount_to_volume_subpaths() {
        let resolved = json!({
            "services": {
                "backend": {
                    "image": "app",
                    "volumes": [
                        {
                            "type": "bind",
                            "source": "/var/lib/svartal-test-mount/workspace/knit-tools/knithub",
                            "target": "/app"
                        },
                        {
                            "type": "bind",
                            "source": "/elsewhere/cache",
                            "target": "/cache"
                        },
                        {
                            "type": "bind",
                            "source": "/var/lib/svartal-test-mount/secrets/ca.pem",
                            "target": "/etc/ca.pem",
                            "read_only": true
                        },
                        {
                            "type": "volume",
                            "source": "db-data",
                            "target": "/var/lib/postgresql/data"
                        },
                        "/var/lib/svartal-test-mount/workspace/knit-tools/web:/srv/web:ro",
                        "node_modules:/app/node_modules"
                    ]
                },
                "db": { "image": "postgres:17" }
            }
        });

        let (document, left_alone) =
            build_engine_override(&resolved, &engine(), "my-bundle").unwrap();

        let backend = &document["services"]["backend"];
        assert_eq!(
            backend["extra_hosts"],
            json!(["host.docker.internal:host-gateway"])
        );
        assert_eq!(
            backend["labels"],
            json!({
                "io.knit.runtime.bundle": "my-bundle",
                "io.knit.runtime.owner": "ws-1"
            })
        );
        // Named volumes, tmpfs and out-of-mount binds contribute nothing.
        assert_eq!(
            backend["volumes"],
            json!([
                {
                    "type": "volume",
                    "source": "svartal-ws-1",
                    "target": "/app",
                    "volume": { "subpath": "workspace/knit-tools/knithub" }
                },
                {
                    "type": "volume",
                    "source": "svartal-ws-1",
                    "target": "/etc/ca.pem",
                    "read_only": true,
                    "volume": { "subpath": "secrets/ca.pem" }
                },
                {
                    "type": "volume",
                    "source": "svartal-ws-1",
                    "target": "/srv/web",
                    "read_only": true,
                    "volume": { "subpath": "workspace/knit-tools/web" }
                }
            ])
        );
        assert_eq!(left_alone, vec!["/elsewhere/cache".to_string()]);

        // A service with no binds still gets the host entry and the labels.
        let db = &document["services"]["db"];
        assert_eq!(
            db["extra_hosts"],
            json!(["host.docker.internal:host-gateway"])
        );
        assert!(db.get("volumes").is_none());

        assert_eq!(
            document["volumes"],
            json!({ "svartal-ws-1": { "external": true, "name": "svartal-ws-1" } })
        );
    }

    #[test]
    fn build_engine_override_omits_the_subpath_at_the_mount_root_and_the_owner_label() {
        let resolved = json!({
            "services": {
                "shell": {
                    "volumes": [
                        { "type": "bind", "source": "/var/lib/svartal-test-mount", "target": "/w" }
                    ]
                }
            }
        });
        let engine = EngineView {
            owner: None,
            ..engine()
        };

        let (document, left_alone) = build_engine_override(&resolved, &engine, "b").unwrap();

        assert_eq!(
            document["services"]["shell"]["volumes"],
            json!([{ "type": "volume", "source": "svartal-ws-1", "target": "/w" }])
        );
        assert_eq!(
            document["services"]["shell"]["labels"],
            json!({ "io.knit.runtime.bundle": "b" })
        );
        assert!(left_alone.is_empty());
    }
}
