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
knit clone https://<host>/<owner>/<project> [target] [--token <token>] [--active-bundle <bundle>] [--no-worktree] [--json]
knit clone <owner>/<project> [target] [--remote <name>] [--url <url>] [--token <token>] [--active-bundle <bundle>] [--no-worktree] [--json]
knit init <name> [--agents]
knit agents [project]                         # refresh workspace + project AGENTS.md sections
knit project add <repo-id> <repo-path> [--base <branch>] [--observe] [--agents]
knit project set-base <repo-id> <branch> [--project <name>]
knit project push [name] [--remote <name>] [--prune]
knit project pull [name] --repo <repo-id> [--agents]
knit project agents [name]
knit project command set <name> [--repo <repo>]... [--cwd <path>] [--env KEY=VALUE]... -- <command> [args...]
knit project command list
knit project command remove <name>
knit project list
knit project show [name]
knit project remove <name> [--repo <repo-id>]... [--force]
knit remote add <name> <url> [--token <token>|--token-stdin] [--global]
knit remote list [--global]
knit remote show <name> [--global]
knit remote remove <name> [--global]
knit remote projects [--remote <name>] [--json]
knit remote auth-status <name> [--json]
knit remote sync-helpers <name>
knit remote token <name> [token] [--clear] [--global]
knit git-credential --remote <name> get|store|erase
knit view list [--project <name>]
knit view show [name] [--project <name>] [--repos]
knit view save <name> [--base default|none] [--from <view>] [--include <repo>]... [--exclude <repo>]... [--from-bundle] [--project <name>]
knit view freeze <name> [--project <name>]
knit view include <name> <repo>... [--project <name>]
knit view exclude <name> <repo>... [--project <name>]
knit view unset <name> <repo>... [--project <name>]
knit view default [name] [--clear] [--project <name>]
knit view rm <name> [--project <name>]
knit view edit [--project <name>]
knit bundle                          # show the resolved bundle
knit bundle "<title>"                # create a bundle (git-branch-style shorthand)
knit bundle "<title>" [--project <name>] [--repo <repo-id>]... [--all-repos] [--view <name>] [--include <repo>]... [--exclude <repo>]... [--offline|--from-local-base] [--no-worktree] [--in-place] [--force] [--agents] [--cd [<repo>]]
knit bundle add <repo-path-or-project-repo-id>... [--base <branch>] [--offline|--from-local-base] [--in-place] [--no-worktree]
knit bundle remove <repo-id>... [--keep-worktree|--delete-branch] [--force]
knit bundle worktree
knit bundle pull <slug> [--json]
knit bundle apply-view <name> [--keep-worktree|--delete-branch] [--force]
knit bundle list [--all] [--archived] [--deleted]
knit bundle archive <bundle> [--reason <reason>] [--keep-worktrees] [--force]
knit bundle restore <bundle>
knit bundle delete <bundle> --force [--worktrees] [--branches] [--force-branches] [--remote-branches]
knit bundle prune [--no-refresh] [--report] [--untracked] [--apply] [--delete] [--all] [--worktrees] [--force] [--branches] [--force-branches] [--remote-branches] [--remote-bundles] [--archived]
knit bundle path
knit bundle print
knit bundle validate
knit switch <bundle> --workspace
knit add [-r <repo>] [-N] [-u] [repo-or-pathspec...]
knit clean [--plans] [--worktrees] [--archived] [--merge-worktrees] [--all] [--force]
knit status
knit workspace status
knit diff [--stat] [repo-id-or-path...]
knit fetch [--mode all|git|knit] [--remote <name>] [repo-id-or-path...]
knit pull [--base] [--current] [--bundles] [--all] [--rebase] [--force] [--feature] [--remote <name>] [--no-remote] [--merge] [repo-id-or-path...]
knit push [--all] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit run <project-command> [--repo <repo>]... [--all]
knit run [--repo <repo>] [--all] -- <command> [args...]
knit run up|status                             # bundle runtime stack
knit run down [--purge]
knit run eject [--force]
knit run --list
knit check run <project-command> [--repo <repo>]... [--all]
knit check record <name> --pass|--fail [--detail <text>]
knit check status
knit publish create [--from-artifact <path>] [--out <path>] [--no-push] [--provider <id>|--github] [--base <branch>|--base <repo=branch>] [--draft] [--renew] [--sync|--no-sync] [--set-upstream] [--remote <name>]... [--no-remote] [repo-id-or-path...]
knit publish sync [--from-artifact <path>] [--out <path>] [--provider <id>|--github] [repo-id-or-path...]
knit publish status [--live] [--provider <id>|--github] [repo-id-or-path...]
knit request ...                               # alias for `knit publish`
knit land
knit land --lane <name>
knit land --target <branch>
knit land plan [--provider github|gitlab|forgejo|bitbucket] [--out <path>] [--force]
knit land check
knit land update [--push] [--continue-merge] [repo-id-or-path...]
knit land apply [--plan <path>] [--from-artifact <path>] [--out <path>] [--skip-checks] [--keep-worktrees] [--remote <remote>]... [--no-remote] [--tag [<name>]] [--no-tag]
knit land resume [--run <path>] [--remote <remote>]... [--no-remote]
knit land rollback [--run <path>] [--apply]
knit land status [--run <path>]
knit merge <source-bundle-or-ref> --into <target-branch-or-bundle> [--fetch] [--push] [--set-upstream] [--manual]
knit merge status [--run <id-or-path>]
knit merge show [--run <id-or-path>]
knit merge push [--run <id-or-path>] [--repo <repo-id>]... [--set-upstream]
knit merge --continue
knit merge --abort
knit tag <name> [-r <repo>]... [--no-push|--no-git]
knit tag [list]
knit tag show <name>
knit cherrypick --from <bundle> [-r <repo>]... [--dry-run] <selector>...
knit config set advice true|false
knit config set stealth true|false
knit config set auto-tag true|false
knit config set push-sync true|false
knit config set sync-remote <name>
knit config set sync-remotes <name>[,<name>...]
knit schema print <bundle|project|merge-run|land-plan|land-run|config>
knit doctor
knit migrate [--check]
knit sync                                      # record git commits made outside Knit (local reconcile)
knit sync push [--bundles] [--history] [--views] [--architecture] [--kg] [--all] [--remote <name>]...
knit sync pull [--bundles] [--history] [--views] [--architecture] [--kg] [--all] [--remote <name>]...
knit history [list] [-n <count>] [--repo <repo>] [--bundle <bundle>] [--kind <kind>]... [--project <project>]
knit history refresh [--rebuild] [--project <project>]
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
knit run down      # stop the stack; preserve restart data
knit run down --purge  # also remove bundle-owned volumes and local images
```

`knit run up` lifts every bundle repo with a compose file — the runtime is "docker compose up in each repo, with the bundle's code" — with zero configuration. `runtime.stacks` narrows the set to explicit repo ids, and the legacy `runtime.stackRepo` forces a single stack. Each stack's compose file is `runtime.composeFile` when set (applying to the configured stack repo), else `docker-compose.knit.yml` when present, else the repo's own `docker-compose.yml`/`compose.yaml`. A single stack runs as compose project `knit-run-<bundle>`; several stacks run as `knit-run-<bundle>--<repo>` each, so networks and named volumes stay isolated per stack. References from one stack's environment to a sibling stack's published host port are rewritten to the sibling's bundle port, so stacks find each other's bundle instances; ports of repos outside the bundle are left alone and keep pointing at the dev instances. A repeated `up` reuses that bundle's recorded service and bundle-database ports, including while its compose projects already own the listeners; it reallocates only when the recorded shape no longer matches or a stopped runtime's old port has been claimed. Plain `down` preserves named volumes and local images for a fast restart, but removes anonymous volumes because Compose cannot reattach them on the next `up`; `down --purge` additionally removes named volumes and locally tagged images owned by the bundle's Compose projects. External volumes and explicitly tagged images remain outside Knit's lifecycle. Landing, archiving, deleting with worktree cleanup, or explicitly cleaning a bundle's worktrees automatically performs the same scoped purge. `down`/`status` resolve containers by project label, so they keep working even after the worktree is gone. Run state lands in `.knit/runtime-runs/<bundle>/state.json`, recorded only after every stack starts; if `up` fails partway, `knit run down` still cleans up by derived project names. A project command configured with one of these runtime names takes precedence over the built-in verb.

**Transform mode (default).** A plain compose file — the one developers already use on `main` — is lifted automatically. Knit resolves it with `docker compose config` against the source repo location, then rewrites the resolved shape:

- every path that resolves inside a tracked repo's source checkout (build contexts, additional contexts, dockerfiles, build args, bind-mount sources) is remapped to that repo's bundle worktree — "main everywhere, except the repos this bundle changes"
- every published host port is allocated per bundle (stepping by `ports.step` from the original and reusing recorded allocations on later runs), container-side ports untouched
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
| `KNIT_PORT_<SERVICE>` | one stable bundle host port per pool in `ports.services` (service name -> base port), stepping all pools together by `ports.step`; with no `services` map, a backend/frontend pair from `ports.backendBase`/`frontendBase` |
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

`knit check run <name>` executes the configured project command of that name (the same definition `knit run <name>` uses — define it with `knit project command set ci -- cargo test`) and records a `check.recorded` node: pass if every targeted repo exited 0, fail otherwise. A failing run is still recorded before the command errors, so the red verdict is on the ledger. `knit check record` is the door for verdicts computed elsewhere — another tool, a host CI run, a human — without making that tool a second source of truth: the record always lives in the bundle artifact and syncs to the remote with it.

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

### Tags

A **tag** is a cross-repo known-good marker: the state of the configured project bases, recorded as one named set. Per repo it pins the commit `origin/<base_branch>` points at after a fresh fetch — the answer a monorepo gets for free from a single SHA. The `tag.created` ledger node is the source of truth; annotated git tags `knit/<name>` in each repo are a default-on export of it, so hosts, CI, deploy scripts, and humans can consume the marker with zero Knit knowledge, and the pinned commits stay protected from garbage collection:

```sh
knit tag v1-launch --bundle checkout-flow   # tag the configured project bases
knit tag                                    # list knit/* tags across repos, marking partial sets
knit tag show v1-launch                     # per-repo local/remote SHAs, subject, ledger provenance
```

The intended workflow is land → verify → tag: `knit land apply` merges and deploys, you verify the configured project bases by whatever you trust (the deploy, CI, QA), and then `knit tag` records that combination as good. Not every land gets tagged — tagging is a deliberate act, which is why it stays a manual verb. Landing into a terminal destination archives the bundle and clears the workspace pointer, so tag it explicitly with `--bundle <slug>` (bundle resolution accepts archived bundles). A review landed into an alternate publication base does not change tag semantics: `knit tag` still pins the project's configured bases.

**Tagging on landing.** When you want the tag every time, let landing do it: `knit land apply --tag [name]` records the tag as part of a successful land (an omitted name defaults to the bundle slug), and `knit config set auto-tag true` makes that the default for every land (`--no-tag` opts out of the default for one run). Landing has already merged and archived by the time the tag runs, so a tag failure is a warning with a retry hint, never a failed land. A landing into an intermediate destination has not reached the configured bases at all, so it refuses `--tag` and skips the `auto-tag` default. Tagging on land only works with local checkouts, not `knit land apply --from-artifact`.

**Honesty model.** The tag records exactly what Knit can prove, never more. The annotation and node message carry the bundle id, the land run when one exists, recorded check verdicts explicitly labeled as computed on the feature branches (not the tagged commits), and best-effort **configured-base CI** verdicts — provider check runs and commit statuses queried for each pinned SHA itself. Red or pending evidence, and a landed feature head that is not an ancestor of the tagged commit (normal after squash or rebase merges), are printed as warnings and recorded, never errors: the human decides whether the state deserves the tag. Per-repo green CI still does not prove the cross-repo *combination* works — that claim is yours, and the tag records that you made it, when, and on what evidence.

The read verbs (`knit tag`, `knit tag list`, `knit tag show`) are project-wide: they scan the active project's full repo set regardless of which bundle context they run from, since tags are facts about the whole project, not one bundle. Deliberate targeting with `--bundle`/`KNIT_BUNDLE` scopes them to that bundle's repos, and ad-hoc workspaces without a project use the resolved bundle.

**Immutability and resume.** A tag name can never be reused or moved: creation refuses when `knit/<name>` exists locally or on origin in any targeted repo, and there is no `--force`. Re-running `knit tag <name>` on the bundle that recorded it resumes instead — missing local tags are recreated at the ledger-pinned SHAs, existing ones are verified against the pins (a mismatch is an error naming the repo), and only repos whose origin lacks the tag are pushed. A partially pushed set therefore converges by re-running the same command. `--no-push` stops after local tags and the ledger node; for a repo without `origin`, this mode pins its local configured base and labels that source explicitly in the annotation. `--no-git` records the ledger node only; `-r/--repo` tags a subset (with a notice, since a partial set weakens the claim).

### Views

A project's repo list is shared by everyone, with `--observe` marking repos kept out of default bundle starts. A **view** is per-user config layered on top of that shared project: a named "bundle shape" expressed as include/exclude deltas over the project's default repo set. Views are stored per user at `.knit/views/<project-id>.views.json` and never touch the shared project artifact, so a junior member can work against two repos while a staff member keeps several shapes for the same project.

```sh
knit view save backend --exclude frontend,docs
knit view save frontend --include design-system --exclude backend
knit view default backend            # bare `knit bundle` now uses this shape
knit view list                       # `*` marks the default
knit view show frontend --repos      # print the repos this view resolves to
```

A view's `base` chooses its seed set. The default, `base: default`, seeds from the project's `includeByDefault` repos, so the view tracks later changes to the default set — right for team shapes layered on a small, stable default set. `--base none` seeds empty: the include list **is** the complete shape, and the view never absorbs default-set changes — right for pinned selections in large projects where most repos are observed. `--from <view>` seeds a new view from an existing one before the flags apply, and `knit view freeze <name>` converts a delta view in place into the absolute repo list it currently resolves to:

```sh
knit view save platform --include api,worker,queue        # delta: default set + three repos
knit view save legacy --base none --include api,worker    # absolute: exactly these repos
knit view save platform-plus --from platform --include docs   # seeded copy, then deltas
knit view freeze legacy                                   # pin a delta view as absolute
```

`--base none` rejects `--exclude` (there is no seed set to remove from), and `knit view exclude` refuses absolute views — use `knit view unset` to drop repos from them.

`knit bundle "title"` applies the default view (if set); `--view <name>` selects another. `--repo`/`--all-repos` ignore views and select an explicit set. Ad-hoc `--include <repo>` / `--exclude <repo>` adjust the resolved set in any mode, so `knit bundle "x" --view backend --include docs` and `knit bundle "y" --all-repos --exclude sej` both work.

A live bundle can be reshaped at any time, with the worktree consequences:

```sh
knit bundle add docs                 # materialize the repo's branch + worktree
knit bundle remove frontend          # tear down its worktree
knit bundle remove frontend --delete-branch    # also delete the local feature branch
knit bundle apply-view backend       # reshape the bundle to match a saved view
```

`knit bundle remove` refuses to discard uncommitted or unpushed work unless `--force`; pass `--keep-worktree` to remove only the tracking entry and leave the worktree on disk. Views sync to the remote as the user's own config with `knit sync push --views` / `knit sync pull --views`, are uploaded alongside `knit project push`, and are restored by `knit clone`.

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
        "timeoutSeconds": 1800,
        "command": ["fly", "deploy"]
      },
      {
        "id": "deploy-frontend",
        "repoId": "frontend",
        "mode": "push"
      }
    ],
    "targets": {
      "staging": {
        "deployments": [
          {
            "id": "deploy-staging",
            "repoId": "backend",
            "mode": "push"
          }
        ]
      },
      "preproduction": {
        "deployments": [
          {
            "id": "deploy-preproduction",
            "repoId": "backend",
            "checkout": { "branch": "preproduction", "remote": "origin", "update": "pull" },
            "command": ["sh", "deploy-preproduction.sh"]
          }
        ]
      }
    },
    "lanes": {
      "staging": {
        "defaultBranch": "staging"
      },
      "production": {
        "terminal": true,
        "branches": {
          "backend": "stable",
          "frontend": "master",
          "scripts": "master"
        },
        "deployments": [
          {
            "id": "deploy-production-backend",
            "repoId": "backend",
            "command": ["bj", "deploy", "production"]
          }
        ]
      }
    }
  }
}
```

