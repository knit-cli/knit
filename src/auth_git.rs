//! Per-invocation Git credentials. Repository bindings never alter Git config
//! for Knit-launched commands; the installer below deliberately adds a
//! Knit-owned include file so plain Git resolves the same selection.
use crate::auth::{self, ResolvedCredential};
use crate::cli::GitCredentialOperation;
use crate::model::KnitProject;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Add only credential references to the command line. The token is loaded by
/// the helper subprocess after Git supplies a matching HTTPS repository path.
pub fn configure(
    cwd: &Path,
    args: &[OsString],
    command: &mut Command,
) -> Result<Vec<ResolvedCredential>> {
    let Some((operation, targets)) = network_targets(cwd, args)? else {
        return Ok(Vec::new());
    };
    // Operations that reach remotes Knit has no binding for — a clone, or a
    // reachability probe — must never hang on a raw Git username/password
    // prompt: with no credential selected there is nothing to answer it, so
    // the prompts are disabled for the child and the fast failure feeds
    // guided local token setup. Working ambient access is untouched — SSH
    // agent keys and credential helpers, ambient or Knit-installed, never
    // reach the terminal-prompt path. In-workspace fetch/push keeps its
    // previous behavior (suppression only when a credential is selected).
    if matches!(operation.as_str(), "clone" | "ls-remote") {
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_ASKPASS", "");
        command.arg("-c").arg("core.askPass=");
    }
    if changes_repository(args) {
        let registry = auth::load()?;
        if let Ok((root, project)) = auth::project_context(cwd, None) {
            if registry
                .projects
                .get(&auth::project_key(&root, &project.id)?)
                .is_some_and(|bindings| !bindings.is_empty())
            {
                bail!("Git repository location overrides are unavailable with project credentials; run Knit from the intended project checkout instead");
            }
        }
    }
    let mut selected = Vec::new();
    for target in targets {
        let urls = remote_urls(cwd, &target, operation == "push");
        for remote in urls {
            if let Some(fallback) = auth::git_fallback(cwd, &remote)? {
                let ssh = fallback.transport == auth::GitFallbackTransport::Ssh;
                crate::git_fallback::configure_native(cwd, command, ssh);
                if ssh {
                    if let Some(url) = crate::git_fallback::ssh_url(&remote) {
                        command
                            .arg("-c")
                            .arg(format!("url.{url}.insteadOf={remote}"));
                    }
                }
                continue;
            }
            let Some(credential) = auth::resolve(cwd, Some(&remote))? else {
                continue;
            };
            let (host, path) = exact_target(&remote)?;
            if selected.is_empty() {
                // Reset inherited helpers for this child, including requests
                // from recursive submodule fetches. A project binding must not
                // fall back to a broader ambient credential on another path.
                command.args([
                    "-c",
                    "credential.helper=",
                    "-c",
                    "credential.useHttpPath=true",
                ]);
                command.env("GIT_TERMINAL_PROMPT", "0");
                command.env("GIT_ASKPASS", "");
                command.env("SSH_ASKPASS", "");
                for (name, _) in std::env::vars_os() {
                    if name.to_string_lossy().starts_with("GIT_TRACE") {
                        command.env_remove(name);
                    }
                }
                command.env_remove("GIT_CURL_VERBOSE");
                command.env("GIT_TRACE_REDACT", "1");
                command.args([
                    "-c",
                    "core.askPass=",
                    "-c",
                    "http.followRedirects=false",
                    "-c",
                    "http.extraHeader=",
                ]);
            }
            let executable = helper_executable()?;
            let helper = format!(
                "!{} auth git-credential --credential {} --host {} --path {}",
                shell_quote(&executable.to_string_lossy()),
                shell_quote(&credential.name),
                shell_quote(&host),
                shell_quote(&path),
            );
            configure_http_auth(command, &host, &path);
            // Send the assigned credential on the first request as well. This
            // prevents libcurl's .netrc from authenticating first on older Git
            // versions that do not understand http.proactiveAuth. --config-env
            // keeps the header out of argv and persistent Git configuration.
            let variable = format!("KNIT_GIT_AUTH_HEADER_{}", selected.len());
            let encoded =
                base64(format!("{}:{}", credential.git_username(), credential.token).as_bytes());
            command.env(&variable, format!("Authorization: Basic {encoded}"));
            command.arg(format!(
                "--config-env=http.https://{host}/{path}.extraHeader={variable}"
            ));
            let scope = format!("credential.https://{host}/{path}.helper");
            command.arg("-c").arg(format!("{scope}="));
            command.arg("-c").arg(format!("{scope}={helper}"));
            // The selected credential must ride the HTTPS transport: an SSH
            // origin is rewritten to HTTPS instead of bypassing the token
            // through the user's SSH agent, and an HTTPS origin gets the same
            // exact identity rewrite so an inherited broad rewrite (a global
            // SSH workaround like url.ssh://git@host/.insteadOf=https://host/)
            // cannot divert this exact URL back to SSH. Git resolves insteadOf
            // by longest matching prefix, so the exact key always outranks the
            // inherited one for this invocation only.
            command
                .arg("-c")
                .arg(format!("url.https://{host}/{path}.insteadOf={remote}"));
            selected.push(credential);
        }
    }
    Ok(selected)
}

