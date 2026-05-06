# Architecture

Knit is split by responsibility so command behavior, persistence, git subprocesses, and pure helpers can be analyzed independently.

## Source Layout

```txt
src/
  main.rs       binary entry point only
  lib.rs        module wiring and command dispatch
  cli.rs        clap command definitions
  commands/
    mod.rs      command module wiring
    init.rs
    add.rs
    remove.rs
    worktree.rs
    stage.rs
    status.rs
    diff.rs
    sync.rs
    commit.rs
    log.rs
    revert.rs
    git_passthrough.rs
  checkout.rs   checkout mode helpers and in-place branch guards
  model.rs      bundle / ChangeGroup data structures
  store.rs      .knit config and bundle file persistence
  git.rs        git subprocess helpers
  ids.rs        slugs, commit group ids, SHA formatting
  paths.rs      path comparison helpers
  status.rs     git status classification
  time.rs       timestamp formatting
tests/
  ids.rs
  status.rs
```

Rust does not use classes in the TypeScript sense. The equivalent separation here is modules plus explicit data types. `model.rs` owns the long-lived schema types, including the `ChangeGroup` bundle and node ledger; each file in `commands/` coordinates one user-facing command with filesystem and git operations.

## Boundaries

- `main.rs` should stay tiny. It parses CLI arguments and calls `knit::run`.
- `cli.rs` should contain only argument shape and help text.
- Each file in `commands/` owns one user-facing command or tightly coupled command pair.
- `commands/mod.rs` should only re-export command entry points.
- `git.rs` is the only place that should construct raw `git` subprocess calls.
- `store.rs` is the only place that should load the active bundle from `.knit/config.json`.
- `checkout.rs` owns checkout path resolution, checkout mode labels, and in-place branch safety checks.
- Pure helper behavior should live in small modules and have integration tests under `tests/`.

## Testing

Cheap pure behavior belongs in integration tests:

- Slug and id behavior: `tests/ids.rs`
- Status classification: `tests/status.rs`

End-to-end git behavior is documented as a manual smoke test in [manual-test.md](manual-test.md). As Knit grows, that smoke test can become an automated integration test using temporary toy repos.

## Bundle Ledger

The bundle carries both current state and history:

- `repos`: current tracked repos, checkout modes, branches, and checkout paths.
- `commitGroups`: compatibility list of logical commits across repos.
- `nodes`: ordered ledger entries such as `feature.created`, `repo.added`, `worktree.materialized`, `commit.group`, `git.observed`, `revert.group`, and `repo.removed`.
- `headNodeId`: the latest node in the ledger.

Command files should append nodes when they create meaningful reviewable state. Gloss can consume a node or the current bundle head without owning git lifecycle.
