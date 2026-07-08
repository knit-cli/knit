//! Minimal YAML emitter for ejected compose files: block style, deterministic
//! quoting. Exists so `knit run eject` can write hand-editable YAML without
//! pulling a YAML dependency into the crate; `docker compose config` JSON is
//! the only input shape it has to cover.

use serde_json::Value;

pub(crate) fn to_yaml(value: &Value) -> String {
    let mut lines = Vec::new();
    emit(value, 0, &mut lines);
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn emit(value: &Value, indent: usize, lines: &mut Vec<String>) {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, entry) in map {
                let key = scalar(&Value::String(key.clone()));
                if is_block(entry) {
                    lines.push(format!("{pad}{key}:"));
                    emit(entry, indent + 1, lines);
                } else {
                    lines.push(format!("{pad}{key}: {}", scalar(entry)));
                }
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                if is_block(item) {
                    let mut child = Vec::new();
                    emit(item, indent + 1, &mut child);
                    // "- " is exactly one indent level, so splicing it into the
                    // first child line keeps the block aligned.
                    child[0] = format!("{pad}- {}", child[0].trim_start());
                    lines.append(&mut child);
                } else {
                    lines.push(format!("{pad}- {}", scalar(item)));
                }
            }
        }
        other => lines.push(format!("{pad}{}", scalar(other))),
    }
}

/// Whether a value renders as a nested block (non-empty container) rather
/// than an inline scalar.
fn is_block(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => {
            if plain_safe(text) {
                text.clone()
            } else {
                serde_json::to_string(text).expect("strings always serialize")
            }
        }
        // Containers reach here only when empty.
        Value::Object(_) => "{}".to_string(),
        Value::Array(_) => "[]".to_string(),
    }
}

/// Whether a string survives as a plain (unquoted) scalar under YAML 1.1
/// parsers (docker compose's). Anything else — `${VAR}` interpolations,
/// `host:port` pairs (1.1 sexagesimal!), number-like and boolean-like words —
/// is emitted as a JSON-escaped double-quoted scalar, which YAML accepts
/// verbatim.
fn plain_safe(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    if !(first.is_ascii_alphanumeric() || first == '/' || first == '.') {
        return false;
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
    {
        return false;
    }
    const RESERVED: [&str; 10] = [
        "true", "false", "yes", "no", "on", "off", "null", "none", "y", "n",
    ];
    if RESERVED.contains(&text.to_ascii_lowercase().as_str()) {
        return false;
    }
    if text.parse::<f64>().is_ok() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn emits_block_style_with_safe_quoting() {
        let value = json!({
            "services": {
                "backend": {
                    "image": "postgres:17",
                    "ports": ["${KNIT_PORT_BACKEND:-4000}:4000"],
                    "environment": {
                        "PORT": "4000",
                        "DEBUG": "false",
                        "PATH_HINT": "/usr/local/bin",
                        "NAME": "plain-name"
                    },
                    "profiles": ["bundle-db"],
                    "healthcheck": {
                        "test": ["CMD-SHELL", "pg_isready -U postgres"],
                        "retries": 20
                    },
                    "depends_on": {"db": {"condition": "service_healthy", "required": false}}
                }
            },
            "volumes": {"db-data": {}}
        });
        let yaml = to_yaml(&value);
        // serde_json maps are sorted, so keys emit alphabetically.
        let expected = r#"services:
  backend:
    depends_on:
      db:
        condition: service_healthy
        required: false
    environment:
      DEBUG: "false"
      NAME: plain-name
      PATH_HINT: /usr/local/bin
      PORT: "4000"
    healthcheck:
      retries: 20
      test:
        - CMD-SHELL
        - "pg_isready -U postgres"
    image: "postgres:17"
    ports:
      - "${KNIT_PORT_BACKEND:-4000}:4000"
    profiles:
      - bundle-db
volumes:
  db-data: {}
"#;
        assert_eq!(yaml, expected);
    }

    #[test]
    fn emits_arrays_of_maps_with_spliced_dash() {
        let value = json!({
            "volumes": [
                {"type": "bind", "source": "${KNIT_ROOT}", "target": "/workspace"},
                "short-form"
            ]
        });
        let yaml = to_yaml(&value);
        let expected = r#"volumes:
  - source: "${KNIT_ROOT}"
    target: /workspace
    type: bind
  - short-form
"#;
        assert_eq!(yaml, expected);
    }
}
