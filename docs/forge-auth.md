# Forge credentials

Knit keeps your GitHub, Bitbucket, GitLab, and Forgejo tokens locally. Your sync remote token authorizes the ledger separately; it is never used as a forge token.

## Sync remote tokens: the other credential

The hosted Knit service (sync remote) uses its **own** token. It is not interchangeable with any forge token: a forge token cannot sync bundles, and a sync token cannot push Git.

```sh
knit auth remote hosted            # hidden prompt, verifies with the server, then saves
knit remote auth-status hosted     # inspect the resolved token (kind, subject, scopes)
knit auth remote hosted            # rotate later: same flow, replaces the stored token
```

`knit auth remote` works from any directory — the remote always lives in the user-level Knit config, never in shared workspace config. With a name omitted at a terminal it lists your configured remotes and accepts an existing or new name; an existing remote reuses its saved URL, a new one asks for it (or pass `--url`). The flow always collects a **fresh** token at a hidden prompt — paste, press Enter; the previously stored token is never re-sent, least of all to a changed URL — and the server verifies it **before** anything is saved. A rejected token (HTTP 401/403) is re-prompted at a terminal; Ctrl-C cancels and leaves the old configuration untouched. A network failure or server error never claims the token invalid, changes nothing, and suggests retrying — or `--offline`, which saves the token without contacting the service; check it later with `knit remote auth-status <name>`. A successful save prints the exact config file it wrote plus that check command.

