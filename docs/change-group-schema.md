# ChangeGroup Bundle Schema

Knit writes a bundle as a language-neutral JSON file:

```txt
.knit/bundles/<slug>.bundle.json
```

The user-facing name is bundle. The technical schema type is `ChangeGroup`.

Project-wide history is stored separately from bundle artifacts. Local history events live in `.knit/history/<project>.history.jsonl` and KnitHub stores the same event shape in its backend. Those events point back to bundle nodes and Git commit SHAs; they do not store patches or file contents.

## Top-Level Fields

```json
{
  "schemaVersion": "0.1",
  "kind": "ChangeGroup",
  "id": "venue-capacity",
  "title": "venue capacity",
  "state": "open",
  "createdAt": "2026-05-05T00:00:00.000Z",
  "updatedAt": "2026-05-05T00:00:00.000Z",
  "headNodeId": "kg_20260505_abc123",
  "repos": [],
  "commitGroups": [],
  "nodes": [],
  "publications": []
}
```

- `repos` is the current tracked repo state.
- `commitGroups` is the flat list of logical cross-repo commits.
- `nodes` is the append-only-ish feature ledger.
- `headNodeId` points at the latest ledger node.
- `publications` records provider metadata for published tracked branches.
- `state` is `open`, `closed`, `archived`, or `deleted`; the timestamp fields are present when that lifecycle transition has happened.

## Repo Entry

```json
{
  "id": "backend",
  "path": "/absolute/path/to/backend",
  "remote": "git@github.com:org/backend.git",
  "baseBranch": "main",
  "checkoutMode": "worktree",
  "baseSha": "000aaa",
  "featureBranch": "knit/venue-capacity",
  "worktreePath": ".knit/worktrees/venue-capacity/backend",
  "headSha": "abc123"
}
```

`path` is absolute. `checkoutMode` is `worktree` for generated Knit worktrees or `inPlace` for operating in the original repo checkout. `worktreePath` is relative to the Knit workspace in `worktree` mode and is the original absolute repo path in `inPlace` mode.

`baseSha` is the starting commit for the repo's feature branch. `headSha` is the last feature-branch tip recorded by Knit. When actual git `HEAD` differs from `headSha`, Knit reports unrecorded git commits and `knit sync` records them.

## Commit Group

```json
{
  "id": "kg_20260505_abc123",
  "message": "Add venue capacity integration",
  "createdAt": "2026-05-05T00:00:00.000Z",
  "commits": [
    {
      "repoId": "backend",
      "sha": "abc123"
    }
  ]
}
```

Knit creates the repo commits sequentially, but records them as one logical group.

## Nodes

Node shape is intentionally simple:

```json
{
  "id": "kg_20260505_abc123",
  "type": "commit.group",
  "createdAt": "2026-05-05T00:00:00.000Z"
}
```

Current node types:

- `feature.created`
- `repo.added`
- `worktree.materialized`
- `commit.group`
- `git.observed`
- `land.update`
- `revert.group`
- `feature.landed`
- `pr.revert`
- `feature.closed`
- `repo.removed`
- `check.recorded`

`commit.group` nodes include `commitGroupId`, `message`, `commits`, and `repoChanges`. `revert.group` nodes include the same fields plus `targetNodeId`, pointing at the bundle node that was reverted. `git.observed` nodes include `repoChanges`. `land.update` nodes include `provider` and `repoChanges` for feature-branch updates performed during landing preparation. Repo/worktree nodes include `repoIds`. `feature.landed` nodes include `planId`, `runId`, `provider`, `repoIds`, and `publicationUrls`. `pr.revert` nodes include `targetNodeId`, `provider`, `repoIds`, and the newly created revert PR `publicationUrls`. `feature.closed` nodes include an optional `reason`. `check.recorded` nodes carry the check name in `title`, the verdict in `message` (a machine-parsable `pass`/`fail` prefix followed by detail), and the per-repo head pins the verdict applies to in `commits`.

`repoChanges` records how a repo moved:

```json
{
  "repoId": "frontend",
  "movement": "advanced",
  "beforeSha": "abc123",
  "afterSha": "def456",
  "commits": ["def456"]
}
```

Movement values:

- `advanced`: the branch moved forward from `beforeSha` to `afterSha`; `commits` lists newly added commits.
- `rewound`: the branch was reset to an ancestor; `droppedCommits` lists commits no longer reachable from the branch head.
- `diverged`: the branch was rewritten to a different line; `commits` lists new commits and `droppedCommits` lists replaced commits.

Rewind example:

```json
{
  "repoId": "frontend",
  "movement": "rewound",
  "beforeSha": "def456",
  "afterSha": "abc123",
  "commits": [],
  "droppedCommits": ["def456"]
}
```

## Publications

`knit publish create` and `knit publish sync` record review-object (PR/MR) metadata in `publications`:

