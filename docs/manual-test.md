# Manual Test

This smoke test uses two default toy git repos, one observed toy git repo, and one Knit workspace.

```sh
mkdir -p /tmp/knit-smoke
cd /tmp/knit-smoke

mkdir backend frontend docs workspace
git -C backend init
git -C backend checkout -b main
git -C backend config user.email knit@example.test
git -C backend config user.name "Knit Smoke"
printf "backend\n" > backend/app.txt
git -C backend add app.txt
git -C backend commit -m "Initial backend"

git -C frontend init
git -C frontend checkout -b main
git -C frontend config user.email knit@example.test
git -C frontend config user.name "Knit Smoke"
printf "frontend\n" > frontend/app.txt
git -C frontend add app.txt
git -C frontend commit -m "Initial frontend"

git -C docs init
git -C docs checkout -b main
git -C docs config user.email knit@example.test
git -C docs config user.name "Knit Smoke"
printf "docs\n" > docs/app.txt
git -C docs add app.txt
git -C docs commit -m "Initial docs"

cd workspace
knit project init venues
knit project add backend ../backend
knit project add frontend ../frontend
knit project add docs ../docs --observe
knit bundle start "venue capacity"

printf "capacity\n" >> .knit/worktrees/venue-capacity/backend/app.txt
printf "capacity\n" >> .knit/worktrees/venue-capacity/frontend/app.txt

knit bundle
knit status
knit diff --stat
knit git status --short
knit git status --short ../frontend
knit add
knit commit -m "Add venue capacity integration"
knit log -1
knit show HEAD
knit revert HEAD
knit revert HEAD --apply
knit log -1
```

Expected result:

- `.knit/projects/venues.project.json` exists with `backend` and `frontend` included by default and `docs` observed.
- `.knit/bundles/venue-capacity.bundle.json` exists.
- The bundle has `feature.created`, `repo.added`, and `worktree.materialized` nodes after `knit bundle start`.
- `.knit/worktrees/venue-capacity/backend` and `.knit/worktrees/venue-capacity/frontend` exist, while `docs` is not tracked until explicitly added.
- `knit add` reports staged changes before the commit.
- `knit commit` creates one commit in each staged checkout.
- `knit log -1` shows one logical commit group with both repo SHAs, and the bundle has a `commit.group` node.
- `knit show HEAD` shows the node details and git stats for the latest logical group.
- `knit revert HEAD` writes a plan, and `knit revert HEAD --apply` creates one revert commit per affected repo plus a `revert.group` node.

To test a raw git commit outside Knit:

```sh
printf "manual frontend polish\n" >> .knit/worktrees/venue-capacity/frontend/app.txt
git -C .knit/worktrees/venue-capacity/frontend add app.txt
git -C .knit/worktrees/venue-capacity/frontend commit -m "Manual frontend polish"

knit status
knit sync
knit log
knit show HEAD
```

Expected result: `knit status` reports `unrecorded commits: 1` for `frontend`, `knit sync` appends a `git.observed` node to the bundle, `knit log` shows `observed git changes` with the frontend commit SHA, and `knit show HEAD` shows the raw commit stats.

To test a reset/rewind:

```sh
git -C .knit/worktrees/venue-capacity/frontend reset --hard HEAD~1

knit status
knit sync
knit log
```

Expected result: `knit status` reports `rewound commits: 1` for `frontend`, `knit sync` appends another `git.observed` node with `movement: "rewound"` and `droppedCommits`, and `knit log` shows the rewind.

To test parallel bundles over the same repo:

```sh
knit bundle start "backend only" --repo backend
knit bundle list

knit status
knit status --bundle venue-capacity
knit status --bundle backend-only

cd .knit/worktrees/venue-capacity/backend
knit status

cd ../../../backend-only/backend
knit status
```

Expected result: the same source `backend` repo has both `knit/venue-capacity` and `knit/backend-only` branches, each generated worktree resolves its own bundle from cwd, and `--bundle` overrides cwd/workspace context.

To test local integration into a staging branch:

```sh
git -C ../backend branch staging
git -C ../frontend branch staging

knit merge venue-capacity --into staging --fetch
knit merge status
knit bundle compat venue-capacity backend-only --title "venue backend compat"
knit merge venue-capacity --into venue-backend-compat
```

Expected result: branch-target merges use `.knit/merge-worktrees/staging/<repo>/`, write `.knit/merge-runs/<run-id>.json`, and either merge every repo in the run or roll back the run to the pre-merge SHAs. `knit merge status` shows the run and per-repo checkout paths. Bundle-target merges update the target bundle branches and append a `git.observed` node to that target bundle.

To inspect and repair workspace metadata:

```sh
knit schema print bundle
knit doctor
knit migrate --check
```

To discard a throwaway bundle and its generated local state:

```sh
knit bundle delete documentation-quick-wins --force --worktrees --branches --force-branches
```

Expected result: the bundle JSON moves to `.knit/deleted/bundles/`, generated worktrees under `.knit/worktrees/<bundle>/` are removed, and local `knit/<bundle>` branches are deleted from the source repos. Remote branches are preserved.

To test landing with real disposable GitHub PRs, push/publish first, then inspect before applying:

```sh
knit publish github create --base main --no-sync
knit land plan
knit land status
```

Expected result: `.knit/land-plans/venue-capacity.land.json` lists one `merge_pr` step per published repo. Only run `knit land apply` against PRs you are comfortable merging. A failed apply writes `.knit/land-runs/<plan-id>-<timestamp>.run.json`; after fixing the failed step, run `knit land resume`.

Knit is not a database transaction layer. If a commit succeeds in one repo and fails in another, inspect the affected repos manually before retrying.
