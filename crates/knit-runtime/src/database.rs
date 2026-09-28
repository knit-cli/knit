//! Shared-database attachment policy for transform-mode stacks.
//!
//! One project can lift several compose stacks in one bundle run, and the
//! configured `database.service` name (say `db`) is common enough that an
//! unrelated stack can carry a service with the same name whose database is
//! NOT the project's shared dev database. Deciding "the configured service
//! is the shared dev database" by name alone would strip that stack's real
//! database (transform mode) or profile-gate it away (eject), silently
//! pointing the stack at the dev database.
//!
//! [`shared_database_attachment`] is the one decision point both callers
//! share: `knit run up` consults it before stripping the service and
//! rewiring references onto the shared dev database, and `knit run eject`
//! consults it before turning the service into the profile-gated
//! `KNIT_DB_*` pattern. The policy, in order:
//!
//! 1. An explicit `database.repos` scope restricts attachment to the
//!    listed repos. A listed repo uses the configured service name as-is —
//!    the explicit scope overrides any identity check; an unlisted repo
//!    keeps its own database service and volume.
//! 2. Without a scope, the service must *declare* the shared database's
//!    name in its environment — `POSTGRES_DB` (defaulting to
//!    `POSTGRES_USER`, like the official postgres image), `MYSQL_DATABASE`,
//!    or `MARIADB_DATABASE`. A declared name that differs from the
//!    configured `database.name` keeps the service and its volume.
//! 3. A service whose identity knit cannot verify — it declares no
//!    recognizable database name, or no `database.name` is configured to
//!    verify against — is kept regardless of stack count: an unverifiable
//!    service may be an independent database (say, one its image or entry
//!    scripts initialize), and redirecting it onto the shared dev database
//!    would run the stack's code against the wrong data. The diagnostic
//!    suggests `database.repos` as the explicit opt-in.
//!
//! The decision only ever reads database *names*. Diagnostics name
//! environment keys, never environment values, so secrets that a resolved
//! compose config inlines from env files cannot leak through them.
//!
//! Bundle database mode never consults this policy: it starts a dedicated
//! per-bundle database instead of attaching to the shared one, and eject's
//! bundle-mode parameterization bypasses the policy the same way.

use crate::config::ProjectRuntimeDatabase;
use serde_json::Value;

/// Environment keys that declare the database name a compose service
/// creates or uses, in the order they are checked.
const DATABASE_NAME_KEYS: [&str; 3] = ["POSTGRES_DB", "MYSQL_DATABASE", "MARIADB_DATABASE"];

/// Whether a transform stack's configured database service should attach
/// to the shared dev database. See the module docs for the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedDatabaseAttachment {
    /// The configured service is this stack's instance of the shared dev
    /// database: strip it and rewire dependents onto the shared dev
    /// database (`knit run up`), or parameterize it onto the `KNIT_DB_*`
    /// contract's shared-mode wiring (`knit run eject`).
    Attach,
    /// The configured service is not — or cannot safely be assumed to be —
    /// this stack's instance of the shared dev database: keep the service
    /// and its volume. The diagnostic is printable as-is and never
    /// contains environment values. `None` when there is nothing worth
    /// reporting (no configured service, or the service is absent from
    /// this stack, which is not an isolation decision at all).
    Keep(Option<String>),
}

