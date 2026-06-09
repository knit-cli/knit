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

Knit stores local state under the directory where `knit init`, or `knit bundle` first creates a workspace:

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
  land-worktrees/
    <slug>/
      <repo-name>/
        <branch>/
  revert-plans/
    <node-id>.json
  worktrees/
    <slug>/
      <repo-name>/
```

The bundle file is the source of truth for a feature. `config.json` tracks workspace fallback state, while generated worktree paths and optional folder contexts let multiple agents work in parallel bundles without fighting over one global active bundle.

User-global Knit config lives outside the workspace at `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. Workspace `.knit/config.json` overrides global values of the same name.

## Quickstart

From a workspace folder that sits beside your local repos:

```sh
knit init venues
knit project add backend ../backend
knit project add frontend ../frontend
knit project add scraper ../scraper --observe
knit project command set dev --repo frontend -- docker compose up
knit bundle "venue capacity" --agents
```

For one-off work without a project, start a bundle and add repos directly:

```sh
knit bundle "venue capacity"
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
knit commit --all -m "Add venue capacity integration"
```

The created bundle is printed by `knit bundle` and lives at:

```txt
.knit/bundles/venue-capacity.bundle.json
```

Bundle-aware commands resolve their bundle from `--bundle`, then `KNIT_BUNDLE`, then generated worktree paths such as `.knit/worktrees/<bundle>/<repo>`, then folder contexts from `knit switch --here`, and finally the workspace fallback bundle. This lets parallel agents work in different Knit worktrees without sharing one mutable active bundle.

## Commands

```sh
knit init <name> [--agents]
knit project add <repo-id> <repo-path> [--base <branch>] [--observe] [--agents]
knit project agents [name]
knit project command set <name> [--repo <repo>]... [--cwd <path>] [--env KEY=VALUE]... -- <command> [args...]
knit project command list
knit project command remove <name>
knit project list
knit project show [name]
knit project remove <name> --force
knit view list [--project <name>]
knit view show [name] [--project <name>] [--repos]
knit view save <name> [--include <repo>]... [--exclude <repo>]... [--from-bundle] [--project <name>]
knit view include <name> <repo>... [--project <name>]
knit view exclude <name> <repo>... [--project <name>]
knit view unset <name> <repo>... [--project <name>]
knit view default [name] [--clear] [--project <name>]
knit view rm <name> [--project <name>]
knit view edit [--project <name>]
knit view push [--project <name>] [--remote <name>]
knit view pull [--project <name>] [--remote <name>]
knit bundle                          # show the resolved bundle
knit bundle "<title>"                # create a bundle (git-branch-style shorthand)
knit bundle "<title>" [--project <name>] [--repo <repo-id>]... [--all-repos] [--view <name>] [--include <repo>]... [--exclude <repo>]... [--no-worktree] [--in-place] [--force] [--agents] [--cd [<repo>]]
knit bundle add <repo-path-or-project-repo-id>... [--base <branch>] [--in-place] [--no-worktree]
knit bundle remove <repo-id>... [--keep-worktree|--delete-branch] [--force]
knit bundle worktree
knit bundle apply-view <name> [--keep-worktree|--delete-branch] [--force]
knit bundle list [--all] [--archived] [--deleted]
knit bundle close [--reason <reason>]
knit bundle archive <bundle>
knit bundle restore <bundle>
knit bundle delete <bundle> --force [--worktrees] [--branches] [--force-branches] [--remote-branches]
knit bundle prune [--no-refresh] [--apply] [--all] [--worktrees] [--force] [--branches] [--force-branches] [--remote-branches] [--remote-bundles]
knit bundle compat <source-bundle>... [--title <title>] [--project <name>] [--all-repos] [--no-worktree] [--in-place] [--force]
knit bundle split <source-bundle> <selector>... [--title <title>] [--repo <repo>]... [--force]
knit bundle path
knit bundle print
knit bundle validate
knit switch <bundle> [--workspace|--here]
knit add [-r <repo>] [-N] [-u] [repo-or-pathspec...]
knit checkpoint "<note>"
knit clean [--plans] [--worktrees] [--closed] [--merge-worktrees] [--all] [--force]
knit status
knit diff [--stat] [repo-id-or-path...]
knit fetch [--all] [repo-id-or-path...]
knit pull [--main] [--bundles] [--all] [--rebase] [--force] [--feature] [repo-id-or-path...]
knit push [--all] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit run <project-command> [--repo <repo>]... [--all]
knit run [--repo <repo>] [--all] -- <command> [args...]
knit run --list
knit publish create [--base <branch>|--base <repo=branch>] [--draft] [--sync|--no-sync] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit publish sync [repo-id-or-path...]
knit publish status [--live] [repo-id-or-path...]
knit publish github <create|sync|status> ...   # back-compat alias
knit land
knit land plan [--provider github|gitlab|forgejo] [--out <path>] [--force]
knit land check
knit land update [--push] [--continue-merge] [repo-id-or-path...]
knit land apply [--plan <path>] [--remote <remote>]... [--no-remote]
knit land resume [--run <path>] [--remote <remote>]... [--no-remote]
knit land sync [--remote <remote>]...
knit land status [--run <path>]
knit merge <source-bundle-or-ref> --into <target-branch-or-bundle> [--fetch] [--push] [--set-upstream] [--manual]
knit merge status [--run <id-or-path>]
knit merge show [--run <id-or-path>]
knit merge push [--run <id-or-path>] [--repo <repo-id>]... [--set-upstream]
knit merge --continue
knit merge --abort
knit config set advice true|false
knit config set push-sync true|false
knit config set sync-remote <name>
knit config set sync-remotes <name>[,<name>...]
knit schema print <bundle|project|contexts|merge-run|land-plan|land-run|config>
knit doctor
knit migrate [--check]
knit sync
knit history [list] [-n <count>] [--repo <repo>] [--bundle <bundle>] [--project <project>]
knit history refresh [--project <project>]
knit history push|pull|sync [--project <project>] [--remote <name>]
knit related [--repo <repo>] [--project <project>] [--pull] [--remote <name>] [--limit <count>] [--commit-limit <count>] <path>...
knit commit -m "<message>" [--stage]
knit log [-<count>]
knit log [-n [count]]
knit revert <sha|node|HEAD|HEAD~N> [--plan]
knit revert <sha|node|HEAD|HEAD~N> --apply
knit reset [--soft|--mixed|--hard] [<commit>] [--repo <repo>] [--all]
knit git [--repo <repo>] [--all] <git-args...> [repo-selector...]
knit show <sha|node|HEAD|HEAD~N>
```

