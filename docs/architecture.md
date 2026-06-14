# Architecture

Knit is split by responsibility so command behavior, persistence, git subprocesses, and pure helpers can be analyzed independently.

## Source Layout

```txt
src/
  main.rs       binary entry point only
  lib.rs        module wiring and command dispatch
  cli.rs        clap command definitions
  commands/
    mod.rs            command module wiring
    bundle/           bundle inspection, listing, switching; lifecycle.rs archive/restore/delete, validate.rs artifact checks, prune/ scans dead work
    land/             landing plan/check/execute/update/validate/display
    merge/            local integration runs and reports
    publish/          publish workflow: scope/remote/sync/status phases plus PR body generation
    remote/           KnitHub transport: client, facade, per-artifact push/pull/history, clone
    agents.rs         generated AGENTS.md guidance
    cherrypick.rs     move recorded commits between bundles
    clean.rs
    commit.rs
    config.rs
    diff.rs
    doctor.rs
    fetch.rs
    git_passthrough.rs
    history/          `knit history` / `knit related`: target resolution + git/Knit-history join
    init.rs
    log.rs
    project.rs
    pull.rs
    push.rs
    remove.rs
    revert/           revert plans (plan.rs) and execution (apply.rs)
    run.rs            configured/one-off commands inside checkouts
    runtime/          bundle runtime stacks (`knit run up/status/down`): mod.rs orchestrates, transform.rs lifts compose shapes
    schema.rs
    shape.rs          live reshaping: `knit bundle add/remove/apply-view`
    stage.rs
    status.rs
    sync.rs
    track.rs
    view.rs           per-user saved bundle shapes
    worktree.rs
  providers/
    mod.rs        Forge trait, PrTarget, host detection, shared CLI runner + publication helpers
    github/       GitHub forge adapter: gh CLI impl, REST api ops, HTTP transport
    gitlab.rs     GitLab forge adapter via the glab CLI (merge requests)
    forgejo.rs    Codeberg/Forgejo forge adapter via the tea CLI
  model/
    mod.rs        module wiring and shared types
    bundle.rs     bundle / ChangeGroup artifact and node ledger
    config.rs     workspace config and context
    history.rs    project history event types
    project.rs    reusable project templates
    view.rs       saved view types
  checkout.rs   checkout mode helpers and in-place branch guards
  advice.rs     sparse next-step advice helpers
  store.rs      .knit config, context, project, and bundle persistence
  git.rs        git subprocess helpers
  history.rs    project history ledger persistence and refresh
  ids.rs        slugs, commit group ids, SHA formatting
  output.rs     terminal output helpers
  paths.rs      path comparison helpers
  repo_selectors.rs shared tracked-repo selector resolution
  selectors.rs  bundle log selector resolution for HEAD, node ids, and SHAs
  status.rs     git status classification
  time.rs       timestamp formatting
  tracking.rs   tracked-branch ancestry helpers
tests/
  common/       shared test harness (toy repos, isolated KNIT_HOME)
  bundle.rs  cleanup.rs  config.rs  feature_flow.rs  ids.rs  land.rs
  merge.rs  model.rs  project.rs  publish.rs  runtime.rs  status.rs  sync.rs
```

Rust does not use classes in the TypeScript sense. The equivalent separation here is modules plus explicit data types. `model/` owns the long-lived schema types, including the `ChangeGroup` bundle and node ledger; each module in `commands/` coordinates one user-facing command with filesystem and git operations. A command starts as a single file and becomes a directory module only when it grows distinct phases (plan/execute/report), as `bundle/`, `history/`, `land/`, `merge/`, `publish/`, `remote/`, `revert/`, and `runtime/` have. Keep command files under ~700 lines; split by phase or concern once they pass it.

## Boundaries