Deployment entries are first-class landing steps. `landing.lanes` declares named, project-level destinations whose `branches` map may differ by repository; `defaultBranch` (or `branches["*"]`) supplies a fallback. `knit land --lane production` resolves and stores the complete per-repo map in `targetBranches`, retargets each review independently, and includes only that lane's deployments. Lane deployments receive `KNIT_LAND_LANE`; repo-scoped deployments also receive their resolved `KNIT_LAND_TARGET_BRANCH`.

A destination is either terminal or intermediate, and that is what decides the bundle's fate. Landing into a terminal destination is the bundle's last stop: it archives the bundle and removes its generated worktrees. Landing into an intermediate destination — a staging lane, a preview branch — merges, deploys, and records the landing, but leaves the bundle open with its worktrees intact, because the work has not reached its final home yet. Knit's default answer is the configured base branches: a lane or target is terminal when it maps every repository onto that repository's configured `baseBranch`, and intermediate otherwise. Declare `"terminal": true` on `landing.lanes.<name>` or `landing.targets.<branch>` when a destination finishes the bundle without being the configured base — a release branch, say — and `"terminal": false` to keep a bundle open even after it reaches the configured bases. The generated plan records the resolved answer in its `terminal` field, `knit land` prints it before you apply, and the plan file remains editable if one bundle needs a different answer. Because `knit tag` pins the configured project bases, `knit land apply --tag` is refused for an intermediate destination and automatic tagging is skipped there.