A bundle is the cross-repo analogue of a git branch: `knit bundle "<title>"` creates one (like `git branch <name>`), `knit bundle` shows the current one, and creation flags go straight on it, e.g. `knit bundle "<title>" --project <name> --repo <repo>`. A project is initialized once with `knit init <name>` (like `git init`). Everyday VCS verbs (`add`, `commit`, `push`, `pull`, `switch`, `status`, `diff`, `log`, `revert`, `reset`, …) live at the top level; bundle/repo management lives under `knit bundle`.

## Projects And Bundles

Projects are optional repo templates. They remove the repetitive step of adding the same repo set for every bundle:

```sh
knit init venues
knit project add backend ../backend
knit project add frontend ../frontend
knit project add docs ../docs --observe
```

Projects can also define commands that run inside bundle checkouts:

```sh
knit project command set dev --repo frontend -- docker compose up
knit project command set api-test --repo backend -- cargo test
knit run dev
knit run api-test
```

`knit run <name>` resolves the active bundle, enters the configured repo worktree, sets `KNIT_ROOT`, `KNIT_BUNDLE`, `KNIT_REPO`, and `KNIT_CHECKOUT`, then executes the command without a shell. For one-off commands, pass the command after `--`:

```sh
knit run --repo backend -- docker compose ps
```

### Views

A project's repo list is shared by everyone, with `--observe` marking repos kept out of default bundle starts. A **view** is per-user config layered on top of that shared project: a named "bundle shape" expressed as include/exclude deltas over the project's default repo set. Views are stored per user at `.knit/views/<project-id>.views.json` and never touch the shared project artifact, so a junior member can work against two repos while a staff member keeps several shapes for the same project.

```sh
knit view save backend --exclude frontend,docs
knit view save frontend --include design-system --exclude backend
knit view default backend            # bare `knit bundle` now uses this shape
knit view list                       # `*` marks the default
knit view show frontend --repos      # print the repos this view resolves to
```

