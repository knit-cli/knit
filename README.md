# Knit

Knit is a local-first Rust CLI for authoring and coordinating multi-repo feature work. Think of it as "git for cross-repo feature work": it keeps a small bundle of related repositories, creates coordinated worktrees, commits staged changes across those worktrees, and records the result in a language-neutral JSON artifact.

Knit shells out to `git` in v0. It does not use libgit2 and it does not try to replace git.

For local development:

```sh
cargo install --path .
```

After that, `knit` should be available anywhere your shell can see `~/.cargo/bin`.

## Knit And Gloss

Knit and Gloss share a single artifact:

- User-facing name: bundle
- Technical schema type: `ChangeGroup`
- File name pattern: `<slug>.bundle.json`

Knit owns authoring and workspace mechanics: local repos, worktrees, feature branches, commit groups, and bundle updates.

Gloss reads a bundle later and produces review/ranking/explanation output. Gloss should not own worktrees, commits, reverts, or branch lifecycle.

## Storage

Knit stores local state under the directory where `knit init` runs:

```txt
.knit/
  config.json
  bundles/
    <slug>.bundle.json
  revert-plans/
    <node-id>.json
  worktrees/
    <slug>/
      <repo-name>/
```

The bundle file is the source of truth. `config.json` only tracks the active bundle for convenience.

## Quickstart

From a workspace folder that sits beside your local repos:

```sh
knit init "venue capacity"
knit add ../backend ../frontend ../scraper
```

Make changes inside the generated worktrees, stage the files with git, then inspect and commit:

```sh
knit status
knit stage
knit commit -m "Add venue capacity integration"
knit log
```

For a one-step stage-and-commit:

```sh
knit commit --stage -m "Add venue capacity integration"
```

The active bundle is printed by `knit init` and lives at:

```txt
.knit/bundles/venue-capacity.bundle.json
```

## Commands

```sh
knit init "<title>" [--force]
knit add <repo-path>... [--base <branch>] [--in-place] [--no-worktree]
knit remove <repo-id>...
knit worktree
knit stage
knit status
knit diff [--stat] [repo-id-or-path...]
knit sync
knit commit -m "<message>" [--stage]
knit log [-<count>]
knit log [-n [count]]
knit revert <sha|node|HEAD|HEAD~N> [--plan]
knit revert <sha|node|HEAD|HEAD~N> --apply
knit git [--repo <repo>] [--all] <git-args...> [repo-selector...]
knit show <commit-group-id>
```

`knit add` accepts one or more repo paths. It resolves all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, inferred base branch, and checkout mode. By default it creates the `knit/<bundle-id>` branch and a generated worktree for each added repo. Use `--no-worktree` for metadata-only registration.

Use `--in-place` to make Knit operate directly in the original repo checkout instead of creating `.knit/worktrees/<bundle>/<repo>`. Knit will create or check out the `knit/<bundle-id>` branch in that repo. The original checkout must be clean before Knit switches branches. Later mutating commands refuse to operate if the in-place repo is no longer on the expected feature branch.

Base inference prefers the current branch only when it is clean and named `main` or `master`; otherwise it looks for `main`, then `master`. Use `--base` when that is not right.

`knit worktree` is still available as an idempotent repair/rerun command. It creates missing `knit/<bundle-id>` branches and worktrees under `.knit/worktrees/<bundle-id>/<repo-id>`. Existing branches or worktrees are reported and reused where possible.

`knit stage` runs `git add -A` in every tracked checkout. `knit status` shows ordinary git status, checkout mode, wrong-branch warnings for in-place repos, and unrecorded commits when a tracked branch moved outside Knit.

`knit diff` shows cross-repo diffs against each repo's recorded `baseSha`. It includes committed, staged, and unstaged tracked-file changes in the current checkout. Use `--stat` for a compact summary, or pass repo ids/paths to limit the output:

```sh
knit diff
knit diff --stat
knit diff backend
knit diff --stat ../backend
```

`knit sync` records commits that happened outside Knit as `git.observed` nodes and advances each affected repo's remembered `headSha`. `knit log` shows both Knit commit groups and observed git movement from the node ledger. Use `knit log -2` for the latest two log entries. `knit log -n 3` also works, and `knit log -n` defaults to the latest ten.