/// Decide whether the service configured as `database.service` is this
/// stack's instance of the shared dev database.
///
/// - `repo_id`: the stack repo whose resolved compose config `config` is.
/// - `repo_scope`: the explicit `database.repos` list (empty = no explicit
///   scope). Callers that already hold the typed field pass
///   `&database.repos`; [`explicit_repo_scope`] derives it otherwise.
/// - `multi_stack`: whether the run lifts more than one stack. The
///   decision itself does not depend on the stack count — verification is
///   required either way; the flag only tailors the diagnostic wording.
///
/// This is the *shared* attachment decision. Bundle database mode callers
/// do not consult it: their per-bundle databases are not the shared dev
/// database, whatever the service declares.
pub(crate) fn shared_database_attachment(
    repo_id: &str,
    database: &ProjectRuntimeDatabase,
    repo_scope: &[String],
    config: &Value,
    multi_stack: bool,
) -> SharedDatabaseAttachment {
    let Some(service_name) = database.service.as_deref() else {
        return SharedDatabaseAttachment::Keep(None);
    };
    let Some(service) = config
        .get("services")
        .and_then(|services| services.get(service_name))
    else {
        // The configured service is not in this stack: nothing to attach,
        // nothing to report.
        return SharedDatabaseAttachment::Keep(None);
    };

    // An explicit scope trusts the configured service name — and restricts
    // attachment to exactly the listed repos.
    if !repo_scope.is_empty() {
        if repo_scope.iter().any(|repo| repo == repo_id) {
            return SharedDatabaseAttachment::Attach;
        }
        return SharedDatabaseAttachment::Keep(Some(format!(
            "repo `{repo_id}` is not listed in `database.repos`, so its \
             `{service_name}` service stays this stack's own database \
             (service and volume kept)"
        )));
    }

    if database.name.is_empty() {
        // No configured name to verify the service against: the identity
        // is unknown, not conflicting — and unverifiable means kept,
        // whatever the stack count.
        return SharedDatabaseAttachment::Keep(Some(format!(
            "no `database.name` is configured to verify repo `{repo_id}` \
             service `{service_name}` against, so knit cannot verify it is \
             the shared dev database; the service and its volume are kept. \
             Configure `database.name`, or list the repo in `database.repos`"
        )));
    }

    match declared_database_name(service) {
        Some((_, name)) if name == database.name => SharedDatabaseAttachment::Attach,
        Some((key, _)) => SharedDatabaseAttachment::Keep(Some(format!(
            "repo `{repo_id}` service `{service_name}` declares its database \
             name in `{key}`, which does not match the configured shared \
             `database.name`; the service and its volume are kept. List the \
             repo in `database.repos` to attach it anyway"
        ))),
        None if multi_stack => SharedDatabaseAttachment::Keep(Some(format!(
            "repo `{repo_id}` service `{service_name}` declares no database \
             name ({}), and in a multi-stack run the same service name can \
             mean different databases; the service and its volume are kept, \
             so an independent database stays with its stack. List the repo \
             in `database.repos` to attach it explicitly",
            DATABASE_NAME_KEYS.join("`, `")
        ))),
        None => SharedDatabaseAttachment::Keep(Some(format!(
            "repo `{repo_id}` service `{service_name}` declares no database \
             name ({}), so knit cannot verify it is the shared dev database; \
             the service and its volume are kept, so an independent database \
             stays with its stack. List the repo in `database.repos` to \
             attach it explicitly",
            DATABASE_NAME_KEYS.join("`, `")
        ))),
    }
}

/// The explicit `database.repos` scope of a database config: the repos
/// whose configured `database.service` is the shared dev database, by id.
///
pub(crate) fn explicit_repo_scope(database: &ProjectRuntimeDatabase) -> Vec<String> {
    database.repos.clone()
}

/// The database name the service declares for itself, with the environment
/// key that declared it: the first of [`DATABASE_NAME_KEYS`] present. A
/// postgres service without `POSTGRES_DB` defaults its created database to
/// `POSTGRES_USER`, like the official image. `None` when the service
/// declares no recognizable database identity.
fn declared_database_name(service: &Value) -> Option<(&'static str, String)> {
    for key in DATABASE_NAME_KEYS {
        if let Some(name) = environment_value(service, key) {
            return Some((key, name));
        }
    }
    environment_value(service, "POSTGRES_USER").map(|user| ("POSTGRES_USER", user))
}