`knit bundle "title"` applies the default view (if set); `--view <name>` selects another. `--repo`/`--all-repos` ignore views and select an explicit set. Ad-hoc `--include <repo>` / `--exclude <repo>` adjust the resolved set in any mode, so `knit bundle "x" --view backend --include docs` and `knit bundle "y" --all-repos --exclude sej` both work.

A live bundle can be reshaped at any time, with the worktree consequences:

```sh
knit bundle add docs                 # materialize the repo's branch + worktree
knit bundle remove frontend          # tear down its worktree
knit bundle remove frontend --delete-branch    # also delete the local feature branch
knit bundle apply-view backend       # reshape the bundle to match a saved view
```

`knit bundle remove` refuses to discard uncommitted or unpushed work unless `--force`; pass `--keep-worktree` to remove only the tracking entry and leave the worktree on disk. Views sync to KnitHub as the user's own config with `knit view push` / `knit view pull`, are uploaded alongside `knit project push`, and are restored by `knit clone`.

Projects can define a default landing template. `knit land plan` expands it into the bundle-specific `.knit/land-plans/<bundle-id>.land.json`, where it can still be edited for that one bundle before `knit land apply`:

```json
{
  "landing": {
    "provider": "github",
    "merge": {
      "repoOrder": ["arbient-odds-store", "scrapers", "betsnitch", "arbient-engine", "betsnitch-frontend"],
      "method": "merge",
      "requiredChecksOnly": true
    },
    "deployments": [
      {
        "id": "deploy-betsnitch",
        "repoId": "betsnitch",
        "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
        "command": ["fly", "deploy"]
      },
      {
        "id": "deploy-frontend",
        "repoId": "betsnitch-frontend",
        "mode": "push"
      }
    ]
  }
}
```

Deployment entries are first-class landing steps. Command deployments run without a shell unless the command explicitly invokes one, and a deployment checkout uses a managed `.knit/land-worktrees/<bundle>/<repo>/<branch>/` checkout so the feature worktree is not switched away from its Knit branch. `update: "pull"` and `update: "fetch"` both refresh the managed checkout from the configured remote branch before running the command.

Default project repos are included by `knit bundle`; observed repos are available by id but are not branched or tracked until added explicitly:

```sh
knit bundle "venue capacity"
knit bundle add docs
```

Bundles are the branch-like feature units. The same source repo can appear in many bundles at once. Knit creates separate feature branches and generated worktrees, for example `.knit/worktrees/fix-a/backend` and `.knit/worktrees/fix-b/backend`.

Use `knit bundle "<title>" --cd` to create the bundle from the current workspace project's default repos and immediately start your shell in `.knit/worktrees/<bundle>`. That bundle worktree root gets its own `AGENTS.md` with bundle-wide guidance. Pass `--project` when you want a project other than the current one, pass `--repo` only when you want to limit which repos are included, and pass a `--cd` value such as `--cd backend` only when you want a specific repo checkout instead.

For parallel agent work, move each agent into the generated checkout it owns, such as `.knit/worktrees/fix-a/backend`. Commands run from inside a generated checkout resolve that checkout's bundle from the path, independent of the shared workspace fallback.

For coding agents in the source workspace, "move into the checkout" means each shell/tool call must actually run with that checkout as its cwd/workdir. A narrated `cd`, or a `cd` from a previous non-persistent shell command, is not enough. If this agent is working on one feature, open the generated worktree folder and keep tool calls rooted there. If several agents or features are active, open a separate folder or agent rooted at each new worktree. From the source workspace, use explicit `--bundle <bundle>` on bundle-scoped Knit commands for the feature being changed:

```sh
knit --bundle fix-a status
knit --bundle fix-a add
knit --bundle fix-a commit --all -m "Describe the feature change"
knit --bundle fix-a push --set-upstream
```

Do not use bare `knit switch <bundle>` from the workspace root to recover context. Root-level switching requires `--workspace` so changing the shared fallback is always deliberate.

When more than one open bundle exists, Knit refuses source-root status and mutating commands that would use the shared workspace fallback. Use `knit --bundle <bundle> ...` from the source workspace or run the command from the intended worktree.

Compatibility bundles are ordinary bundles created from the union of repos in other bundles. They do not have a special target branch; use them as integration branches when two feature bundles need to be made compatible before either one lands:

```sh
knit bundle compat feature-x feature-y --title "x y compat"
knit merge feature-x --into x-y-compat
knit merge feature-y --into x-y-compat --manual
```