A lane is an environment, and a bundle passes through several of them: staging, preproduction, then production. An intermediate lane landing merges each repository's feature branch into that lane's branch and pushes it, then runs the lane's deployments. It does not touch the bundle's review objects: they stay open against the destination that ends the bundle's life, so the same bundle can land into the next environment afterwards, and the shared staging branch is never promoted wholesale into production. Those merge steps appear in the plan as `merge_branch` with their own `targetBranch`, they use a managed `.knit/merge-worktrees/<branch>/<repo>/` checkout that is detached (the merge is pushed as `HEAD:refs/heads/<branch>`, so your source checkout is never touched, whatever branch it sits on), and a destination branch that exists neither locally nor on `origin` is an error naming the repository. An intermediate lane must also send each repository somewhere other than that repository's own review base: merging the feature branch into the branch the review is already pointed at puts the review's commits into its base, and the forge closes it as merged. Knit refuses that landing by name when it plans it and again when it applies it, because it cannot both merge there and leave the review open. A conflict against the environment branch stops the run with the merge undone; merge the destination into the feature branch and land again. `knit land rollback` compensates by opening revert PRs, which only exists for review merges, so it reports branch merges it cannot undo instead of claiming there was nothing to roll back.

Not every repository has every environment. A library, a command-line tool or a bag of one-off scripts is released rather than deployed, so it has no staging branch and never will. Give such a repository a `null` branch in the lane — `"branches": { "api-client": null }` — and the lane skips it: no merge step, no error, and no invented branch to maintain. The repository keeps its work for the terminal landing, where its review merges into its own base as usual. A `null` entry beats `branches["*"]` and `defaultBranch`, so a lane can send everything to one branch and name its exceptions; a `null` under `"*"` inverts that into an allow-list, and combining a `null` `"*"` with a `defaultBranch` is refused because they contradict each other. Absence is always written per repository: a missing entry still errors, because a missing entry is also what a typo looks like. The repositories a lane skips are listed in the plan's `laneAbsent` and printed under `Not in this lane:` by `knit land` and `knit land status`, so a repository skipped on purpose never reads like one dropped by a bug. A lane that declares a repository absent while also deploying it is refused. If every repository a bundle changed is absent from the lane, the landing is refused too: there is no environment for that work to reach. A lane that skips anything is never terminal — declaring `"terminal": true` alongside a `null` branch is refused, and an otherwise terminal-looking lane becomes intermediate — because a bundle's last stop has to carry every repository or the skipped ones keep an open review after the bundle is archived. The hosted path enforces all of this too: Svartal resolves the lane and passes `--repo-absent` and an explicit `--terminal`/`--intermediate` to artifact landing.

