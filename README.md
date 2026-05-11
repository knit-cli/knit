# Knit

Knit is a local-first Rust CLI for authoring and coordinating multi-repo feature work. Think of it as "git for cross-repo feature work": it keeps a small bundle of related repositories, creates coordinated checkouts, commits staged changes across those checkouts, and records the result in a language-neutral JSON artifact.

Knit currently shells out to `git`. It does not use libgit2 and it does not try to replace git.

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

Knit stores local state under the directory where `knit project init`, `knit bundle start`, or `knit init` first creates a workspace:

```txt
.knit/
  config.json
  contexts.json
  bundles/
    <slug>.bundle.json
  projects/
    <project>.project.json
  locks/
    <bundle>.lock
  merge-runs/
    <run-id>.json
  merge-worktrees/
    <target-branch>/
      <repo-name>/
  land-plans/
    <slug>.land.json
  land-runs/
    <plan-id>-<timestamp>.run.json
  revert-plans/
    <node-id>.json
  worktrees/
    <slug>/
      <repo-name>/
```

The bundle file is the source of truth for a feature. `config.json` tracks workspace fallback state, while generated worktree paths and optional folder contexts let multiple agents work in parallel bundles without fighting over one global active bundle.

## Quickstart

From a workspace folder that sits beside your local repos:

```sh
knit project init venues
knit project add backend ../backend
knit project add frontend ../frontend
knit project add scraper ../scraper --observe
knit bundle start "venue capacity" --agents
```

For one-off work without a project, start a bundle and add repos directly:

```sh
knit bundle start "venue capacity"
knit bundle add ../backend ../frontend ../scraper
```

Make changes inside the generated worktrees, add the files, then inspect and commit:

```sh
knit status
knit add
knit commit -m "Add venue capacity integration"
knit log
```

For a one-step stage-and-commit:

```sh
knit commit --stage -m "Add venue capacity integration"
```

The created bundle is printed by `knit bundle start` and lives at:

```txt
.knit/bundles/venue-capacity.bundle.json
```

Bundle-aware commands resolve their bundle from `--bundle`, then `KNIT_BUNDLE`, then generated worktree paths such as `.knit/worktrees/<bundle>/<repo>`, then folder contexts from `knit bundle switch --here`, and finally the workspace fallback bundle. This lets parallel agents work in different Knit worktrees without sharing one mutable active bundle.

## Commands

```sh
knit project init <name>
knit project add <repo-id> <repo-path> [--base <branch>] [--observe]
knit project list
knit project show [name]
knit bundle
knit bundle start "<title>" [--project <name>] [--repo <repo-id>]... [--all-repos] [--no-worktree] [--in-place] [--force] [--agents]
knit bundle add <repo-path-or-project-repo-id>... [--base <branch>] [--in-place] [--no-worktree]
knit bundle remove <repo-id>...
knit bundle list [--all] [--archived] [--deleted]
knit bundle switch <bundle> [--workspace|--here]
knit bundle close [--reason <reason>]
knit bundle archive <bundle>
knit bundle restore <bundle>
knit bundle delete <bundle> --force [--worktrees] [--branches] [--force-branches]
knit bundle compat <source-bundle>... [--title <title>] [--project <name>] [--all-repos] [--no-worktree] [--in-place] [--force]
knit init "<title>" [--force] [--agents]
knit track <repo-path>... [--base <branch>] [--in-place] [--no-worktree]
knit add [-r <repo>] [-N] [-u] [repo-or-pathspec...]
knit stage [-r <repo>] [-N] [-u] [repo-or-pathspec...]
knit untrack <repo-id>...
knit remove <repo-id>...
knit worktree
knit bundle path
knit bundle print
knit bundle validate
knit checkpoint "<note>"
knit close [--reason <reason>]
knit clean [--plans] [--worktrees] [--closed] [--merge-worktrees] [--all] [--force]
knit status
knit diff [--stat] [repo-id-or-path...]
knit fetch [--all] [repo-id-or-path...]
knit pull [--all] [--rebase] [--force] [--feature] [repo-id-or-path...]
knit push [--all] [--set-upstream] [repo-id-or-path...]
knit publish github create [--base <branch>|--base <repo=branch>] [--draft] [--sync|--no-sync] [--set-upstream] [repo-id-or-path...]
knit publish github sync [repo-id-or-path...]
knit publish github status [repo-id-or-path...]
knit land plan [--provider github] [--out <path>] [--force]
knit land update [--push] [--continue-merge] [repo-id-or-path...]
knit land apply [--plan <path>]
knit land resume [--run <path>]
knit land status [--run <path>]
knit merge <source-bundle-or-ref> --into <target-branch-or-bundle> [--fetch] [--push] [--set-upstream] [--manual]
knit merge status [--run <id-or-path>]
knit merge show [--run <id-or-path>]
knit merge push [--run <id-or-path>] [--repo <repo-id>]... [--set-upstream]
knit merge --continue
knit merge --abort
knit config set advice true|false
knit schema print <bundle|project|contexts|merge-run|land-plan|land-run|config>
knit doctor
knit migrate [--check]
knit sync
knit commit -m "<message>" [--stage]
knit log [-<count>]
knit log [-n [count]]
knit revert <sha|node|HEAD|HEAD~N> [--plan]
knit revert <sha|node|HEAD|HEAD~N> --apply
knit git [--repo <repo>] [--all] <git-args...> [repo-selector...]
knit show <sha|node|HEAD|HEAD~N>
```