When a bundle has grown messy or a previously used PR head branch is no longer a good publishing unit, split selected recorded commits into a fresh bundle instead of continuing to pile onto the old one:

```sh
knit bundle split feature-x HEAD~1 --title "feature x clean follow-up"
knit bundle split feature-x abc123 def456 --repo backend --repo frontend --title "feature x api"
```

`knit bundle split` creates a normal new bundle, materializes the selected repos, cherry-picks the requested source bundle commits, and records the resulting destination commits as observed git movement. If you already have a destination bundle, use `knit cherrypick --from <source-bundle> <selector>...` directly.

`knit bundle add` accepts one or more repo paths or project repo ids. It resolves all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, inferred base branch, and checkout mode. By default it creates the `knit/<bundle-id>` branch and a generated worktree for each tracked repo. Use `--no-worktree` for metadata-only registration.

Generated worktrees get local `AGENTS.md` guidance by default: one bundle-wide guide at `.knit/worktrees/<bundle>/AGENTS.md`, plus repo-local guides inside each generated repo checkout. Those worktree guides assume the agent opened the generated worktree folder directly, so their examples rely on cwd and do not include `--bundle`.

Use `knit bundle "<title>" --agents` when you want Knit to write an `AGENTS.md` tutorial into the source workspace. The workspace guide explains projects, bundles, parallel worktrees, and why source-workspace mutating commands should use explicit `--bundle <bundle>`. Use `knit project agents [name]` or replay `knit init <name> --agents` to write project-specific guidance from the project JSON, including the current default repo list. If `AGENTS.md` already exists, Knit preserves the rest of the file and appends or refreshes its own managed section.

Use `knit bundle add --in-place` or `knit bundle add --in-place` to make Knit operate directly in the original repo checkout instead of creating `.knit/worktrees/<bundle>/<repo>`. Knit will create or check out the `knit/<bundle-id>` branch in that repo. The original checkout must be clean before Knit switches branches. Later mutating commands refuse to operate if the in-place repo is no longer on the expected feature branch.

Base inference prefers the current branch only when it is clean and named `main` or `master`; otherwise it looks for `main`, then `master`. Use `--base` when that is not right.