pub fn redact(credentials: &[ResolvedCredential], text: &str) -> String {
    credentials
        .iter()
        .fold(text.to_owned(), |text, credential| {
            let encoded =
                base64(format!("{}:{}", credential.git_username(), credential.token).as_bytes());
            credential.redact(&text).replace(&encoded, "[REDACTED]")
        })
}

/// The exact-URL HTTP settings Knit resets, shared by Knit-launched
/// invocations (`configure_http_auth`) and the generated include: inherited
/// owner/host headers or cookies must not authenticate before — or instead
/// of — the assigned (or refused) credential. Proxy and SSL settings stay
/// untouched.
const HTTP_AUTH_RESETS: [(&str, &str); 6] = [
    ("extraHeader", ""),
    ("cookieFile", ""),
    ("saveCookies", "false"),
    ("followRedirects", "false"),
    ("emptyAuth", "false"),
    ("proactiveAuth", "basic"),
];

// Git's HTTP URL matching gives scoped settings precedence over global -c
// values. Reset at the exact repository URL so inherited owner/host settings
// cannot authenticate with an unrelated header or cookie before our helper.
fn configure_http_auth(command: &mut Command, host: &str, path: &str) {
    for (key, value) in HTTP_AUTH_RESETS {
        command
            .arg("-c")
            .arg(format!("http.https://{host}/{path}.{key}={value}"));
    }
}

fn changes_repository(args: &[OsString]) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        if !arg.starts_with('-') {
            break;
        }
        if arg == "-C"
            || arg.starts_with("-C")
            || arg == "--git-dir"
            || arg.starts_with("--git-dir=")
            || arg == "--work-tree"
            || arg.starts_with("--work-tree=")
        {
            return true;
        }
        index += if matches!(arg, "-c" | "--namespace" | "--config-env") {
            2
        } else {
            1
        };
    }
    false
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        result.push(ALPHABET[(a >> 2) as usize] as char);
        result.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

pub fn credential_helper(
    name: &str,
    host: &str,
    path: &str,
    operation: GitCredentialOperation,
) -> Result<()> {
    if !matches!(operation, GitCredentialOperation::Get) {
        return Ok(());
    }
    let fields = read_credential_request()?;
    // Do not load the secret for another host, path, or protocol. Multiple
    // helpers may be installed for `fetch --all`, so a mismatch emits nothing.
    if fields.get("protocol").map(String::as_str) != Some("https")
        || fields.get("host").map(String::as_str) != Some(host)
        || fields.get("path").map(String::as_str) != Some(path)
        || fields.contains_key("url")
    {
        return Ok(());
    }
    let credential = auth::credential(name)?;
    if credential.host != host {
        bail!("credential host does not match Git request");
    }
    emit(&credential)
}

/// Read one Git credential request from stdin. Scope scalars may appear at
/// most once; array metadata such as `capability[]` is ignored. A `url` field
/// is captured (never parsed) so callers can refuse split/rewritten requests.
fn read_credential_request() -> Result<BTreeMap<String, String>> {
    let stdin = std::io::stdin();
    let mut fields = BTreeMap::new();
    let mut total = 0;
    for line in stdin.lock().lines() {
        let line = line?;
        total += line.len();
        if total > 64 * 1024 {
            bail!("Git credential request exceeds limit");
        }
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            if !matches!(key, "protocol" | "host" | "path" | "url") {
                continue;
            }
            if fields.insert(key.to_owned(), value.to_owned()).is_some() {
                bail!("duplicate field in Git credential request");
            }
        }
    }
    Ok(fields)
}

