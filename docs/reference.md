# Knit Reference

This is the full command and behavior reference for Knit. For a guided introduction, see the [quickstart](quickstart.md).

## Storage

Knit stores local state under the directory where `knit init`, or `knit bundle` first creates a workspace:

```txt
.knit/
  config.json
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

The bundle file is the source of truth for a feature. `config.json` tracks workspace fallback state, while generated worktree paths let multiple agents work in parallel bundles without fighting over one global active bundle.

User-global Knit config lives outside the workspace at `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. Workspace `.knit/config.json` overrides global values of the same name.

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
knit bundle                          # show the resolved bundle
knit bundle "<title>"                # create a bundle (git-branch-style shorthand)
knit bundle "<title>" [--project <name>] [--repo <repo-id>]... [--all-repos] [--view <name>] [--include <repo>]... [--exclude <repo>]... [--no-worktree] [--in-place] [--force] [--agents] [--cd [<repo>]]
knit bundle add <repo-path-or-project-repo-id>... [--base <branch>] [--in-place] [--no-worktree]
knit bundle remove <repo-id>... [--keep-worktree|--delete-branch] [--force]
knit bundle worktree
knit bundle apply-view <name> [--keep-worktree|--delete-branch] [--force]
knit bundle list [--all] [--archived] [--deleted]
knit bundle archive <bundle> [--reason <reason>] [--keep-worktrees] [--force]
knit bundle restore <bundle>
knit bundle delete <bundle> --force [--worktrees] [--branches] [--force-branches] [--remote-branches]
knit bundle prune [--no-refresh] [--apply] [--all] [--worktrees] [--force] [--branches] [--force-branches] [--remote-branches] [--remote-bundles]
knit bundle path
knit bundle print
knit bundle validate
knit switch <bundle> --workspace
knit add [-r <repo>] [-N] [-u] [repo-or-pathspec...]
knit clean [--plans] [--worktrees] [--archived] [--merge-worktrees] [--all] [--force]
knit status
knit diff [--stat] [repo-id-or-path...]
knit fetch [--all] [repo-id-or-path...]
knit pull [--main] [--bundles] [--all] [--rebase] [--force] [--feature] [repo-id-or-path...]
knit push [--all] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit run <project-command> [--repo <repo>]... [--all]
knit run [--repo <repo>] [--all] -- <command> [args...]
knit run up|status|down                        # bundle runtime stack
knit run --list
knit check run <project-command> [--repo <repo>]... [--all]
knit check record <name> --pass|--fail [--detail <text>]
knit check status
knit publish create [--provider <id>|--github] [--base <branch>|--base <repo=branch>] [--draft] [--sync|--no-sync] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit publish sync [--provider <id>|--github] [repo-id-or-path...]
knit publish status [--live] [--provider <id>|--github] [repo-id-or-path...]
knit request ...                               # alias for `knit publish`
knit land
knit land plan [--provider github|gitlab|forgejo] [--out <path>] [--force]
knit land check
knit land update [--push] [--continue-merge] [repo-id-or-path...]
knit land apply [--plan <path>] [--keep-worktrees] [--remote <remote>]... [--no-remote]
knit land resume [--run <path>] [--remote <remote>]... [--no-remote]
knit land rollback [--run <path>] [--apply]
knit land status [--run <path>]
knit merge <source-bundle-or-ref> --into <target-branch-or-bundle> [--fetch] [--push] [--set-upstream] [--manual]
knit merge status [--run <id-or-path>]
knit merge show [--run <id-or-path>]
knit merge push [--run <id-or-path>] [--repo <repo-id>]... [--set-upstream]
knit merge --continue
knit merge --abort
knit config set advice true|false
knit config set stealth true|false
knit config set push-sync true|false
knit config set sync-remote <name>
knit config set sync-remotes <name>[,<name>...]
knit schema print <bundle|project|merge-run|land-plan|land-run|config>
knit doctor
knit migrate [--check]
knit sync                                      # record git commits made outside Knit (local reconcile)
knit sync push [--bundles] [--history] [--views] [--architecture] [--kg] [--all] [--remote <name>]...
knit sync pull [--bundles] [--history] [--views] [--all] [--remote <name>]...
knit history [list] [-n <count>] [--repo <repo>] [--bundle <bundle>] [--project <project>]
knit history refresh [--project <project>]
knit related [--repo <repo>] [--project <project>] [--pull] [--remote <name>] [--limit <count>] [--commit-limit <count>] <path>...
knit commit -m "<message>" [--stage]
knit log [-<count>]
knit log [-n [count]]
knit revert <sha|node|HEAD|HEAD~N> [--plan]
knit revert <sha|node|HEAD|HEAD~N> --apply
knit git [--repo <repo>] [--all] <git-args...> [repo-selector...]
knit show <sha|node|HEAD|HEAD~N>
```

A bundle is the cross-repo analogue of a git branch: `knit bundle "<title>"` creates one (like `git branch <name>`), `knit bundle` shows the current one, and creation flags go straight on it, e.g. `knit bundle "<title>" --project <name> --repo <repo>`. A project is initialized once with `knit init <name>` (like `git init`). Everyday VCS verbs (`add`, `commit`, `push`, `pull`, `switch`, `status`, `diff`, `log`, `revert`, …) live at the top level; bundle/repo management lives under `knit bundle`.

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

### Bundle runtimes

For the step-by-step "prepare a new project" walkthrough, see [runtime-setup.md](runtime-setup.md); this section is the behavior reference.

Three more `knit run` verbs start a disposable stack instance per bundle — the same composed shape the repos already run, with different ports and the bundle's code substituted in:

```sh
knit run up        # build and start the bundle stack
knit run status    # live service states, ports, and URLs
knit run down      # stop and remove the bundle stack
```

`knit run up` lifts every bundle repo with a compose file — the runtime is "docker compose up in each repo, with the bundle's code" — with zero configuration. `runtime.stacks` narrows the set to explicit repo ids, and the legacy `runtime.stackRepo` forces a single stack. Each stack's compose file is `runtime.composeFile` when set (applying to the configured stack repo), else `docker-compose.knit.yml` when present, else the repo's own `docker-compose.yml`/`compose.yaml`. A single stack runs as compose project `knit-run-<bundle>`; several stacks run as `knit-run-<bundle>--<repo>` each, so networks and named volumes stay isolated per stack. References from one stack's environment to a sibling stack's published host port are rewritten to the sibling's freshly allocated bundle port, so stacks find each other's bundle instances; ports of repos outside the bundle are left alone and keep pointing at the dev instances. `down`/`status` resolve containers by project label, so they keep working even after the worktree is gone. Run state lands in `.knit/runtime-runs/<bundle>/state.json`, recorded only after every stack starts; if `up` fails partway, `knit run down` still cleans up by derived project names. A project command configured with one of these three names takes precedence over the runtime verb.

**Transform mode (default).** A plain compose file — the one developers already use on `main` — is lifted automatically. Knit resolves it with `docker compose config` against the source repo location, then rewrites the resolved shape:

- every path that resolves inside a tracked repo's source checkout (build contexts, additional contexts, dockerfiles, build args, bind-mount sources) is remapped to that repo's bundle worktree — "main everywhere, except the repos this bundle changes"
- every published host port is reallocated to a free one (stepping by `ports.step` from the original), container-side ports untouched
- textual references to remapped host ports inside environment values and build args are rewritten (`http://localhost:5173` -> `http://localhost:5183`) — heuristic by design, since shifted host ports are otherwise invisible to app config
- `container_name` and the top-level `name` are stripped so instances cannot collide

