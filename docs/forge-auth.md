# Project-aware forge credentials

Give a project credentials, then explicitly link each forge repository to the credential Knit should use. Usually one credential serves many repositories; a project can also use several credentials on the same host or across different backends. Setup is native to Knit: no KnitHub account, sync remote, or Ivaldi installation is required.

```sh
knit auth setup
knit auth setup --project tools
```

`knit project auth` opens the same wizard:

1. Review the full project's repository → credential mapping and missing links.
2. Choose a saved credential or `new` (use `use NAME` for a credential named `new` or `done`). For a new credential, choose a project host; Knit infers public providers and asks for the backend on custom hosts. Supply an environment variable reference or enter a hidden token.
3. Select compatible repository IDs or numbers, separated by commas or spaces. `all` links every displayed repository on that host, across owners. Selected existing links are replaced; Enter cancels this selection.
4. Repeat for further credentials or reassignments, then type `done`. Missing links are reported as incomplete; saved links remain. When all links exist, you can optionally check Git read access.

`knit auth setup --repo api --repo web` limits editable repositories, including what `all` selects. The wizard still displays coverage and missing links for the entire project. Links specify where Knit uses credentials; they do not grant or change permissions on the provider. Choose tokens whose provider permissions cover the repositories and operations you intend to use.

## Project-defined authentication requirements

A project can declare what credentials its repositories need, so collaborators do not have to guess. The declaration lives in the project artifact (`knit.project.json`, and the workspace copy under `.knit/projects/`) as `auth.groups`, travels with the portable `knitProject` export, and is edited by project maintainers through their hosted project settings — never by end users setting up a clone:

```json
{
  "auth": {
    "groups": [
      {
        "id": "github-work",
        "name": "GitHub work token",
        "provider": "github",
        "host": "github.com",
        "repos": ["api", "web"],
        "tokenTypes": ["fine_grained_pat"],
        "permissions": ["contents:read", "pull_requests:write"],
        "instructions": "Create a fine-grained token in the org, scoped to both repositories.",
        "tokenUrl": "https://github.com/settings/personal-access-tokens/new"
      },
      {
        "id": "bitbucket-cloud",
        "name": "Bitbucket Cloud",
        "provider": "bitbucket",
        "host": "bitbucket.org",
        "repos": ["legacy"],
        "tokenTypes": ["atlassian_api_token", "access_token"]
      }
    ]
  }
}
```

Rules the declaration must satisfy:

- `id`, `name`, `provider`, `host`, `repos`, and `tokenTypes` are required; `repos` and `tokenTypes` are nonempty. Group ids are unique, and a repository id may appear in at most one group. Several groups may share a host.
- `provider` is one of `github`, `gitlab`, `bitbucket`, `forgejo`. `tokenTypes` come from that provider: GitHub `fine_grained_pat`/`classic_pat`; Bitbucket `atlassian_api_token`/`access_token`; GitLab `personal_access_token`/`project_access_token`/`group_access_token`; Forgejo `access_token`.
- Referenced repository ids must exist in the project and sit on the group's forge: the remote URL is authoritative for known hosts, the declared provider and host cover custom hosts. Ambiguous or unknown references are rejected rather than guessed.
- `permissions`, `instructions`, and `tokenUrl` are optional descriptive text. `tokenUrl` must be plain HTTPS without embedded credentials. Knit displays them but never executes instructions or opens links.
- Metadata never contains tokens or personal credential names, and unknown fields inside `auth` are rejected on import so credential material can never ride along. An explicit clear is `{"groups": []}`, which restores the no-recommendation behavior.