fn emit(credential: &ResolvedCredential) -> Result<()> {
    let username = credential.git_username();
    if username.contains(['\n', '\r']) || credential.token.contains(['\n', '\r']) {
        bail!("invalid Git credential value");
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(
        stdout,
        "username={username}\npassword={}\n",
        credential.token
    )?;
    Ok(())
}

/// Dynamic Git credential helper for plain Git: the credential is selected
/// per request from the reconstructed HTTPS target and the helper's context
/// (explicit workspace/project, else cwd), so token rotation and overrides
/// apply without regenerating anything; `store`/`erase` are ignored. Anything
/// Knit cannot honor answers `quit=1` plus a stderr explanation — Git's
/// remaining helpers and its password prompt stop, so ambient credentials
/// never take over.
pub fn resolve_helper(
    operation: GitCredentialOperation,
    context: Option<(PathBuf, String)>,
) -> Result<()> {
    if !matches!(operation, GitCredentialOperation::Get) {
        return Ok(());
    }
    let fail_closed = |reason: String| {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let _ = writeln!(stdout, "quit=1\n");
        let _ = stdout.flush();
        eprintln!("knit: {reason}");
        Ok(())
    };
    // Every broken or malformed `get` — unreadable request, impossible host
    // or path, failed selection, even a failed stdout write — must reach Git
    // as quit=1 with an explanation, never as a bare process error that Git
    // would answer with a credential prompt.
    match resolve_helper_get(context) {
        // No Knit selection here: emit nothing and let Git continue (prompt,
        // or inherited helpers on hosts Knit did not take over).
        Ok(None) => Ok(()),
        Ok(Some(credential)) => match emit(&credential) {
            Ok(()) => Ok(()),
            Err(error) => fail_closed(format!("could not emit Git credential: {error:#}")),
        },
        Err(error) => fail_closed(format!("{error:#}")),
    }
}

/// The per-request selection for `--resolve`: reconstruct the exact HTTPS
/// target from the split credential fields and resolve it like any Knit
/// network operation. `Ok(None)` means no Knit selection applies. An explicit
/// `(workspace, project)` context — persisted by installations that serve
/// external source checkouts — pins resolution to that project instead of
/// deriving it from the helper process's cwd.
fn resolve_helper_get(context: Option<(PathBuf, String)>) -> Result<Option<ResolvedCredential>> {
    let fields = read_credential_request()?;
    let (host, path) = match (
        fields.get("protocol").map(String::as_str),
        fields.get("host"),
        fields.get("path"),
    ) {
        (Some("https"), Some(host), Some(path)) if !fields.contains_key("url") => (host, path),
        _ => bail!("Knit serves HTTPS credential requests with exact host and path only"),
    };
    // A host with a path separator would parse as an innocent host plus a
    // longer path (`github.com/org` + `repo.git` -> `github.com` +
    // `org/repo.git`), silently changing the target a secret is served for.
    // Reject it before parsing; reconstructed-URL validation covers the rest.
    if host.contains('/') {
        bail!("Git credential request host must not contain a path separator");
    }
    // Reconstruct and validate the exact request target: this rejects
    // confused hosts (embedded credentials, custom ports, unusual schemes),
    // malformed paths, and anything url-normalization could disguise.
    let url = format!("https://{host}/{path}");
    let (normalized_host, _) =
        auth::remote_target(&url).context("rejected Git credential request target")?;
    // The parsed host must be the host Git asked about (case-insensitively);
    // any divergence means the reconstruction did not round-trip.
    if !normalized_host.eq_ignore_ascii_case(host) {
        bail!("Git credential request host does not match the reconstructed target");
    }
    let cwd = match context {
        Some((root, project_id)) => {
            let path = crate::store::project_path(&root, &project_id);
            let project: KnitProject = crate::store::read_json(&path).with_context(|| {
                format!(
                    "project `{project_id}` not found in workspace {}",
                    root.display()
                )
            })?;
            if project.id != project_id {
                bail!(
                    "project `{project_id}` not found in workspace {}",
                    root.display()
                );
            }
            // Pin this single helper process to the explicit workspace and
            // project; the override is process-local state.
            auth::set_project_override(Some(project.id));
            root
        }
        None => std::env::current_dir().context("locating checkout for Git credential request")?,
    };
    auth::resolve(&cwd, Some(&url))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The executable path Git invokes for the credential helper: the invoked
/// argv[0] as an absolute path (bare names found via `PATH`, relative ones
/// against cwd) when it addresses the same file as `current_exe`, keeping a
/// stable brew symlink unresolved; `current_exe()` otherwise.
fn helper_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locating Knit credential helper")?;
    let running = fs::canonicalize(&current).ok();
    let same_file = |candidate: &Path| {
        running
            .as_deref()
            .zip(fs::canonicalize(candidate).ok())
            .is_some_and(|(running, invoked)| running == invoked)
    };
    if let Some(argv0) = std::env::args_os().next().map(PathBuf::from) {
        if argv0.is_absolute() {
            if same_file(&argv0) {
                return Ok(argv0);
            }
        } else if argv0.components().count() > 1 {
            let resolved = match std::env::current_dir() {
                Ok(cwd) => cwd.join(&argv0),
                Err(_) => PathBuf::new(),
            };
            if same_file(&resolved) {
                return Ok(resolved);
            }
        } else if let Some(search) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&search) {
                if directory.as_os_str().is_empty() {
                    continue;
                }
                let candidate = directory.join(&argv0);
                if same_file(&candidate) {
                    return Ok(candidate);
                }
            }
        }
    }
    Ok(current)
}

/// Generated include installed in each checkout's actual Git directory
/// (main `.git` or a linked worktree's `.git/worktrees/<name>`).
const CREDENTIAL_INCLUDE: &str = "knit-credentials.inc";

/// Result of one installer pass over a checkout.
#[derive(PartialEq, Eq, Debug)]
pub enum InstallOutcome {
    /// Knit policy (selection or fail-closed refusal) covers at least one
    /// forge host of this checkout; the generated include is in place.
    /// `changed` reports whether anything was written or linked this pass.
    Active { changed: bool },
    /// No forge host carries Knit policy; nothing is installed (`changed`
    /// reports whether generated entries were just removed, restoring the
    /// repository's inherited Git behavior).
    Cleared { changed: bool },
}

/// Install (or refresh) the Knit-owned credential helper for one checkout's
/// forge remotes, deriving the project context from the checkout itself (the
/// normal worktree callers). See [`install_for_project`] for the explicit
/// variant.
pub fn install(checkout: &Path) -> Result<InstallOutcome> {
    let context = auth::optional_project_context(checkout, None)?;
    install_with_context(
        checkout,
        context
            .as_ref()
            .map(|(root, project)| (root.as_path(), project)),
    )
}