**Contract mode.** A compose file named `docker-compose.knit.yml` or containing `${KNIT_*}` variable references opts out of transformation and is run as-is with the contract injected — full control for stacks with unusual builds. `runtime.mode` (`transform`/`contract`) forces a mode when detection is wrong:

| Variable | Value |
| --- | --- |
| `KNIT_ROOT` / `KNIT_BUNDLE` | workspace root and bundle id |
| `COMPOSE_PROJECT_NAME` | `knit-run-<bundle>` |
| `KNIT_CHECKOUT_<REPO>` | absolute checkout path (bundle worktree when tracked, source path otherwise) |
| `KNIT_SRC_<REPO>` | the same path relative to `KNIT_ROOT` |
| `KNIT_REV_<REPO>` | HEAD revision of that checkout |
| `KNIT_PORT_<SERVICE>` | one allocated free host port per pool in `ports.services` (service name -> base port), stepping all pools together by `ports.step`; with no `services` map, a backend/frontend pair from `ports.backendBase`/`frontendBase` |
| `KNIT_DB_MODE`, `KNIT_DB_HOST`, `KNIT_DB_PORT`, `KNIT_DB_NAME`, `KNIT_DB_HOST_PORT` | resolved database identity |

Repo and service ids are uppercased with non-alphanumerics mapped to `_` (`gloss-web-ui` -> `KNIT_CHECKOUT_GLOSS_WEB_UI`).

