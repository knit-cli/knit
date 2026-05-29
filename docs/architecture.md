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
    config.rs
    doctor.rs
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
    schema.rs
    git_passthrough.rs
  providers/
    mod.rs        Forge trait, PrTarget, host detection, shared CLI runner + publication helpers
    github.rs     GitHub forge adapter via the gh CLI
    gitlab.rs     GitLab forge adapter via the glab CLI (merge requests)
    forgejo.rs    Codeberg/Forgejo forge adapter via the tea CLI
  checkout.rs   checkout mode helpers and in-place branch guards
  advice.rs     sparse next-step advice helpers
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
- `commands/publish.rs` owns the user-facing publish workflow. It resolves a `providers::Forge` per repo from the repo's remote and calls through the trait; it does not hard-code a host.
- `commands/land.rs` owns landing plan/run orchestration. It reads publication metadata and project landing templates, writes `.knit/land-plans/` and `.knit/land-runs/`, manages deployment checkouts under `.knit/land-worktrees/`, and appends `feature.landed` only after every step succeeds.
- `commands/merge.rs` owns local bundle/ref integration into target branches or other bundles. It writes `.knit/merge-runs/`, uses managed branch checkouts under `.knit/merge-worktrees/`, rolls back failed non-manual runs to their pre-run SHAs, and records target-bundle merges as `git.observed`.
- `commands/doctor.rs` owns workspace validation and additive JSON migrations. `commands/schema.rs` prints bundled JSON Schemas for Knit artifacts.
- `providers/` owns the `Forge` trait and its host adapters. `mod.rs` defines the trait, the canonical `PullRequest`/`CheckRun` types, host detection from a remote URL (`for_remote`/`for_repo`/`by_id`), a shared CLI runner, and the provider-agnostic publication helpers. Each adapter (`github.rs`/`gitlab.rs`/`forgejo.rs`) maps its CLI's JSON onto the canonical types. GitLab and Codeberg/Forgejo are detected from the remote host; every other remote defaults to GitHub. Provider modules expose small operations; command modules decide workflow policy.
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
