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
    project.rs
    land.rs
    merge.rs
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
  providers/
    github.rs     provider-specific forge operations through gh
  checkout.rs   checkout mode helpers and in-place branch guards
  model.rs      project, config, context, bundle / ChangeGroup data structures
  store.rs      .knit config, context, project, and bundle persistence
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
- `commands/project.rs` owns reusable project repo templates under `.knit/projects/`.
- `commands/bundle.rs` owns bundle inspection, listing, switching, and validation. It must not create a second review handoff object.
- `commands/publish.rs` owns the user-facing publish workflow. Provider-specific calls live in `providers/`, starting with `providers/github.rs`.
- `commands/land.rs` owns landing plan/run orchestration. It reads publication metadata, writes `.knit/land-plans/` and `.knit/land-runs/`, and appends `feature.landed` only after every step succeeds.
- `commands/merge.rs` owns local bundle/ref integration into target branches or other bundles. It writes `.knit/merge-runs/`, uses managed branch checkouts under `.knit/merge-worktrees/`, rolls back failed non-manual runs to their pre-run SHAs, and records target-bundle merges as `git.observed`.
- `providers/` owns forge-specific subprocess behavior such as GitHub PR view/check/merge through `gh`. Provider modules should expose small operations; command modules decide workflow policy.
- `commands/mod.rs` should only re-export command entry points.
- `git.rs` is the only place that should construct raw `git` subprocess calls.
- `store.rs` is the only place that should resolve bundle context. Resolution prefers `--bundle`, `KNIT_BUNDLE`, generated worktree cwd, folder context, then workspace fallback config.
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
- `commitGroups`: flat list of logical commits across repos.
- `nodes`: ordered ledger entries such as `feature.created`, `feature.closed`, `feature.landed`, `repo.added`, `worktree.materialized`, `checkpoint`, `commit.group`, `git.observed`, `revert.group`, and `repo.removed`.
- `publications`: provider metadata for PRs or other forge review objects created or synced by Knit.
- `headNodeId`: the latest node in the ledger.

Command files should append nodes when they create meaningful reviewable state. Gloss can consume a node or the current bundle head without owning git lifecycle.

## Project And Context State

Projects are optional templates for repeated repo sets. They live under `.knit/projects/<project>.project.json`, record stable repo ids, default base branches, checkout mode, and whether a repo is included by default or only observed.

Bundle context is intentionally local. `.knit/config.json` stores the workspace fallback bundle and active project, `.knit/contexts.json` stores folder-level bundle fallbacks, and generated worktree paths always identify their owning bundle. Mutating bundle commands use per-bundle locks under `.knit/locks/`; project mutation uses a project-specific lock.