`knit init`, `knit track`, `knit remove`, and `knit close` are aliases for the bundle workflow.

## Projects And Bundles

Projects are optional repo templates. They remove the repetitive step of adding the same repo set for every bundle:

```sh
knit project init venues
knit project add backend ../backend
knit project add frontend ../frontend
knit project add docs ../docs --observe
```

Default project repos are included by `knit bundle start`; observed repos are available by id but are not branched or tracked until added explicitly:

```sh
knit bundle start "venue capacity"
knit bundle add docs
```

Bundles are the branch-like feature units. The same source repo can appear in many bundles at once. Knit creates separate feature branches and generated worktrees, for example `.knit/worktrees/fix-a/backend` and `.knit/worktrees/fix-b/backend`.

Compatibility bundles are ordinary bundles created from the union of repos in other bundles. They do not have a special target branch; use them as integration branches when two feature bundles need to be made compatible before either one lands:

```sh
knit bundle compat feature-x feature-y --title "x y compat"
knit merge feature-x --into x-y-compat
knit merge feature-y --into x-y-compat --manual
```

`knit bundle add` accepts one or more repo paths or project repo ids. It resolves all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, inferred base branch, and checkout mode. By default it creates the `knit/<bundle-id>` branch and a generated worktree for each tracked repo. Use `--no-worktree` for metadata-only registration.

Use `knit bundle start "<title>" --agents` or `knit init "<title>" --agents` when you want Knit to write an `AGENTS.md` tutorial into the workspace. The file explains projects, bundles, parallel worktrees, and the core Knit commands for coding agents. If `AGENTS.md` already exists, Knit preserves the rest of the file and appends or refreshes its own managed section.

Use `knit bundle add --in-place` or `knit track --in-place` to make Knit operate directly in the original repo checkout instead of creating `.knit/worktrees/<bundle>/<repo>`. Knit will create or check out the `knit/<bundle-id>` branch in that repo. The original checkout must be clean before Knit switches branches. Later mutating commands refuse to operate if the in-place repo is no longer on the expected feature branch.

Base inference prefers the current branch only when it is clean and named `main` or `master`; otherwise it looks for `main`, then `master`. Use `--base` when that is not right.

`knit worktree` is still available as an idempotent repair/rerun command. It creates missing `knit/<bundle-id>` branches and worktrees under `.knit/worktrees/<bundle-id>/<repo-id>`. Existing branches or worktrees are reported and reused where possible.