The file follows `KNIT_HOME`, then `XDG_CONFIG_HOME`, then the home directory: `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. Its folder is created on the first successful save, so its absence beforehand is expected.

Noninteractive use needs a name, an existing remote or `--url`, and `--token-stdin`; a rejected token fails the command there instead of re-prompting:

```sh
printf '%s\n' "$TOKEN" | knit auth remote hosted --token-stdin
```

Environment overrides (`KNIT_REMOTE_<NAME>_TOKEN`, `KNIT_REMOTE_TOKEN`) are reported by variable name only, are never verified by this flow, and still win over the stored token at use time. Bare `knit auth` offers the same flow as its `r` option alongside the forge menu. The compatibility commands `knit remote add <name> <url> --global [--token-stdin]` and `knit remote token <name> --global` store a token without verification; `knit remote auth-status <name>` checks any of them afterwards, and a failed optional forge-credential probe there never invalidates the sync login it already confirmed.

## Default tokens: the normal setup

```sh
knit auth                         # choose a forge, paste its token, repeat
knit clone demo --remote hosted
```

Press Enter at the forge menu to finish. Your default token for each **exact host** serves every project, clone, pull, push, and forge API operation unless you choose an override. No repository selection or credential flags are needed. Bitbucket asks which token kind you have; an Atlassian API token also needs your account email.

The first regular token saved for a host becomes its default. Adding another never displaces it. A legacy store with exactly one token that is not project-only inherits that token as its default; with several tokens, choose explicitly using `knit auth default NAME`. The bare wizard can also create a new default or deliberately replace the current default's secret. Replacing an environment-backed default with a pasted token clears its environment reference.

## Permissions for the normal workflow

For clone/pull/push/publish/status/land, give the token access to the repositories you use and these permissions:

| Forge / token kind | Permissions |
| --- | --- |
| GitHub fine-grained PAT | Contents and Pull requests: **Read + Write**; Checks and Commit statuses: **Read**; Metadata: **Read** (automatic). |
| GitHub classic PAT | `repo`. |
| Bitbucket Atlassian API token | Repositories and Pull requests: **Read + Write**, using a token **with scopes** (details below). |
| GitLab personal access token | `api` (includes Git read/write and merge-request API access). |
| GitLab project/group access token | `api` + `write_repository`; choose a role allowed to push and merge. |
| Forgejo access token | `write:repository` (includes pull requests and checks); include your repositories, not public-only access for private repositories. |

For changes to GitHub workflow files, also grant **Workflows: Write** (fine-grained) or `workflow` (classic). Token permissions do not override repository access, organization policies, or merge rules. Saving a token does not validate its scopes.

Sources: [GitHub permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens), [classic scopes](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps), [GitLab scopes](https://docs.gitlab.com/security/tokens/access_token_scopes/), [Forgejo scopes](https://forgejo.org/docs/latest/user/authentication/token-scope/).

## Bitbucket token requirements

For an Atlassian API token (`atlassian_api_token`), select **Create API token with scopes**, choose **Bitbucket** as the app, and enable these permissions for the normal Knit workflow (clone/pull/push/publish/status/land):

- **Repositories: Read + Write** — `read:repository:bitbucket`, `write:repository:bitbucket`.
- **Pull requests: Read + Write** — `read:pullrequest:bitbucket`, `write:pullrequest:bitbucket`.

Unscoped Atlassian API tokens cannot authenticate Bitbucket. See Atlassian's [creation instructions](https://support.atlassian.com/bitbucket-cloud/docs/create-an-api-token/), [API token permissions](https://support.atlassian.com/bitbucket-cloud/docs/api-token-permissions/), and [Git requirements](https://support.atlassian.com/bitbucket-cloud/docs/using-api-tokens/).

Bitbucket repository/project/workspace access tokens (`access_token`) remain supported and use their own repository permissions, not Atlassian API-token scopes. Saving either kind in Knit does not validate its scopes or repository access.

## Optional project overrides

```sh
knit auth --project tools
# Equivalent from that workspace:
knit auth setup
# Limit the repositories being edited:
knit auth setup --repo api --repo web
```

`knit project auth` opens the same project wizard. For each declared group, or each host when no group is declared:

- **Enter:** use the host default, clearing only the selected repositories' project overrides.
- **t:** enter a project-only token. It never becomes a global default, even if it is the first token on that host.
- **s:** keep the current configuration.

The wizard displays the group's token guidance and selects its repositories for you. Groups outside the local workspace or `--repo` selection are skipped. The final summary shows full-project coverage. Project setup never rotates a shared secret or changes another project's assignments.

## Project-defined authentication requirements

Maintainers can declare token guidance in hosted project settings or `knit.project.json`. The `auth` field travels in project exports; it contains descriptions, never credentials or personal credential names.

```json
{
  "auth": {
    "groups": [{
      "id": "github-work",
      "name": "GitHub work token",
      "provider": "github",
      "host": "github.com",
      "repos": ["api", "web"],
      "tokenTypes": ["fine_grained_pat"],
      "permissions": ["contents:read", "pull_requests:write"],
      "instructions": "Create a token scoped to these repositories.",
      "tokenUrl": "https://github.com/settings/personal-access-tokens/new"
    }]
  }
}
```

Group IDs must be unique; a repository may belong to only one group. Groups reference portable repository IDs and must match their repositories' hosts and providers. Several groups may share a host. Unknown fields, unknown/ambiguous repository references, and mismatched hosts are rejected. `{"groups": []}` explicitly clears guidance; omitting `auth` during an update preserves hosted guidance.

Supported token kinds:

| Provider | Token kinds |
| --- | --- |
| GitHub | `fine_grained_pat`, `classic_pat` |
| Bitbucket Cloud | `atlassian_api_token`, `access_token` |
| GitLab | `personal_access_token`, `project_access_token`, `group_access_token` |
| Forgejo | `access_token` |

`permissions`, `instructions`, and `tokenUrl` are optional. Links must use HTTPS without embedded credentials. Knit displays guidance without executing instructions or opening links. Recommendations never replace a working default or prove that a token has the required permissions.

## Clone and recovery

An ordinary clone uses saved defaults automatically. With complete local credential coverage, Knit skips the hosted credential-helper lookup. An ordinary ledger-only remote token does not need access to hosted forge credentials.

Credential coverage means a saved credential resolves; it does not verify access. Knit initially sends a selected credential over HTTPS, including when the original URL uses SSH. If a Bitbucket host default fails authentication during clone or a read-access probe, Knit checks ordinary Git authentication automatically, then tries the same repository over SSH on bitbucket.org when HTTPS authentication is unavailable. A working route is remembered in personal configuration and retained when checkout helpers are refreshed; the saved token and host default stay unchanged. Explicit repository assignments and `--credential` selections remain authoritative and do not fall back. Network errors and missing refs do not trigger credential recovery.

After updating Knit, recover repositories missing from an existing project checkout with `knit pull --bundles`. Working ordinary Git authentication is reused automatically; no new token, per-repository setup, or fresh project clone is needed for this recovery.

For Bitbucket, the recorded token type determines Git authentication: `atlassian_api_token` uses `x-bitbucket-api-token-auth`, while `access_token` uses `x-token-auth`. The account email is needed for API-token REST calls, not to select the Git username. Older credentials without a recorded type retain their previous username-based behavior until classified. A credential with a known token type drives authentication automatically everywhere it is selected — the host default serves every repository on that host in every project, with no per-repository setup. See Atlassian's [API token](https://support.atlassian.com/bitbucket-cloud/docs/using-api-tokens/) and [access token](https://support.atlassian.com/bitbucket-cloud/docs/using-access-tokens/) documentation.

When access is missing, interactive clone guides you through declared groups or infers a host from a failed Git operation. New regular tokens become host defaults. Existing project-only tokens are not automatically reused for another project. Noninteractive clones never prompt and explain how to finish setup.

When you paste a replacement Bitbucket token through `knit auth`, Knit asks for its token type again and updates its account metadata; keeping the existing token makes no changes.

If a credential is rejected, recovery saves a **new project-only credential for the affected repositories**, leaving the old secret, environment reference, and default untouched. Failed repositories are retried once. Use `knit pull --bundles` to reconcile missing repositories in an existing workspace.

An interrupted clone that has checkouts but no configured workspace can resume with the same `knit clone` command and destination. Knit adopts only real checkout roots whose repository IDs and origins match, preserving branches and dirty work. Unrelated files, foreign checkouts, and symlinked scaffolding are refused. Finished bundles remain imported history; they are not automatically selected as active work.

For an explicit automation override, `knit clone --credential NAME` remains available and repeatable, with at most one credential per host. Names and token availability are validated before export fetch or destination changes. The selection saves personal assignments for in-scope repositories before fetching, using the same resolver as later pull/push operations. Assignments survive failed clones so a token repair followed by `knit pull --bundles` recovers in place. An existing conflicting destination assignment is an error. No credential from the surrounding workspace is borrowed.

## Scriptable setup and inspection

```sh
# First token on each host becomes its default:
knit auth add shared --provider github --token-env PROJECT_GITHUB_TOKEN
knit auth add cloud --provider bitbucket \
  --username you@example.com --token-env PROJECT_BITBUCKET_TOKEN

