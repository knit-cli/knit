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
    bundle.rs
    checkpoint.rs
    clean.rs
    close.rs
    init.rs
    track.rs
    remove.rs
    worktree.rs
    stage.rs
    status.rs
    diff.rs
    fetch.rs
    pull.rs
    push.rs
    publish.rs
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
  repo_selectors.rs shared tracked-repo selector resolution
  selectors.rs  bundle log selector resolution for HEAD, node ids, and SHAs
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
- `commands/bundle.rs` only inspects or validates the existing bundle artifact; it must not create a second review handoff object.
- `commands/publish.rs` owns provider publishing. It may call provider CLIs such as `gh`, but it should keep publication state as metadata on the bundle and never replace git branches/SHAs as the code source of truth.
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
- `nodes`: ordered ledger entries such as `feature.created`, `feature.closed`, `repo.added`, `worktree.materialized`, `checkpoint`, `commit.group`, `git.observed`, `revert.group`, and `repo.removed`.
- `publications`: provider metadata for PRs or other forge review objects created or synced by Knit.
- `headNodeId`: the latest node in the ledger.

Command files should append nodes when they create meaningful reviewable state. Gloss can consume a node or the current bundle head without owning git lifecycle.