`knit bundle` shows the resolved bundle. `knit bundle path`, `print`, and `validate` inspect the existing `.bundle.json` / `ChangeGroup` artifact. They do not produce a separate review object:

```sh
knit bundle
knit bundle path
knit bundle print
knit bundle validate
```

Gloss should read this bundle and inspect the referenced repos, branches, and SHAs directly.

`knit checkpoint "<note>"` appends a non-git ledger node to the resolved bundle. It is useful when the feature has meaningful state that is not ready for a git commit yet:

```sh
knit checkpoint "frontend wired, backend pending"
```

Checkpoints show up in `knit log` and `knit show HEAD`. They do not create commits, move branches, or change repo state.

`knit close` appends a `feature.closed` node to the bundle without deleting worktrees, branches, commits, or source repos:

```sh
knit close
knit close --reason "merged"
```

The close node shows up in `knit log` and `knit show HEAD`. It is a ledger marker only.

`knit bundle delete <bundle> --force` moves the bundle JSON artifact to `.knit/deleted/bundles/` and clears the active bundle if needed. By default it preserves git state. Add `--worktrees` to remove Knit-generated worktrees for that bundle before moving the artifact. Add `--branches` to delete the local `knit/<bundle>` feature branches after those generated worktrees are removed:

```sh
knit bundle delete documentation-quick-wins --force
knit bundle delete documentation-quick-wins --force --worktrees
knit bundle delete documentation-quick-wins --force --worktrees --branches
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches
```

`--branches` uses `git branch -d`, so it refuses to delete branches with unmerged commits. `--force-branches` uses `git branch -D`. Knit only deletes local feature branches recorded by the bundle; remote branches are not deleted.

`knit clean` removes only Knit-generated local state after an explicit target flag. It never deletes source repos or git branches:

```sh
knit clean --plans
knit clean --worktrees
knit clean --closed --worktrees
knit clean --merge-worktrees
knit clean --all
```

`--plans` removes `.knit/revert-plans`. `--worktrees` removes generated worktrees for the resolved bundle with `git worktree remove` and clears their recorded `worktreePath`; in-place checkouts are preserved. `--closed --worktrees` applies that cleanup to closed and archived bundles. `--merge-worktrees` removes clean branch-target merge worktrees for succeeded or aborted merge runs. Use `--force` to pass `--force` to `git worktree remove` for dirty generated worktrees.

`knit add` stages file changes inside tracked checkouts, like `git add`. With no arguments, it runs `git add -A` in every tracked checkout, including untracked files. You can limit it by repo or path:

```sh
knit add
knit add backend
knit add backend app.txt
knit add --repo frontend src/App.tsx
knit add --intent-to-add frontend new-file.ts
```

`knit stage` is kept as an alias for `knit add`, because Git also accepts `git stage` as an alias for `git add`.

`knit status` shows the resolved bundle source, ordinary git status, checkout mode, wrong-branch warnings for in-place repos, and unrecorded commits when a tracked branch moved outside Knit.

`knit diff` shows cross-repo diffs against each repo's recorded `baseSha`. It follows `git diff`: committed, staged, and unstaged tracked-file changes are shown, while untracked files are not shown until they are added to the index. Use `knit status` or `knit git status --short` to see untracked files. Use `--stat` for a compact summary, or pass repo ids/paths to limit the output:

```sh
knit diff
knit diff --stat
knit diff backend
knit diff --stat ../backend
```

`knit fetch` updates remote refs and local object availability without merging, rebasing, moving checkouts, or changing bundle state. It is the safer way to give Knit and Gloss fresher git history:

```sh
knit fetch
knit fetch backend
knit fetch --all
```

`knit pull` updates tracked repos from their remotes. By default it runs in the original repo path on the recorded base branch and uses `git pull --ff-only`, then updates the repo's recorded `baseSha` in the bundle. It refuses to run when an affected checkout has uncommitted changes unless `--force` is passed. Use `--rebase` for `git pull --rebase`.