In contract mode the `database` block picks between two modes. `shared` attaches the stack to an existing dev database on `host`/`port` and fails fast when it is unreachable (an optional `startCommand`, run in the stack checkout, can boot it). `bundle` gives each runtime its own database: Knit names it from `nameTemplate` (`{bundleId}` substituted), publishes it on `portBase`, and activates the compose file's `bundle-db` profile so a profile-gated database service starts.

In transform mode the lifted shape brings its own database service by default, with a fresh project-scoped volume per bundle — isolated and empty. To test bundles against real dev data instead, set `database.mode: "shared"` and name the compose service that IS the database in `database.service`: the service is stripped from every lifted stack that has it, and references to it in environments and build args are rewired to `host`/`port` — connection URLs (`@db:5432` → `@host:port`), values exactly equal to the service name (split HOST vars), and values equal to `containerPort` (default 5432) whose key mentions PORT. Reachability is checked before anything starts. Note the tradeoff: bundle code, including its migrations, then runs against the shared dev database.

### Checks

A **check** is a named verdict recorded on the bundle ledger — the bundle-level analogue of a commit status. Each verdict is pinned to the exact per-repo head SHAs it was computed against, so it can never silently claim more than it saw:

```sh
knit check run ci          # run the project command `ci`, record pass/fail
knit check record functional --pass --detail "manual QA on staging"
knit check status          # latest verdict per check, with freshness
```

`knit check run <name>` executes the configured project command of that name (the same definition `knit run <name>` uses — define it with `knit project command set ci -- cargo test`) and records a `check.recorded` node: pass if every targeted repo exited 0, fail otherwise. A failing run is still recorded before the command errors, so the red verdict is on the ledger. `knit check record` is the door for verdicts computed elsewhere — another tool, a host CI run, a human — without making that tool a second source of truth: the record always lives in the bundle artifact and syncs to KnitHub with it.

**Freshness.** A verdict is *fresh* while every repo currently tracked in the bundle still sits on the head SHA the verdict was pinned to. Any new commit, any repo added later, and the verdict reads *stale*. There is no way to assert "merge ready" directly — readiness is always derived: required checks green **and** fresh at the current heads. `knit check status` shows both dimensions:

```txt
check       status  state   recorded
ci          green   fresh   2026-06-12T09:14:03.118Z knit@b245236
functional  green   stale   2026-06-11T22:40:11.402Z knit@9020475
```

**Gating landing.** Checks are purely informational by default: recording them never blocks anything, and projects that configure nothing are completely unaffected. Gating is opt-in — a project that wants it requires named checks in its landing template:

```json
{ "landing": { "requireChecks": ["ci"] } }
```

`knit land plan` copies `requireChecks` into the editable per-bundle plan, `knit land check` reports each required check (green/red/stale/missing) and counts anything non-green as blocked, and `knit land apply`/`resume` refuse to execute until every required check is green and fresh — `--skip-checks` is the explicit escape hatch. Re-record after the last commit, land while it is still fresh.

Checks are attestations, not hosted CI: Knit runs one command per check, the exit code is the verdict, and whoever can write the bundle can record one — the same trust model as committing. Knit never schedules, watches, or retries checks.

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

`knit bundle remove` refuses to discard uncommitted or unpushed work unless `--force`; pass `--keep-worktree` to remove only the tracking entry and leave the worktree on disk. Views sync to KnitHub as the user's own config with `knit sync push --views` / `knit sync pull --views`, are uploaded alongside `knit project push`, and are restored by `knit clone`.

