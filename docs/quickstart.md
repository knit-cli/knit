# Quickstart

This walkthrough takes you from install to a landed multi-repo change in about ten minutes. It uses two toy repos so you can run it anywhere.

## 1. Install
Brew:
```sh
brew tap marc-merino/knit && brew install knit   # macOS / Linux binaries
```
Cargo:
```sh
cargo install knit-cli --version 0.1.0-alpha.4   # from crates.io (the binary is named `knit`;
                                                 # cargo needs the explicit version while only pre-releases exist)
# or from a checkout:
cargo install --path .
```

After that, `knit` is available anywhere your shell can see `~/.cargo/bin`.

## 2. Create two toy repos

Knit coordinates repos you already have. For this walkthrough, make two throwaway ones side by side:

```sh
mkdir backend frontend
for r in backend frontend; do
  ( cd "$r" && git init -b main && echo "# $r" > README.md && git add -A && git commit -m "initial $r" )
done
```

## 3. Initialize a project

A **project** is a reusable template that remembers which repos belong together, so you do not re-add them for every bundle. Create a workspace folder beside the repos and register both:

```sh
mkdir workspace && cd workspace
knit init demo
knit project add backend ../backend
knit project add frontend ../frontend
```

## 4. Start a bundle

A **bundle** is the cross-repo analogue of a git branch. Starting one creates a `knit/<bundle>` branch and a generated worktree for each project repo:

```sh
knit bundle "my feature" --cd --agents
```

This creates `.knit/bundles/my-feature.bundle.json` and checkouts under
`.knit/worktrees/my-feature/<repo>/`. `--cd` drops your shell straight into the
bundle's worktree root, and `--agents` writes an `AGENTS.md` tutorial for the
workspace (the bundle worktree root gets its own `AGENTS.md` guidance by
default). From that directory you can simply run `claude`, `opencode`, or any
other coding agent — it wakes up inside an isolated checkout whose context
resolves automatically, with guidance already on disk.

For repos with an `origin`, bundle creation first fetches each project's
configured base branch and records the exact remote commit before creating any
feature branch. Dirty or differently checked-out source repos are left alone.
Local-only repos use their local configured base. Use `knit workspace status`
to compare current checkouts with configured bases; `--offline` uses cached
remote refs and `--from-local-base` deliberately starts from local bases.

If your tool opens the *source* folder instead — Cursor, Codex, or any
agent rooted at the workspace — that works too: the agent can create a bundle
and commit into it from there with `knit --bundle my-feature …`. Opening the
bundle worktree is still the recommended setup, because cwd-based resolution
removes a whole class of wrong-bundle mistakes.

Because every bundle gets its own branches and worktrees, this is also the
parallelism model: start several bundles from the same repos and run one agent
per bundle — they never collide.

## 5. Make changes in the worktrees

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

<!-- image: docs/assets/quickstart-status.png — `knit status` in a real two-repo bundle -->

## 6. Commit across both repos at once

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

## 7. Ship it

From here the path depends on whether your repos have a code host.

**With GitHub (or GitLab / Forgejo) remotes** — open one PR per repo and land them as a set:

```sh
knit publish create     # pushes branches, opens a PR per repo, cross-links them
knit land               # creates/shows the landing plan (does not merge)
knit land apply         # merges, runs deploy steps, archives, removes worktrees
```

`knit land` is safe on its own: it only creates or shows the plan. Nothing merges until `knit land apply`.

Before landing, you can record test verdicts on the bundle itself: `knit check run ci` runs the project command named `ci` and pins the pass/fail result to the exact per-repo commits it ran against. Verdicts go stale the moment the bundle moves. Checks are informational by default; a project can opt in to gating by requiring named checks to be green and fresh before `knit land apply` will execute (`landing.requireChecks`). With several bundles in flight — especially agent-driven ones — `knit check status` answers "which of these is actually ready?" from the ledger instead of from claims. If a landing fails halfway, `knit land resume` continues it, and `knit land rollback --apply` opens revert PRs for the steps that already merged (or set `onFailure: "rollback"` in the landing template to do that automatically). On full success, `knit land apply` archives the bundle and removes its generated worktrees; pass `--keep-worktrees` to keep those checkouts.

**Local-only, no code host** — integrate the bundle's feature branches into a target branch directly:

```sh
knit merge my-feature --into main
```

This merges every repo's `knit/my-feature` branch into its own `main`, recording the run so it can be rolled back. After it succeeds, `backend:main` and `frontend:main` both contain the change.

After landing, once you have verified that main actually works (the deploy, CI, a quick QA pass — whatever you trust), you can record that fact as a cross-repo known-good marker:

```sh
knit tag v1 --bundle my-feature
```

This fetches each repo's origin, pins `origin/main` across all repos as one named set on the bundle ledger, and exports annotated git tags `knit/v1` everywhere — the whole-system snapshot a monorepo gets from a single SHA. Tags are immutable and re-running the same command resumes a partially pushed set. `knit tag` lists them; `knit tag show v1` shows per-repo SHAs and provenance.

## 8. Wrap up

If you used `knit land apply`, the bundle is already archived and its generated worktrees are gone. After a local-only `knit merge`, or when you want to close an unlanded bundle manually, archive it yourself; this removes generated worktrees but keeps branches and the bundle artifact:

```sh
knit bundle archive my-feature --reason done
```

That is the full loop: project → bundle → cross-repo edit → cross-repo commit → publish/land (or merge) → verify → tag → archive.