`knit revert <target>` resolves bundle log selectors like `HEAD`, `HEAD~1`, full node ids, unique node id prefixes, and git commit SHAs shown in `knit log`. A commit SHA resolves to the latest bundle node that mentions that commit, so if a commit was later observed as dropped by a reset, reverting by that SHA restores it from the latest rewind node. By default it writes a checked revert plan under `.knit/revert-plans/` and prints the per-repo operations. `knit revert <target> --apply` requires that plan to exist, verifies each affected worktree is still clean and at the planned head, then creates one revert commit per affected repo and appends a `revert.group` node.

Revert behavior is based on the target node:

- `commit.group` and `revert.group`: revert the recorded commits.
- `git.observed` with `advanced`: revert the observed commits.
- `git.observed` with `rewound`: cherry-pick the dropped commits back.
- `git.observed` with `diverged`: revert added commits, then cherry-pick dropped commits.

`knit git` passes arguments directly to git in tracked bundle worktrees. With no repo selector it runs against every tracked repo:

```sh
knit git status
knit git status --short
knit git status --short backend
knit git status --short ../backend
knit git status --short '*'
knit git --repo backend diff --stat
```

Repo selectors can be repo ids, original repo paths, or worktree paths. Quote `'*'` when you want Knit to receive the literal all-repos selector instead of your shell expanding it. If a git argument is ambiguous with a repo id, use `--repo`.

Knit colors interactive terminal output for scanability. It disables color automatically when output is piped, when `NO_COLOR` is set, or when `TERM=dumb`. Use `KNIT_COLOR=always` or `KNIT_COLOR=never` to force a mode.

If a tracked branch is reset backward, `knit status` reports rewound commits and `knit sync` records a `git.observed` node with `movement: "rewound"` and `droppedCommits`. Existing `commit.group` nodes remain as history; current state is derived from each repo's latest `headSha`.

`knit commit` commits only repos with staged changes in their bundle worktrees. With `--stage`, it stages first and then commits. `knit commit` also syncs unrecorded git commits before creating a new logical commit group, so the ledger remains ordered.

The git commits are created sequentially, one repo at a time. Knit records them as one logical commit group in the bundle. Every repo commit gets the same logical message plus trailers:

```txt
Knit-Group: <commit-group-id>
Knit-Bundle: <bundle-id>
```

The bundle records the full mapping from logical commit group to repo commit SHAs.

`knit remove <repo-id>...` removes repos from bundle tracking and appends a `repo.removed` node. It intentionally leaves existing git branches and worktrees in place in v0.

## Bundle Nodes

The bundle is a feature ledger. It stores current state in `repos` and `commitGroups`, and an ordered node chain in `nodes`.

Typical node types:

- `feature.created`
- `repo.added`
- `worktree.materialized`
- `commit.group`
- `git.observed`
- `revert.group`
- `repo.removed`

`headNodeId` points at the latest node. Gloss can inspect any node, but the most useful review usually comes from the current head or the final pre-PR bundle.

## V0 Limitations

- Knit v0 is not perfectly transactional. If one repo commit succeeds and a later repo commit fails, Knit reports the failure but does not roll back the earlier commit.
- `knit add` is atomic-ish for bundle writes, but branch/worktree creation can still partially succeed before a later git operation fails.
- Knit uses a simple `.knit/knit.lock` file to prevent concurrent bundle writes. If a process crashes, a stale lock may need manual removal.
- Worktree creation relies on `git worktree add` and inherits its constraints, including branch checkout conflicts.
- `knit commit` only looks for staged changes inside bundle worktrees.
- `knit revert --apply` preflights all affected repos before writing, but cross-repo revert commits are still created sequentially. If a conflict or commit failure happens after an earlier repo succeeds, inspect the affected repos manually before retrying.
- `knit revert` cannot restore historical `repo.removed` nodes yet because older bundle nodes did not store the full removed repo record.
- Bundle schema validation is currently serde-based, not a standalone JSON Schema file.
- Knit does not create GitHub PRs.
- Knit does not run LLMs, MCP servers, or review agents.

## Manual Test With Toy Repos

See [docs/manual-test.md](docs/manual-test.md) for a small two-repo smoke test.

See [docs/change-group-schema.md](docs/change-group-schema.md) for the current bundle fields.

## Code Layout

See [docs/architecture.md](docs/architecture.md) for the module boundaries and test layout. `src/main.rs` is intentionally only the binary entry point; command logic lives in `src/commands/`, schema types in `src/model.rs`, persistence in `src/store.rs`, and git subprocess helpers in `src/git.rs`.

## Roadmap

- Standalone JSON Schema for `ChangeGroup`
- Safer partial-failure recovery for multi-repo commits
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