When groups exist, `knit auth setup` runs the guided flow instead of the open wizard: each group shows its name, token kinds, permissions, instructions, and token creation URL; you pick a compatible saved credential or create one (choosing from the group's accepted token kinds), review the current and proposed repository mapping, and only an explicit confirmation assigns anything. Groups are projected onto the repositories this workspace actually has — a scoped clone (`--repo`/`--view`) keeps the full portable definition but only prompts and maps for repositories present locally, and a group with no local repositories is reported and skipped. During `knit clone`, the guided setup runs before the first private Git fetch when a terminal is available; without one the clone reports each group and the `knit auth setup` / `knit auth add` + `knit auth use` path instead of hanging.

`knit auth status` includes each group's coverage (`authGroups` with `linked`/`missing` in `--json`, per-repo `authGroup` attribution, plus repositories no group covers). Missing coverage is allowed while drafting and reported, never silently assigned. A malformed declaration (unknown repository reference, off-host group, overlapping repositories) makes `status` fail with the offending group instead of rendering misleading coverage. Group hosts are matched case-insensitively; credentials created from a group store the lowercase host.

### Token kinds and older credentials

Saved credentials record which token kind they are (`--token-type` on `knit auth add`, or the guided flow's choice). The kind is never guessed from an opaque token value: selecting an older unclassified credential in the guided flow asks you to classify it once, and skipping the question honestly leaves the credential unclassified. Bitbucket credentials additionally keep their account email consistent with the kind — an Atlassian API token needs it, a repository/project/workspace access token must not carry one — and the guided flow repairs a mismatch (asking for a missing email, offering to clear a stale one) even for credentials that already carry a token kind. A credential whose recorded kind is not among a group's recommended types still works after an explicit confirmation; recommendations are defaults, not policy.

### Advanced overrides

Groups are requirements on what to set up, not enforcement of a specific credential: `knit auth add` and `knit auth use` remain available for deliberate overrides (a second credential on the same host, machine-specific access, or splitting a group's repositories across credentials). A project without groups — or with `{"groups": []}` — keeps the original unconfigured wizard exactly as before. A malformed group declaration fails setup loudly with the offending group and repository instead of silently falling back.

### Permission limitations

`auth status --check` performs a Git read probe (`git ls-remote`) only. A green check never proves API, push, pull-request, or merge access, regardless of what a group's `permissions` text recommends; publishing and landing validate access through the real forge operations. `permissions` is guidance for token creation, not something Knit enforces or verifies.

## Scriptable setup and repository overrides

For the common case, use one credential for several repositories, even when their owners differ:

```sh
knit auth add shared --provider github --token-env PROJECT_GITHUB_TOKEN
knit auth use shared --project tools --repo api --repo web --repo docs
```

For a project with different access requirements and backends, retain `shared` for `web` and `docs`, reassign `api`, and link a Bitbucket repository:

```sh
knit auth add restricted --provider github --token-env API_GITHUB_TOKEN
knit auth use restricted --project tools --repo api
knit auth add cloud --provider bitbucket \
  --username you@example.com --token-env PROJECT_BITBUCKET_TOKEN
knit auth use cloud --project tools --repo legacy
```

These are example repository IDs. Assign every forge repository in your project, including observed repositories you intend to use. `auth use` requires `--repo`, which can be repeated; every selected repository must match the credential's host. For convenient bulk selection, use `all` in the setup wizard. Named credentials can also be reused deliberately across projects.

Omit `--token-env` for an interactive hidden token prompt, or use `--token-stdin` to pipe a token from your secret manager. An environment reference must be available when Knit uses it; adding the reference does not require its value. For Bitbucket Atlassian API tokens, supply your account email with `--username`. For repository, project, or workspace access tokens, omit it. Other providers are `gitlab` and `forgejo`; use `--host git.example.com` for a self-hosted forge. Bitbucket support is Bitbucket Cloud.

Inspect or check access:

```sh
knit auth list
knit auth status --project tools
knit auth status --project tools --check
knit auth status --project tools --json
```

`status` shows assignments without network requests. `--check` runs `git ls-remote` against each forge repository with a 30-second timeout per repository. A successful check confirms Git read access only; it does not prove API, push, pull-request, or merge permissions. Publishing and landing still validate access through the actual forge operations. The check exits nonzero if an assignment, credential, or Git access probe fails.

## Selection and storage

- Explicit project selection wins. Otherwise Knit uses the selected bundle's project in a bundle worktree or with `--bundle`, then the workspace's active project. Creating a bundle with `--project` uses that project's assignments for its initial fetches.
- Assignments are personal to the workspace's canonical project artifact path. They are not committed or synced in `knit.project.json`, project exports, or bundle artifacts. On another machine or after moving a workspace, run setup again; named credentials on the same machine can be reused.
- A project with no assignments retains existing Git credentials, SSH, `gh auth login`, and provider environment behavior.
- Once a project has assignments, every forge repository it accesses needs an explicit assignment. Unknown targets, missing credentials, and authentication failures stop the operation. Knit does not retry a selected credential with a broader ambient token. Local filesystem remotes continue working.
- Knit applies credentials separately to Git and forge API operations. Parallel operations do not change the parent process's token environment. Git uses a temporary credential helper and a repository-scoped authorization header supplied through the child environment; SSH forge URLs are translated to HTTPS for that invocation when a token is assigned. Saved remotes and Git config are unchanged. This requires Git 2.31 or newer (`--config-env` support); older versions fail rather than using another credential.
- Repository-location overrides (`-C`, `--git-dir`, `--work-tree`) on network commands are rejected when project credentials are configured. Run Knit from the intended checkout instead.
- Bare Git commands outside Knit retain their existing authentication. Project assignments apply to Knit operations, including `knit git`.

Credential metadata and assignments live in `forge-auth.json` next to your user-level Knit config. Pasted tokens live in `forge-secrets.json` in that same directory; this is a **private plaintext file, not an encrypted vault**. On Unix the directory is mode `0700` and files are `0600`; on Windows they inherit the user's directory access controls. The location follows `KNIT_HOME`, `XDG_CONFIG_HOME`, and the same platform fallbacks as Knit config. Prefer environment references when a secret manager already supplies your tokens. Tokens are not accepted as command-line arguments, printed by status/list, or written to portable artifacts.

An assignment limits where Knit uses a token. It does not reduce a classic token's underlying forge permissions. Organization repositories can often use fine-grained tokens; outside-collaborator arrangements and some operations may require classic tokens. See [GitHub's token documentation](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens).

For read-only use, grant repository read permissions. For GitHub publish/land, grant Contents and Pull requests read/write plus Metadata read; collaborator management is a separate administrative permission. For Bitbucket, GitLab, and Forgejo, grant the corresponding repository and review permissions for the operations you use.

## Rotation and removal

```sh
# Rotates this named credential for every project using it:
knit auth add restricted --provider github --replace

# Reassign a repository:
knit auth use shared --project tools --repo api

# Remove assignments (omit --repo to clear the entire project):
knit auth clear --project tools --repo legacy

# Only unused credentials can be removed:
knit auth remove restricted
```

Clearing the last assignment restores the project's existing ambient authentication behavior. Rotation and removal affect local Knit storage; revoke a token on its forge when it should no longer be valid.