`knit bundle worktree` is still available as an idempotent repair/rerun command. It creates missing `knit/<bundle-id>` branches and worktrees under `.knit/worktrees/<bundle-id>/<repo-id>`. Existing branches or worktrees are reported and reused where possible.

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
knit bundle close --reason "merged"
```

The close node shows up in `knit log` and `knit show HEAD`. It is a ledger marker only. If that bundle is still the resolved context, `knit status` still shows its generated worktrees and local feature branches because they still exist.

`knit bundle delete <bundle> --force` moves the bundle JSON artifact to `.knit/deleted/bundles/` and clears the active bundle if needed. By default it preserves git state. Add `--worktrees` to remove Knit-generated worktrees for that bundle before moving the artifact. Add `--branches` to delete the local `knit/<bundle>` feature branches after those generated worktrees are removed:

```sh
knit bundle delete documentation-quick-wins --force
knit bundle delete documentation-quick-wins --force --worktrees
knit bundle delete documentation-quick-wins --force --worktrees --branches
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches --remote-branches
```

`--branches` uses `git branch -d`, so it refuses to delete branches with unmerged commits. `--force-branches` uses `git branch -D`. Knit only deletes local feature branches recorded by the bundle unless `--remote-branches` is also passed, which deletes the matching recorded feature branches from `origin` and removes local `origin/<branch>` tracking refs when present.

`knit prune` scans workspace bundles and lists dead-work candidates: clean bundles with no recorded open PRs. Existing PR records are refreshed from GitHub before deciding, missing PR records are allowed, and dirty generated checkouts keep the bundle alive. Add `--no-refresh` for a cached/offline scan. `--worktrees` also removes orphaned `.knit/worktrees/<bundle>` directories that no longer have bundle artifacts when they contain no pending files. Pass `--force` (included in `--all`) to discard uncommitted work and remove dirty orphan worktree dirs too. `--all` is a cleanup preset for generated worktrees, local feature branches, forced local branch deletion, matching `origin` branches, and matching KnitHub remote bundle records. `knit bundle prune` is the longer namespaced form:

```sh
knit bundle prune
knit bundle prune --no-refresh
knit bundle prune --apply --worktrees --branches
knit bundle prune --apply --all
```

A bundle whose only uncommitted work is untracked files is otherwise dead work, so prune does not delete it by default; instead it lists it under "Blocked by untracked files". Pass `--untracked` to treat those bundles as dead-work candidates — combine with `--worktrees` (or `--all`) on `--apply` so the untracked files are discarded with the generated checkout. Bundles with tracked, uncommitted changes are still preserved even with `--untracked`.

`--report` prints every scanned bundle and why it is prunable or kept (open PRs, merged PRs, tracked changes, or untracked-only files), not just the deletable candidates:

```sh
knit bundle prune --report
knit bundle prune --untracked
knit bundle prune --apply --untracked --worktrees
```

Remote bundle cleanup uses the configured KnitHub sync remote, requires a token with `bundle:delete`, and marks matching remote bundle records deleted. Use explicit flags instead of `--all` when you want local/Git branch cleanup but want to preserve KnitHub bundle history.

With `--remote-bundles`, prune also detects **remote orphans**: bundle records that exist on the sync remote but have no local artifact and whose recorded PRs are all merged or closed. Without this, a plain `knit bundle prune --apply` could delete a local artifact while leaving its KnitHub record behind, and no later prune could ever reach it again. These are listed under "Remote orphan bundle candidates" and deleted on `--apply`; their live PR state is refreshed from the host by URL during detection (the synced artifact can be stale), falling back to the recorded state when the lookup fails. Prune is also best-effort: an unreadable bundle file, a failed PR lookup, or an unverifiable checkout is reported as a warning and skipped (the bundle is kept to be safe) instead of aborting the whole scan.

So the common cleanup distinction is:

```sh
knit bundle close --reason "merged"                                           # keep local checkouts/branches
knit clean --closed --worktrees                                        # remove generated worktrees, keep branches
knit bundle delete documentation-quick-wins --force --worktrees --branches
```

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

`knit add` is the staging command (the standalone `knit stage` alias was removed in the CLI cleanup).

`knit status` shows the resolved bundle source, ordinary git status, checkout mode, wrong-branch warnings for in-place repos, and unrecorded commits when a tracked branch moved outside Knit.

`knit diff` prints the resolved bundle and source, then shows cross-repo diffs against each repo's recorded `baseSha`. It follows `git diff`: committed, staged, and unstaged tracked-file changes are shown, while untracked files are not shown until they are added to the index. Use `knit status` or `knit git status --short` to see untracked files. Use `--stat` for a compact summary, or pass repo ids/paths to limit the output:

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

`knit pull` is context-aware. With no target flags:

- **At the workspace base** (the shared workspace fallback, e.g. several open bundles and no specific one resolved) it updates *everything*: every active-project repo's source checkout plus every open bundle, and prints a per-target report instead of refusing.
- **For a specific resolved bundle** (inside a worktree, `--bundle <id>`, `KNIT_BUNDLE`, or a single-bundle workspace) it pulls that bundle's tracked repos and then its KnitHub artifact, as before.

Target flags drive the aggregate, best-effort report directly and may be combined:

- `--main` updates each active-project repo's *source checkout* on its current branch with `git pull --ff-only`.
- `--bundles` updates every open bundle's feature checkouts from its KnitHub artifact.

Aggregate pulls run in parallel — git work on the same source repo is serialized, distinct repos run concurrently — and never abort on the first problem: a dirty tree or non-fast-forward is reported (`skipped`/`failed`) while the rest proceed.

```sh
knit pull                 # at the base: project main repos + every open bundle, reported
knit pull --main          # update all project repos' current branch (fast-forward only)
knit pull --bundles       # fast-forward every open bundle's checkouts from KnitHub
knit pull --main --bundles
knit pull backend         # single-bundle: pull a specific tracked repo's base checkout
knit pull --rebase frontend
```

Single-bundle pulls still default to the original repo path on the recorded base branch with `git pull --ff-only`, updating the recorded `baseSha`, and refuse on uncommitted changes unless `--force` (use `--rebase` for `git pull --rebase`). Use `knit pull --feature` to pull the tracked Knit feature checkout instead; feature pulls are recorded as `git.observed` nodes when the feature branch head moves.

`knit push` pushes tracked feature branches to `origin`. It does not create PRs, update GitHub metadata, or change bundle state. Selected repo pushes run in parallel; bundle artifact and history sync wait until every Git push succeeds. By default it pushes the current feature branch to `origin/<branch>` without setting upstream; use `--set-upstream` when you want git's upstream tracking configured:

```sh
knit push
knit push backend
knit push --all
knit push --set-upstream frontend
```

`knit publish` publishes tracked feature branches to a code host. Knit is host-independent: it detects each repo's host from its git remote and drives that host's CLI. GitLab (`glab`, merge requests) and Codeberg/Forgejo (`tea`, pull requests) are detected from their remote hosts; every other remote defaults to GitHub (`gh`, pull requests). Install and authenticate the matching CLI for the repos you are publishing.

```sh
knit publish create
knit publish create --draft
knit publish create backend
knit publish create --base release
knit publish create --base backend=stable --base frontend=main
knit publish create --no-sync
knit publish create --no-remote
knit publish sync
knit publish status
```

`knit publish github …` is kept as a back-compat alias for `knit publish create/sync/status`.

`knit publish create` is a best-effort two-phase operation. It pushes every selected tracked feature branch, creates missing review objects (PRs/MRs) or reuses an existing one for the same feature/base branch, stores publishing metadata in the bundle's `publications`, then rewrites the managed Knit block in every selected review body with the complete cross-repo list. The base defaults to each repo's bundle `baseBranch`; pass `--base release` to use the same base for every selected repo, or repeat `--base repo=branch` for per-repo bases. Body sync is on by default; `--sync` is accepted for explicitness, and `--no-sync` skips that second phase. If body sync fails after review objects were created, run `knit publish sync` after fixing auth or network issues.

Hosted services that run Knit from bundle artifacts can set `KNIT_GITHUB_API_TRANSPORT=curl-ipv4` to make GitHub artifact-mode publish and landing use direct GitHub REST API calls through `curl --ipv4` instead of `gh pr ...` commands. This requires `curl` and `GH_TOKEN` or `GITHUB_TOKEN` in the subprocess environment. It is intended for non-interactive runtimes where provider CLI prompts, host credential stores, or default IPv6 routing can hang simple GitHub I/O. Local workspace commands keep using the normal provider CLIs unless this environment variable is set.

When KnitHub sync remotes are configured, `knit publish create` and `knit push` also push the bundle artifact to those remotes so the host and KnitHub stay in sync. This is on by default; disable it globally with `knit config set push-sync false`, skip it for one command with `--no-remote`, or force one or more remotes with repeated `--remote <name>`.

Remotes can be workspace-local or user-global. Workspace `.knit/config.json` remotes override global remotes of the same name; otherwise commands fall back to the user-level config at `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. This lets every workspace share the same hosted KnitHub remote unless a workspace deliberately points that name somewhere else:

