//! Env-file de-inlining for generated compose files. `docker compose config`
//! resolves `env_file:` entries by copying every value — secrets included —
//! into each service's `environment` map, so writing that resolved config to
//! disk persists plaintext credentials in a generated artifact. This module
//! puts the indirection back: values the referenced env files provide
//! verbatim are dropped from the generated `environment` and the `env_file`
//! references are re-attached, so secrets stay in the user's env files and
//! only knit's rewrites (remapped ports, database rewiring) remain inline as
//! explicit overrides — compose gives `environment` precedence over
//! `env_file`, so the rewrites still win at `up` time.
//!
//! The references come from a second `docker compose config
//! --no-env-resolution` pass; values are compared against a lenient dotenv
//! parse. Both are conservative by construction: when the flag is missing or
//! a value does not match byte-for-byte, the value stays inlined — behavior
//! never changes, only where it is written down.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One `env_file:` entry of a service, as emitted by `docker compose config
/// --no-env-resolution` (paths already resolved absolute).
#[derive(Debug, Clone)]
pub(crate) struct EnvFileRef {
    pub(crate) path: PathBuf,
    pub(crate) required: bool,
}

/// Per-service `env_file` references from a config resolved with
/// `--no-env-resolution`. Accepts every shape compose emits or accepts: a
/// single string, a list of strings, or a list of `{path, required}` maps.
pub(crate) fn service_env_files(config: &Value) -> BTreeMap<String, Vec<EnvFileRef>> {
    let mut refs = BTreeMap::new();
    let Some(services) = config.get("services").and_then(Value::as_object) else {
        return refs;
    };
    for (name, service) in services {
        let Some(entry) = service.get("env_file") else {
            continue;
        };
        let parsed = parse_refs(entry);
        if !parsed.is_empty() {
            refs.insert(name.clone(), parsed);
        }
    }
    refs
}

fn parse_refs(entry: &Value) -> Vec<EnvFileRef> {
    match entry {
        Value::String(path) => vec![EnvFileRef {
            path: PathBuf::from(path),
            required: true,
        }],
        Value::Array(items) => items.iter().flat_map(parse_refs).collect(),
        Value::Object(map) => {
            let Some(path) = map.get("path").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![EnvFileRef {
                path: PathBuf::from(path),
                required: map.get("required").and_then(Value::as_bool).unwrap_or(true),
            }]
        }
        _ => Vec::new(),
    }
}

/// Drop inlined `environment` values the referenced env files provide
/// verbatim and re-attach the `env_file` references. Values that differ —
/// knit's port/database rewrites, shell overrides — stay inline and win over
/// the env file. `express` renders each env-file path for the target file
/// (absolute for run artifacts, `${KNIT_ROOT}`-relative for committed eject
/// files). Returns how many values were de-inlined.
pub(crate) fn detach_env_files(
    config: &mut Value,
    refs: &BTreeMap<String, Vec<EnvFileRef>>,
    express: &dyn Fn(&Path) -> String,
) -> usize {
    let mut detached = 0;
    let Some(services) = config
        .get_mut("services")
        .and_then(|services| services.as_object_mut())
    else {
        return 0;
    };
    for (name, service) in services.iter_mut() {
        let Some(service_refs) = refs.get(name) else {
            continue;
        };
        let Some(service) = service.as_object_mut() else {
            continue;
        };

        // Later files override earlier ones, mirroring compose's precedence.
        let mut provided: BTreeMap<String, String> = BTreeMap::new();
        for reference in service_refs {
            provided.extend(parse_env_file(&reference.path));
        }

        if let Some(environment) = service
            .get_mut("environment")
            .and_then(Value::as_object_mut)
        {
            let covered: Vec<String> = environment
                .iter()
                .filter(|(key, value)| {
                    value
                        .as_str()
                        .is_some_and(|text| provided.get(*key).map(String::as_str) == Some(text))
                })
                .map(|(key, _)| key.clone())
                .collect();
            detached += covered.len();
            for key in covered {
                environment.remove(&key);
            }
            if environment.is_empty() {
                service.remove("environment");
            }
        }

        let entries: Vec<Value> = service_refs
            .iter()
            .map(|reference| {
                let mut map = Map::new();
                map.insert("path".to_string(), Value::String(express(&reference.path)));
                map.insert("required".to_string(), Value::Bool(reference.required));
                Value::Object(map)
            })
            .collect();
        service.insert("env_file".to_string(), Value::Array(entries));
    }
    detached
}

/// Lenient dotenv parse, for equality checks only: blank lines and comments
/// are skipped, an `export ` prefix is accepted, and one level of matching
/// surrounding quotes is stripped. A line this parser reads differently from
/// compose just fails the equality check and stays inlined — never wrong,
/// only less tidy. Missing or unreadable files contribute nothing.
fn parse_env_file(path: &Path) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return values;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        values.insert(
            key.to_string(),
            strip_matching_quotes(value.trim()).to_string(),
        );
    }
    values
}