Projects can define a default landing template. `knit land plan` expands it into the bundle-specific `.knit/land-plans/<bundle-id>.land.json`, where it can still be edited for that one bundle before `knit land apply`:

```json
{
  "landing": {
    "provider": "github",
    "onFailure": "rollback",
    "merge": {
      "repoOrder": ["schema-store", "scrapers", "backend", "engine", "frontend"],
      "method": "merge",
      "requiredChecksOnly": true
    },
    "deployments": [
      {
        "id": "deploy-backend",
        "repoId": "backend",
        "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
        "command": ["fly", "deploy"]
      },
      {
        "id": "deploy-frontend",
        "repoId": "frontend",
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

When two feature bundles need to be made compatible before either one lands, start an ordinary bundle with the union of their repos and merge both in:

```sh
knit bundle "x y compat" --repo backend --repo frontend
knit merge feature-x --into x-y-compat
knit merge feature-y --into x-y-compat --manual
```

When a bundle has grown messy or a previously used PR head branch is no longer a good publishing unit, start a fresh bundle and cherry-pick the commits worth keeping instead of continuing to pile onto the old one:

```sh
knit bundle "feature x clean follow-up" --repo backend
knit cherrypick --from feature-x HEAD~1
```

`knit cherrypick` records the resulting destination commits as observed git movement.

`knit bundle add` accepts one or more repo paths or project repo ids. It resolves all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, inferred base branch, and checkout mode. By default it creates the `knit/<bundle-id>` branch and a generated worktree for each tracked repo. Use `--no-worktree` for metadata-only registration.

Generated worktrees get local `AGENTS.md` guidance by default: one bundle-wide guide at `.knit/worktrees/<bundle>/AGENTS.md`, the parent directory of every repo checkout. Knit never writes `AGENTS.md` inside a repo checkout — a repo that tracks its own `AGENTS.md` would commit the bundle-specific section and conflict on every publish. The bundle guide assumes the agent opened the generated worktree folder directly, so its examples rely on cwd and do not include `--bundle`.

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

`knit bundle archive <bundle>` marks a bundle done. It appends a `feature.archived` node (with an optional `--reason`), removes the bundle's generated worktrees, and preserves local feature branches and the JSON artifact:

```sh
knit bundle archive feature-x --reason "merged"
knit bundle archive feature-x --keep-worktrees   # ledger/state change only
knit bundle restore feature-x                    # reopen; `knit bundle worktree` rematerializes checkouts
```

Archiving refuses to discard dirty generated worktrees unless `--force` is passed.

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
knit bundle archive documentation-quick-wins --reason "merged"              # remove worktrees, keep branches
knit bundle delete documentation-quick-wins --force --worktrees --branches  # discard everything local
```

`knit clean` removes only Knit-generated local state after an explicit target flag. It never deletes source repos or git branches:

```sh
knit clean --plans
knit clean --worktrees
knit clean --archived --worktrees
knit clean --merge-worktrees
knit clean --all
```

`--plans` removes `.knit/revert-plans`. `--worktrees` removes generated worktrees for the resolved bundle with `git worktree remove` and clears their recorded `worktreePath`; in-place checkouts are preserved. `--archived --worktrees` applies that cleanup to archived bundles (for example ones archived with `--keep-worktrees`). `--merge-worktrees` removes clean branch-target merge worktrees for succeeded or aborted merge runs. Use `--force` to pass `--force` to `git worktree remove` for dirty generated worktrees.

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

`knit publish create` auto-detects each repo's host (GitHub, GitLab, Forgejo/Codeberg) and publishes to all of them. Pass `--provider <id>` (or the `--github` shorthand) to restrict a run to repos on a single host. `knit request` is an alias for `knit publish`.

`knit publish create` is a best-effort two-phase operation. It pushes every selected tracked feature branch, creates missing review objects (PRs/MRs) or reuses an existing one for the same feature/base branch, stores publishing metadata in the bundle's `publications`, then rewrites the managed Knit block in every selected review body with the complete cross-repo list. The base defaults to each repo's bundle `baseBranch`; pass `--base release` to use the same base for every selected repo, or repeat `--base repo=branch` for per-repo bases. Body sync is on by default; `--sync` is accepted for explicitness, and `--no-sync` skips that second phase. If body sync fails after review objects were created, run `knit publish sync` after fixing auth or network issues.

