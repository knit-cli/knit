# Manual Test

This smoke test uses two toy git repos and one Knit workspace.

```sh
mkdir -p /tmp/knit-smoke
cd /tmp/knit-smoke

mkdir backend frontend workspace
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

cd workspace
knit init "venue capacity"
knit track ../backend ../frontend

printf "capacity\n" >> .knit/worktrees/venue-capacity/backend/app.txt
printf "capacity\n" >> .knit/worktrees/venue-capacity/frontend/app.txt

knit status
knit diff --stat
knit git status --short
knit git status --short ../frontend
knit add
knit commit -m "Add venue capacity integration"
knit log -1
knit revert HEAD
knit revert HEAD --apply
knit log -1
```

Expected result:

- `.knit/bundles/venue-capacity.bundle.json` exists.
- The bundle has `feature.created`, `repo.added`, and `worktree.materialized` nodes after `knit track`.
- `knit add` reports staged changes before the commit.
- `knit commit` creates one commit in each staged checkout.
- `knit log -1` shows one logical commit group with both repo SHAs, and the bundle has a `commit.group` node.
- `knit revert HEAD` writes a plan, and `knit revert HEAD --apply` creates one revert commit per affected repo plus a `revert.group` node.

To test a raw git commit outside Knit:

```sh
printf "manual frontend polish\n" >> .knit/worktrees/venue-capacity/frontend/app.txt
git -C .knit/worktrees/venue-capacity/frontend add app.txt
git -C .knit/worktrees/venue-capacity/frontend commit -m "Manual frontend polish"

knit status
knit sync
knit log
```

Expected result: `knit status` reports `unrecorded commits: 1` for `frontend`, `knit sync` appends a `git.observed` node to the bundle, and `knit log` shows `observed git changes` with the frontend commit SHA.

To test a reset/rewind:

```sh
git -C .knit/worktrees/venue-capacity/frontend reset --hard HEAD~1

knit status
knit sync
knit log
```

Expected result: `knit status` reports `rewound commits: 1` for `frontend`, `knit sync` appends another `git.observed` node with `movement: "rewound"` and `droppedCommits`, and `knit log` shows the rewind.

Knit v0 is not perfectly transactional. If a commit succeeds in one repo and fails in another, inspect the affected repos manually before retrying.