## 9. Find related cross-repo work

Before touching a file, ask what feature work already touched it — across all
repos, not just the one you are in:

```sh
knit related --repo backend src/api/billing.rs
```

`knit related` works by joining two histories. First it asks Git which commits
touched the path. Then it looks those SHAs up in Knit's project history ledger
(`.knit/history/`, built from bundle ledgers) to recover each commit's bundle
and commit group — and with them the **companion commits in other repos** that
shipped as part of the same logical change. Git stays the source of truth for
file contents; Knit supplies the cross-repo context Git cannot see:

```txt
Bundle: move-work-item-consumption-into-sej Move work item consumption into sej
Scope: commit group kg_20260610_9ab2a5
Touched path:
  sej     d2d5df2  Move work item consumption from knit into sej
Related in same scope:
  knit    5d698c3  Move work item consumption from knit into sej
```

The file you queried lived in one repo; the answer tells you which commit in a
*different* repo must be read alongside it. Pass `--pull` to refresh the
history ledger from a sync remote first.

## 10. Host it on KnitHub (optional)

Knit is local-first, but bundles, project history, and saved views can be
hosted on your KnitHub deployment so they survive machines and show up in
dashboards:

```sh
knit remote add hosted https://<your-knit-api-url> --token <your-token> --global
knit config set --global sync-remotes hosted
knit project push                       # create the hosted project (uploads views too)
KNIT_BUNDLE=my-feature knit sync push   # bundles + history for the walkthrough bundle
```

`knit project push` creates the hosted project record; run it once per project
before the first sync. `knit sync push` resolves a bundle the same way every
other command does — since step 8 archived `my-feature`, name it explicitly
with `KNIT_BUNDLE` (or sync before archiving).

Once a sync remote is configured, publish and land keep the hosted artifacts in
sync automatically, hosted dashboards show bundles and project history, and
`knit clone <owner>/<project>` can rebuild a working workspace on another
machine. `knit related --pull` and `knit sync pull` read the same hosted
history back.

<!-- image: docs/assets/knithub-dashboard.png — a project's bundles in a KnitHub deployment -->

Projects can also be cloned from Knit. `knit clone` will rebuild the project
as it is set up in Knit, given your git and KnitHub tokens allow it.

KnitHub contains `gloss`. `gloss` is another tool that allows you to run a cross repo analysis/code review
in the same page.

## Concepts

**Project.** A reusable repo template, created once with `knit init <name>` (like `git init`, but for a set of repos). Add repos with `knit project add <id> <path>`; mark a repo `--observe` to keep it available but out of default bundle starts. Projects can also define commands (`knit project command set …`) and a default landing template. Projects are optional — you can also `knit bundle add <path>` repos directly into an ad-hoc bundle.

**Bundle.** The branch-like feature unit and the cross-repo analogue of a git branch. `knit bundle "<title>"` creates one; bare `knit bundle` shows the resolved one. The same source repo can appear in many bundles at once, each on its own `knit/<bundle>` feature branch and worktree (`.knit/worktrees/fix-a/backend`, `.knit/worktrees/fix-b/backend`), so parallel features never collide. Bundle-aware commands resolve their bundle from `--bundle`, then `KNIT_BUNDLE`, then the generated worktree path, then the workspace fallback. The bundle's `.bundle.json` is the source of truth for the feature.

**Worktrees.** Each tracked repo gets a generated checkout under `.knit/worktrees/<bundle>/<repo>/`. Make all changes there. Everyday VCS verbs operate across every tracked checkout at once: `knit add`, `knit status`, `knit diff`, `knit commit`, `knit push`, `knit log`. For one repo, target it by id or path (`knit diff backend`). `knit git <args>` passes through to git in each checkout.

**Ledger and nodes.** A bundle is an append-only feature ledger. It stores current state in `repos` and `commitGroups`, plus an ordered `nodes` chain (`feature.created`, `commit.group`, `git.observed`, `feature.landed`, `repo.removed`, …). A `knit commit` records all repo commits as one `commit.group`. Because the ledger is append-only, undo goes through `knit revert` (there is no `knit reset`), and commits made outside Knit are folded in with `knit sync`.

**Publish/land vs merge.** Two different ways to ship a bundle. `knit publish create` opens one review object (PR/MR) per repo on each repo's detected host, and `knit land` / `knit land apply` merge that whole set into each PR's base branch and run any deployments — this is the path when your repos have a code host and you want reviewed PRs. `knit merge <bundle> --into <branch>` is for local branch integration with no forge: it merges the bundle's feature branches straight into a target branch (or another bundle), all-or-none with rollback on conflict. Do not merge host PRs directly (e.g. `gh pr merge`) for Knit-owned bundles.

**Views and commands.** A **view** is per-user config layered over a project: a named "bundle shape" of include/exclude deltas over the project's repo set, so different people can start bundles with different subsets of the same project (`knit view save`, `knit view default`). A project can also register reusable commands that run inside a bundle's checkouts (`knit project command set dev --repo frontend -- docker compose up`, then `knit run dev`). `knit run up|status|down` additionally starts a disposable stack instance per bundle: Knit lifts the docker-compose shape the repos already run on `main` — bundle worktrees substituted for source paths, published ports reallocated, isolated compose project — or, for stacks that want precise control, runs a repo-owned compose file written against Knit's `KNIT_*` environment contract. Both are covered in full in the reference.

See [reference.md](reference.md) for the complete behavior of every command and subsystem.