A terminal landing merges the recorded review objects, exactly as a plain `knit land` does, and then archives the bundle. `knit land --target <branch>` follows the same rule as a lane: it is an ad-hoc lane that sends every changed repository to the one branch. When that branch is intermediate, each repository's feature branch is merged into it and pushed while the reviews stay open; when it is terminal — every repository's configured base, or declared with `landing.targets.<branch>.terminal: true` — the recorded reviews are retargeted to it, merged there, and the bundle is archived. Once a landing run finishes, asking for a different destination — `knit land --lane <next>`, or a bare `knit land` for the recorded review bases — plans that destination instead of reporting the finished run; a failed or rolled-back run still has to be resumed or rolled back first. A bare request never applies a lane or target plan: pass the same `--lane`/`--target` to apply it, or regenerate with `knit land plan --force`. If pushing a lane branch fails, the local merge is undone so `knit land resume` fetches the branch's new tip, merges again and pushes.

A deployment runs when something it deploys changed. A bundle carries a project's whole default repository set, but a given landing usually touches a few of those repositories, and redeploying the rest is at best wasted time and at worst an unannounced restart of an application nobody changed. So a deployment watches a set of repositories and is planned only when the bundle recorded work in one of them — the same "did this bundle change it" answer the merge steps already use, so deploy scope and merge scope cannot drift apart.

By default a deployment watches the repository it deploys. `whenChanged` widens that, because a deployment does not always depend only on its own repository: an image that builds another repository's binary into itself has to redeploy when *that* repository changes, or it quietly ships a stale one. Write `"whenChanged": ["api", "api-client", "shared-tools"]` on the deployment and it runs when any of them changed. A literal `"*"` runs it on every landing, as in `landing.lanes.<name>.branches`. `whenChanged` is held to a shape that cannot silently mean nothing: the list must be non-empty and free of repeats, every id must name a repository of this project, and `"*"` must stand alone rather than sit beside named repositories. Ids are checked even when `"*"` is present, so a typo cannot ride along unvalidated. All of this is refused rather than ignored, because watching a repository that does not exist means never deploying, and a typo is exactly what that looks like. A push deployment reports only that its own repository's merge triggered it, so watching another repository is refused there outright — use `mode: "command"` for a deployment something else triggers. The deployments a plan leaves out are recorded in its `deploymentsSkipped`, with what each one watches, and printed under `Deployments not run:` — for the same reason lane absences are printed, so a step missing on purpose never reads like one lost to a bug. Skipping resolves against `needs`: a deployment nothing triggers is still planned when a deployment that *is* running depends on it, transitively, because `B needs A` means A has to run.

A terminal landing has to carry everything the bundle changed. It archives the bundle and removes its worktrees, so a changed repository left out of it is work stranded on a branch nobody will land — while the forge says the feature shipped. Knit refuses to plan one that does not cover every changed repository, naming them and which fix applies: a missing review means `knit publish create`, and a repository the project's `merge.repoOrder` excludes under `includeUnlisted: false` means adding it to the order. Intermediate destinations are deliberately allowed to carry a subset; that is what `laneAbsent` records. The hosted path holds the same line: `knit land apply --from-artifact` refuses a terminal landing, declared or inferred, when a changed repository has no recorded review, naming it, exactly as the local plan does.

A plan describes the bundle as it was when the plan was generated, so it records `changedRepos` and each repository's `bundleHeads`. Committing more work makes that plan a description of the past: the new repository would never merge, and deployments would be scoped to a change set that no longer exists. Both `knit land apply` and `knit land resume` refuse a plan whose pin no longer matches and tell you to regenerate it with `knit land plan --force`. `knit land update` moves feature heads on purpose, so it re-pins the plan it just prepared rather than invalidating it — update-then-land keeps working. Plans written before pinning existed carry no pin and are accepted unpinned. The same pins decide what a finished run means: asking for the destination that last succeeded — `knit land --lane staging` again after committing more work — plans the new work instead of reporting the old run, because a run whose plan no longer describes the bundle is history rather than an answer.

`knit land resume` finishes a landing exactly as `knit land apply` does. A resumed terminal run records the landed node, archives the bundle, removes generated worktrees, clears the workspace's active bundle and honours `--tag`/`auto-tag`; it takes the same `--keep-worktrees`, `--tag` and `--no-tag` flags for that reason. A run records the steps of the plan it started from, so `knit land resume` refuses a run whose plan was regenerated in between, naming the steps that were added or removed, and points at a fresh `knit land apply` instead.

`landing.targets` remains the branch-keyed deployment mechanism for raw/common targets. `knit land --target staging` selects `landing.targets.staging.deployments` for the whole landing; without `--target` or `--lane`, recorded per-repo review bases select matching targets and mixed bases can select more than one. A target can declare multiple repo deployments; repo-scoped entries are included only for reviews landing into that target. Target deployment ids must be unique across targets that can be selected together.

The top-level `landing.deployments` list remains the backward-compatible configured-base lane. It runs for deploy-only plans and when every review targets its repo's configured project base and no matching branch-keyed target overrides it. An undeclared alternate branch receives no automatic deployment steps; the generated plan calls that out so the operator can declare `landing.targets.<branch>` or add an explicit step to that bundle's editable plan. Command deployments run without a shell unless the command explicitly invokes one, stream their output live while retaining only a bounded tail in the run artifact, and time out after `timeoutSeconds` (30 minutes by default). A deployment checkout uses a managed `.knit/land-worktrees/<bundle>/<repo>/<branch>/` checkout so the feature worktree is not switched away from its Knit branch. `update: "pull"` and `update: "fetch"` both refresh the managed checkout from the configured remote branch before running the command.

Artifact-mode branch merging is implemented for GitHub only today; GitLab, Forgejo and Bitbucket refuse it by name and point at landing the lane from a workspace, which merges in a local checkout instead. Everything else about artifact landing is provider-neutral. Compatible remotes can use either the raw `knit land --target <branch> apply --from-artifact ...` contract or a named lane. For artifact landing, the trusted host resolves the lane from its project metadata and supplies one `--repo-target <repo>=<branch>` per publication alongside `--lane`; Knit rejects incomplete maps. The host also owns the lane's lifecycle answer: `--terminal` or `--intermediate` states whether the landing finishes the bundle, and without either flag Knit falls back to the same configured-base rule using the artifact's recorded bases. The Knit artifact command itself remains command-deployment-free because it has no trusted project checkout. A host such as Svartal may preflight and execute the selected lane's commands after the merge when its landing machine has explicitly configured trusted repository roots.

Default project repos are included by `knit bundle`; observed repos are available by id but are not branched or tracked until added explicitly. Before recording any selected repo, Knit fetches its configured `origin/<baseBranch>`, snapshots the exact fetched commit in `baseSha`, and creates the feature branch from that commit. Source checkouts are not switched or moved, and dirty source files do not affect the snapshot. Base fetches are an all-or-nothing preflight: if any selected remote base cannot be fetched, no repos or feature branches are recorded.

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

`knit bundle add` accepts one or more repo paths or project repo ids. It resolves and fetches all inputs before writing the bundle, then stores each absolute git repo path, repo id, origin remote when available, inferred base branch, exact base SHA, and checkout mode. By default it creates the `knit/<bundle-id>` branch and a generated worktree for each tracked repo. Use `--no-worktree` for metadata-only registration. It refuses repos already tracked in the bundle so an add cannot silently rewrite a live baseline.