Hosted services that run Knit from bundle artifacts can set `KNIT_GITHUB_API_TRANSPORT=ipv4` (the historical `curl`/`curl-ipv4` values still work, as do `native`/`api`) to make GitHub artifact-mode publish and landing use Knit's built-in GitHub REST client instead of `gh pr ...` commands. The client resolves hostnames IPv4-first and requires `GH_TOKEN` or `GITHUB_TOKEN` in the environment; no external `curl` is needed. It is intended for non-interactive runtimes where provider CLI prompts, host credential stores, or default IPv6 routing can hang simple GitHub I/O. Local workspace commands keep using the normal provider CLIs unless this environment variable is set. `KNIT_GITHUB_API_BASE` overrides the API base URL (defaults to `https://api.github.com`), mainly for tests.

When KnitHub sync remotes are configured, `knit publish create` and `knit push` also push the bundle artifact to those remotes so the host and KnitHub stay in sync. This is on by default; disable it globally with `knit config set push-sync false`, skip it for one command with `--no-remote`, or force one or more remotes with repeated `--remote <name>`. A missing implicit sync remote is skipped after the git branch push; explicitly requested remotes still have to exist.

### Syncing artifacts with KnitHub

`knit sync` with no subcommand is a local-only reconcile: it records git commits made outside Knit as `git.observed` nodes and never touches the network. Its `push`/`pull` subcommands are the one verb family for moving Knit artifacts (bundles, project history, and saved views) between the workspace and KnitHub remotes:

```sh
knit sync push                 # push bundle + history + views + architecture for the resolved project/bundle
knit sync push --bundles       # push only the bundle artifact (e.g. after landing)
knit sync push --history       # push only project history events
knit sync push --views         # push only your saved views
knit sync push --kg            # push the knowledge-graph viz slice (explicit only)
knit sync pull                 # pull bundle + history + views
knit sync pull --history       # pull only project history events
knit sync push --remote hosted    # use an explicit remote
```

With no target flag (`--bundles`/`--history`/`--views`/`--architecture`/`--all`), `knit sync push`/`pull` move every routine artifact family. The knowledge-graph viz slice (produced by `urdir kg viz`, often several MB) is deliberately excluded from `--all` and bare invocations — push it with an explicit `knit sync push --kg` after regenerating it. Remotes default to the configured sync remotes (`knit config set sync-remotes ...`, then `sync-remote`), falling back to the sole configured remote; override with one or more `--remote <name>`.

The git-shaped verbs keep their git semantics but route through the same internal sync module: `knit push --remote <name>` still pushes branches and then the bundle artifact, and `knit fetch --bundles` / `knit pull --bundles` still pull recorded bundle state. Landing's automatic artifact sync (when `push-sync` is enabled) goes through the same module too. There is one implementation behind several differently shaped doors.


