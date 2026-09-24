//! Reusable project templates: repos, run commands, runtime, and landing plan.

use super::{CheckoutMode, SCHEMA_VERSION};
pub use knit_runtime::config::{
    DatabaseMode, ProjectRuntime, ProjectRuntimeDatabase, ProjectRuntimePorts, RuntimeMode,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROJECT_CONFIG_FILE: &str = "knit.project.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitProject {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default)]
    pub repos: Vec<ProjectRepoEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, ProjectRunCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ProjectRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<ProjectLandingPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<ProjectRequirements>,
    /// Project-defined forge authentication requirements: which repositories
    /// need a forge credential, of which kind, from which host. Purely
    /// descriptive metadata — it never contains tokens or personal
    /// credential names, and it never assigns anything on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProjectAuth>,
}

impl KnitProject {
    pub fn new(id: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: "KnitProject".to_string(),
            id,
            created_at: now.clone(),
            updated_at: now,
            org_id: None,
            repos: Vec::new(),
            commands: BTreeMap::new(),
            runtime: None,
            landing: None,
            requirements: None,
            auth: None,
        }
    }
}

/// Token kinds each forge offers. A project auth group declares one or more
/// of its provider's values; they are labels for humans creating tokens, not
/// something Knit verifies against the forge.
pub const GITHUB_TOKEN_TYPES: &[&str] = &["fine_grained_pat", "classic_pat"];
pub const GITLAB_TOKEN_TYPES: &[&str] = &[
    "personal_access_token",
    "project_access_token",
    "group_access_token",
];
pub const BITBUCKET_TOKEN_TYPES: &[&str] = &["atlassian_api_token", "access_token"];
pub const FORGEJO_TOKEN_TYPES: &[&str] = &["access_token"];

/// Token types valid for a provider id, accepting the credential-provider
/// aliases (`codeberg`, `gitea`) for forgejo. Empty for unknown providers.
pub fn token_types_for_provider(provider: &str) -> &'static [&'static str] {
    match provider {
        "github" => GITHUB_TOKEN_TYPES,
        "gitlab" => GITLAB_TOKEN_TYPES,
        "bitbucket" => BITBUCKET_TOKEN_TYPES,
        "forgejo" | "codeberg" | "gitea" => FORGEJO_TOKEN_TYPES,
        _ => &[],
    }
}

/// Providers a project auth group may declare.
pub const AUTH_GROUP_PROVIDERS: &[&str] = &["github", "gitlab", "bitbucket", "forgejo"];

// Generous ceilings for untrusted descriptive text, shared with the hosted
// backend's validation: enough for real guidance, small enough that a
// hostile artifact cannot balloon project metadata. All limits are UTF-8
// byte lengths.
const AUTH_GROUP_MAX_ID: usize = 100;
const AUTH_GROUP_MAX_NAME: usize = 200;
const AUTH_GROUP_MAX_TEXT: usize = 8000;
const AUTH_GROUP_MAX_URL: usize = 2000;
const AUTH_GROUP_MAX_PERMISSIONS: usize = 50;
const AUTH_GROUP_MAX_PERMISSION: usize = 200;
const AUTH_GROUP_MAX_REPOS: usize = 200;
const AUTH_GROUP_MAX_TOKEN_TYPES: usize = 8;
const AUTH_GROUP_MAX_GROUPS: usize = 64;

/// Project-defined forge authentication requirements.
///
/// An empty group list means the project makes no recommendation; local
/// setup behaves exactly as before the field existed. `groups` is always
/// serialized — an explicit clear must roundtrip as `{"groups":[]}`, never
/// collapse to `{}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectAuth {
    #[serde(default)]
    pub groups: Vec<ProjectAuthGroup>,
}