fn strip_matching_quotes(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_env_file(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("knit-envfile-test-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn parses_every_env_file_shape() {
        let config = json!({
            "services": {
                "string": {"env_file": ".env"},
                "list": {"env_file": [".env", ".env.local"]},
                "objects": {"env_file": [{"path": "/abs/.env", "required": false}]},
                "none": {"image": "nginx"}
            }
        });
        let refs = service_env_files(&config);
        assert_eq!(refs["string"].len(), 1);
        assert!(refs["string"][0].required);
        assert_eq!(refs["list"].len(), 2);
        assert_eq!(refs["objects"][0].path, PathBuf::from("/abs/.env"));
        assert!(!refs["objects"][0].required);
        assert!(!refs.contains_key("none"));
    }

    #[test]
    fn dotenv_parse_is_lenient() {
        let path = temp_env_file(
            "lenient",
            "# comment\n\nSECRET=hunter2\nexport EXPORTED=yes\nQUOTED=\"a b\"\nSINGLE='c d'\nSPACED = padded \nNOEQ\n",
        );
        let values = parse_env_file(&path);
        assert_eq!(values["SECRET"], "hunter2");
        assert_eq!(values["EXPORTED"], "yes");
        assert_eq!(values["QUOTED"], "a b");
        assert_eq!(values["SINGLE"], "c d");
        assert_eq!(values["SPACED"], "padded");
        assert!(!values.contains_key("NOEQ"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn detach_strips_covered_values_and_keeps_overrides() {
        let env_path = temp_env_file(
            "detach",
            "SECRET=hunter2\nFRONTEND_URL=http://localhost:3000\n",
        );
        // SECRET was inlined verbatim (env_file or `${SECRET:-}` interpolation
        // — both read the same file); FRONTEND_URL was rewritten by the port
        // transform; EXTRA never came from the env file.
        let mut config = json!({
            "services": {
                "web": {
                    "environment": {
                        "SECRET": "hunter2",
                        "FRONTEND_URL": "http://localhost:3010",
                        "EXTRA": "inline"
                    }
                },
                "plain": {"environment": {"SECRET": "hunter2"}}
            }
        });
        let mut refs = BTreeMap::new();
        refs.insert(
            "web".to_string(),
            vec![EnvFileRef {
                path: env_path.clone(),
                required: false,
            }],
        );

        let detached = detach_env_files(&mut config, &refs, &|path| path.display().to_string());
        assert_eq!(detached, 1);

        let web = &config["services"]["web"];
        assert!(web["environment"].get("SECRET").is_none());
        assert_eq!(web["environment"]["FRONTEND_URL"], "http://localhost:3010");
        assert_eq!(web["environment"]["EXTRA"], "inline");
        assert_eq!(
            web["env_file"],
            json!([{"path": env_path.display().to_string(), "required": false}])
        );
        // No refs recorded for `plain`: untouched, even with a matching value.
        assert_eq!(
            config["services"]["plain"]["environment"]["SECRET"],
            "hunter2"
        );
        assert!(config["services"]["plain"].get("env_file").is_none());
        std::fs::remove_file(env_path).unwrap();
    }

    #[test]
    fn detach_with_missing_optional_file_only_reattaches_the_reference() {
        let mut config = json!({
            "services": {"web": {"environment": {"SECRET": "hunter2"}}}
        });
        let mut refs = BTreeMap::new();
        refs.insert(
            "web".to_string(),
            vec![EnvFileRef {
                path: PathBuf::from("/nonexistent/.env"),
                required: false,
            }],
        );
        let detached = detach_env_files(&mut config, &refs, &|path| path.display().to_string());
        assert_eq!(detached, 0);
        assert_eq!(
            config["services"]["web"]["environment"]["SECRET"],
            "hunter2"
        );
        assert_eq!(
            config["services"]["web"]["env_file"][0]["path"],
            "/nonexistent/.env"
        );
    }

    #[test]
    fn detach_removes_environment_when_fully_covered() {
        let env_path = temp_env_file("full", "ONLY=value\n");
        let mut config = json!({
            "services": {"web": {"environment": {"ONLY": "value"}}}
        });
        let mut refs = BTreeMap::new();
        refs.insert(
            "web".to_string(),
            vec![EnvFileRef {
                path: env_path.clone(),
                required: true,
            }],
        );
        detach_env_files(&mut config, &refs, &|path| path.display().to_string());
        assert!(config["services"]["web"].get("environment").is_none());
        std::fs::remove_file(env_path).unwrap();
    }
}