# Explicit repository override and host default selection:
knit auth use restricted --project tools --repo api --repo web
knit auth default shared

knit auth list
knit auth status --project tools --check
knit auth status --project tools --json
```

Omit `--token-env` to paste a token at a hidden prompt, or use `--token-stdin` with a secret manager. Use `--host git.example.com` for a self-hosted forge and `--token-type` to record a known kind. An environment reference must be set when the credential is used. For Bitbucket repository/project/workspace access tokens, omit `--username`.

`status` reports coverage without network access. `--check` probes Git read access with a 30-second timeout per repository and exits nonzero on missing credentials or failed access. It does not prove push, API, review, or merge permissions; those are checked by the actual operation.

```sh
knit auth add restricted --provider github --replace  # deliberate shared rotation
knit auth clear --project tools --repo api             # restore default/allowed ambient access
knit auth remove restricted                           # only when unused
```

## Selection and storage

Selection order is **explicit operation/project assignment → exact-host default → permitted ambient Git access**. Missing or rejected explicit credentials fail rather than falling back to a broader token. Hostless API targets must match project membership or the actual checkout origin unambiguously.

A project without assignments retains existing Git helpers, SSH, `gh auth login`, and provider environment behavior where no Knit default applies. Once assignments exist, other forge repositories need a default, an assignment, or a recorded ambient allowance for their exact remote. Declaring an auth group or changing a remote invalidates that ambient allowance. Local filesystem remotes require no credentials.

Assignments are keyed by the workspace's canonical project artifact path. They stay local: a moved workspace needs its project overrides configured again; host defaults remain available. Project selection follows explicit selection, then bundle context, then the active workspace project.

Metadata, defaults, and assignments live in `forge-auth.json`; pasted tokens live in `forge-secrets.json`, a **private plaintext file, not an encrypted vault**. These follow `KNIT_HOME`, `XDG_CONFIG_HOME`, and the platform's user-config location. Unix directories use `0700` and files `0600`; Windows uses the user's inherited access controls. Tokens never appear in command arguments, status output, project exports, or bundles.

Knit applies credentials to child Git processes and forge API calls without changing the parent environment or saved Git remotes. Token-backed SSH URLs use HTTPS for that operation. Git 2.31 or newer is required. Network operations with repository-location overrides (`-C`, `--git-dir`, `--work-tree`) are rejected when project credentials are configured; run from the intended checkout. Bare Git commands outside Knit keep their existing authentication.

## Plain Git inside Knit-managed checkouts

Ordinary `git fetch`, `git pull`, and `git push` — no Knit wrapper — resolve the same selection Knit uses: the repository's explicit assignment, else the exact-host default. For each Git directory it manages (a source checkout's `.git` and every linked bundle worktree's own directory), Knit links one owned include from the shared repository config via a conditional `includeIf.gitdir:` match, so each checkout keeps its own `knit-credentials.inc` and its own resolved context — including source checkouts that live outside the workspace, whose generated helper pins their workspace/project association. The generated file holds no secrets: for the exact repository targets Knit takes over it resets inherited credential helpers and inherited HTTP auth settings (an exact-URL `http.*.extraHeader` can never outrank the selected token), installs the dynamic helper (`knit auth git-credential --resolve`), and for each selected SSH remote adds an exact `url.<https>.insteadOf` rewrite — the saved remote URL itself is never changed. Remotes Knit does not take over keep their existing helpers and SSH configuration, and removing the generated entries restores inherited behavior completely.

The helper resolves per request from the checkout's own Knit context, so token rotation, `knit auth default` changes, and per-project overrides apply immediately without regeneration. Requests Knit cannot honor — a malformed request, a missing assignment the project requires, or a broken explicit credential — fail closed: the helper answers `quit=1`, stopping Git's remaining helpers and its password prompt, instead of letting a stale cached or ambient credential answer. `git credential store`/`erase` requests are ignored; the personal Knit store is the only token storage.

Activation is automatic: the `knit auth` flows, `knit project add`, and bundle worktree materialization install or refresh the include. **Existing installs activate with `knit auth status`** — noninteractive, using already-saved credentials, no token re-entry; it also refreshes the helper path after a Knit upgrade. A refresh touches only the current project's source checkouts and its own materialized bundle worktrees, so two projects sharing one external repository cannot disturb each other's credentials. The installer only reads credential metadata, never tokens.
