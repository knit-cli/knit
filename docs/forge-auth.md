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

## Cloning a private project before it exists locally

`knit auth setup` and `knit auth use` need a local project to link repositories
into, but `knit clone` creates the project only after Git has already fetched
the repositories. When your sync remote token cannot export forge credentials
(the remote answers its connected-forge lookup with 403), save a personal
credential first and select it by name for that one clone:

```sh
knit auth add work-github --provider github
knit clone team-project --remote hosted --credential work-github
```

`--credential` is repeatable and host-scoped: a project whose repositories
live on several forges selects one credential per host.

```sh
knit auth add work-github --provider github
knit auth add team-bitbucket --provider bitbucket
knit clone team-project --remote hosted \
  --credential work-github --credential team-bitbucket
```

What the selection does:

- Credentials are validated before anything changes: unknown names, two
  selected credentials for the same host, and credentials whose token is not
  currently resolvable (unsaved token, unset environment reference) fail
  before the export is fetched or the target directory is created.
- The selection applies only to the repository URLs this clone actually
  clones, matched by exact host and path, and only to Git: the sync remote's
  API keeps using its own token. SSH remote URLs are rewritten to HTTPS for
  that invocation, as with project assignments.
- Hosts without a selected credential keep their existing behavior (SSH keys,
  installed helpers, public access) and are reported as uncovered. A
  `--repo`-scoped clone does not need credentials for repositories outside its
  scope: cloning the GitHub set does not require a Bitbucket token.
- When the selection covers every forge repository being cloned, the hosted
  credential-helper lookup is skipped entirely.
- If a selected credential's token is denied, the failed repository names the
  credential and how to update it; there is no silent fallback to another
  credential.
- After a successful clone, each cloned repository is assigned to the selected
  credential covering its host in your personal assignment store, exactly as
  `knit auth use` would have written — the new workspace works with `knit
  auth status` without a setup pass. Assignments and tokens are never synced.

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