Remotes can be workspace-local or user-global. Workspace `.knit/config.json` remotes override global remotes of the same name; otherwise commands fall back to the user-level config at `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. This lets every workspace share the same hosted KnitHub remote unless a workspace deliberately points that name somewhere else:

```sh
knit remote add --global hosted https://<your-knit-api-url>
export KNIT_REMOTE_HOSTED_TOKEN="<KnitHub API token>"
knit config set --global sync-remotes hosted
knit config show
knit remote show hosted
```

Workspace-only overrides stay local:

```sh
knit remote add staging http://localhost:4000
knit config set sync-remotes staging
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
knit land rollback
```

`knit land check` is a read-only preflight: it fetches each recorded PR once and prints a readiness table (state, mergeable, checks, review decision, and a verdict) so you can see whether `knit land apply` will succeed and why not. A `conflict` verdict points you at `knit land update`; an already-merged PR shows `already landed`. `knit publish status --live` shows the same live columns alongside the recorded review objects. Both are non-mutating.

`knit land plan` writes an editable JSON plan to `.knit/land-plans/<bundle-id>.land.json`. Without a project landing template, the default plan is linear in bundle repo order, merges each recorded GitHub PR into that PR's GitHub base branch with `merge`, waits for required checks, and does not delete feature branches. With a project landing template, Knit uses the configured merge priority, merge defaults, and deployment list. In Knit, a PR with no required checks has passed the required-check gate. You can edit the generated bundle plan to change merge order, use `squash` or `rebase`, insert `wait_checks` steps, insert local `run` steps, or tune typed `deploy` steps before applying.

Bare `knit land` is safe: it creates or shows the default plan and stops. It never merges PRs, deploys, waits, or runs plan commands. Execute the plan explicitly with `knit land apply` after inspection.

`knit land update` prepares published PR branches for landing by fetching each PR's base branch, merging that base into the feature checkout, and recording the movement as a first-class `land.update` bundle node. This is the preferred way to resolve routine "base moved" landing conflicts because the integration merge is attributed to landing prep instead of appearing later as an incidental `git.observed` movement. Pass `--push` to push the updated feature branches after recording the node. If a merge conflicts, resolve and commit it in the feature checkout, then run `knit land update --continue-merge` to record the already-resolved movement as `land.update`.

`knit land apply` preflights referenced PRs, refuses draft/closed/missing PRs, writes a durable run file under `.knit/land-runs/`, then executes the plan step by step. Already-merged PRs are treated as satisfied and skipped (whether or not a prior run exists), and an open PR that conflicts with its base is rejected with guidance to run `knit land update` first. `deploy` steps support `deploymentMode: "command"` for real deployment commands and `deploymentMode: "push"` for deployments that are triggered by the PR merge itself. A command deployment can specify a `checkout` branch; Knit creates or refreshes a managed detached checkout under `.knit/land-worktrees/` before running the command. If a step fails, the run stops and records the exact step status, stdout/stderr for `run` and command `deploy` steps, and failure detail; generated bundle worktrees are left intact so `knit land resume` and `knit land rollback` can continue from the recorded run. `knit land resume` continues that run from pending or failed steps only; succeeded steps are not repeated.

A failed run can leave some PRs merged and others not — merged PRs cannot be un-merged, so Knit offers compensation instead of reset. `knit land rollback` previews the merge steps the failed run completed (verifying each PR is live-MERGED), and `knit land rollback --apply` opens a provider-side revert PR for each of them, records a `pr.revert` node targeting the run, and marks the run rolled back so `knit land resume` refuses to continue it. Setting `onFailure: "rollback"` in the land plan (or in the project landing template, which `knit land plan` copies into generated plans) makes `knit land apply` perform this rollback automatically when a step fails; the default `onFailure: "resume"` keeps today's stop-and-resume behavior. A fully successful `knit land apply` appends a `feature.landed` node, archives the bundle with a `feature.archived` node, removes generated worktrees under `.knit/worktrees/<bundle>/`, and preserves local feature branches plus the bundle artifact; pass `--keep-worktrees` to archive without removing those checkouts. It then syncs the updated bundle artifact to configured KnitHub remotes when push-sync is enabled. Use repeated `--remote <name>` to force remotes, `--no-remote` to skip this sync, or `knit sync push --bundles` to push the landed artifact later.

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
knit bundle "x y compat" --repo backend --repo frontend
knit merge feature-x --into x-y-compat
knit merge feature-y --into x-y-compat --manual
knit merge x-y-compat --into staging
knit merge x-y-compat --into feature-y
```

`knit sync` records commits that happened outside Knit as `git.observed` nodes and advances each affected repo's remembered `headSha`. `knit log` shows both Knit commit groups and observed git movement from the node ledger. Use `knit log -2` for the latest two log entries. `knit log -n 3` also works, and `knit log -n` defaults to the latest ten.

Knit also keeps a project-wide history ledger under `.knit/history/<project>.history.jsonl` and syncs it with KnitHub when history APIs are available. This ledger is metadata only: it records bundle ids, repo ids, branch names, Knit node ids, timestamps, and Git commit SHAs. Git remains the source of truth for file contents, diffs, and file-level history.

