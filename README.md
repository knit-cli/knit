# Knit

Knit is a local-first Rust CLI for authoring and coordinating multi-repo feature work. Think of it as "git for cross-repo feature work": a **bundle** is the cross-repo analogue of a git branch, holding a small set of related repositories. Knit creates a coordinated checkout per repo, commits staged changes across them all at once, and records the result in a language-neutral JSON artifact. From that bundle it can open one pull request per repo and land them as a single set, or merge the feature branches into local targets when there is no code host. Parallel features stay isolated: the same source repo can appear in many bundles at once, each on its own branch and worktree.

Knit shells out to `git`. It does not use libgit2 and it does not try to replace git — everyday verbs (`status`, `diff`, `add`, `commit`, `push`, `log`) just run across every repo in the bundle at once.

## Quickstart

This walkthrough takes you from install to a landed multi-repo change in about ten minutes. It uses two toy repos so you can run it anywhere.

### 1. Install

```sh
cargo install --path .
```

After that, `knit` is available anywhere your shell can see `~/.cargo/bin`.

### 2. Create two toy repos

Knit coordinates repos you already have. For this walkthrough, make two throwaway ones side by side:

```sh
mkdir backend frontend
for r in backend frontend; do
  ( cd "$r" && git init -b main && echo "# $r" > README.md && git add -A && git commit -m "initial $r" )
done
```

### 3. Initialize a project

A **project** is a reusable template that remembers which repos belong together, so you do not re-add them for every bundle. Create a workspace folder beside the repos and register both:

```sh
mkdir workspace && cd workspace
knit init demo
knit project add backend ../backend
knit project add frontend ../frontend
```

### 4. Start a bundle

A **bundle** is the cross-repo analogue of a git branch. Starting one creates a `knit/<bundle>` branch and a generated worktree for each project repo:

```sh
knit bundle "my feature"
```

This creates `.knit/bundles/my-feature.bundle.json` and checkouts under
`.knit/worktrees/my-feature/<repo>/`.

### 5. Make changes in the worktrees

Edit files inside the generated worktrees, not the original repos:

```sh
echo "backend change"  >> .knit/worktrees/my-feature/backend/README.md
echo "frontend change" >> .knit/worktrees/my-feature/frontend/README.md
```

Inspect the cross-repo state:

```sh
knit status
knit diff
```

`knit status` shows each repo's branch, worktree, and status; `knit diff` shows the combined diff against each repo's recorded base:

```txt
Bundle: my-feature (workspace)
State: open

repo       branch           worktree                              mode       status
backend    knit/my-feature  .knit/worktrees/my-feature/backend    worktree   modified
frontend   knit/my-feature  .knit/worktrees/my-feature/frontend   worktree   modified
```

### 6. Commit across both repos at once

```sh
knit commit --all -m "Add my feature across backend and frontend"
knit log
```

`--all` stages then commits. Knit records both repo commits as one logical commit group in the bundle:

```txt
kg_20260610_907b25  Add my feature across backend and frontend
  backend    6b5fa99
  frontend   af3a6bb
```

### 7. Ship it

From here the path depends on whether your repos have a code host.

**With GitHub (or GitLab / Forgejo) remotes** — open one PR per repo and land them as a set:

```sh
knit publish create     # pushes branches, opens a PR per repo, cross-links them
knit land               # creates/shows the landing plan (does not merge)
knit land apply         # merges each PR into its base, then runs any deploy steps
```

`knit land` is safe on its own: it only creates or shows the plan. Nothing merges until `knit land apply`.

**Local-only, no code host** — integrate the bundle's feature branches into a target branch directly:

```sh
knit merge my-feature --into main
```

This merges every repo's `knit/my-feature` branch into its own `main`, recording the run so it can be rolled back. After it succeeds, `backend:main` and `frontend:main` both contain the change.

### 8. Wrap up

Mark the bundle done. This removes the generated worktrees but keeps the branches and the bundle artifact:

```sh
knit bundle archive my-feature --reason done
```

