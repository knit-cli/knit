# ChangeGroup Bundle Schema

Knit writes a bundle as a language-neutral JSON file:

```txt
.knit/bundles/<slug>.bundle.json
```

The user-facing name is bundle. The technical schema type is `ChangeGroup`.

## Top-Level Fields

```json
{
  "schemaVersion": "0.1",
  "kind": "ChangeGroup",
  "id": "venue-capacity",
  "title": "venue capacity",
  "createdAt": "2026-05-05T00:00:00.000Z",
  "updatedAt": "2026-05-05T00:00:00.000Z",
  "headNodeId": "kg_20260505_abc123",
  "repos": [],
  "commitGroups": [],
  "nodes": []
}
```

- `repos` is the current tracked repo state.
- `commitGroups` is the compatibility list of logical cross-repo commits.
- `nodes` is the append-only-ish feature ledger.
- `headNodeId` points at the latest ledger node.

## Repo Entry

```json
{
  "id": "backend",
  "path": "/absolute/path/to/backend",
  "remote": "git@github.com:org/backend.git",
  "baseBranch": "main",
  "baseSha": "000aaa",
  "featureBranch": "knit/venue-capacity",
  "worktreePath": ".knit/worktrees/venue-capacity/backend",
  "headSha": "abc123"
}
```

`path` is absolute. `worktreePath` is relative to the Knit workspace.

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
- `repo.removed`

`commit.group` nodes include `commitGroupId`, `message`, `commits`, and `repoChanges`. `git.observed` nodes include `repoChanges`. Repo/worktree nodes include `repoIds`.

`repoChanges` records how a repo moved:

```json
{
  "repoId": "frontend",
  "beforeSha": "abc123",
  "afterSha": "def456",
  "commits": ["def456"]
}
```

Gloss should treat the bundle as read-only input. Gloss can analyze the current `headNodeId`, a specific `commit.group` node, or the full current bundle.
