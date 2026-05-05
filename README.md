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
knit add <repo-path>... [--base <branch>] [--no-worktree]
knit remove <repo-id>...
knit worktree
knit stage
knit status
knit commit -m "<message>" [--stage]
knit log
knit show <commit-group-id>
```

`knit add` accepts one or more repo paths. It resolves all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, and inferred base branch. By default it also creates the `knit/<bundle-id>` branch and the worktree for each added repo. Use `--no-worktree` for metadata-only registration.

Base inference prefers the current branch only when it is clean and named `main` or `master`; otherwise it looks for `main`, then `master`. Use `--base` when that is not right.

`knit worktree` is still available as an idempotent repair/rerun command. It creates missing `knit/<bundle-id>` branches and worktrees under `.knit/worktrees/<bundle-id>/<repo-id>`. Existing branches or worktrees are reported and reused where possible.

`knit stage` runs `git add -A` in every tracked worktree. `knit commit` commits only repos with staged changes in their bundle worktrees. With `--stage`, it stages first and then commits.

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
- `repo.removed`

`headNodeId` points at the latest node. Gloss can inspect any node, but the most useful review usually comes from the current head or the final pre-PR bundle.

## V0 Limitations

- Knit v0 is not perfectly transactional. If one repo commit succeeds and a later repo commit fails, Knit reports the failure but does not roll back the earlier commit.
- `knit add` is atomic-ish for bundle writes, but branch/worktree creation can still partially succeed before a later git operation fails.
- Knit uses a simple `.knit/knit.lock` file to prevent concurrent bundle writes. If a process crashes, a stale lock may need manual removal.
- Worktree creation relies on `git worktree add` and inherits its constraints, including branch checkout conflicts.
- `knit commit` only looks for staged changes inside bundle worktrees.
- Bundle schema validation is currently serde-based, not a standalone JSON Schema file.
- Knit does not create GitHub PRs.
- Knit does not run LLMs, MCP servers, or review agents.
- Knit does not implement automatic revert.

## Manual Test With Toy Repos

See [docs/manual-test.md](docs/manual-test.md) for a small two-repo smoke test.

See [docs/change-group-schema.md](docs/change-group-schema.md) for the current bundle fields.

## Code Layout

See [docs/architecture.md](docs/architecture.md) for the module boundaries and test layout. `src/main.rs` is intentionally only the binary entry point; command logic lives in `src/commands/`, schema types in `src/model.rs`, persistence in `src/store.rs`, and git subprocess helpers in `src/git.rs`.

## Roadmap

- `knit revert <group-id> --plan`
- `knit revert <group-id> --apply`
- Standalone JSON Schema for `ChangeGroup`
- Safer partial-failure recovery for multi-repo commits
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