Generated worktrees get local `AGENTS.md` guidance by default: one bundle-wide guide at `.knit/worktrees/<bundle>/AGENTS.md`, the parent directory of every repo checkout. Knit never writes `AGENTS.md` inside a repo checkout — a repo that tracks its own `AGENTS.md` would commit the bundle-specific section and conflict on every publish. The bundle guide assumes the agent opened the generated worktree folder directly, so its examples rely on cwd and do not include `--bundle`.

Use `knit agents [project]` to refresh both generated sections in the workspace root `AGENTS.md`: the workspace tutorial plus project-specific guidance from the selected or active project JSON. This direct refresh avoids creating or overwriting a bundle merely to update documentation. `knit bundle "<title>" --agents`, `knit project agents [name]`, and `knit init <name> --agents` remain narrower workflow-integrated doors. If `AGENTS.md` already exists, Knit preserves the rest of the file and appends or refreshes only its managed sections.

Use `knit bundle "<title>" --in-place` or `knit bundle add <repo> --in-place` to make Knit operate directly in the original repo checkout instead of creating `.knit/worktrees/<bundle>/<repo>`. Knit will create or check out the `knit/<bundle-id>` branch in that repo. The original checkout must be clean before Knit switches branches. Later mutating commands refuse to operate if the in-place repo is no longer on the expected feature branch.

Base inference first uses cached `origin/HEAD`, then best-effort live remote default metadata. Without either, a clean current branch that tracks its same-named origin branch is preferred; Knit uses `main` or `master` only when the choice is unambiguous, and otherwise requires `--base`. This supports repositories whose real base is named `stable` without relying on a host-specific CLI.

Use `knit project set-base <repo-id> <branch>` to change only a project repo's configured base. Knit validates the branch from a fresh origin ref when available, then cached origin or local refs; it preserves the repo's path, checkout mode, and default/observed status. Existing bundles remain pinned to their recorded `baseBranch` and `baseSha` because rewriting those values would change their diff and review target. The command lists affected open bundles and the safe remove-with-`--delete-branch`/add workflow for untouched feature checkouts.

Fresh remote bases are the default. `--offline` skips network access and prefers a cached `origin/<base>` before falling back to the local base; `--from-local-base` deliberately snapshots the local base branch.

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

`knit bundle delete <bundle> --force` moves the bundle JSON artifact to `.knit/deleted/bundles/` and clears the active bundle if needed. By default it preserves git state. With push-sync remotes configured, it also archives the bundle's record on each sync remote so hosted dashboards stop counting the deleted work as open; that mirror is best-effort (offline deletes warn and continue) and a later `knit bundle prune --apply --remote-bundles` catches anything it missed. Add `--worktrees` to remove Knit-generated worktrees for that bundle before moving the artifact. Add `--branches` to delete the local `knit/<bundle>` feature branches after those generated worktrees are removed:

```sh
knit bundle delete documentation-quick-wins --force
knit bundle delete documentation-quick-wins --force --worktrees
knit bundle delete documentation-quick-wins --force --worktrees --branches
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches --remote-branches
```

`--branches` uses `git branch -d`, so it refuses to delete branches with unmerged commits. `--force-branches` uses `git branch -D`. Knit only deletes local feature branches recorded by the bundle unless `--remote-branches` is also passed, which deletes the matching recorded feature branches from `origin` and removes local `origin/<branch>` tracking refs when present.

`knit bundle prune` scans workspace bundles and lists dead-work candidates: clean bundles with no recorded open PRs. Existing PR records are refreshed from GitHub before deciding, missing PR records are allowed, and dirty generated checkouts keep the bundle alive. Add `--no-refresh` for a cached/offline scan. Every scan also reports orphaned `.knit/worktrees/<bundle>` directories that no longer have bundle artifacts; add `--worktrees` to make clean orphan directories removable on `--apply`. Pass `--force` (included in `--all`) to discard uncommitted work and remove dirty orphan worktree dirs too. `--all` is a cleanup preset for generated worktrees, local feature branches, forced local branch deletion, matching `origin` branches, and matching remote bundle records:

```sh
knit bundle prune
knit bundle prune --no-refresh
knit bundle prune --apply
knit bundle prune --apply --all
knit bundle prune --apply --delete --worktrees --branches
```

On `--apply`, dead bundles are **archived, not deleted**: finished work — a bundle whose PRs all merged, even outside `knit land` — becomes history, exactly like `knit bundle archive`. The archive node records why the bundle was dead (for example "recorded PRs are merged"), generated worktrees are removed, local feature branches are preserved, and with push-sync remotes configured the terminal state is pushed so hosted dashboards flip together with the local ledger. This is what keeps the remote from accumulating bundles that read "open" after their work merged. The explicit `--branches`/`--remote-branches` cleanups still apply in either mode. Pass `--delete` to discard the artifacts to `.knit/deleted/bundles/` instead of archiving them.

Landed and archived bundles are finished work, not dead work: their artifacts are the audit record of what shipped, so prune keeps them (and says how many it kept) unless `--archived` opts them into the scan. `--archived` requires `--delete` because pruning finished bundles discards history artifacts. `--all` deliberately implies neither.

A bundle whose only uncommitted work is untracked files is otherwise dead work, so prune does not touch it by default; instead it lists it under "Blocked by untracked files". Pass `--untracked` to treat those bundles as dead-work candidates — the untracked files are discarded with the generated checkout on `--apply`. Bundles with tracked, uncommitted changes are still preserved even with `--untracked`.

`--report` prints every scanned bundle and why it is prunable or kept (open PRs, merged PRs, tracked changes, or untracked-only files), not just the deletable candidates:

```sh
knit bundle prune --report
knit bundle prune --untracked
knit bundle prune --apply --untracked --worktrees
```

Remote bundle cleanup archives matching remote bundle records — it never deletes them, because a record whose local artifact is gone is often the last remaining trace of shipped work. Archiving rides the everyday `bundle:push` scope. True remote deletion stays an explicit decision via `knit bundle prune --apply --delete --remote-bundles`, which requires a `bundle:delete` token.

With `--remote-bundles`, prune also detects **remote orphans**: bundle records that exist on the sync remote but have no local artifact and are dead work — their artifact sits in the local `.knit/deleted/bundles/` quarantine (a locally deleted bundle is dead even with no recorded PRs), or every recorded PR is merged or closed. Without this, a delete that could not reach the remote (offline, or made before delete-time archiving existed) would leave its record behind forever, and no later prune could reach it through the local bundle scan. These are listed under "Remote orphan bundle candidates" and archived on `--apply`; records already archived on the remote are skipped. Live PR state is refreshed from the host by URL during detection (the synced artifact can be stale), falling back to the recorded state when the lookup fails. Prune is also best-effort: an unreadable bundle file, a failed PR lookup, or an unverifiable checkout is reported as a warning and skipped (the bundle is kept to be safe) instead of aborting the whole scan.

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