```sh
knit remote add --global knithub https://api.knithub.dev
export KNIT_REMOTE_KNITHUB_TOKEN="<KnitHub API token>"
knit config set --global sync-remotes knithub
knit config show
knit remote show knithub
```

Workspace-only overrides stay local:

```sh
knit remote add local http://localhost:4000
knit config set sync-remotes local,knithub
knit push
```

Knit preserves user-written PR text and only replaces the block between `<!-- BEGIN KNIT BUNDLE -->` and `<!-- END KNIT BUNDLE -->`.

When PRs are approved and the user says to land, merge, release, ship, or continue after review, keep the workflow on the Knit bundle:

```sh
knit publish status
knit land
knit land apply
```

Do not merge the host review objects directly (for example `gh pr merge`) for Knit-owned bundles, and do not use `knit merge --into main` as a substitute for PR landing unless you explicitly want direct branch integration instead of PR landing.

`knit land` coordinates landing the recorded cross-repo review set. It resolves each repo's host adapter from its remote (GitHub, GitLab, or Codeberg/Forgejo):

```sh
knit land plan
knit land check
knit land update --push
knit land apply
knit land status
knit land resume
```

`knit land check` is a read-only preflight: it fetches each recorded PR once and prints a readiness table (state, mergeable, checks, review decision, and a verdict) so you can see whether `knit land apply` will succeed and why not. A `conflict` verdict points you at `knit land update`; an already-merged PR shows `already landed`. `knit publish status --live` shows the same live columns alongside the recorded review objects. Both are non-mutating.