- `main.rs` should stay tiny. It parses CLI arguments and calls `knit::run`.
- `cli.rs` should contain only argument shape and help text.
- Each module in `commands/` owns one user-facing command or tightly coupled command pair.
- `commands/project.rs` owns reusable project repo templates under `.knit/projects/`.
- `commands/bundle/` owns bundle inspection, listing, switching, and validation. It must not create a second review handoff object.
- `commands/publish/` owns the user-facing publish workflow. It resolves a `providers::Forge` per repo from the repo's remote and calls through the trait; it does not hard-code a host.
- `commands/land/` owns landing plan/run orchestration. It reads publication metadata and project landing templates, writes `.knit/land-plans/` and `.knit/land-runs/`, manages deployment checkouts under `.knit/land-worktrees/`, and appends `feature.landed` only after every step succeeds.
- `commands/merge/` owns local bundle/ref integration into target branches or other bundles. It writes `.knit/merge-runs/`, uses managed branch checkouts under `.knit/merge-worktrees/`, rolls back failed non-manual runs to their pre-run SHAs, and records target-bundle merges as `git.observed`.
- `commands/remote/` owns all KnitHub artifact transport. `facade.rs` selects artifact families and remotes; the per-artifact helpers in `push.rs`/`pull.rs`/`history.rs` own transport. Every door (`knit sync push/pull`, `knit push --remote`, `knit fetch`/`pull --bundles`, post-land auto-sync) routes through it.
- `commands/runtime/` owns disposable per-bundle stacks (`knit run up/status/down`). `mod.rs` picks the mode per compose file — explicit `runtime.mode`, the `docker-compose.knit.yml` filename, or `${KNIT_*}` references select contract mode; everything else is transformed — and owns port allocation and run state under `.knit/runtime-runs/<bundle>/` (recorded only after a successful start; `down`/`status` resolve containers by compose project label so they survive missing state and torn-down worktrees). `transform.rs` rewrites resolved compose JSON: paths into tracked repos to bundle worktrees, published host ports to free ports, textual port references in env/args. Docker is reached only through `docker compose` subprocesses, and a project command named `up`/`down`/`status` always wins over the runtime verbs. Transform heuristics must not grow: a stack the transform cannot lift commits a contract compose file instead.
- `commands/doctor.rs` owns workspace validation and additive JSON migrations. `commands/schema.rs` prints bundled JSON Schemas for Knit artifacts.
- `providers/` owns the `Forge` trait and its host adapters. `mod.rs` defines the trait, the canonical `PullRequest`/`CheckRun` types, host detection from a remote URL (`for_remote`/`for_repo`/`by_id`), a shared CLI runner, and the provider-agnostic publication helpers. Each adapter (`github.rs`/`gitlab.rs`/`forgejo.rs`) maps its CLI's JSON onto the canonical types. GitLab and Codeberg/Forgejo are detected from the remote host; every other remote defaults to GitHub. Provider modules expose small operations; command modules decide workflow policy.
- `commands/mod.rs` should only re-export command entry points.
- `git.rs` is the only place that should construct raw `git` subprocess calls.
- `store.rs` is the only place that should resolve bundle context. Resolution prefers `--bundle`, `KNIT_BUNDLE`, generated worktree cwd, then workspace fallback config.
- `checkout.rs` owns checkout path resolution, checkout mode labels, and in-place branch safety checks.
- Pure helper behavior should live in small modules and have integration tests under `tests/`.

## Testing

Cheap pure behavior lives in small integration tests (`tests/ids.rs`, `tests/status.rs`, `tests/model.rs`).

End-to-end git behavior is automated: `tests/common/` builds temporary toy repos and a workspace with an isolated `KNIT_HOME` (tests must never read the real `~/.config/knit` or reach a live KnitHub), and the flow tests (`tests/feature_flow.rs`, `tests/bundle.rs`, `tests/land.rs`, `tests/merge.rs`, `tests/publish.rs`, `tests/sync.rs`, `tests/project.rs`, `tests/config.rs`, `tests/cleanup.rs`) drive the real binary against them. [manual-test.md](manual-test.md) remains as a hands-on walkthrough.

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

With no target flag, both move every relevant artifact family. Remote selection resolves explicit `--remote` overrides first, then configured sync remotes, then the sole configured remote.

The absorbed verbs are deleted, not aliased or hidden: `knit bundle push`, `knit history push/pull/sync` (only `history list` and `history refresh` remain), `knit view push/pull`, and `knit land sync` no longer exist. The philosophy is one way per outcome — delete, do not hide.

The git-parity verbs keep their git shapes because they are about branches first: `knit push --remote <name>` pushes branches and then the bundle artifact, and `knit fetch --bundles` / `knit pull --bundles` pull recorded bundle state. They are not duplicate implementations — they route through the same `commands/remote/` helpers that the `knit sync` subcommands and landing's automatic sync use. `commands/remote/facade.rs` is the thin selector that the `knit sync` subcommands call: it owns choosing artifact families and remotes, then delegates transport to the per-artifact helpers in `commands/remote/{push,pull,history}.rs`. One implementation, several differently shaped doors into it. The `push-sync`/`sync-remote`/`sync-remotes` config keys are unchanged and still drive automatic artifact sync after a successful land.

## Project And Context State

Projects are optional templates for repeated repo sets. They live under `.knit/projects/<project>.project.json`, record stable repo ids, default base branches, checkout mode, and whether a repo is included by default or only observed.

Bundle context is intentionally local. `.knit/config.json` stores the workspace fallback bundle and active project, and generated worktree paths always identify their owning bundle. Mutating bundle commands use per-bundle locks under `.knit/locks/`; project mutation uses a project-specific lock.
