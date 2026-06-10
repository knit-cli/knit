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
    clean.rs
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
- `nodes`: ordered ledger entries such as `feature.created`, `feature.archived`, `feature.landed`, `pr.revert`, `repo.added`, `worktree.materialized`, `commit.group`, `git.observed`, `revert.group`, and `repo.removed`. Older artifacts may carry `feature.closed` and `checkpoint` nodes from removed commands; readers must tolerate unknown node types.
- `publications`: provider metadata for PRs or other forge review objects created or synced by Knit.
- `headNodeId`: the latest node in the ledger.

Command files should append nodes when they create meaningful reviewable state. Gloss can consume a node or the current bundle head without owning git lifecycle.

## Project History Ledger

Knit also maintains project-wide history events derived from bundle ledgers. Locally these events live in `.knit/history/<project>.history.jsonl`; KnitHub stores the same metadata in its project history table and includes it in project exports.

History events are pointers, not patches. They record the project, bundle, repo, branch, Knit node, commit group, Git commit SHA, movement, and timestamps. Git remains the source of truth for file contents and file-level history.

This split enables related-work queries without duplicating Git. `knit related` first asks Git which commits touched a path, then joins those SHAs to Knit history to recover the bundle and cross-repo commit context. If a commit was made wholly outside Knit and never recorded into a bundle, Git can still report it for the path, but Knit history has no bundle context for it.

## KnitHub Artifact Sync

Three kinds of Knit artifact move between the workspace and KnitHub remotes: bundle artifacts, project history events, and per-user saved views. Historically these were reached through eight overlapping doors: `knit push --remote`, `knit bundle push`, `knit fetch --bundles`, `knit pull --bundles`, `knit history push/pull/sync`, `knit view push/pull`, `knit land sync`, and the automatic push-sync-on-land driven by the `push-sync`/`sync-remote`/`sync-remotes` config keys. Several verbs did the same thing under different names, and the command surface gave no single place to learn "how do I move artifacts to KnitHub".

This is consolidated into one verb family. `knit sync` keeps its original meaning exactly — a local-only reconcile that records git commits made outside Knit — and gains two subcommands that are the one explicit way to move artifacts:

- `knit sync push [--bundles|--history|--views|--all] [--remote <name>]...`
- `knit sync pull [--bundles|--history|--views|--all] [--remote <name>]...`

With no target flag, both move every relevant artifact family. Remote selection resolves explicit `--remote` overrides first, then configured sync remotes, then a remote named `knithub`.

The absorbed verbs are deleted, not aliased or hidden: `knit bundle push`, `knit history push/pull/sync` (only `history list` and `history refresh` remain), `knit view push/pull`, and `knit land sync` no longer exist. The philosophy is one way per outcome — delete, do not hide.

The git-parity verbs keep their git shapes because they are about branches first: `knit push --remote <name>` pushes branches and then the bundle artifact, and `knit fetch --bundles` / `knit pull --bundles` pull recorded bundle state. They are not duplicate implementations — they route through the same `commands/remote/` helpers that the `knit sync` subcommands and landing's automatic sync use. `commands/remote/facade.rs` is the thin selector that the `knit sync` subcommands call: it owns choosing artifact families and remotes, then delegates transport to the per-artifact helpers in `commands/remote/{push,pull,history}.rs`. One implementation, several differently shaped doors into it. The `push-sync`/`sync-remote`/`sync-remotes` config keys are unchanged and still drive automatic artifact sync after a successful land.

## Project And Context State

Projects are optional templates for repeated repo sets. They live under `.knit/projects/<project>.project.json`, record stable repo ids, default base branches, checkout mode, and whether a repo is included by default or only observed.

Bundle context is intentionally local. `.knit/config.json` stores the workspace fallback bundle and active project, `.knit/contexts.json` stores folder-level bundle fallbacks, and generated worktree paths always identify their owning bundle. Mutating bundle commands use per-bundle locks under `.knit/locks/`; project mutation uses a project-specific lock.