`knit land plan` writes an editable JSON plan to `.knit/land-plans/<bundle-id>.land.json`. Without a project landing template, the default plan is linear in bundle repo order, merges each recorded GitHub PR into that PR's GitHub base branch with `merge`, waits for required checks, and does not delete feature branches. With a project landing template, Knit uses the configured merge priority, merge defaults, and deployment list. In Knit, a PR with no required checks has passed the required-check gate. You can edit the generated bundle plan to change merge order, use `squash` or `rebase`, insert `wait_checks` steps, insert local `run` steps, or tune typed `deploy` steps before applying.

Bare `knit land` is safe: it creates or shows the default plan and stops. It never merges PRs, deploys, waits, or runs plan commands. Execute the plan explicitly with `knit land apply` after inspection.

`knit land update` prepares published PR branches for landing by fetching each PR's base branch, merging that base into the feature checkout, and recording the movement as a first-class `land.update` bundle node. This is the preferred way to resolve routine "base moved" landing conflicts because the integration merge is attributed to landing prep instead of appearing later as an incidental `git.observed` movement. Pass `--push` to push the updated feature branches after recording the node. If a merge conflicts, resolve and commit it in the feature checkout, then run `knit land update --continue-merge` to record the already-resolved movement as `land.update`.

`knit land apply` preflights referenced PRs, refuses draft/closed/missing PRs, writes a durable run file under `.knit/land-runs/`, then executes the plan step by step. Already-merged PRs are treated as satisfied and skipped (whether or not a prior run exists), and an open PR that conflicts with its base is rejected with guidance to run `knit land update` first. `deploy` steps support `deploymentMode: "command"` for real deployment commands and `deploymentMode: "push"` for deployments that are triggered by the PR merge itself. A command deployment can specify a `checkout` branch; Knit creates or refreshes a managed detached checkout under `.knit/land-worktrees/` before running the command. If a step fails, the run stops and records the exact step status, stdout/stderr for `run` and command `deploy` steps, and failure detail. `knit land resume` continues that run from pending or failed steps only; succeeded steps are not repeated. A fully successful run appends a `feature.landed` node to the bundle with the plan id, run id, provider, repo ids, and publication URLs, then syncs the updated bundle artifact to configured KnitHub remotes when push-sync is enabled. Use repeated `--remote <name>` to force remotes, `--no-remote` to skip this sync, or `knit land sync` to push the landed artifact later.

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

Knit also keeps a project-wide history ledger under `.knit/history/<project>.history.jsonl` and syncs it with KnitHub when history APIs are available. This ledger is metadata only: it records bundle ids, repo ids, branch names, Knit node ids, timestamps, and Git commit SHAs. Git remains the source of truth for file contents, diffs, and file-level history.

Use `knit history list` to inspect the local project history, `knit history refresh` to rebuild it from local bundle artifacts, and `knit history push`, `knit history pull`, or `knit history sync` to exchange history events with a KnitHub remote.

Use `knit related` before editing a file or area with possible cross-repo coupling. The command asks Git which commits touched the path, joins those SHAs to Knit history, then expands matching events to their bundle, commit group, and companion repo commits:

```sh
knit related --repo frontend src/routes/billing.tsx
knit related frontend/src/routes/billing.tsx
knit related --repo frontend src/routes/billing.tsx --pull
```

The output includes the touched-path commits, related commits in the same Knit scope, other commits from the same bundle, and `git show --stat` commands for inspection. Commits made wholly outside Knit appear in Git history but only appear in Knit-related results after they have been recorded into a bundle, for example with `knit sync`.

`knit show <target>` uses the same bundle log selectors as `knit revert`: `HEAD`, `HEAD~1`, full node ids, unique node id prefixes, commit group ids, and recorded git commit SHAs. Commit and revert group nodes show `git show --stat --oneline` for each repo commit. Observed git nodes show the branch movement and the relevant added or dropped commits when those commits are still available locally.

`knit revert <target>` resolves bundle log selectors like `HEAD`, `HEAD~1`, full node ids, unique node id prefixes, and git commit SHAs shown in `knit log`. A commit SHA resolves to the latest bundle node that mentions that commit, so if a commit was later observed as dropped by a reset, reverting by that SHA restores it from the latest rewind node. By default it writes a checked revert plan under `.knit/revert-plans/` and prints the per-repo operations. `knit revert <target> --apply` requires that plan to exist. For local git entries, it verifies each affected worktree is still clean and at the planned head, then creates one revert commit per affected repo and appends a `revert.group` node. For a landed PR group, it verifies the recorded PRs are merged, runs the provider-native PR revert for each repo (`gh pr revert` for GitHub), records the newly opened revert PRs as the current publications, and appends a `pr.revert` node so the group can be landed through Knit.

