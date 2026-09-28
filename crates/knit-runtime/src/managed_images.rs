//! Registry-only image consumption and a deliberately restricted Dockerfile grammar.
//! Never resolve a workload's image names directly against the shared host cache.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn registry_reference(reference: &str) -> Result<String> {
    if reference.is_empty()
        || !reference
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-/:@".contains(&c))
    {
        bail!("Managed image source must be a literal registry reference: {reference}");
    }
    let lower = reference.to_ascii_lowercase();
    if lower.starts_with("sha256:")
        || (reference.len() >= 12 && reference.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("Host-local image IDs are not allowed: {reference}");
    }
    let (name, digest) = match reference.split_once('@') {
        Some((name, digest)) => {
            let hash = digest
                .strip_prefix("sha256:")
                .context("Only sha256 registry digests are supported")?;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("Invalid registry digest");
            }
            (name, Some(digest))
        }
        None => (reference, None),
    };
    let leaf = name.rsplit('/').next().unwrap_or("");
    if leaf.to_ascii_lowercase().starts_with("knit-") {
        bail!("The knit- image namespace is reserved for managed build outputs");
    }
    if name
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("Invalid registry image name");
    }
    let first = name.split('/').next().unwrap();
    let explicit_registry =
        name.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    let mut canonical = if explicit_registry {
        name.to_string()
    } else if name.contains('/') {
        format!("docker.io/{name}")
    } else {
        format!("docker.io/library/{name}")
    };
    if let Some(digest) = digest {
        canonical.push('@');
        canonical.push_str(digest);
    } else if !leaf.contains(':') {
        canonical.push_str(":latest");
    }
    Ok(canonical)
}

fn expand(source: &str, args: &BTreeMap<String, String>) -> Result<String> {
    let mut result = String::new();
    let mut rest = source;
    while let Some(index) = rest.find('$') {
        result.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let key;
        if let Some(braced) = rest.strip_prefix('{') {
            let end = braced
                .find('}')
                .context("Unclosed Dockerfile source variable")?;
            key = &braced[..end];
            rest = &braced[end + 1..];
        } else {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            key = &rest[..end];
            rest = &rest[end..];
        }
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            bail!(
                "Managed Dockerfile image sources support only simple $NAME or ${{NAME}} arguments"
            );
        }
        result.push_str(args.get(key).with_context(|| {
            format!("Dockerfile image source argument {key} must have an explicit value")
        })?);
    }
    result.push_str(rest);
    if result.contains('$') {
        bail!("Nested Dockerfile source expansion is not supported");
    }
    Ok(result)
}