/// Install with an explicit workspace/project context: for source checkouts
/// outside the workspace, where the checkout location cannot reveal which
/// project's credentials and overrides apply. Selection here — and the
/// per-request context persisted into the generated helper command — both pin
/// to the given project. A checkout installed with explicit context carries
/// that association until a later explicit install replaces it; linked
/// worktrees stay independent because each Git directory gets its own
/// include (see below). This never mutates process-global project state.
pub fn install_for_project(
    checkout: &Path,
    root: &Path,
    project: &KnitProject,
) -> Result<InstallOutcome> {
    install_with_context(checkout, Some((root, project)))
}

/// Install (or refresh) the Knit-owned credential helper for one checkout's
/// forge remotes. Selection is metadata-only — tokens load per request — and
/// sections are scoped to the exact HTTPS URLs of taken-over remotes (never a
/// whole host), so a sibling repository with no Knit selection keeps its
/// ambient helpers. A selection that resolves *or* refuses takes the URL over:
/// the refusal must fail closed rather than slide to ambient credentials.
/// Each taken-over URL gets an exact `url.<https>.insteadOf` rewrite — SSH
/// remotes ride the token over HTTPS (an SSH agent would bypass the
/// fail-closed helper) and HTTPS remotes get the identity rewrite that
/// outranks a broad inherited SSH workaround; no generic rewrite, no remote
/// mutation. The include lives in the checkout's actual Git directory,
/// referenced by an exact `includeIf "gitdir:"` condition, so main checkout
/// and linked worktrees carry independent includes; an explicit workspace/
/// project context rides the helper arguments.
fn install_with_context(
    checkout: &Path,
    context: Option<(&Path, &KnitProject)>,
) -> Result<InstallOutcome> {
    let git_dir = absolute_git_dir(checkout)?;
    let registry = auth::load()?;
    let mut rewrites: Vec<(String, String)> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for remote in all_remotes(checkout) {
        // Raw configured URLs: `git remote get-url` would apply the very
        // include this installer writes, so a second activation would read
        // its own HTTPS rewrite back as the stored URL and drop the SSH
        // mapping. `remote.<name>.pushurl` is empty unless set explicitly,
        // in which case pushes reuse the fetch URLs already collected.
        let urls = raw_remote_urls(checkout, &remote, false)
            .into_iter()
            .chain(raw_remote_urls(checkout, &remote, true));
        for url in urls {
            if auth::is_local_remote(&url) {
                continue;
            }
            let Ok((host, path)) = auth::remote_target(&url) else {
                continue;
            };
            // Metadata only: no secret is read during installation, and no
            // process-global project override is set — explicit context goes
            // through the pure selection function.
            let target = (host.clone(), path);
            let selected = auth::select_credential_with_context(&registry, context, &target);
            // Verified ordinary Git access belongs only to a host default.
            // Explicit repository assignments still install the strict helper.
            if let Some(fallback) = auth::git_fallback_with_context(&registry, context, &target)
                .ok()
                .flatten()
            {
                if fallback.transport == auth::GitFallbackTransport::Ssh {
                    if let Some(ssh) = crate::git_fallback::ssh_url(&url) {
                        if ssh != url {
                            rewrites.push((ssh, url));
                        }
                    }
                }
                continue;
            }
            // Every taken-over URL is pinned to its exact HTTPS target. A
            // refused selection must not keep SSH either: the SSH agent
            // would silently bypass the fail-closed helper. A URL with no
            // selection contributes nothing, so ambient access survives.
            if matches!(selected, Ok(None)) {
                continue;
            }
            if let Ok((exact_host, exact_path)) = exact_target(&url) {
                let https_url = format!("https://{exact_host}/{exact_path}");
                targets.push(https_url.clone());
                if !rewrites
                    .iter()
                    .any(|(target, original)| target == &https_url && original == &url)
                {
                    rewrites.push((https_url, url));
                }
            }
        }
    }
    rewrites.sort();
    // One credential section per exact HTTPS repository URL represented by
    // the taken-over remotes — derived from the rewrites, never the host.
    targets.sort();
    targets.dedup();
    let include_path = git_dir.join(CREDENTIAL_INCLUDE);
    if targets.is_empty() && rewrites.is_empty() {
        if !include_path.exists() {
            config_remove_include(checkout, &git_dir);
            return Ok(InstallOutcome::Cleared { changed: false });
        }
        fs::remove_file(&include_path).context("removing generated Git credential include")?;
        config_remove_include(checkout, &git_dir);
        return Ok(InstallOutcome::Cleared { changed: true });
    }
    let executable = helper_executable()?;
    let executable = executable.to_string_lossy();
    // Git config and the helper's shell line are both line-oriented: an
    // executable path holding a newline or carriage return cannot be quoted
    // safely, so refuse it instead of corrupting the generated include.
    if executable.contains(['\n', '\r']) {
        bail!(
            "Knit executable path contains a newline; refusing to write it into Git configuration"
        );
    }
    let content = render_include(
        &targets,
        &rewrites,
        &executable,
        context.map(|(root, project)| (root, project.id.as_str())),
    );
    let changed = match fs::read_to_string(&include_path) {
        Ok(existing) if existing == content => false,
        _ => {
            write_include_file(&include_path, &content)?;
            true
        }
    };
    config_ensure_include(checkout, &git_dir)?;
    Ok(InstallOutcome::Active { changed })
}