Revert behavior is based on the target node:

- `commit.group` and `revert.group`: revert the recorded commits.
- `git.observed` with `advanced`: revert the observed commits.
- `git.observed` with `rewound`: cherry-pick the dropped commits back.
- `git.observed` with `diverged`: revert added commits, then cherry-pick dropped commits.
- `feature.landed`: create provider-native revert PRs for the landed PR group across repos.

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

`knit reset` runs `git reset` across tracked checkouts, mirroring git's own modes: `--soft` moves the branch pointer only, `--mixed` (the default) resets the index but keeps the working tree, and `--hard` resets the index and working tree. The optional `<commit>` defaults to `HEAD`. Scope is context-aware: when a bundle is resolved explicitly (`--bundle`), via `KNIT_BUNDLE`, a worktree cwd, or folder context, reset targets that bundle's checkouts; run from the workspace root it instead resets the active project's source repo checkouts, which is the fast way to discard changes a tool made directly on the source branches without a bundle. Like git, `--hard` does not remove untracked files; follow up with `knit git --all clean -fd` if you also need to drop new untracked files.

```sh
knit reset --hard --all          # discard tracked changes in every source repo (from workspace root)
knit reset --hard --repo knit    # just one repo
knit --bundle feature-a reset --hard --all
knit reset --soft HEAD~1         # undo the last commit, keep the changes
```

Knit colors interactive terminal output for scanability. It disables color automatically when output is piped, when `NO_COLOR` is set, or when `TERM=dumb`. Use `KNIT_COLOR=always` or `KNIT_COLOR=never` to force a mode.

If a tracked branch is reset backward, `knit status` reports rewound commits and `knit sync` records a `git.observed` node with `movement: "rewound"` and `droppedCommits`. Existing `commit.group` nodes remain as history; current state is derived from each repo's latest `headSha`.

`knit commit` commits only repos with staged changes in their tracked checkouts. With `-a`/`--all`, it stages first and then commits. `knit commit` also syncs unrecorded git commits before creating a new logical commit group, so the ledger remains ordered.

The git commits are created sequentially, one repo at a time. Knit records them as one logical commit group in the bundle. Every repo commit gets the same logical message plus trailers:

```txt
Knit-Group: <commit-group-id>
Knit-Bundle: <bundle-id>
```

The bundle records the full mapping from logical commit group to repo commit SHAs.

`knit bundle remove <repo-id>...` removes repos from the bundle and appends a `repo.removed` node, tearing down their worktrees by default (`--keep-worktree` to only untrack, `--delete-branch` to also drop the feature branch, `--force` to discard dirty/unpushed work).

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
- `pr.revert`
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
- `knit push` pushes feature branches to `origin` and, when KnitHub sync remotes are configured and `push-sync` is enabled, the bundle artifact to those remotes; use `knit publish create` to publish review objects.
- `knit publish` detects the host from each repo's remote: GitLab uses `glab`, Codeberg/Forgejo uses `tea`, and every other remote defaults to GitHub's `gh`. The matching CLI must be installed and authenticated. Bitbucket and other hosts would need new adapters. The GitLab and Forgejo adapters target current `glab`/`tea` JSON; their field mapping may need tuning across CLI versions, and `tea` does not surface commit-status checks, so landing treats Forgejo PRs as having no required checks.
- `knit publish create` is not perfectly transactional. Branch pushes, review creation, and body updates happen sequentially. If phase two fails after review objects are created, run `knit publish sync`.
- `knit land` resolves the host adapter per repo from its remote. A merge lands into the recorded base branch. Remote merges cannot be automatically unmerged by Knit, so failed land runs are recorded in `.knit/land-runs/`; fix the failed step and use `knit land resume`.
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

## Active Work Items

- `eb83e259-da29-4f80-a813-5ed08acd54cd` — update the docs with a comment about this work item

## Roadmap

- Standalone JSON Schema for `ChangeGroup`
- Safer partial-failure recovery for multi-repo commits
- More host adapters (e.g. Bitbucket) and richer GitLab/Forgejo check integration
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
