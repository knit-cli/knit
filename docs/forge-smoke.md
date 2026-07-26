# Forge smoke tests

`scripts/forge-smoke.sh` exercises the same private two-repository Knit bundle on
GitHub, GitLab, Codeberg/Forgejo, or Bitbucket Cloud:

```sh
scripts/forge-smoke.sh github --dry-run
scripts/forge-smoke.sh gitlab
scripts/forge-smoke.sh codeberg
scripts/forge-smoke.sh bitbucket
```

The harness creates two explicitly private repositories, verifies their
visibility through the forge API, clones them into an isolated temporary Knit
workspace, and runs bundle → commit → push → publish → live status → land. It
then verifies both reviews are merged and both `main` branches advanced. An
`EXIT` trap deletes the temporary repositories; `--keep` preserves them for
manual inspection.

Every Knit invocation uses a temporary `KNIT_HOME` and `GIT_CONFIG_GLOBAL`.
Tokens remain in environment variables and are supplied to Git through an
askpass helper whose file contains variable references, never credential
values. The harness does not print tokens.

## Authentication

- GitHub: authenticate the intended account with `gh auth login`; the harness
  checks `gh auth status`.
- GitLab: export `GITLAB_TOKEN` or `KNIT_GITLAB_TOKEN` with a personal access
  token carrying `api` scope. `glab` also reads `GITLAB_TOKEN`; the harness
  passes the same environment token to Git-over-HTTPS without storing it.
- Codeberg: export `KNIT_FORGEJO_TOKEN` with repository read/write access.
- Bitbucket: export `KNIT_BITBUCKET_EMAIL`,
  `KNIT_BITBUCKET_API_TOKEN`, and `BITBUCKET_SMOKE_WORKSPACE`. The Atlassian API
  token needs repository and pull-request write permissions.

`--dry-run` prints the plan and exits before credential checks or network
access. A normal run reports all missing commands or environment variables and
exits with status 2 before creating anything.

## Persistent manual fixtures

After the first successful live matrix, keep one private pair per forge named
`knit-forge-test-backend` and `knit-forge-test-frontend`. Record their
owner/namespace in a private operator note, not this public repository. A local
manual project can then be recreated without embedding account details:

```sh
knit init forge-test
knit project add backend /path/to/knit-forge-test-backend
knit project add frontend /path/to/knit-forge-test-frontend
knit bundle "manual forge retest"
```

## Last verified

All live runs used newly created private repositories, verified visibility
before cloning, retained the repositories with `--keep`, and kept credentials
in process environment variables. No live fixture was deleted.

| Forge | Date | Knit commit | CLI version | Result |
|---|---|---|---|---|
| GitHub | 2026-07-26 | bundle `forge-parity-live-smoke` (base `1d9bc3c`) | `gh 2.93.0` | pass: private create → status → land → API verify |
| GitLab | 2026-07-26 | bundle `forge-parity-live-smoke` (base `1d9bc3c`) | `glab 1.109.0` + REST | pass: private create → status → land → API verify |
| Codeberg/Forgejo | 2026-07-26 | bundle `forge-parity-live-smoke` (base `1d9bc3c`) | native REST (`tea 0.14.2` installed) | pass: private create → status → land → API verify |
| Bitbucket Cloud | 2026-07-26 | bundle `forge-parity-live-smoke` (base `1d9bc3c`) | native REST | pass: private create → status → land → API verify |

A separate retained four-repository Knit project was also verified with one
private repository per forge. One bundle created four review objects, live
status reported all four ready, and every review body contained the URLs of all
four reviews. Those reviews remain open intentionally; the cross-forge example
was not landed or tagged.