/// A non-empty string value of `key` in the service's `environment`,
/// reading both the map form and the legacy `KEY=VALUE` list form.
fn environment_value(service: &Value, key: &str) -> Option<String> {
    let environment = service.get("environment")?;
    match environment {
        Value::Object(map) => match map.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
            _ => None,
        },
        Value::Array(entries) => entries.iter().find_map(|entry| {
            let text = entry.as_str()?;
            let (entry_key, value) = text.split_once('=')?;
            (!value.is_empty() && entry_key == key).then(|| value.to_string())
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn database(service: Option<&str>, name: &str) -> ProjectRuntimeDatabase {
        ProjectRuntimeDatabase {
            service: service.map(str::to_string),
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn scope(repos: &[&str]) -> Vec<String> {
        repos.iter().map(|repo| repo.to_string()).collect()
    }

    /// A resolved-compose-shaped stack whose `db` service declares
    /// `key=value` (`key: None` declares no database environment at all).
    fn stack(key: Option<&str>, value: &str) -> Value {
        let mut environment = serde_json::Map::new();
        if let Some(key) = key {
            environment.insert(key.to_string(), Value::String(value.to_string()));
        }
        json!({
            "services": {
                "db": {"image": "postgres:17", "environment": environment},
                "app": {"image": "scratch"}
            }
        })
    }

    fn decide(
        repo: &str,
        database: &ProjectRuntimeDatabase,
        scope: &[String],
        config: &Value,
        multi_stack: bool,
    ) -> SharedDatabaseAttachment {
        shared_database_attachment(repo, database, scope, config, multi_stack)
    }

    fn kept_note(decision: SharedDatabaseAttachment) -> String {
        match decision {
            SharedDatabaseAttachment::Keep(Some(note)) => note,
            other => panic!("expected a kept decision with a note, got {other:?}"),
        }
    }

    #[test]
    fn explicit_scope_restricts_attachment_to_listed_repos() {
        // Even with a matching declared identity, an unlisted repo keeps
        // its own database service and volume.
        let database = database(Some("db"), "app_dev");
        let config = stack(Some("POSTGRES_DB"), "app_dev");
        for multi_stack in [false, true] {
            let note = kept_note(decide(
                "alpha",
                &database,
                &scope(&["beta"]),
                &config,
                multi_stack,
            ));
            assert!(note.contains("`alpha`"), "{note}");
            assert!(note.contains("database.repos"), "{note}");
        }
    }

    #[test]
    fn explicit_scope_uses_the_named_service_normally() {
        // A listed repo attaches on the configured name alone, overriding
        // identity checks — even a declared mismatch.
        let database = database(Some("db"), "app_dev");
        let config = stack(Some("POSTGRES_DB"), "alpha_records");
        for multi_stack in [false, true] {
            assert_eq!(
                decide("alpha", &database, &scope(&["alpha"]), &config, multi_stack),
                SharedDatabaseAttachment::Attach
            );
        }
    }

    #[test]
    fn matching_declared_identity_attaches() {
        let database = database(Some("db"), "app_dev");
        for (key, value) in [
            ("POSTGRES_DB", "app_dev"),
            ("MYSQL_DATABASE", "app_dev"),
            ("MARIADB_DATABASE", "app_dev"),
        ] {
            let config = stack(Some(key), value);
            for multi_stack in [false, true] {
                assert_eq!(
                    decide("alpha", &database, &scope(&[]), &config, multi_stack),
                    SharedDatabaseAttachment::Attach,
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn same_service_name_with_different_identity_is_kept() {
        // The generic service name matches the configuration, but the
        // declared database is another database entirely.
        let database = database(Some("db"), "app_dev");
        let config = stack(Some("POSTGRES_DB"), "alpha_records");
        for multi_stack in [false, true] {
            let note = kept_note(decide(
                "alpha",
                &database,
                &scope(&[]),
                &config,
                multi_stack,
            ));
            assert!(note.contains("`POSTGRES_DB`"), "{note}");
            assert!(note.contains("`db`"), "{note}");
            // Diagnostics name keys, never environment values.
            assert!(!note.contains("alpha_records"), "{note}");
        }

        let mysql = stack(Some("MYSQL_DATABASE"), "alpha_records");
        assert!(matches!(
            decide("alpha", &database, &scope(&[]), &mysql, true),
            SharedDatabaseAttachment::Keep(Some(_))
        ));
    }

    #[test]
    fn postgres_user_defaults_the_declared_database_name() {
        let database = database(Some("db"), "app_dev");
        let matching = stack(Some("POSTGRES_USER"), "app_dev");
        assert_eq!(
            decide("alpha", &database, &scope(&[]), &matching, true),
            SharedDatabaseAttachment::Attach
        );

        let other = stack(Some("POSTGRES_USER"), "postgres");
        assert!(matches!(
            decide("alpha", &database, &scope(&[]), &other, true),
            SharedDatabaseAttachment::Keep(Some(_))
        ));
    }

    #[test]
    fn unknown_identity_is_kept_regardless_of_stack_count() {
        // No declared identity, no scope: even a single-stack run cannot
        // verify the service is the shared dev database — it may be an
        // independent database its image or scripts initialize — so it is
        // kept, with the explicit opt-in suggested.
        let database = database(Some("db"), "app_dev");
        let config = stack(None, "");
        for multi_stack in [false, true] {
            let note = kept_note(decide(
                "alpha",
                &database,
                &scope(&[]),
                &config,
                multi_stack,
            ));
            assert!(note.contains("database.repos"), "{note}");
            assert!(note.contains("`alpha`"), "{note}");
            assert!(note.contains("`db`"), "{note}");
        }
    }

    #[test]
    fn without_configured_name_the_identity_cannot_be_verified() {
        // An empty `database.name` is an unknown identity, not a mismatch:
        // kept in every stack count, with both remedies suggested.
        let database = database(Some("db"), "");
        let config = stack(Some("POSTGRES_DB"), "app_dev");
        for multi_stack in [false, true] {
            let note = kept_note(decide(
                "alpha",
                &database,
                &scope(&[]),
                &config,
                multi_stack,
            ));
            assert!(note.contains("`database.name`"), "{note}");
            assert!(note.contains("database.repos"), "{note}");
            assert!(!note.contains("app_dev"), "{note}");
        }
    }

    #[test]
    fn unconfigured_or_absent_service_is_a_quiet_keep() {
        // No configured service: nothing to decide.
        let unconfigured = database(None, "app_dev");
        assert_eq!(
            decide("alpha", &unconfigured, &scope(&[]), &stack(None, ""), false),
            SharedDatabaseAttachment::Keep(None)
        );
        // The configured service is not in this stack: strip would be a
        // no-op, so there is nothing to report either.
        let elsewhere = database(Some("store"), "app_dev");
        assert_eq!(
            decide("alpha", &elsewhere, &scope(&[]), &stack(None, ""), false),
            SharedDatabaseAttachment::Keep(None)
        );
    }

    #[test]
    fn legacy_environment_list_form_declares_identity() {
        let mut config = stack(Some("POSTGRES_DB"), "app_dev");
        config["services"]["db"]["environment"] = json!(["POSTGRES_DB=app_dev", "OTHER=1"]);
        let database = database(Some("db"), "app_dev");
        assert_eq!(
            decide("alpha", &database, &scope(&[]), &config, true),
            SharedDatabaseAttachment::Attach
        );
    }

    #[test]
    fn explicit_repo_scope_reads_the_configured_repos() {
        let mut config = database(Some("db"), "app_dev");
        assert!(explicit_repo_scope(&config).is_empty());
        config.repos = vec!["alpha".to_string()];
        assert_eq!(explicit_repo_scope(&config), vec!["alpha".to_string()]);
    }
}