```sh
knit pull
knit pull backend
knit pull --all
knit pull --rebase frontend
```

Use `knit pull --feature` when you intentionally want to pull the tracked Knit feature checkout instead of the original/base checkout. Feature pulls are recorded as `git.observed` nodes when the feature branch head moves.

`knit push` pushes tracked feature branches to `origin`. It does not create PRs, update GitHub metadata, or change bundle state. By default it pushes the current feature branch to `origin/<branch>` without setting upstream; use `--set-upstream` when you want git's upstream tracking configured:

```sh
knit push
knit push backend
knit push --all
knit push --set-upstream frontend
```

`knit publish` publishes tracked feature branches to a code hosting provider. GitHub is the only provider implemented right now, and it uses the GitHub CLI (`gh`), so you need `gh` installed and authenticated for the repos you are publishing:

```sh
knit publish github create
knit publish github create --draft
knit publish github create backend
knit publish github create --base release
knit publish github create --base backend=stable --base frontend=main
knit publish github create --no-sync
knit publish github sync
knit publish github status
```

`knit publish github create` is a best-effort two-phase operation. It pushes every selected tracked feature branch, creates missing GitHub PRs or reuses an existing PR for the same feature/base branch, stores publishing metadata in the bundle's `publications`, then rewrites the managed Knit block in every selected PR body with the complete cross-repo PR list. The PR base defaults to each repo's bundle `baseBranch`; pass `--base release` to use the same base for every selected repo, or repeat `--base repo=branch` for per-repo bases. Body sync is on by default; `--sync` is accepted for explicitness, and `--no-sync` skips that second phase. If body sync fails after PRs were created, run `knit publish github sync` after fixing auth or network issues.

Knit preserves user-written PR text and only replaces the block between `<!-- BEGIN KNIT BUNDLE -->` and `<!-- END KNIT BUNDLE -->`.

`knit land` coordinates landing the recorded cross-repo PR set. It is provider-neutral at the command boundary, but GitHub is the only provider implemented today:

```sh
knit land plan
knit land update --push
knit land apply
knit land status
knit land resume
```

`knit land plan` writes an editable JSON plan to `.knit/land-plans/<bundle-id>.land.json`. The default plan is linear in repo order, merges each recorded GitHub PR with `squash`, waits for required checks, and does not delete feature branches. You can edit the plan to change merge order, use `merge` or `rebase`, insert `wait_checks` steps, or insert local `run` steps such as deploy commands. `run.command` is an argv array; use `["sh", "-lc", "..."]` when you intentionally need shell behavior.

`knit land update` prepares published PR branches for landing by fetching each PR's base branch, merging that base into the feature checkout, and recording the movement as a first-class `land.update` bundle node. This is the preferred way to resolve routine "base moved" landing conflicts because the integration merge is attributed to landing prep instead of appearing later as an incidental `git.observed` movement. Pass `--push` to push the updated feature branches after recording the node. If a merge conflicts, resolve and commit it in the feature checkout, then run `knit land update --continue-merge` to record the already-resolved movement as `land.update`.

`knit land apply` preflights referenced PRs, refuses draft/closed/missing PRs, writes a durable run file under `.knit/land-runs/`, then executes the plan step by step. If a step fails, the run stops and records the exact step status, stdout/stderr for `run` steps, and failure detail. `knit land resume` continues that run from pending or failed steps only; succeeded steps are not repeated. A fully successful run appends a `feature.landed` node to the bundle with the plan id, run id, provider, repo ids, and publication URLs.

`knit merge` is for local branch integration that is not a PR landing. It can merge a bundle or git ref into a target branch, or into another bundle's feature branches:

```sh
knit merge feature-x --into staging
knit merge feature-y --into staging --manual
knit merge x-y-compat --into feature-y
```

