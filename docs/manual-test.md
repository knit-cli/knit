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
knit add ../backend ../frontend

printf "capacity\n" >> .knit/worktrees/venue-capacity/backend/app.txt
printf "capacity\n" >> .knit/worktrees/venue-capacity/frontend/app.txt

knit status
knit stage
knit commit -m "Add venue capacity integration"
knit log
```

Expected result:

- `.knit/bundles/venue-capacity.bundle.json` exists.
- The bundle has `feature.created`, `repo.added`, and `worktree.materialized` nodes after `knit add`.
- `knit stage` reports staged changes before the commit.
- `knit commit` creates one commit in each staged worktree.
- `knit log` shows one logical commit group with both repo SHAs, and the bundle has a `commit.group` node.

Knit v0 is not perfectly transactional. If a commit succeeds in one repo and fails in another, inspect the affected repos manually before retrying.