/// Sources are rewritten, including external COPY sources. Docker never gets an
/// opportunity to select a host-local source. Caller freezes the returned text.
pub(super) fn rewrite_dockerfile(
    source: &str,
    build_args: Option<&Value>,
    mut resolve: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut supplied = BTreeMap::new();
    if let Some(values) = build_args {
        for (name, value) in values
            .as_object()
            .context("Expected resolved build args map")?
        {
            if name.to_ascii_uppercase().starts_with("BUILDKIT_") {
                bail!("Managed builds forbid frontend/cache control argument {name}");
            }
            supplied.insert(
                name.clone(),
                value
                    .as_str()
                    .context("Build args must have explicit string values")?
                    .to_string(),
            );
        }
    }
    let mut logical = Vec::new();
    let mut current = String::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            let comment = line.trim_start_matches('#').trim().to_ascii_lowercase();
            if comment
                .strip_prefix("syntax")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .is_some_and(|value| {
                    ["docker/dockerfile:1", "docker.io/docker/dockerfile:1"].contains(&value.trim())
                })
            {
                // Use the trusted engine's built-in frontend rather than resolve
                // even this familiar frontend name from the shared image store.
                continue;
            }
            if ["syntax", "escape", "check"].iter().any(|key| {
                comment
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            }) {
                bail!("Managed builds forbid Dockerfile parser/frontend directives; remove {line}");
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if line.contains("<<") {
            bail!("Managed Dockerfiles do not support heredocs; use a script in the build context");
        }
        if let Some(prefix) = line.strip_suffix('\\') {
            current.push_str(prefix);
            current.push(' ');
        } else {
            current.push_str(line);
            logical.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        bail!("Unfinished Dockerfile continuation");
    }
    let mut global_args = BTreeMap::new();
    let mut stages = BTreeSet::new();
    let mut stage_count = 0;
    let mut rewritten = Vec::new();
    for line in logical {
        let split = line
            .find(char::is_whitespace)
            .context("Invalid Dockerfile instruction")?;
        let instruction = line[..split].to_ascii_uppercase();
        let body = line[split..].trim();
        match instruction.as_str() {
            "ARG" => {
                let (name, default) = body
                    .split_once('=')
                    .map(|(k, v)| (k, Some(v)))
                    .unwrap_or((body, None));
                if name.to_ascii_uppercase().starts_with("BUILDKIT_") {
                    bail!("Managed builds forbid frontend/cache control argument {name}");
                }
                if stage_count == 0 {
                    if let Some(value) = supplied
                        .get(name)
                        .cloned()
                        .or_else(|| default.map(str::to_string))
                    {
                        global_args.insert(name.to_string(), value);
                    }
                }
                rewritten.push(line);
            }
            "FROM" => {
                let mut tokens: Vec<String> = body.split_whitespace().map(str::to_string).collect();
                if tokens.first().is_some_and(|s| s.starts_with("--")) {
                    bail!(
                        "Managed Dockerfile sources must use the engine's native platform; remove FROM platform options"
                    );
                }
                let source_index = 0;
                if tokens.len() != source_index + 1 && tokens.len() != source_index + 3 {
                    bail!("Unsupported FROM syntax");
                }
                let source = expand(&tokens[source_index], &global_args)?;
                let lower = source.to_ascii_lowercase();
                if lower != "scratch" && !stages.contains(&lower) {
                    tokens[source_index] = resolve(&registry_reference(&source)?)?;
                } else {
                    tokens[source_index] = lower;
                }
                if tokens.len() == source_index + 3 {
                    let alias = tokens[source_index + 2].to_ascii_lowercase();
                    if !tokens[source_index + 1].eq_ignore_ascii_case("as")
                        || !alias.starts_with(|c: char| c.is_ascii_alphabetic())
                        || !alias
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                        || !stages.insert(alias)
                    {
                        bail!("Invalid or duplicate Dockerfile stage alias");
                    }
                }
                stage_count += 1;
                rewritten.push(format!("FROM {}", tokens.join(" ")));
            }
            "COPY" => {
                let mut rest = body;
                let mut flags = Vec::new();
                let mut from_seen = false;
                while rest.starts_with("--") {
                    let end = rest
                        .find(char::is_whitespace)
                        .context("COPY flag requires source and target")?;
                    let flag = &rest[..end];
                    rest = rest[end..].trim_start();
                    if let Some(source) = flag.strip_prefix("--from=") {
                        if from_seen {
                            bail!("Duplicate COPY --from");
                        }
                        from_seen = true;
                        let source = expand(source, &global_args)?;
                        let lower = source.to_ascii_lowercase();
                        let resolved = if stages.contains(&lower) {
                            lower
                        } else if source.bytes().all(|c| c.is_ascii_digit()) {
                            if source.parse::<usize>()? >= stage_count {
                                bail!("COPY stage index is not an earlier stage");
                            }
                            source
                        } else {
                            resolve(&registry_reference(&source)?)?
                        };
                        flags.push(format!("--from={resolved}"));
                    } else if ["--chown=", "--chmod=", "--exclude="]
                        .iter()
                        .any(|prefix| flag.starts_with(prefix))
                        || ["--link", "--parents"].contains(&flag)
                    {
                        flags.push(flag.to_string());
                    } else {
                        bail!("Unsupported managed COPY option {flag}");
                    }
                }
                rewritten.push(format!(
                    "COPY {}{rest}",
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!("{} ", flags.join(" "))
                    }
                ));
            }
            "RUN" => {
                if body.starts_with("--") {
                    bail!("Managed builds forbid RUN mounts, devices and network/security options");
                }
                rewritten.push(line);
            }
            "ONBUILD" => bail!("Managed builds forbid ONBUILD instructions"),
            "ADD" => {
                let mut rest = body;
                while rest.starts_with("--") {
                    let end = rest
                        .find(char::is_whitespace)
                        .context("ADD flag requires source and target")?;
                    let flag = &rest[..end];
                    rest = rest[end..].trim_start();
                    if !["--chown=", "--chmod=", "--checksum=", "--exclude="]
                        .iter()
                        .any(|prefix| flag.starts_with(prefix))
                        && !["--link", "--keep-git-dir"].contains(&flag)
                    {
                        bail!("Unsupported managed ADD option {flag}");
                    }
                }
                // Remote ADD has its own shared source cache, outside registry
                // authorization. Accept only literal local context sources.
                let sources: Vec<String> = if rest.starts_with('[') {
                    serde_json::from_str(rest).context(
                        "Managed ADD requires a literal JSON array or simple local paths",
                    )?
                } else {
                    if rest.contains(['\'', '"', '\\']) {
                        bail!(
                            "Managed ADD requires simple local paths; use COPY for quoted sources"
                        );
                    }
                    rest.split_whitespace().map(str::to_string).collect()
                };
                if sources.len() < 2
                    || sources.iter().any(|part| part.contains('$'))
                    || sources[..sources.len() - 1]
                        .iter()
                        .any(|part| part.contains(':') || part.starts_with('/'))
                {
                    bail!(
                        "Managed ADD supports only literal local context sources; fetch remote content inside a RUN step"
                    );
                }
                rewritten.push(line);
            }
            "CMD" | "ENTRYPOINT" | "ENV" | "EXPOSE" | "HEALTHCHECK" | "LABEL" | "MAINTAINER"
            | "SHELL" | "STOPSIGNAL" | "USER" | "VOLUME" | "WORKDIR" => rewritten.push(line),
            _ => bail!("Unsupported managed Dockerfile instruction {instruction}"),
        }
    }
    if stage_count == 0 {
        bail!("Dockerfile requires FROM");
    }
    Ok(format!("{}\n", rewritten.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_external_multistage_sources_are_resolved_and_frozen() {
        let source = "# syntax=docker/dockerfile:1\nARG BASE=alpine:3\nFROM ${BASE} AS builder\nRUN echo synthetic > /result\nFROM scratch\nCOPY --from=builder /result /result\nCOPY --from=0 /result /second\nCOPY --from=busybox:1 /bin/busybox /bin/busybox\n";
        let mut consumed = Vec::new();
        let result =
            rewrite_dockerfile(source, Some(&json!({"BASE":"alpine:3.21"})), |reference| {
                consumed.push(reference.to_string());
                Ok(format!(
                    "{}@sha256:{}",
                    reference.rsplit_once(':').unwrap().0,
                    "a".repeat(64)
                ))
            })
            .unwrap();
        assert_eq!(
            consumed,
            vec![
                "docker.io/library/alpine:3.21",
                "docker.io/library/busybox:1"
            ]
        );
        assert!(result.contains("FROM docker.io/library/alpine@sha256:"));
        assert!(result.contains("COPY --from=docker.io/library/busybox@sha256:"));
        assert!(result.contains("COPY --from=builder /result /result"));
        assert!(result.contains("COPY --from=0 /result /second"));
        assert!(!result.contains("${BASE}"));
    }

    #[test]
    fn cached_output_ids_frontends_cache_mounts_and_hidden_sources_are_rejected() {
        for source in [
            "FROM knit-foreign-output:runtime",
            "FROM sha256:0123456789abcdef",
            "FROM 0123456789abcdef",
            "FROM scratch\nCOPY --from=knit-foreign-output:runtime /secret /secret",
            "ARG BASE=knit-foreign-output:runtime\nFROM $BASE",
            "# syntax = custom/frontend:latest\nFROM scratch",
            "# escape=`\nFROM scratch",
            "ARG BUILDKIT_SYNTAX=custom/frontend\nFROM scratch",
            "FROM alpine\nRUN --mount=from=knit-foreign-output:runtime,target=/secret cat /secret",
            "FROM alpine\nRUN --mount=type=cache,id=foreign,target=/secret cat /secret",
            "FROM alpine\nONBUILD COPY --from=knit-foreign-output:runtime /secret /secret",
            "FROM alpine\nADD --from=foreign /secret /secret",
            "FROM alpine\nADD https://example.test/source.tar /app",
            "FROM alpine\nADD $SOURCE /app",
            "FROM alpine\nADD [\"https://example.test/source.tar\",\"/app\"]",
            "FROM --platform=linux/other alpine",
            "FROM alpine\nRUN <<EOF\necho ambiguous\nEOF",
        ] {
            assert!(
                rewrite_dockerfile(source, None, |reference| Ok(reference.to_string())).is_err(),
                "{source}"
            );
        }
        assert!(
            rewrite_dockerfile(
                "FROM scratch",
                Some(&json!({"BUILDKIT_SYNTAX":"custom/frontend"})),
                |s| Ok(s.to_string())
            )
            .is_err()
        );
        assert!(registry_reference("registry.example.test/team/knit-foreign:runtime").is_err());
    }

    #[test]
    fn familiar_digest_names_and_registry_names_are_canonicalized() {
        assert_eq!(
            registry_reference("alpine:3").unwrap(),
            "docker.io/library/alpine:3"
        );
        assert_eq!(
            registry_reference("team/app").unwrap(),
            "docker.io/team/app:latest"
        );
        let digest = format!("alpine@sha256:{}", "a".repeat(64));
        assert_eq!(
            registry_reference(&digest).unwrap(),
            format!("docker.io/library/{digest}")
        );
        assert_eq!(
            registry_reference("registry.example.test:5000/team/app:stable").unwrap(),
            "registry.example.test:5000/team/app:stable"
        );
    }
}