/// The checkout's actual Git directory: the main `.git`, or a linked worktree's
/// `.git/worktrees/<name>` — where this checkout's generated include lives.
fn absolute_git_dir(checkout: &Path) -> Result<PathBuf> {
    let output = raw_git(checkout, &["rev-parse", "--absolute-git-dir"])
        .context("locating the repository's Git directory")?;
    Ok(PathBuf::from(output.trim()))
}

fn render_include(
    targets: &[String],
    rewrites: &[(String, String)],
    executable: &str,
    context: Option<(&Path, &str)>,
) -> String {
    let mut text = String::from(
        "# Generated by Knit. Plain Git forge authentication from saved Knit\n\
         # credentials (host defaults and project overrides). No secrets are\n\
         # stored here; tokens are resolved per request. Regenerate with\n\
         # `knit auth status`; remove this file and its includeIf entry to\n\
         # restore inherited Git credential behavior.\n",
    );
    for (https_url, original) in rewrites {
        text.push_str(&format!(
            "[url {}]\n\tinsteadOf = {}\n",
            config_quote(https_url),
            config_quote(original)
        ));
    }
    // An explicit workspace/project rides the helper command line, so the
    // per-request resolution keeps the installing project's context even for
    // source checkouts outside the workspace.
    let resolve_args = context.map(|(root, project)| {
        format!(
            " --workspace {} --project {}",
            shell_quote(&root.to_string_lossy()),
            shell_quote(project)
        )
    });
    let helper = config_quote(&format!(
        "!{} auth git-credential --resolve{}",
        shell_quote(executable),
        resolve_args.unwrap_or_default()
    ));
    for target in targets {
        text.push_str(&format!("[http {}]\n", config_quote(target)));
        for (key, value) in HTTP_AUTH_RESETS {
            if value.is_empty() {
                text.push_str(&format!("\t{key} =\n"));
            } else {
                text.push_str(&format!("\t{key} = {value}\n"));
            }
        }
        text.push_str(&format!(
            "[credential {}]\n\
             \thelper =\n\
             \thelper = {helper}\n\
             \tuseHttpPath = true\n",
            config_quote(target)
        ));
    }
    text
}

/// Quote one value for a Git config file. Git config strings interpret C-style
/// escapes inside double quotes, but only `\n`, `\t`, `\b`, `\\`, and `\"` —
/// so backslashes and quotes are escaped and newlines become `\n`. Values
/// carrying a raw carriage return (which Git would fold into the line) are
/// refused by the caller before they reach this quoting.
fn config_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_include_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("creating Git directory for credential include")?;
    }
    // A process-unique sibling temp plus rename keeps concurrent Knit
    // processes from colliding on the include write; rename replaces the
    // destination atomically (including on Windows).
    let temporary =
        path.with_file_name(format!(".{CREDENTIAL_INCLUDE}.{}.tmp", std::process::id()));
    fs::write(&temporary, content).context("writing generated Git credential include")?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("installing generated Git credential include");
    }
    Ok(())
}