For branch targets, Knit creates or reuses managed checkouts under `.knit/merge-worktrees/<target>/<repo>/`. A merge run is recorded under `.knit/merge-runs/`. By default, if any repo conflicts, Knit aborts the failed merge and resets every repo touched by that run back to its pre-run SHA, so the run behaves all-or-none from Knit’s point of view. Pass `--manual` when you want to resolve the conflicted repo yourself; after resolving and committing in the printed checkout, run `knit merge --continue`, or use `knit merge --abort` to roll back the run.

Use `--fetch` to refresh branch targets from `origin/<target>` before merging. Use `--push` to push branch targets only after every local merge step succeeds, or push later with `knit merge push`. `knit merge status` and `knit merge show` inspect recorded merge runs and their per-repo push state.

When the target is another bundle, successful merges update that bundle's feature branches and append a `git.observed` node to the target bundle. This makes compatibility workflows explicit without inventing project-level branch targets:

```sh
knit bundle compat feature-x feature-y --title "x y compat"
knit merge feature-x --into x-y-compat
knit merge feature-y --into x-y-compat --manual
knit merge x-y-compat --into staging
knit merge x-y-compat --into feature-y
```

`knit sync` records commits that happened outside Knit as `git.observed` nodes and advances each affected repo's remembered `headSha`. `knit log` shows both Knit commit groups and observed git movement from the node ledger. Use `knit log -2` for the latest two log entries. `knit log -n 3` also works, and `knit log -n` defaults to the latest ten.

`knit show <target>` uses the same bundle log selectors as `knit revert`: `HEAD`, `HEAD~1`, full node ids, unique node id prefixes, commit group ids, and recorded git commit SHAs. Commit and revert group nodes show `git show --stat --oneline` for each repo commit. Observed git nodes show the branch movement and the relevant added or dropped commits when those commits are still available locally.

`knit revert <target>` resolves bundle log selectors like `HEAD`, `HEAD~1`, full node ids, unique node id prefixes, and git commit SHAs shown in `knit log`. A commit SHA resolves to the latest bundle node that mentions that commit, so if a commit was later observed as dropped by a reset, reverting by that SHA restores it from the latest rewind node. By default it writes a checked revert plan under `.knit/revert-plans/` and prints the per-repo operations. `knit revert <target> --apply` requires that plan to exist, verifies each affected worktree is still clean and at the planned head, then creates one revert commit per affected repo and appends a `revert.group` node.

Revert behavior is based on the target node:

- `commit.group` and `revert.group`: revert the recorded commits.
- `git.observed` with `advanced`: revert the observed commits.
- `git.observed` with `rewound`: cherry-pick the dropped commits back.
- `git.observed` with `diverged`: revert added commits, then cherry-pick dropped commits.

`knit git` passes arguments directly to git in tracked checkouts. With no repo selector it runs against every tracked repo:

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

`knit commit` commits only repos with staged changes in their tracked checkouts. With `--stage`, it stages first and then commits. `knit commit` also syncs unrecorded git commits before creating a new logical commit group, so the ledger remains ordered.

The git commits are created sequentially, one repo at a time. Knit records them as one logical commit group in the bundle. Every repo commit gets the same logical message plus trailers:

```txt
Knit-Group: <commit-group-id>
Knit-Bundle: <bundle-id>
```

The bundle records the full mapping from logical commit group to repo commit SHAs.

`knit untrack <repo-id>...` removes repos from bundle tracking and appends a `repo.removed` node. It intentionally leaves existing git branches and checkouts in place. `knit remove` remains as an alias.

## Bundle Nodes

The bundle is a feature ledger. It stores current state in `repos` and `commitGroups`, and an ordered node chain in `nodes`.

Typical node types:

- `feature.created`
- `feature.closed`
- `repo.added`
- `worktree.materialized`
- `checkpoint`
- `commit.group`
- `git.observed`
- `revert.group`
- `feature.landed`
- `land.update`
- `repo.removed`

`headNodeId` points at the latest node. Gloss can inspect any node, but the most useful review usually comes from the current head or the final pre-PR bundle.