`--plans` removes `.knit/revert-plans`. `--worktrees` removes generated worktrees for the resolved bundle with `git worktree remove` and clears their recorded `worktreePath`; in-place checkouts are preserved. `--archived --worktrees` sweeps finished bundles, including archived bundles kept with `--keep-worktrees` and landed bundles whose cleanup was interrupted. `--merge-worktrees` removes clean branch-target merge worktrees for succeeded or aborted merge runs. Use `--force` to pass `--force` to `git worktree remove` for dirty generated worktrees.

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
knit fetch --mode all
knit fetch --mode git
knit fetch --mode knit
```

`knit pull` is context-aware. With no target flags:

- **At the workspace base** (the shared workspace fallback, e.g. several open bundles and no specific one resolved) it updates *everything*: every active-project repo's source checkout plus every open bundle, and prints a per-target report instead of refusing.
- **For a specific resolved bundle** (inside a worktree, `--bundle <id>`, `KNIT_BUNDLE`, or a single-bundle workspace) it pulls that bundle's tracked repos and then its remote artifact, as before.

Target flags drive the aggregate, best-effort report directly and may be combined:

- `--base` fetches each configured `origin/<baseBranch>` and safely fast-forwards the matching local base branch without switching the source checkout. A checked-out dirty base, local-only commits, or divergence is reported as a failure and makes the command exit nonzero.
- `--current` updates each active-project repo's *source checkout* on its current branch with `git pull --ff-only`. The old hidden `--main` spelling remains as a compatibility alias.
- `--bundles` updates every open bundle's feature checkouts from its remote artifact.

Aggregate pulls run in parallel — git work on the same source repo is serialized, distinct repos run concurrently — and report every target instead of stopping at the first problem. Current-checkout and bundle targets remain best-effort. Explicit `--base` is a preparation gate, so any failed base update makes the command exit nonzero after printing the full report. The remote artifact side is tolerant: Knit walks the configured sync remotes in order and uses the first one that responds, reporting and skipping each unreachable remote. When no remote answers, the pull says so and the git work still runs — an offline sync remote never blocks updating checkouts. An explicit `--remote <name>` still fails hard, because you named the remote you wanted.

```sh
knit workspace status     # show current checkout, configured base, cached origin divergence, dirt, and open bundles
knit pull                 # at the base: current source checkouts + every open bundle, reported
knit pull --base          # fetch and fast-forward configured stable/base branches
knit pull --current       # update source checkouts' current branches (fast-forward only)
knit pull --bundles       # fast-forward every open bundle's checkouts from the sync remote
knit pull --base --current --bundles
knit pull backend         # single-bundle: pull a specific tracked repo's base checkout
knit pull --rebase frontend
```

Single-bundle pulls still default to the original repo path on the recorded base branch with `git pull --ff-only`, updating the recorded `baseSha`, and refuse on uncommitted changes unless `--force` (use `--rebase` for `git pull --rebase`). Use `knit pull --feature` to pull the tracked Knit feature checkout instead; feature pulls are recorded as `git.observed` nodes when the feature branch head moves.

`knit push` is the branch-only path: it pushes tracked feature branches to `origin` and nothing else — no PRs, no GitHub metadata, no bundle state change. For the review path, go straight from `knit commit` to `knit publish create`, which pushes the branches itself. Selected repo pushes run in parallel, at most `KNIT_GIT_JOBS` (default 8) at a time; bundle artifact and history sync wait until every Git push succeeds. By default it pushes the current feature branch to `origin/<branch>` without setting upstream; use `--set-upstream` when you want git's upstream tracking configured:

```sh
knit push
knit push backend
knit push --all
knit push --set-upstream frontend
```

Each `git push` is bounded: it is killed after `KNIT_GIT_PUSH_TIMEOUT` seconds (default 300), so a stalled connection cannot hold the command open, and a push that lost its connection is retried up to three times. A push the remote answered — rejected, non-fast-forward, stale lease, refused credentials — fails immediately, because repeating it only earns the same answer. Repos that fail are listed at the end and the command exits non-zero; the run keeps going for the other repos, and re-running pushes only what is still missing.

Fan-out limits and retries are tunable through the environment: `KNIT_GIT_JOBS` (concurrent git pushes, default 8), `KNIT_FORGE_JOBS` (concurrent forge writes, default 4), `KNIT_GIT_PUSH_TIMEOUT` (seconds per push, default 300), and `KNIT_RETRY_BASE_MS` (backoff step, default 1000). Each must be a positive whole number; a value that is not is an error rather than a silently ignored setting.

`knit publish` publishes tracked feature branches to a code host. Knit is host-independent: it detects each repo's host from its git remote. GitHub uses `gh`, GitLab uses `glab`, Codeberg/Forgejo uses `tea` plus REST for richer metadata, and Bitbucket Cloud uses its REST API. Unrecognized remotes retain the historical GitHub fallback.

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

`knit publish create` auto-detects each repo's host (GitHub, GitLab, Forgejo/Codeberg, or Bitbucket) and publishes to all of them. Pass `--provider <id>` (or the `--github` shorthand) to restrict a run to repos on a single host. `knit request` is an alias for `knit publish`.

`knit publish create` is a best-effort two-phase operation and the whole review path after `knit commit`; it does its own branch push, so no separate `knit push` is needed. Repos are published at most `KNIT_FORGE_JOBS` (default 4) at a time, forge calls that fail because the host was momentarily unavailable (5xx, a rate limit honoring `Retry-After`, a dropped connection) are retried up to four times with 1s/2s/4s backoff, and calls the host answered (bad credentials, 404, 422) fail at once. A repo whose publish fails does not stop the others: every repo is reported, the command exits non-zero listing the failures, and re-running creates only what is missing — a repo whose review object already exists is adopted rather than duplicated. It pushes every selected tracked feature branch, creates missing review objects (PRs/MRs) or reuses an existing one for the same feature/base branch, stores publishing metadata in the bundle's `publications`, then rewrites the managed Knit block in every selected review body with the complete cross-repo list. The base defaults to each repo's bundle `baseBranch`; pass `--base release` to use the same base for every selected repo, or repeat `--base repo=branch` for per-repo bases. That target is recorded with the publication. A later native `knit land --target <branch>` can deliberately replace those recorded review bases as part of its landing contract. Body sync is on by default; `--sync` is accepted for explicitness, and `--no-sync` skips that second phase. If body sync fails after review objects were created, run `knit publish sync` after fixing auth or network issues.

For a named lane, select it through Knit itself. The generated plan records `lane`, immutable `targetBranches`, and whether the lane is `terminal`; apply retargets each open review object to its mapped branch, refreshes readiness, merges, and runs that lane's deployments. An intermediate lane like `staging` leaves the bundle open afterwards:

```sh
knit publish create
knit land --lane staging
knit land --lane staging apply
```

Creating reviews against staging up front with `knit publish create --base staging` remains supported, but it is an optimization rather than the landing contract.

When a bundle continues after its recorded reviews were merged or closed, pass `--renew` to start a fresh review round without replacing the bundle. Knit verifies that each recorded review is terminal, refuses open or unverifiable reviews, and refuses renewal when the feature branch still points at the recorded review head. The new review replaces the current per-repo publication projection; the terminal review remains unchanged on its host. Open renewed publications make a previously landed bundle effectively open again. Because an existing landing plan may predate the new repo set, regenerate it with `knit land plan --force` and inspect it before applying.

Hosted services that run Knit from bundle artifacts can set `KNIT_GITHUB_API_TRANSPORT=ipv4` (the historical `curl`/`curl-ipv4` values still work, as do `native`/`api`) to make GitHub artifact-mode publish and landing use Knit's built-in GitHub REST client instead of `gh pr ...` commands. The client resolves hostnames IPv4-first and requires `GH_TOKEN` or `GITHUB_TOKEN` in the environment; no external `curl` is needed. It is intended for non-interactive runtimes where provider CLI prompts, host credential stores, or default IPv6 routing can hang simple GitHub I/O. Local workspace commands keep using the normal provider CLIs unless this environment variable is set. `KNIT_GITHUB_API_BASE` overrides the API base URL (defaults to `https://api.github.com`), mainly for tests.