Use `knit history list` to inspect the local project history and `knit history refresh` to rebuild it from local bundle artifacts. Exchange history events with a KnitHub remote through the shared sync verbs: `knit sync push --history` and `knit sync pull --history`.

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

Knit has no `reset` of its own: the bundle ledger is append-only, so undo goes through `knit revert`. To discard uncommitted changes in checkouts, run git directly through the passthrough, e.g. `knit git --all reset --hard` or `knit git --all clean -fd`.

Knit colors interactive terminal output for scanability. It disables color automatically when output is piped, when `NO_COLOR` is set, or when `TERM=dumb`. Use `KNIT_COLOR=always` or `KNIT_COLOR=never` to force a mode.

If a tracked branch is reset backward, `knit status` reports rewound commits and `knit sync` records a `git.observed` node with `movement: "rewound"` and `droppedCommits`. Existing `commit.group` nodes remain as history; current state is derived from each repo's latest `headSha`.

`knit commit` commits only repos with staged changes in their tracked checkouts. With `-a`/`--all`, it stages first and then commits. `knit commit` also syncs unrecorded git commits before creating a new logical commit group, so the ledger remains ordered.

The git commits are created sequentially, one repo at a time. Knit records them as one logical commit group in the bundle. Every repo commit gets the same logical message plus trailers:

```txt
Knit-Group: <commit-group-id>
Knit-Bundle: <bundle-id>
```

The bundle records the full mapping from logical commit group to repo commit SHAs.

Set `knit config set stealth true` to keep Knit-created git commit messages to the logical message only. Stealth mode suppresses the `Knit-*` trailers in git commits and local revert commits; the bundle ledger still records the commit group, bundle id, revert target, author, and repo SHA mapping.

`knit bundle remove <repo-id>...` removes repos from the bundle and appends a `repo.removed` node, tearing down their worktrees by default (`--keep-worktree` to only untrack, `--delete-branch` to also drop the feature branch, `--force` to discard dirty/unpushed work).

## Bundle Nodes

The bundle is a feature ledger. It stores current state in `repos` and `commitGroups`, and an ordered node chain in `nodes`.

Typical node types:

- `feature.created`
- `feature.archived`
- `repo.added`
- `worktree.materialized`
- `commit.group`
- `git.observed`
- `revert.group`
- `feature.landed`
- `pr.revert`
- `land.update`
- `check.recorded`
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
- `knit land` resolves the host adapter per repo from its remote. A merge lands into the recorded base branch. Remote merges cannot be automatically unmerged by Knit, so failed land runs are recorded in `.knit/land-runs/`; fix the failed step and use `knit land resume`, or use `knit land rollback` to open revert PRs for the steps that already merged.
- `knit land plan` never executes local commands. `run` steps execute only during `apply` or `resume`.
- `knit clean --worktrees` removes generated worktree directories only. It leaves source repos and feature branches in place. `knit bundle delete --worktrees --branches --force-branches` is the explicit local discard path for a bundle's generated worktrees and local feature branches.
- `knit commit` only looks for staged changes inside tracked checkouts.
- `knit revert --apply` preflights all affected repos before writing, but cross-repo revert commits are still created sequentially. If a conflict or commit failure happens after an earlier repo succeeds, inspect the affected repos manually before retrying.
- `knit revert` cannot restore historical `repo.removed` nodes yet because older bundle nodes did not store the full removed repo record.
- JSON Schema files are bundled for workspace artifacts; `knit doctor` uses serde-backed validation and structural checks.
- Knit does not run LLMs, MCP servers, or review agents.

## Manual Test With Toy Repos

See [manual-test.md](manual-test.md) for a small two-repo smoke test.

See [change-group-schema.md](change-group-schema.md) for the current bundle fields.

## Code Layout

See [architecture.md](architecture.md) for the module boundaries and test layout. `src/main.rs` is intentionally only the binary entry point; command logic lives in `src/commands/`, schema types in `src/model.rs`, persistence in `src/store.rs`, and git subprocess helpers in `src/git.rs`.

## Roadmap

- Standalone JSON Schema for `ChangeGroup`
- Safer partial-failure recovery for multi-repo commits
- More host adapters (e.g. Bitbucket) and richer GitLab/Forgejo check integration
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
