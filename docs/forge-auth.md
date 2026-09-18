# Forge credentials

Knit keeps your GitHub, Bitbucket, GitLab, and Forgejo tokens locally. Your sync remote token authorizes the ledger separately; it is never used as a forge token.

## Default tokens: the normal setup

```sh
knit auth                         # choose a forge, paste its token, repeat
knit clone demo --remote hosted
```

Press Enter at the forge menu to finish. Your default token for each **exact host** serves every project, clone, pull, push, and forge API operation unless you choose an override. No repository selection or credential flags are needed. Bitbucket asks which token kind you have; an Atlassian API token also needs your account email.

The first regular token saved for a host becomes its default. Adding another never displaces it. A legacy store with exactly one unscoped token inherits that token as its default; with several tokens, choose explicitly using `knit auth default NAME`. The bare wizard can also create a new default or deliberately replace the current default's secret. Replacing an environment-backed default with a pasted token clears its environment reference.

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
- **m:** Bitbucket only — repair the saved token's kind/email for the selected repositories: same secret, saved as a copy scoped to those repositories while the original keeps its token and default. Repositories are grouped by their actually resolved credential and repaired per credential. No token is pasted.
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

Credential coverage means a saved credential resolves; it does not verify access. When Knit selects one, its Git subprocess clears inherited credential helpers and sends that credential over HTTPS, including when the original URL uses SSH. A successful ordinary Git clone outside Knit may therefore use a different identity or transport. Check that difference before replacing a token. Clearing a repository assignment still leaves any host default in effect.

For Bitbucket, the recorded token type determines Git authentication: `atlassian_api_token` uses `x-bitbucket-api-token-auth`, while `access_token` uses `x-token-auth`. The account email is needed for API-token REST calls, not to select the Git username. Older credentials without a recorded type retain their previous username-based behavior until classified. See Atlassian's [API token](https://support.atlassian.com/bitbucket-cloud/docs/using-api-tokens/) and [access token](https://support.atlassian.com/bitbucket-cloud/docs/using-access-tokens/) documentation.

When access is missing, interactive clone guides you through declared groups or infers a host from a failed Git operation. New regular tokens become host defaults. Existing project-only tokens are not automatically reused for another project. Noninteractive clones never prompt and explain how to finish setup.

If a credential is rejected, recovery saves a **new project-only credential for the affected repositories**, leaving the old secret, environment reference, and default untouched. Failed repositories are retried once. Use `knit pull --bundles` to reconcile missing repositories in an existing workspace.

When a repository was recovered with ordinary Git instead — cloned into its expected project path, which supplies the missing checkout — no failed clone retry opens the guided menu above. Run `knit auth setup --repo <id>` on the existing checkout and choose `m` for Bitbucket to repair the saved credential's metadata in place. The clone itself is not an authentication fix: plain Git in Knit-managed checkouts goes through the saved credential once the helper is installed or refreshed there. `knit auth clear` removes assignments only and leaves the host default in effect; deleting a credential is not recovery, since declared groups and other assignments still require credentials.

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