`publications` records provider metadata for published branches. It is useful for linking the GitHub PR set that belongs to the bundle, but it is not the source of truth for code state; git branches, SHAs, and bundle nodes remain the source of truth.

`knit schema print <name>` prints bundled JSON Schemas. `knit doctor` validates workspace JSON and repairable local state such as stale locks, missing repo paths, and missing recorded worktrees. `knit migrate` rewrites older additive JSON files into the current shape; `knit migrate --check` reports what would change without writing.

Sparse advice is enabled by default for new workspaces. It prints a `Next:` line only when Knit detects an interrupted or incomplete state, such as a manual merge conflict. Use `knit config set advice false` or `KNIT_ADVICE=0` to suppress it.

## Current Limitations

- Knit is not a database transaction layer. If one repo commit succeeds and a later repo commit fails, Knit reports the failure but does not roll back the earlier commit.
- `knit bundle add` resolves repo inputs before writing the bundle, but branch/worktree creation can still partially succeed before a later git operation fails.
- `knit merge` emulates all-or-none behavior for local branch and bundle integration by resetting every repo touched by a failed run back to its pre-run SHA. That rollback is scoped to the current merge run.
- Knit uses named lock files under `.knit/locks/` to prevent concurrent writes to the same bundle or project. If a process crashes, a stale lock may need manual removal.
- Worktree creation relies on `git worktree add` and inherits its constraints, including branch checkout conflicts.
- `knit fetch` fetches the `origin` remote for each selected repo. Repos without `origin` are reported as failures.
- `knit pull` coordinates ordinary git pulls but does not resolve merge/rebase conflicts across repos. If git stops for a conflict, resolve that repo's git state before retrying.
- `knit push` only pushes feature branches to `origin`; use `knit publish github create` for GitHub PR publishing.
- `knit publish` currently supports only GitHub through the `gh` CLI. GitLab/Bitbucket/Forgejo support would need provider adapters.
- `knit publish github create` is not perfectly transactional. Branch pushes, PR creation, and PR body updates happen sequentially. If phase two fails after PRs are created, run `knit publish github sync`.
- `knit land` currently supports only GitHub PR publications through the `gh` CLI. A GitHub PR merge lands into that PR's base branch. Remote PR merges cannot be automatically unmerged by Knit, so failed land runs are recorded in `.knit/land-runs/`; fix the failed step and use `knit land resume`.
- `knit land plan` never executes local commands. `run` steps execute only during `apply` or `resume`.
- `knit clean --worktrees` removes generated worktree directories only. It leaves source repos and feature branches in place. `knit bundle delete --worktrees --branches --force-branches` is the explicit local discard path for a bundle's generated worktrees and local feature branches.
- `knit commit` only looks for staged changes inside tracked checkouts.
- `knit revert --apply` preflights all affected repos before writing, but cross-repo revert commits are still created sequentially. If a conflict or commit failure happens after an earlier repo succeeds, inspect the affected repos manually before retrying.
- `knit revert` cannot restore historical `repo.removed` nodes yet because older bundle nodes did not store the full removed repo record.
- JSON Schema files are bundled for workspace artifacts; `knit doctor` uses serde-backed validation and structural checks.
- Knit does not run LLMs, MCP servers, or review agents.

## Manual Test With Toy Repos

See [docs/manual-test.md](docs/manual-test.md) for a small two-repo smoke test.

See [docs/change-group-schema.md](docs/change-group-schema.md) for the current bundle fields.

## Code Layout

See [docs/architecture.md](docs/architecture.md) for the module boundaries and test layout. `src/main.rs` is intentionally only the binary entry point; command logic lives in `src/commands/`, schema types in `src/model.rs`, persistence in `src/store.rs`, and git subprocess helpers in `src/git.rs`.

## Roadmap

- Standalone JSON Schema for `ChangeGroup`
- Safer partial-failure recovery for multi-repo commits
- Provider adapters for non-GitHub publishing and landing
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
