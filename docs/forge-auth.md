# Project-aware forge credentials

Knit stores forge credentials locally and maps them to project repositories. Setup is native to Knit: no hosted account or desktop app is required. The sync remote token authorizes ledger access separately; it is never used as a forge token.

## Default tokens first: `knit auth`

Bare `knit auth` manages your personal **default tokens** — one per forge, used for *everything* on that forge (every project, clone, fetch, and push, Git and forge API alike) unless a project overrides it. No project, workspace, or repository selection is involved:

```sh
knit auth          # pick a forge, paste one hidden token, repeat, Enter to finish
```

The first token saved for a forge becomes its default automatically; adding a second token never displaces a chosen default, and with several tokens on one forge `knit auth default NAME` picks the default explicitly. A deliberate update in the wizard rotates the default token in place (a pasted replacement always becomes a local secret — an environment reference is cleared so the two sources never compete). `knit auth list` and `knit auth status` show which token serves what, and whether it is a chosen default or the only token on its forge.

After configuring the sync remote separately, save your forge defaults and clone:

```sh
knit auth                          # GitHub and Bitbucket tokens, once
knit clone demo --remote hosted
```

**`knit auth --project NAME`** configures one project: per forge the project uses, choose `Enter` to use the shared default token (clearing only that project's overrides, so inheritance works) or `t` for a **project-only token** — saved as a new local credential bound to just that project's repositories on that forge. Project tokens are *scoped*: they never become the host's implicit global default, and the global default and its secret are never touched. Switching back to the default clears only the selected project's override; other projects and the defaults are unaffected.

Resolution order everywhere: an explicit repository assignment (or operation selection) wins over the host default; the host default wins over ambient Git access; a host with no resolvable default — or with several tokens and no explicit choice — guesses nothing. Hostless or ambiguous targets are never matched speculatively.

For a new workspace without defaults, run the ordinary clone command: `knit clone <project> --remote <name>` runs the guided setup itself, using the project's declared groups (see [Project-defined authentication requirements](#project-defined-authentication-requirements)), and stores local tokens before the first private fetch. To configure an existing workspace, or to edit its assignments later, run:

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

Groups are requirements on what to set up, not enforcement of a specific credential: `knit auth add` and `knit auth use` remain available for deliberate overrides (a second credential on the same host, machine-specific access, or splitting a group's repositories across credentials). A project without groups — or with `{"groups": []}` — still uses local defaults; advanced setup allows manual assignments. A malformed group declaration fails setup loudly with the offending group and repository instead of silently falling back.

### Permission limitations

`auth status --check` performs a Git read probe (`git ls-remote`) only. A green check never proves API, push, pull-request, or merge access, regardless of what a group's `permissions` text recommends; publishing and landing validate access through the real forge operations. `permissions` is guidance for token creation, not something Knit enforces or verifies.

## Scriptable setup and repository overrides

For scripted default setup, the first credential added for a host becomes its default automatically. Use `auth use` only when you want an explicit repository override:

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

These are example repository IDs. `auth use` requires `--repo`, which can be repeated; every selected repository must match the credential's host. For convenient bulk selection, use `all` in the setup wizard. Named credentials can also be reused deliberately across projects.

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
- Resolution order is explicit assignment (or operation selection) > the host's default token > a recorded ambient allowance: the exact remote of a repository that was cloned or verified without any Knit credential — public HTTPS, or a working SSH key — and that no declared auth group covers. The allowance is revoked when the repository's URL changes, until the new remote works again. Once a project has assignments, its other forge repositories need one of the three. Unknown targets without any, missing credentials, and authentication failures stop the operation. Knit does not retry a selected credential with a broader ambient token, and a host with several tokens but no chosen default is never guessed. Local filesystem remotes continue working.
- Knit applies credentials separately to Git and forge API operations. Parallel operations do not change the parent process's token environment. Git uses a temporary credential helper and a repository-scoped authorization header supplied through the child environment; SSH forge URLs are translated to HTTPS for that invocation when a token is assigned. Saved remotes and Git config are unchanged. This requires Git 2.31 or newer (`--config-env` support); older versions fail rather than using another credential.
- Repository-location overrides (`-C`, `--git-dir`, `--work-tree`) on network commands are rejected when project credentials are configured. Run Knit from the intended checkout instead.
- Bare Git commands outside Knit retain their existing authentication. Project assignments apply to Knit operations, including `knit git`.

Credential metadata, host defaults, scoping markers for project-only tokens, and assignments live in `forge-auth.json` next to your user-level Knit config. Pasted tokens live in `forge-secrets.json` in that same directory; this is a **private plaintext file, not an encrypted vault**. On Unix the directory is mode `0700` and files are `0600`; on Windows they inherit the user's directory access controls. The location follows `KNIT_HOME`, `XDG_CONFIG_HOME`, and the same platform fallbacks as Knit config. Prefer environment references when a secret manager already supplies your tokens. Tokens are not accepted as command-line arguments, printed by status/list, or written to portable artifacts.

An assignment limits where Knit uses a token. It does not reduce a classic token's underlying forge permissions. Organization repositories can often use fine-grained tokens; outside-collaborator arrangements and some operations may require classic tokens. See [GitHub's token documentation](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens).

For read-only use, grant repository read permissions. For GitHub publish/land, grant Contents and Pull requests read/write plus Metadata read; collaborator management is a separate administrative permission. For Bitbucket, GitLab, and Forgejo, grant the corresponding repository and review permissions for the operations you use.

## Cloning a private project before it exists locally

`knit clone` sets up credentials itself, before any private repository is
fetched. What happens depends on what the project declares:

**A default token covers the forge.** With a host default saved, every group
on that host is served by it — same-host groups share the default with no
prompts and no per-repository bindings, and switching the default later
applies everywhere. A working default is never re-classified or ignored
because a group recommends a different token kind; groups guide *missing*
access only. Deliberate per-repository assignments keep winning over the
default.

**The project declares credential groups** (see above). In a terminal, the
clone walks each group that is not already covered: several compatible saved
credentials (no default among them) get one numbered answer with a
new-token option; a group with nothing saved gets one hidden token prompt,
and the first token entered becomes that host's default — with no naming
choreography and, again, no per-repository bindings. A credential the forge
rejects is repaired the moment the clone sees the denial: one hidden
replacement token, saved as a **new local credential bound only to the
affected repositories** — the shared default's token, environment reference,
and default status are never touched by an automatic repair (deliberate
global rotation stays `knit auth add NAME --replace` or the bare-wizard
update), and the denied repositories are retried once. Repositories no group
covers keep the access they already have: on a host with a default the
default serves them too — including public repositories, intentionally; on a
default-less host, public HTTPS and SSH-working repositories are never asked
for a token and are never locked out by the assigned-credentials gate.
Noninteractive clones (including `--json`) never prompt: with defaults
saved they just clone; otherwise the groups are printed with the scriptable
recovery path (`knit auth`, `knit auth setup`, or `knit auth add` +
`knit auth use`, then `knit pull --bundles` in the new workspace).

**The project declares nothing.** Public and SSH-accessible repositories
clone exactly as before. A private repository without working access can no
longer hang Git on a username prompt — Knit disables raw terminal prompts
for its own clone children — and in a terminal the failure itself enters the
same guided setup, inferred per forge host: a unique compatible saved
credential is linked without a prompt, otherwise one hidden token is asked
per host and the failed repositories are retried once. Noninteractive clones
fail fast with the recoverable-workspace instructions. Nothing is invented:
inferred groups carry no permissions and no required token kinds — for
Bitbucket you choose the token kind, and an Atlassian API token asks for the
account email, so authentication is right without any other presumption.

Recovery works in the existing checkout. `knit pull --bundles` reconciles
missing repositories (including older partial clones whose failed
repositories were never recorded locally — the pending map carries them) and,
in a terminal, runs the same guided setup before cloning: declared groups
first, inferred hosts after, and a rejected link or default is repaired with
one hidden replacement token saved as a new scoped local credential bound
only to the affected repositories. Existing checkouts, dirty or clean, are
never touched.

An interrupted older clone (a legitimate checkout plus empty `.knit`
scaffolding, no workspace yet) is resumable with the ordinary command — no
flags, no new directory: `knit clone <project> --remote <name>` into the same
directory adopts checkouts whose directory name is the repository's own id
and whose origin matches exactly that repository (SSH and HTTPS forms are
equivalent, and the directory must be a real checkout root, not a folder
inside some parent repository), keeps their branch and working tree
untouched, and finishes the workspace. Unrelated files, a checkout of some
other repository, symlinked scaffolding, or an already-configured workspace
are refused before anything is written.

Repositories that clone with working ambient access (public HTTPS, or an SSH
key) keep it: once a project has assignments, its strict assigned-credentials
gate still lets them through, because their exact remote is recorded as
ambient access at clone/recovery time. A URL change revokes the recording
until the new remote works again.

The ordinary clone already covers private repositories: its guided setup
stores local tokens per declared group (or inferred host) and repairs a
rejected credential with a project-only token — a sync remote that answers its
connected-forge lookup with 403 does not change that. For automation, or as
a deliberate override, `--credential` selects saved credentials by name; it
is repeatable and host-scoped — a project whose repositories live on
several forges selects one credential per host:

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
- When the selection — or the guided setup's grouped assignments — covers
  every forge repository being cloned, the hosted credential-helper lookup is
  skipped entirely.
- A target the selection does not cover keeps the clone workspace's own
  grouped assignments; the guard is pinned to the clone target, so the
  surrounding workspace's credentials are never borrowed.
- If a selected credential's token is denied, the failed repository names the
  credential and how to update it; there is no silent fallback to another
  credential.
- After a successful clone, each cloned repository is assigned to the selected
  credential covering its host in your personal assignment store, exactly as
  `knit auth use` would have written — the new workspace works with `knit
  auth status` without a setup pass. A repository the guided setup already
  linked keeps that mapping; the explicit selection never overwrites grouped
  assignments. Assignments and tokens are never synced.

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

Clearing an assignment restores the host default when one exists, otherwise the project's permitted ambient access. Rotation and removal affect local Knit storage; revoke a token on its forge when it should no longer be valid.