```json
{
  "repoId": "backend",
  "provider": "github",
  "kind": "pull_request",
  "number": 123,
  "url": "https://github.com/org/backend/pull/123",
  "baseBranch": "main",
  "headBranch": "knit/venue-capacity",
  "state": "OPEN",
  "title": "venue capacity (backend)",
  "updatedAt": "2026-05-05T00:00:00.000Z"
}
```

Publication metadata is publishing state, not code state. Git branches, SHAs, and bundle nodes remain the source of truth for what changed. Knit uses this field to sync the managed cross-link block in each review body. The `provider` and `kind` identify the host adapter and review object: `github`/`pull_request`, `gitlab`/`merge_request`, or `forgejo`/`pull_request`. Knit records at most one review object per repo per bundle.

The `baseBranch` field is the review target recorded by the provider. `knit land` uses that metadata, so a review object lands into the same base branch shown on its host.

## Project History Events

Project history events are metadata-only records derived from bundle ledgers. They include fields such as `eventId`, `projectId`, `kind`, `bundleId`, `repoId`, `branch`, `commit`, `beforeSha`, `afterSha`, `nodeId`, `nodeType`, `commitGroupId`, `occurredAt`, and `recordedAt`.

Knit uses deterministic event ids so local JSONL history and KnitHub history can be merged idempotently. `knit related` joins Git path history to these events by commit SHA, then expands the result to the related bundle and commit-group context.

## Merge Runs

`knit merge` writes operational run files under `.knit/merge-runs/`:

```json
{
  "schemaVersion": "0.1",
  "kind": "KnitMergeRun",
  "id": "merge_20260511_ab12cd",
  "source": "feature-x",
  "into": "staging",
  "manual": false,
  "status": "succeeded",
  "sourceBundleId": "feature-x",
  "createdAt": "2026-05-11T00:00:00.000Z",
  "updatedAt": "2026-05-11T00:00:00.000Z",
  "steps": [
    {
      "repoId": "backend",
      "repoPath": "/repos/backend",
      "sourceRef": "knit/feature-x",
      "target": "staging",
      "targetKind": "branch",
      "checkoutPath": ".knit/merge-worktrees/staging/backend",
      "beforeSha": "abc123",
      "afterSha": "def456",
      "status": "succeeded",
      "pushedAt": "2026-05-11T00:10:00.000Z",
      "pushedSha": "def456",
      "pushRemote": "origin"
    }
  ]
}
```

Merge run files are operational logs. For branch targets, the branch checkout is stored under `.knit/merge-worktrees/<target>/<repo>/`. `knit merge --push` and `knit merge push` record push state on each branch-target step. For bundle targets, a successful run advances the target bundle's feature branches and appends a `git.observed` node to the target bundle.

## Schemas And Migration

Knit ships JSON Schema files under `schemas/` and prints them with `knit schema print <name>`. The Rust models remain the source of truth for reading and writing workspace files. `knit migrate` rewrites older additive JSON files into the current shape, while `knit migrate --check` reports files that would be changed.

## Landing Plans And Runs

`knit land plan` writes an editable plan under `.knit/land-plans/`:

```json
{
  "schemaVersion": "0.1",
  "kind": "KnitLandPlan",
  "id": "land-venue-capacity",
  "provider": "github",
  "bundleId": "venue-capacity",
  "sourceProjectId": "venues",
  "createdAt": "2026-05-05T00:00:00.000Z",
  "steps": [
    {
      "id": "merge-backend",
      "type": "merge_pr",
      "repoId": "backend",
      "method": "merge",
      "waitForChecks": true,
      "requiredChecksOnly": true,
      "deleteBranch": false
    },
    {
      "id": "deploy-backend",
      "type": "deploy",
      "repoId": "backend",
      "deploymentMode": "command",
      "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
      "command": ["bin/deploy", "staging"],
      "needs": ["merge-backend"]
    }
  ]
}
```

Project files can carry a reusable `landing` template with merge priority, merge defaults, and deployment entries. The generated land plan is still bundle-local and editable, so a one-off release can alter dependencies, commands, checkout branches, or deployment modes without changing the project default. `deploy` steps are structured: `deploymentMode: "command"` runs an argv command, while `deploymentMode: "push"` records a deployment triggered by the merge itself.

`knit land apply` and `knit land resume` write run logs under `.knit/land-runs/`. Run files are operational logs, not the source of truth for code state. The bundle records a `feature.landed` summary node after every step succeeds; successful `knit land apply` then archives the bundle with a `feature.archived` node while preserving the landed node for log selectors and provider reverts. A later `knit revert <feature.landed> --apply` can create provider-native revert PRs across the landed repos and records that follow-up review group as `pr.revert`.

Gloss should treat the bundle as read-only input. Gloss can analyze the current `headNodeId`, a specific `commit.group` node, or the full current bundle.