That is the full loop: project → bundle → cross-repo edit → cross-repo commit → publish/land (or merge) → archive.

## Concepts

**Project.** A reusable repo template, created once with `knit init <name>` (like `git init`, but for a set of repos). Add repos with `knit project add <id> <path>`; mark a repo `--observe` to keep it available but out of default bundle starts. Projects can also define commands (`knit project command set …`) and a default landing template. Projects are optional — you can also `knit bundle add <path>` repos directly into an ad-hoc bundle.

**Bundle.** The branch-like feature unit and the cross-repo analogue of a git branch. `knit bundle "<title>"` creates one; bare `knit bundle` shows the resolved one. The same source repo can appear in many bundles at once, each on its own `knit/<bundle>` feature branch and worktree (`.knit/worktrees/fix-a/backend`, `.knit/worktrees/fix-b/backend`), so parallel features never collide. Bundle-aware commands resolve their bundle from `--bundle`, then `KNIT_BUNDLE`, then the generated worktree path, then the workspace fallback. The bundle's `.bundle.json` is the source of truth for the feature.

**Worktrees.** Each tracked repo gets a generated checkout under `.knit/worktrees/<bundle>/<repo>/`. Make all changes there. Everyday VCS verbs operate across every tracked checkout at once: `knit add`, `knit status`, `knit diff`, `knit commit`, `knit push`, `knit log`. For one repo, target it by id or path (`knit diff backend`). `knit git <args>` passes through to git in each checkout.

**Ledger and nodes.** A bundle is an append-only feature ledger. It stores current state in `repos` and `commitGroups`, plus an ordered `nodes` chain (`feature.created`, `commit.group`, `git.observed`, `feature.landed`, `repo.removed`, …). A `knit commit` records all repo commits as one `commit.group`. Because the ledger is append-only, undo goes through `knit revert` (there is no `knit reset`), and commits made outside Knit are folded in with `knit sync`.

**Publish/land vs merge.** Two different ways to ship a bundle. `knit publish create` opens one review object (PR/MR) per repo on each repo's detected host, and `knit land` / `knit land apply` merge that whole set into each PR's base branch and run any deployments — this is the path when your repos have a code host and you want reviewed PRs. `knit merge <bundle> --into <branch>` is for local branch integration with no forge: it merges the bundle's feature branches straight into a target branch (or another bundle), all-or-none with rollback on conflict. Do not merge host PRs directly (e.g. `gh pr merge`) for Knit-owned bundles.

**Views and commands.** A **view** is per-user config layered over a project: a named "bundle shape" of include/exclude deltas over the project's repo set, so different people can start bundles with different subsets of the same project (`knit view save`, `knit view default`). A project can also register reusable commands that run inside a bundle's checkouts (`knit project command set dev --repo frontend -- docker compose up`, then `knit run dev`). Both are covered in full in the reference.

See [docs/reference.md](docs/reference.md) for the complete behavior of every command and subsystem.

## Reference

- **[docs/reference.md](docs/reference.md)** — the full command synopsis and the complete behavior reference for every subsystem: projects and views, bundle lifecycle and pruning, staging/diff/fetch/pull/push, publish, land, merge, history and `related`, revert, the storage layout, bundle node types, current limitations, and the roadmap.
- **[docs/architecture.md](docs/architecture.md)** — module boundaries and test layout.
- **[docs/manual-test.md](docs/manual-test.md)** — a small two-repo smoke test.
- **[docs/change-group-schema.md](docs/change-group-schema.md)** — the current bundle (`ChangeGroup`) fields.

## Knit And Gloss

Knit and Gloss share a single artifact: the user-facing **bundle** (technical schema type `ChangeGroup`, file pattern `<slug>.bundle.json`). Knit owns authoring and workspace mechanics — local repos, worktrees, feature branches, commit groups, and bundle updates. Gloss reads a bundle later and produces review/ranking/explanation output; it does not own worktrees, commits, reverts, or branch lifecycle.