impl ProjectAuth {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Structural validation that needs no project context: required fields,
    /// supported providers and token types, unique group ids, repository ids
    /// used at most once across all groups, plain-text hygiene (no control
    /// characters except newlines/tabs inside instructions), and a bare-HTTPS
    /// token creation URL. Use [`crate::auth::validate_project_auth`] for the
    /// full check that also resolves repository ids against a concrete
    /// project.
    pub fn validate_structure(&self) -> Result<(), String> {
        if self.groups.len() > AUTH_GROUP_MAX_GROUPS {
            return Err(format!(
                "auth groups exceed the maximum of {AUTH_GROUP_MAX_GROUPS}"
            ));
        }
        let mut group_ids = std::collections::BTreeSet::new();
        let mut repo_ids = std::collections::BTreeSet::new();
        for group in &self.groups {
            if group.id.is_empty() || group.id.len() > AUTH_GROUP_MAX_ID {
                return Err(format!(
                    "auth group ids must be 1-{AUTH_GROUP_MAX_ID} characters"
                ));
            }
            if !group
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return Err(format!(
                    "auth group id `{}` may contain only letters, digits, hyphens, underscores, and dots",
                    group.id
                ));
            }
            if !group_ids.insert(group.id.clone()) {
                return Err(format!("duplicate auth group id `{}`", group.id));
            }
            if group.name.trim().is_empty()
                || group.name.len() > AUTH_GROUP_MAX_NAME
                || group.name.contains(control_char)
            {
                return Err(format!(
                    "auth group `{}` needs a plain-text name of 1-{AUTH_GROUP_MAX_NAME} bytes without control characters",
                    group.id
                ));
            }
            if !AUTH_GROUP_PROVIDERS.contains(&group.provider.as_str()) {
                return Err(format!(
                    "auth group `{}` declares unsupported provider `{}`; expected one of {}",
                    group.id,
                    group.provider,
                    AUTH_GROUP_PROVIDERS.join(", ")
                ));
            }
            validate_auth_group_host(&group.id, &group.host)?;
            if group.repos.is_empty() {
                return Err(format!(
                    "auth group `{}` must list at least one repository",
                    group.id
                ));
            }
            if group.repos.len() > AUTH_GROUP_MAX_REPOS {
                return Err(format!(
                    "auth group `{}` lists more than {AUTH_GROUP_MAX_REPOS} repositories",
                    group.id
                ));
            }
            if group.token_types.is_empty() {
                return Err(format!(
                    "auth group `{}` must list at least one token type",
                    group.id
                ));
            }
            if group.token_types.len() > AUTH_GROUP_MAX_TOKEN_TYPES {
                return Err(format!(
                    "auth group `{}` lists more than {AUTH_GROUP_MAX_TOKEN_TYPES} token types",
                    group.id
                ));
            }
            let supported = token_types_for_provider(&group.provider);
            for token_type in &group.token_types {
                if !supported.contains(&token_type.as_str()) {
                    return Err(format!(
                        "auth group `{}` declares token type `{}`; provider `{}` supports {}",
                        group.id,
                        token_type,
                        group.provider,
                        supported.join(", ")
                    ));
                }
            }
            for repo in &group.repos {
                if repo.is_empty() {
                    return Err(format!(
                        "auth group `{}` lists an empty repository id",
                        group.id
                    ));
                }
                if !repo_ids.insert(repo.clone()) {
                    return Err(format!(
                        "repository `{repo}` appears in more than one auth group (or twice in `{}`); split groups do not overlap",
                        group.id
                    ));
                }
            }
            if group.permissions.len() > AUTH_GROUP_MAX_PERMISSIONS {
                return Err(format!(
                    "auth group `{}` lists more than {AUTH_GROUP_MAX_PERMISSIONS} permissions",
                    group.id
                ));
            }
            for permission in &group.permissions {
                if permission.trim().is_empty()
                    || permission.len() > AUTH_GROUP_MAX_PERMISSION
                    || permission.contains(control_char)
                {
                    return Err(format!(
                        "auth group `{}` has a permission entry outside 1-{AUTH_GROUP_MAX_PERMISSION} plain-text bytes",
                        group.id
                    ));
                }
            }
            if let Some(instructions) = &group.instructions {
                if instructions.trim().is_empty()
                    || instructions.len() > AUTH_GROUP_MAX_TEXT
                    // Instructions are free text for humans: ordinary
                    // newlines and tabs are fine, every other control
                    // character is not.
                    || instructions.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
                {
                    return Err(format!(
                        "auth group `{}` instructions must be 1-{AUTH_GROUP_MAX_TEXT} bytes with no control characters beyond newlines and tabs",
                        group.id
                    ));
                }
            }
            if let Some(url) = &group.token_url {
                validate_auth_group_token_url(&group.id, url)?;
            }
        }
        Ok(())
    }
}

/// Any control character (C0, DEL, and the C1 range), rejected wherever
/// single-line untrusted text is stored.
fn control_char(character: char) -> bool {
    character.is_control()
}

/// One recommended credential: a provider/host, the repositories it should
/// serve, and the token kind(s) to create. All descriptive text is untrusted;
/// Knit prints it but never executes it or opens the URL. Unknown fields are
/// rejected on import so smuggled credential material can never ride along.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectAuthGroup {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub host: String,
    pub repos: Vec<String>,
    pub token_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
}