Bitbucket is always native REST. Authentication is resolved in this order:
`KNIT_BITBUCKET_ACCESS_TOKEN` as a bearer token, or
`KNIT_BITBUCKET_EMAIL` together with `KNIT_BITBUCKET_API_TOKEN` as HTTP Basic
credentials. The latter also accepts the legacy Bitbucket username/app-password
pair. `KNIT_BITBUCKET_API_BASE` overrides the default
`https://api.bitbucket.org/2.0`.

GitLab's richer REST surfaces use `KNIT_GITLAB_TOKEN` then `GITLAB_TOKEN`;
ordinary workspace operations can continue to use `glab auth login`.
`KNIT_GITLAB_API_BASE` defaults to `https://gitlab.com/api/v4`. Forgejo REST
uses `KNIT_FORGEJO_TOKEN`, then `CODEBERG_TOKEN`, then `GITEA_TOKEN`.
`KNIT_FORGEJO_API_BASE` overrides the API base; otherwise Knit derives a
self-hosted base from the remote or defaults artifact operations to
`https://codeberg.org/api/v1`.

When sync remotes are configured, `knit publish create` and `knit push` also push the bundle artifact to those remotes so the host and sync remotes stay in sync. This is on by default; disable it globally with `knit config set push-sync false`, skip it for one command with `--no-remote`, or force one or more remotes with repeated `--remote <name>`. A missing implicit sync remote is skipped after the git branch push; explicitly requested remotes still have to exist.

### Syncing artifacts with sync remotes

`knit sync` with no subcommand is a local-only reconcile: it records git commits made outside Knit as `git.observed` nodes and never touches the network. Its `push`/`pull` subcommands are the one verb family for moving Knit artifacts (bundles, project history, saved views, project architecture, and the explicit knowledge-graph slice) between the workspace and sync remotes:

```sh
knit sync push                 # push bundle + history + views + architecture for the resolved project/bundle
knit sync push --bundles       # push bundle artifacts (open bundles push their feature branches first)
knit sync push --history       # push only project history events
knit sync push --views         # push only your saved views
knit sync push --kg            # push the knowledge-graph viz slice (explicit only)
knit sync pull                 # pull bundle + history + views + architecture
knit sync pull --history       # pull only project history events
knit sync pull --architecture  # pull only the architecture artifact
knit sync pull --kg            # pull the explicit knowledge-graph viz slice
knit sync push --remote hosted    # use an explicit remote
```

With no target flag (`--bundles`/`--history`/`--views`/`--architecture`/`--all`), `knit sync push`/`pull` move every routine artifact family. The knowledge-graph viz slice (produced by `urdir kg viz`, often several MB) is deliberately excluded from `--all` and bare invocations — push it with an explicit `knit sync push --kg` after regenerating it. By default every configured remote is a sync remote — the remotes list itself is the sync set, and names carry no special meaning. `knit config set sync-remotes ...` (or the legacy `sync-remote`) narrows that set when some remotes should stay out of routine sync; override per invocation with one or more `--remote <name>`. Push-style syncs fan out to every sync remote and keep going past a failing one, reporting each failure at the end. Pull-style syncs walk the sync remotes in priority order and use the first one that responds. A pull first reads the project's bundle list, which carries each bundle's artifact metadata but no payloads, and then downloads the payloads it actually needs — one bundle at a time, only where the remote's artifact hash differs from the one the local artifact was last reconciled with (recorded per remote under `syncTargets`). Bundle payloads are never requested in one bulk response.

The git-shaped verbs keep their git semantics but route through the same internal sync module: `knit push --remote <name>` still pushes branches and then the bundle artifact, while `knit fetch --mode knit` and `knit pull --bundles` pull recorded bundle state. Landing's automatic artifact sync (when `push-sync` is enabled) goes through the same module too. There is one implementation behind several differently shaped doors.

Pushing a bundle always means branches + artifact: an open bundle's artifact is never uploaded to a sync remote unless its feature branches are on git `origin`. Bundle pushes (including the project-wide `knit sync push --bundles` sweep) first push any missing or stale feature branch — plain, never forced — from the bundle's checkout; if a branch cannot be pushed or verified, that bundle's artifact upload is skipped with a warning while the rest of the sweep continues. Terminal-state bundles (closed, archived, deleted) sweep artifact-only — their branches were published before landing or archiving.


Remotes can be workspace-local or user-global. Workspace `.knit/config.json` remotes override global remotes of the same name; otherwise commands fall back to the user-level config at `$KNIT_HOME/config.json`, `$XDG_CONFIG_HOME/knit/config.json`, or `~/.config/knit/config.json`. This lets every workspace share the same hosted remote unless a workspace deliberately points that name somewhere else:

```sh
knit remote add --global hosted https://<your-knit-api-url>
export KNIT_REMOTE_HOSTED_TOKEN="<remote API token>"
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

`knit land` coordinates landing the recorded cross-repo review set. It resolves each repo's host adapter from its remote (GitHub, GitLab, Codeberg/Forgejo, or Bitbucket):

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

`knit land plan` writes an editable JSON plan to `.knit/land-plans/<bundle-id>.land.json`. `--lane staging` resolves project-declared per-repo branches; `--target staging` stores one common raw target. Either way the plan records `terminal`: whether landing it finishes the bundle. The options are mutually exclusive. Without either, each review keeps its recorded base. Without a project landing template, the default plan is linear in bundle repo order, uses `merge`, waits for required checks, and does not delete feature branches. With a project landing template, Knit uses the configured merge priority, merge defaults, and selected deployment list. In Knit, a PR with no required checks has passed the required-check gate. You can edit the generated bundle plan to change merge order, use `squash` or `rebase`, insert `wait_checks` steps, insert local `run` steps, or tune typed `deploy` steps before applying.

Bare `knit land` is safe: it creates or shows the default plan and stops. It never merges PRs, deploys, waits, or runs plan commands. Execute the plan explicitly with `knit land apply` after inspection.

`knit land update` prepares published PR branches for landing by fetching each PR's base branch, merging that base into the feature checkout, and recording the movement as a first-class `land.update` bundle node. This is the preferred way to resolve routine "base moved" landing conflicts because the integration merge is attributed to landing prep instead of appearing later as an incidental `git.observed` movement. Pass `--push` to push the updated feature branches after recording the node. If a merge conflicts, resolve and commit it in the feature checkout, then run `knit land update --continue-merge` to record the already-resolved movement as `land.update`.

`knit land apply` validates the inspected plan, refuses draft/closed/missing reviews, writes a durable run file under `.knit/land-runs/`, then executes the plan step by step. When a terminal plan has `targetBranch` or lane `targetBranches`, Knit first retargets each open review through its forge adapter, records the new base in the bundle, and only then evaluates mergeability and checks against that branch; an intermediate plan merges feature branches and leaves the reviews where they are. Passing a different `--target` or `--lane` to apply is refused so an inspected plan cannot silently change destination. Already-merged reviews are accepted only when they landed into the requested target; an open review that conflicts with its new base is rejected with guidance to run `knit land update` first. `deploy` steps support `deploymentMode: "command"` for real deployment commands and `deploymentMode: "push"` for deployments that are triggered by the review merge itself. A command deployment can specify a `checkout` branch; Knit creates or refreshes a managed detached checkout under `.knit/land-worktrees/` before running the command. Run and command-deploy output is streamed live, capture is bounded to protect the caller from unbounded memory growth, and `timeoutSeconds` terminates the command tree when it exceeds its limit (default 1800 seconds). If a step fails, the run stops and records the exact step status, bounded stdout/stderr tails, and failure detail; generated bundle worktrees are left intact so `knit land resume` and `knit land rollback` can continue from the recorded run. `knit land resume` continues that run from pending or failed steps only; succeeded steps are not repeated.

A failed run can leave some PRs merged and others not — merged PRs cannot be un-merged, so Knit offers compensation instead of reset. `knit land rollback` previews the merge steps the failed run completed (verifying each PR is live-MERGED), and `knit land rollback --apply` opens a provider-side revert PR for each of them, records a `pr.revert` node targeting the run, and marks the run rolled back so `knit land resume` refuses to continue it. Setting `onFailure: "rollback"` in the land plan (or in the project landing template, which `knit land plan` copies into generated plans) makes `knit land apply` perform this rollback automatically when a step fails; the default `onFailure: "resume"` keeps today's stop-and-resume behavior. A fully successful `knit land apply` appends a `feature.landed` node recording the destination it landed into. When that destination is terminal it then archives the bundle with a `feature.archived` node, removes generated worktrees under `.knit/worktrees/<bundle>/`, and preserves local feature branches plus the bundle artifact; pass `--keep-worktrees` to archive without removing those checkouts. When the destination is intermediate, the run stops after the landed node: the bundle stays open with its worktrees, and `knit bundle archive <bundle>` closes it by hand if it turns out to go no further. It then syncs the updated bundle artifact to configured sync remotes when push-sync is enabled. Use repeated `--remote <name>` to force remotes, `--no-remote` to skip this sync, or `knit sync push --bundles` to push the landed artifact later.

`knit merge` is for local branch integration that is not a PR landing. It can merge a bundle or git ref into a target branch, or into another bundle's feature branches:

```sh
knit merge feature-x --into staging
knit merge feature-y --into staging --manual
knit merge x-y-compat --into feature-y
```

For branch targets, Knit creates or reuses managed checkouts under `.knit/merge-worktrees/<target>/<repo>/`. Those checkouts are detached: Knit merges there and pushes `HEAD:refs/heads/<target>`, so it never runs inside your source checkout and never mutates it, even when that checkout sits on the target branch. After a merge, the local `<target>` branch is moved to the merge only when no checkout holds it (and the move is a fast-forward); a branch that is checked out somewhere is left alone and Knit prints where the merge is. A merge run is recorded under `.knit/merge-runs/`. By default, if any repo conflicts, Knit aborts the failed merge and resets every repo touched by that run back to its pre-run SHA, so the run behaves all-or-none from Knit’s point of view. Pass `--manual` when you want to resolve the conflicted repo yourself; after resolving and committing in the printed checkout, run `knit merge --continue`, or use `knit merge --abort` to roll back the run.

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

Knit also keeps a project-wide history ledger under `.knit/history/<project>.history.jsonl` and syncs it with sync remotes when history APIs are available. This ledger is metadata only: it records bundle ids, repo ids, branch names, Knit node ids, timestamps, and Git commit SHAs. Git remains the source of truth for file contents, diffs, and file-level history.

Use `knit history list` to inspect the local project history and `knit history refresh` to record new events from local bundle artifacts. Events cover both commits (`commit.recorded`, `commit.observed`, `commit.dropped`, ...) and bundle lifecycle (`bundle.created`, `bundle.landed`, `bundle.archived`, `repo.added`, `repo.removed`); narrow a listing with `--kind` (repeatable), `--repo`, and `--bundle`. Each commit event is named by that commit's subject line and timed by its author date, so a sync sweep that records days of work does not collapse into one timestamp. Exchange history events with a sync remote through the shared sync verbs: `knit sync push --history` and `knit sync pull --history`.

`knit history refresh --rebuild` regenerates the whole ledger from the bundle artifacts on disk, replacing recorded events with their current form — this is how events recorded before their commit detail existed gain messages and real times. Events whose bundle artifact is gone are preserved, and the file is replaced atomically.

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
- `tag.created`
- `repo.removed`

`headNodeId` points at the latest node. Gloss can inspect any node, but the most useful review usually comes from the current head or the final pre-PR bundle.

`publications` records provider metadata for the hosted review set that belongs to the bundle, but it is not the source of truth for code state; git branches, SHAs, and bundle nodes remain the source of truth.

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
- `knit push` pushes feature branches to `origin` and, when sync remotes are configured and `push-sync` is enabled, the bundle artifact to those remotes; it opens no review objects, so use `knit publish create` (which pushes the branches itself) for the PR path.
- `knit publish` detects GitHub, GitLab, Codeberg/Forgejo, and Bitbucket Cloud from each repo remote; unrecognized remotes default to GitHub for compatibility. GitLab and Forgejo keep their CLI paths for the basic workspace loop and use REST for granular CI, review state, mergeability, SHA guards, and retargeting. Without a Forgejo REST token, those richer fields degrade to empty/unknown and the basic `tea` loop remains available. Bitbucket does not expose pre-merge conflict state, so conflicts surface as merge API errors. Bitbucket and Forgejo have no provider-native revert-PR API; GitHub and GitLab do.
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
- Additional self-hosted forge variants and pagination hardening
- Better detection of existing registered worktrees
- Optional bundle export/import flows for handoff to Gloss