/// The exact `includeIf "gitdir:"` condition selecting this checkout's Git
/// directory. No trailing slash: with one, wildmatch would match recursively
/// and leak the include into every nested worktree. Wildmatch glob characters
/// in the path are backslash-escaped at the pattern layer (the Git-config
/// file layer quotes them separately when Git writes the entry).
fn include_condition(git_dir: &Path) -> String {
    let mut pattern = String::from("gitdir:");
    for c in git_dir.to_string_lossy().chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

fn include_entry_value(git_dir: &Path) -> String {
    git_dir
        .join(CREDENTIAL_INCLUDE)
        .to_string_lossy()
        .to_string()
}

/// Link the generated include from the shared config: `git config --local`
/// writes the common config, and the exact `includeIf` condition activates
/// the include only for this checkout's Git directory, leaving linked
/// worktrees free to carry their own. The draft's legacy unconditional
/// `include.path = knit-credentials.inc` link is removed where present.
fn config_ensure_include(checkout: &Path, git_dir: &Path) -> Result<()> {
    let key = format!("includeIf.{}.path", include_condition(git_dir));
    let value = include_entry_value(git_dir);
    let present = raw_git(
        checkout,
        &[
            "config",
            "--local",
            "--fixed-value",
            "--get-all",
            &key,
            &value,
        ],
    )
    .is_some_and(|values| !values.trim().is_empty());
    if !present {
        raw_git(checkout, &["config", "--local", "--add", &key, &value])
            .context("linking generated Git credential include")?;
    }
    Ok(())
}

/// Remove this checkout's conditional link by exact key and value, so any
/// other worktree's entry in the shared config stays untouched.
fn config_remove_include(checkout: &Path, git_dir: &Path) {
    let key = format!("includeIf.{}.path", include_condition(git_dir));
    let _ = raw_git(
        checkout,
        &[
            "config",
            "--local",
            "--fixed-value",
            "--unset-all",
            &key,
            &include_entry_value(git_dir),
        ],
    );
}

/// Preserve the actual Git path, including `.git`, for credential matching.
fn exact_target(remote: &str) -> Result<(String, String)> {
    let url = if remote.contains("://") {
        url::Url::parse(remote)?
    } else {
        let (authority, path) = remote.split_once(':').context("invalid forge remote")?;
        url::Url::parse(&format!("ssh://{authority}/{path}"))?
    };
    if !matches!(url.scheme(), "https" | "ssh") {
        bail!("project credentials require an HTTPS or SSH forge remote");
    }
    if url.query().is_some() || url.fragment().is_some() || url.password().is_some() {
        bail!("forge remote must not contain a password, query, or fragment");
    }
    let host = url.host_str().context("forge remote has no host")?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let path = url.path().trim_start_matches('/');
    if path.is_empty() || path.contains(['\n', '\r']) {
        bail!("forge remote has no repository path");
    }
    Ok((host, path.to_owned()))
}

fn raw_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn remote_urls(cwd: &Path, target: &str, push: bool) -> Vec<String> {
    let args = if push {
        vec!["remote", "get-url", "--push", "--all", target]
    } else {
        vec!["remote", "get-url", target]
    };
    raw_git(cwd, &args)
        .map(|urls| urls.lines().map(str::to_owned).collect())
        .unwrap_or_else(|| vec![target.to_owned()])
}

/// Configured remote URLs before any `insteadOf` rewriting: the installer
/// must see what the user stored, not what Knit previously rewrote it to.
fn raw_remote_urls(cwd: &Path, target: &str, push: bool) -> Vec<String> {
    let key = if push {
        format!("remote.{target}.pushurl")
    } else {
        format!("remote.{target}.url")
    };
    raw_git(cwd, &["config", "--get-all", &key])
        .map(|urls| urls.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn default_remote(cwd: &Path, push: bool) -> String {
    let branch = raw_git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    if push {
        if let Some(remote) = branch
            .as_ref()
            .and_then(|branch| {
                raw_git(
                    cwd,
                    &["config", "--get", &format!("branch.{branch}.pushRemote")],
                )
            })
            .or_else(|| raw_git(cwd, &["config", "--get", "remote.pushDefault"]))
        {
            return remote;
        }
    }
    branch
        .and_then(|branch| {
            raw_git(
                cwd,
                &["config", "--get", &format!("branch.{branch}.remote")],
            )
        })
        .unwrap_or_else(|| "origin".to_owned())
}

fn all_remotes(cwd: &Path) -> Vec<String> {
    raw_git(cwd, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Git remote groups expand once into configured remote names. Resolve all
/// members before constructing a network command, so one missing assignment
/// prevents the entire operation rather than falling back to ambient auth.
fn expand_remote_groups(cwd: &Path, names: Vec<String>, single_fetch: bool) -> Result<Vec<String>> {
    let mut targets = Vec::new();
    for name in names {
        let members = match raw_git(cwd, &["config", "--get-all", &format!("remotes.{name}")]) {
            Some(group) => {
                let members: Vec<_> = group.split_whitespace().map(str::to_owned).collect();
                if members.is_empty() {
                    bail!("Git remote group has no members");
                }
                // Plain fetch only enters Git's group execution path when
                // there are multiple entries (before deduplication). For a
                // singleton it fetches the original operand as a remote/URL.
                if single_fetch && members.len() == 1 {
                    vec![name]
                } else {
                    for member in &members {
                        if raw_git(cwd, &["remote", "get-url", member]).is_none() {
                            bail!("Git remote group contains an unconfigured remote; refusing unauthenticated fallback");
                        }
                    }
                    members
                }
            }
            None => vec![name],
        };
        for member in members {
            if !targets.contains(&member) {
                targets.push(member);
            }
        }
    }
    Ok(targets)
}

/// Extract the remote operand without confusing option values or refspecs for
/// repository names. All network forms used by Knit pass explicit operands.
pub(crate) fn network_targets(
    cwd: &Path,
    args: &[OsString],
) -> Result<Option<(String, Vec<String>)>> {
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut start = 0;
    while start < args.len() && args[start].starts_with('-') {
        match args[start].as_str() {
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" => start += 2,
            _ => start += 1,
        }
    }
    let Some(operation) = args.get(start) else {
        return Ok(None);
    };
    if !matches!(
        operation.as_str(),
        "fetch" | "push" | "pull" | "ls-remote" | "clone" | "remote"
    ) {
        return Ok(None);
    }
    let mut rest = &args[start + 1..];
    if operation == "remote" {
        if rest.first().map(String::as_str) != Some("update") {
            return Ok(None);
        }
        rest = &rest[1..];
        let names = rest
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .collect::<Vec<_>>();
        let targets = if names.is_empty() {
            all_remotes(cwd)
        } else {
            expand_remote_groups(cwd, names, false)?
        };
        return Ok(Some((operation.clone(), targets)));
    }
    if operation == "fetch" && rest.iter().any(|arg| arg == "--all") {
        return Ok(Some((operation.clone(), all_remotes(cwd))));
    }
    let mut operands = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "--" {
            operands.extend_from_slice(&rest[index + 1..]);
            break;
        }
        if arg.starts_with('-') {
            if matches!(
                arg.as_str(),
                "--depth"
                    | "--deepen"
                    | "--shallow-since"
                    | "--shallow-exclude"
                    | "--upload-pack"
                    | "--receive-pack"
                    | "--server-option"
                    | "-o"
                    | "--repo"
                    | "--branch"
                    | "-b"
                    | "--origin"
                    | "--template"
                    | "--reference"
                    | "--reference-if-able"
                    | "--separate-git-dir"
                    | "--filter"
                    | "--jobs"
                    | "-j"
                    | "--negotiation-tip"
                    | "--refmap"
                    | "--recurse-submodules-default"
                    | "--push-option"
                    | "--exec"
            ) {
                if arg == "--repo" && operation == "push" {
                    if let Some(repo) = rest.get(index + 1) {
                        return Ok(Some((operation.clone(), vec![repo.clone()])));
                    }
                }
                index += 1;
            } else if operation == "push" && arg.starts_with("--repo=") {
                return Ok(Some((operation.clone(), vec![arg[7..].to_owned()])));
            }
        } else {
            operands.push(arg.clone());
        }
        index += 1;
    }
    let has_operands = !operands.is_empty();
    let targets = if operation == "fetch" && rest.iter().any(|arg| arg == "--multiple") {
        operands
    } else {
        vec![operands
            .into_iter()
            .next()
            .unwrap_or_else(|| default_remote(cwd, operation == "push"))]
    };
    let targets = if operation == "fetch" && has_operands {
        expand_remote_groups(cwd, targets, !rest.iter().any(|arg| arg == "--multiple"))?
    } else {
        targets
    };
    Ok(Some((operation.clone(), targets)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_http_settings_override_host_and_owner_settings() {
        for (key, expected) in [
            ("extraHeader", ""),
            ("cookieFile", ""),
            ("saveCookies", "false"),
            ("followRedirects", "false"),
            ("emptyAuth", "false"),
            ("proactiveAuth", "basic"),
        ] {
            let mut command = Command::new("git");
            command
                .current_dir(std::env::temp_dir())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    std::env::temp_dir().join("knit-nonexistent-global-config"),
                )
                .args([
                    "-c",
                    &format!("http.{key}=global"),
                    "-c",
                    &format!("http.https://github.com/.{key}=host"),
                    "-c",
                    &format!("http.https://github.com/org/.{key}=owner"),
                    "-c",
                    &format!("http.https://github.com/org/repo.git.{key}=repository"),
                ]);
            configure_http_auth(&mut command, "github.com", "org/repo.git");
            let output = command
                .args([
                    "config",
                    "--get-urlmatch",
                    &format!("http.{key}"),
                    "https://github.com/org/repo.git",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            // extraHeader is multi-valued: the trailing empty value resets
            // any earlier exact-match values when Git consumes this list.
            let value = String::from_utf8(output.stdout).unwrap();
            assert_eq!(value.lines().last(), Some(expected));
            if key != "extraHeader" {
                assert_eq!(value.trim_end(), expected);
            }
        }
    }

    #[test]
    fn redact_git_basic_auth_and_detect_location_overrides() {
        let credential = ResolvedCredential {
            name: "work".into(),
            provider: "github".into(),
            host: "github.com".into(),
            username: "".into(),
            token_type: None,
            token: "scoped-test-secret".into(),
        };
        assert_eq!(
            redact(
                &[credential],
                "Authorization: Basic eC1hY2Nlc3MtdG9rZW46c2NvcGVkLXRlc3Qtc2VjcmV0"
            ),
            "Authorization: Basic [REDACTED]"
        );
        for args in [
            vec!["-C", "/other", "fetch"],
            vec!["--git-dir=/other/.git", "fetch"],
            vec!["-c", "color.ui=false", "--work-tree", "/other", "push"],
        ] {
            assert!(changes_repository(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
        assert!(!changes_repository(&["fetch".into(), "-C".into()]));
    }

    #[test]
    fn exact_target_preserves_git_suffix_and_quotes_shell_values() {
        assert_eq!(
            exact_target("git@github.com:org/repo.git").unwrap(),
            ("github.com".into(), "org/repo.git".into())
        );
        assert_eq!(
            exact_target("https://github.com/org/repo.git").unwrap(),
            ("github.com".into(), "org/repo.git".into())
        );
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn exact_identity_rewrite_outranks_inherited_ssh_workaround() {
        // What configure adds for an HTTPS target whose credential resolved:
        // an exact url.https/<host>/<path>.insteadOf of the URL itself.
        let identity =
            "url.https://github.com/org/repo.git.insteadOf=https://github.com/org/repo.git";
        let inherited = "url.ssh://git@github.com/.insteadOf=https://github.com/";
        let resolved_url = |config: &[&str]| {
            let mut command = Command::new("git");
            command
                .current_dir(std::env::temp_dir())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    std::env::temp_dir().join("knit-nonexistent-global-config"),
                );
            for entry in config {
                command.arg("-c").arg(entry);
            }
            let output = command
                .args(["ls-remote", "--get-url", "https://github.com/org/repo.git"])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        // Without the identity key, the inherited SSH rewrite wins and the
        // URL leaves HTTPS (bypassing the selected credential's transport).
        assert_eq!(
            resolved_url(&[inherited]),
            "ssh://git@github.com/org/repo.git"
        );
        // With it, the exact match outranks the broader inherited prefix.
        assert_eq!(
            resolved_url(&[inherited, identity]),
            "https://github.com/org/repo.git"
        );
    }
    #[test]
    fn network_operands_follow_options_and_explicit_remote() {
        let cwd = Path::new(".");
        for (args, target) in [
            (
                vec!["fetch", "--depth", "1", "upstream", "main"],
                "upstream",
            ),
            (vec!["push", "--set-upstream", "other", "HEAD"], "other"),
            (
                vec![
                    "ls-remote",
                    "--symref",
                    "https://github.com/org/repo",
                    "HEAD",
                ],
                "https://github.com/org/repo",
            ),
            (
                vec![
                    "clone",
                    "--branch",
                    "main",
                    "git@github.com:org/repo.git",
                    "directory",
                ],
                "git@github.com:org/repo.git",
            ),
            (vec!["push", "--repo=upstream", "HEAD"], "upstream"),
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert_eq!(
                network_targets(cwd, &args).unwrap().unwrap().1,
                vec![target]
            );
        }
        assert!(
            network_targets(cwd, &["remote".into(), "get-url".into(), "origin".into()])
                .unwrap()
                .is_none()
        );
    }

    /// Rendered include content must survive a real Git config parse with the
    /// values intact — including executable paths holding spaces, quotes, and
    /// backslashes (config escaping) and the shell layer inside `!` helpers.
    #[test]
    fn rendered_include_parses_with_git_and_escapes_paths() {
        let directory =
            std::env::temp_dir().join(format!("knit-auth-include-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let executable = if cfg!(windows) {
            "C:\\Program Files\\kni\"t\\knit.exe"
        } else {
            "/opt/kni t ' \" \\\\ bin/knit"
        };
        let content = render_include(
            // Credential sections are exact repository URLs, never hosts: a
            // sibling checkout with no Knit selection must keep ambient auth.
            &["https://github.auth.test/team/b.git".to_owned()],
            &[
                (
                    "https://github.auth.test/team/b.git".to_owned(),
                    "git@github.auth.test:team/b.git".to_owned(),
                ),
                (
                    "https://gitlab.example.test/team/tool".to_owned(),
                    "ssh://git@gitlab.example.test/team/tool.git".to_owned(),
                ),
            ],
            executable,
            None,
        );
        let file = directory.join("rendered.inc");
        fs::write(&file, &content).unwrap();
        let read = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&directory)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", directory.join("nonexistent"))
                .arg("config")
                .arg("--file")
                .arg(&file)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git config {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .trim_end()
                .to_owned()
        };
        // The exact SSH rewrite preserves the original remote string.
        assert_eq!(
            read(&["--get", "url.https://github.auth.test/team/b.git.insteadOf"]),
            "git@github.auth.test:team/b.git"
        );
        assert_eq!(
            read(&[
                "--get",
                "url.https://gitlab.example.test/team/tool.insteadOf"
            ]),
            "ssh://git@gitlab.example.test/team/tool.git"
        );
        // The helper command line reaches Git uncorrupted through both the
        // config layer (quotes/backslashes) and the shell layer.
        assert_eq!(
            read(&[
                "--get",
                "credential.https://github.auth.test/team/b.git.helper"
            ]),
            format!("!{} auth git-credential --resolve", shell_quote(executable))
        );
        assert_eq!(
            read(&[
                "--get",
                "credential.https://github.auth.test/team/b.git.useHttpPath"
            ]),
            "true"
        );
        // The exact-URL HTTP resets keep an inherited header or cookie from
        // authenticating before the helper.
        assert_eq!(
            read(&[
                "--get",
                "http.https://github.auth.test/team/b.git.proactiveAuth"
            ]),
            "basic"
        );
        assert_eq!(
            read(&[
                "--get-all",
                "http.https://github.auth.test/team/b.git.extraHeader"
            ]),
            ""
        );
        // A repository URL without a Knit selection gets no section at all:
        // its inherited (for example ambient) credentials stay in charge.
        let unrelated = Command::new("git")
            .current_dir(&directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", directory.join("nonexistent"))
            .arg("config")
            .arg("--file")
            .arg(&file)
            .arg("--get")
            .arg("credential.https://gitlab.example.test/team/tool.helper")
            .output()
            .unwrap();
        assert!(!unrelated.status.success());
        // The empty reset value is what knocks out inherited helpers.
        let helpers = read(&[
            "--get-all",
            "credential.https://github.auth.test/team/b.git.helper",
        ]);
        assert_eq!(helpers.lines().count(), 2);
        assert_eq!(helpers.lines().next(), Some(""));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn config_quote_escapes_c_style_sequences() {
        assert_eq!(config_quote("plain"), "\"plain\"");
        assert_eq!(config_quote("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
    }
}