fn validate_auth_group_host(group_id: &str, host: &str) -> Result<(), String> {
    let invalid = host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host
            .split('.')
            .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b));
    if invalid {
        return Err(format!(
            "auth group `{group_id}` host `{host}` must be a hostname without scheme, port, or path"
        ));
    }
    Ok(())
}

/// Token creation URLs must be plain HTTPS without userinfo: printable,
/// visitable by a human, and impossible to smuggle credentials through.
fn validate_auth_group_token_url(group_id: &str, url: &str) -> Result<(), String> {
    if url.len() > AUTH_GROUP_MAX_URL
        || url.contains(control_char)
        || url.contains(char::is_whitespace)
    {
        return Err(format!(
            "auth group `{group_id}` tokenUrl must be at most {AUTH_GROUP_MAX_URL} bytes with no whitespace or control characters"
        ));
    }
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err(format!(
                "auth group `{group_id}` tokenUrl `{url}` is not a valid URL"
            ))
        }
    };
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(format!(
            "auth group `{group_id}` tokenUrl must be HTTPS without embedded credentials"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRequirements {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ProjectToolRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectToolRequirement {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub for_: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default)]
    pub checkout_mode: CheckoutMode,
    #[serde(default = "default_include_by_default")]
    pub include_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRunCommand {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingPlan {
    /// Versioned recipe extensions are preserved across project edits and export.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// What `knit land apply` does when a step fails: `resume` (default) stops
    /// and waits for `knit land resume`; `rollback` creates revert PRs for the
    /// merge steps that already landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<super::LandOnFailure>,
    /// Named checks (see `knit check`) that must be green and fresh at the
    /// current bundle heads before `knit land apply` will execute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_checks: Vec<String>,
    #[serde(default)]
    pub merge: ProjectLandingMergePlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<ProjectLandingDeployment>,
    /// Branch-keyed landing lanes. When recorded review objects target one of
    /// these branches, Knit appends that target's deployment steps to the
    /// generated land plan.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, ProjectLandingTarget>,
    /// Named landing lanes resolve one logical destination (for example
    /// `staging` or `production`) to the branch used by each repository.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lanes: BTreeMap<String, ProjectLandingLane>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingLane {
    /// Versioned recipe extensions are preserved across project edits and export.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Fallback branch for repositories without an explicit entry in
    /// `branches`. A `"*"` entry in `branches` is accepted as an equivalent
    /// shorthand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Where each repository's work goes in this lane. A `null` value means
    /// the repository is not part of this environment at all: a library or a
    /// script bag has nowhere to be deployed, so the lane skips it instead of
    /// inventing a branch for it. Absence is written per repository on
    /// purpose — a missing entry is also what a typo looks like.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, Option<String>>,
    /// Whether landing into this lane finishes the bundle. A terminal lane is
    /// the bundle's last stop: landing there archives it. An intermediate
    /// lane (a staging environment, say) leaves the bundle open. When unset,
    /// a lane is terminal only if it maps every repository to that
    /// repository's configured base branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<ProjectLandingDeployment>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingTarget {
    /// Versioned recipe extensions are preserved across project edits and export.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Whether landing into this branch finishes the bundle; see
    /// [`ProjectLandingLane::terminal`]. When unset, the branch is terminal
    /// only if it is the configured base branch of every merging repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<ProjectLandingDeployment>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingMergePlan {
    /// False delegates source integration to the declared release procedure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_order: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub needs: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_unlisted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<super::MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_checks_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_branch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingDeployment {
    /// Versioned recipe extensions are preserved across project edits and export.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    pub id: String,
    #[serde(default, alias = "repo", skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// Which repositories' changes make this deployment run. A deployment
    /// usually watches the repository it deploys, but not always: an image
    /// that builds another repository's binary into itself has to redeploy
    /// when *that* repository changes, or it ships a stale one.
    ///
    /// Absent means the deployment's own `repoId`, which every deploy step
    /// has. A literal `"*"` always runs, as in
    /// `landing.lanes.<name>.branches`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_changed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<super::DeployMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<ProjectLandingCheckout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Maximum time a command deployment may run before Knit terminates its
    /// process tree. Defaults to 30 minutes when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLandingCheckout {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<super::DeployCheckoutUpdate>,
}

fn default_include_by_default() -> bool {
    true
}

#[cfg(test)]
mod requirements_tests {
    use super::*;

    #[test]
    fn project_requirements_roundtrip_and_match_schema() {
        let mut project = KnitProject::new("tools".into(), "2026-09-05T00:00:00Z".into());
        assert!(serde_json::to_value(&project)
            .unwrap()
            .get("requirements")
            .is_none());
        project.requirements = Some(serde_json::from_value(serde_json::json!({
            "platforms": ["linux/amd64", "darwin/arm64"],
            "tools": [{"name":"cargo", "minVersion":"1.85"}, {"name":"docker", "optional":true, "for":"runtime"}],
            "diskMib":20480, "memoryMib":4096, "agents":["codex"], "env":["FLY_ACCESS_TOKEN"]
        })).unwrap());
        let value = serde_json::to_value(&project).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/project.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let decoded: KnitProject = serde_json::from_value(value).unwrap();
        let requirements = decoded.requirements.unwrap();
        assert_eq!(requirements.tools[1].for_.as_deref(), Some("runtime"));
        assert!(!requirements.tools[0].optional);
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn group_json() -> serde_json::Value {
        serde_json::json!({
            "id": "github-work",
            "name": "GitHub work token",
            "provider": "github",
            "host": "github.com",
            "repos": ["api", "web"],
            "tokenTypes": ["fine_grained_pat"],
            "permissions": ["contents:read", "pull_requests:write"],
            "instructions": "Create the token in the org, then paste it when asked.",
            "tokenUrl": "https://github.com/settings/personal-access-tokens/new"
        })
    }

    fn auth_with(groups: serde_json::Value) -> ProjectAuth {
        serde_json::from_value(serde_json::json!({ "groups": groups })).unwrap()
    }

    fn project_with_repos() -> KnitProject {
        serde_json::from_value(serde_json::json!({
            "schemaVersion": "0.1", "kind": "KnitProject", "id": "one",
            "createdAt": "", "updatedAt": "",
            "repos": [
                {"id":"api", "path":"api", "remote":"https://github.com/org/api.git", "baseBranch":"main"},
                {"id":"web", "path":"web", "remote":"git@github.com:org/web.git", "baseBranch":"main"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn auth_field_is_omitted_when_absent_and_roundtrips_when_present() {
        let bare = KnitProject::new("tools".into(), "2026-09-05T00:00:00Z".into());
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("auth").is_none());
        let decoded: KnitProject = serde_json::from_value(value).unwrap();
        assert!(decoded.auth.is_none());

        let mut with_auth = project_with_repos();
        with_auth.auth = Some(auth_with(serde_json::json!([group_json()])));
        let value = serde_json::to_value(&with_auth).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/project.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let decoded: KnitProject = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.auth, with_auth.auth);
        // Optional fields stay absent when unset, and empty groups are the
        // legacy shape: no recommendation at all.
        let minimal: ProjectAuth = serde_json::from_value(serde_json::json!({
            "groups": [{"id":"g", "name":"G", "provider":"forgejo", "host": "codeberg.org",
                        "repos": ["api"], "tokenTypes": ["access_token"]}]
        }))
        .unwrap();
        let value = serde_json::to_value(&minimal).unwrap();
        assert!(value["groups"][0].get("permissions").is_none());
        assert!(value["groups"][0].get("instructions").is_none());
        assert!(value["groups"][0].get("tokenUrl").is_none());
        assert!(minimal.validate_structure().is_ok());
        assert!(ProjectAuth::default().validate_structure().is_ok());
    }

    #[test]
    fn explicit_empty_auth_roundtrips_as_groups_array_not_empty_object() {
        // A maintainer clearing requirements must publish `{"groups":[]}`;
        // collapsing the clear to `{}` would be rejected by the backend and
        // read back as absent on other machines.
        let cleared: ProjectAuth =
            serde_json::from_value(serde_json::json!({"groups": []})).unwrap();
        assert!(cleared.is_empty());
        let value = serde_json::to_value(&cleared).unwrap();
        assert_eq!(value, serde_json::json!({"groups": []}));
        let roundtrip: ProjectAuth = serde_json::from_value(value).unwrap();
        assert_eq!(roundtrip, cleared);
        let mut project = project_with_repos();
        project.auth = Some(cleared);
        let value = serde_json::to_value(&project).unwrap();
        assert_eq!(value["auth"], serde_json::json!({"groups": []}));
    }

    #[test]
    fn unknown_auth_fields_are_rejected_so_credentials_cannot_ride_along() {
        for extra in ["token", "password", "secret"] {
            let value = serde_json::json!({
                "groups": [], extra: "SYNTHETIC-SECRET"
            });
            assert!(
                serde_json::from_value::<ProjectAuth>(value).is_err(),
                "unknown auth field `{extra}` must be rejected"
            );
        }
        let group = serde_json::json!({
            "id": "g", "name": "G", "provider": "github", "host": "github.com",
            "repos": ["api"], "tokenTypes": ["classic_pat"], "apiToken": "SYNTHETIC-SECRET"
        });
        assert!(serde_json::from_value::<ProjectAuthGroup>(group).is_err());
    }

    #[test]
    fn control_characters_and_size_limits_are_rejected() {
        let mut group = group_json();
        group["name"] = serde_json::json!("bad\u{0007}name");
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .unwrap_err()
            .contains("control characters"));
        let mut group = group_json();
        group["permissions"] = serde_json::json!(["read\u{001B}"]);
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .unwrap_err()
            .contains("plain-text"));
        let mut group = group_json();
        group["tokenUrl"] =
            serde_json::json!("https://github.com/settings/tokens/new?key=value\u{0000}");
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .unwrap_err()
            .contains("control characters"));
        // Instructions accept ordinary newlines and tabs, nothing stranger.
        let mut group = group_json();
        group["instructions"] = serde_json::json!("line one\nline two\tindented");
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .is_ok());
        group["instructions"] = serde_json::json!("line\u{0007}bell");
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .unwrap_err()
            .contains("control characters"));
        // Permission entry count is capped, as are URL bytes.
        let mut group = group_json();
        group["permissions"] = serde_json::Value::Array(
            (0..51)
                .map(|i| serde_json::json!(format!("p{i}")))
                .collect(),
        );
        assert!(auth_with(serde_json::json!([group.clone()]))
            .validate_structure()
            .unwrap_err()
            .contains("more than 50 permissions"));
        let mut group = group_json();
        group["tokenUrl"] = serde_json::json!(format!("https://github.com/{}", "x".repeat(2000)));
        assert!(auth_with(serde_json::json!([group]))
            .validate_structure()
            .unwrap_err()
            .contains("2000"));
    }

    #[test]
    fn structural_validation_rejects_broken_groups() {
        let base = group_json();
        let reject = |group: serde_json::Value, expected: &str| {
            let error = auth_with(serde_json::json!([group]))
                .validate_structure()
                .unwrap_err();
            assert!(error.contains(expected), "{error} lacked `{expected}`");
        };
        // A missing name never deserializes; an empty one is rejected here.
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove("name");
        assert!(serde_json::from_value::<ProjectAuthGroup>(missing).is_err());
        let mut empty_name = base.clone();
        empty_name["name"] = serde_json::json!("  ");
        reject(empty_name, "name");
        let mut empty_repos = base.clone();
        empty_repos["repos"] = serde_json::json!([]);
        reject(empty_repos, "at least one repository");
        let mut empty_tokens = base.clone();
        empty_tokens["tokenTypes"] = serde_json::json!([]);
        reject(empty_tokens, "at least one token type");
        let mut bad_token_type = base.clone();
        bad_token_type["tokenTypes"] = serde_json::json!(["personal_access_token"]);
        reject(bad_token_type, "token type `personal_access_token`");
        let mut bad_provider = base.clone();
        bad_provider["provider"] = serde_json::json!("sourcehut");
        reject(bad_provider, "unsupported provider `sourcehut`");
        let mut bad_host = base.clone();
        bad_host["host"] = serde_json::json!("https://github.com:443");
        reject(bad_host, "hostname");
        let mut bad_url = base.clone();
        bad_url["tokenUrl"] = serde_json::json!("http://github.com/tokens");
        reject(bad_url, "HTTPS");
        let mut user_info_url = base.clone();
        user_info_url["tokenUrl"] = serde_json::json!("https://secret@github.com/tokens");
        reject(user_info_url, "without embedded credentials");
        // Duplicate group ids and overlapping repos across groups.
        let duplicate_ids = auth_with(serde_json::json!([base.clone(), base.clone()]));
        assert!(duplicate_ids
            .validate_structure()
            .unwrap_err()
            .contains("duplicate auth group id"));
        let mut other = base.clone();
        other["id"] = serde_json::json!("second");
        other["repos"] = serde_json::json!(["api"]);
        let overlap = auth_with(serde_json::json!([base, other]));
        assert!(overlap
            .validate_structure()
            .unwrap_err()
            .contains("more than one auth group"));
        // A repo listed twice inside one group is the same violation.
        let twice = serde_json::json!({"id":"g", "name":"G", "provider":"github",
            "host":"github.com", "repos":["api", "api"], "tokenTypes":["classic_pat"]});
        assert!(auth_with(serde_json::json!([twice]))
            .validate_structure()
            .unwrap_err()
            .contains("more than one auth group"));
    }
}
