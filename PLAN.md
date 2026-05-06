# Knit Roadmap

Knit is the workspace and history tool for cross-repo feature work. The next commands should keep two ideas separate:

- Git-flow convenience: run familiar git operations across every tracked repo safely.
- Knit-flow intent: make the bundle ledger easier to inspect, review, close, and hand off to Gloss.

## Checkout Modes

Knit should support two checkout modes per tracked repo:

- `worktree`: the default and safest mode. Knit creates `.knit/worktrees/<bundle>/<repo>` and operates there.
- `inPlace`: an explicit mode for using the original repo checkout directly.

Proposed command shape:

```sh
knit track ../backend
knit track ../backend --in-place
```

Baseline implemented:

- `knit track --in-place`
- per-repo `checkoutMode`
- in-place status labeling
- wrong-branch guardrails for mutating commands

Vocabulary update implemented:

- `knit track`: add repos/checkouts to the feature.
- `knit untrack`: remove repos from Knit tracking.
- `knit add`: stage file changes inside tracked checkouts, like `git add`.
- `knit stage`: alias for `knit add`, matching Git's own `stage` alias.

`--in-place` is useful when one person/tool owns the repo checkout for the feature and does not need the original checkout to stay on `main`. It should be first-class, not a hack.

Bundle state should record checkout mode per repo, for example:

```json
{
  "id": "backend",
  "path": "../backend",
  "checkoutMode": "inPlace",
  "featureBranch": "knit/venue-capacity",
  "worktreePath": "../backend"
}
```

Safety rules:

- Default remains `worktree`.
- In-place repos must show clearly in `knit status`.
- Mutating commands must verify the repo is on the expected feature branch before operating.
- If an in-place repo is on the wrong branch, `knit status` should warn loudly and mutating commands should refuse unless explicitly forced.
- `knit clean --worktrees` must never delete in-place repo paths.

## History Access Contract

Knit shells out to git, so it can inspect the git history available in each tracked local repo: commits, branches, merge bases, diffs, logs, file contents at refs, and recorded SHAs.

Gloss should be able to inspect the same repo set when it runs in the same local environment. The bundle should give Gloss enough information to locate the relevant repos, checkouts, branches, SHAs, and bundle nodes. The bundle should not duplicate full git history; git remains the history store.

Important caveat: "full history" means the history available in the local clone. Shallow clones, partial clones, missing remotes, unfetched branches, or garbage-collected unreachable commits can limit what Knit and Gloss can inspect.

`knit fetch` should be the main command for improving local history availability before review. `knit review` may later warn when referenced commits or remotes are unavailable.

## Near-Term Priority

1. `knit diff`

   Show cross-repo diffs for the active bundle.

   ```sh
   knit diff
   knit diff --stat
   knit diff backend
   knit diff HEAD~1
   ```

   Default behavior should compare each tracked checkout against its recorded base/head as appropriate. `--stat` should be optimized for quick scanning.

   Baseline implemented: `knit diff`, `knit diff --stat`, and repo id/path filtering against each repo's recorded `baseSha`. Untracked files intentionally follow `git diff` behavior: they appear in status, not diff, until added to the index.

2. Improve `knit show`

   Expand `knit show` beyond commit group ids.

   ```sh
   knit show HEAD
   knit show HEAD~1
   knit show <node-id>
   knit show <git-sha>
   ```

   A git SHA should resolve to the owning bundle node, matching `knit revert` semantics.

   Baseline implemented: `knit show` now resolves `HEAD`, `HEAD~N`, node ids/prefixes, commit group ids, and recorded git commit SHAs through the same selector code as `knit revert`. Commit/revert groups show git stats for each recorded repo commit, observed git nodes show movement plus available commit stats, and removed-repo nodes summarize their repo ids.

3. `knit pull`

   Pull from every tracked repo, primarily for keeping the original repo and/or base refs current.

   ```sh
   knit pull
   knit pull --rebase
   knit pull backend
   knit pull --all
   ```

   Safety rules:

   - Refuse to run when any affected worktree has uncommitted changes unless `--force` is explicitly provided.
   - Prefer pulling in the original repo path for base branches, not blindly inside Knit feature worktrees.
   - If running in feature worktrees, require an explicit mode such as `--feature`.
   - Record meaningful branch movement as `git.observed` when tracked feature branch heads change.
   - Print a per-repo summary of before/after SHAs.

   Baseline implemented: `knit pull`, `knit pull <repo>`, `knit pull --all`, and `knit pull --rebase`. Default pulls run in original repo paths on recorded base branches using `git pull --ff-only`, refuse dirty affected checkouts unless `--force` is passed, print before/after SHAs, and update `baseSha`. `knit pull --feature` explicitly pulls tracked feature checkouts and records feature branch movement as `git.observed`.

4. `knit fetch`

   Fetch all tracked repos without merging.

   ```sh
   knit fetch
   knit fetch backend
   knit fetch --all
   ```

   This should be safer than `pull` and should probably land before or with `pull`.

   Baseline implemented: `knit fetch`, `knit fetch <repo>`, and `knit fetch --all` fetch `origin` in the original repo path. Fetch updates remote refs/object availability only; it does not move checkouts, update `baseSha`, or append bundle nodes.

5. `knit push`

   Push tracked feature branches.

   ```sh
   knit push
   knit push backend
   knit push --set-upstream
   ```

   This should not create PRs. It only coordinates git push for feature branches.

   Baseline implemented: `knit push`, `knit push <repo>`, `knit push --all`, and `knit push --set-upstream` push each tracked feature branch to `origin`. Push validates that the checkout is on the recorded feature branch, reports per-repo success/failure, and leaves PR/GitHub creation out of scope.

## Knit-Native Flow

1. `knit bundle`

   Inspect and validate the existing source-of-truth artifact that Gloss consumes.

   ```sh
   knit bundle path
   knit bundle print
   knit bundle validate
   ```

   This must not produce a second review object. Knit continuously maintains `.knit/bundles/<slug>.bundle.json`; Gloss should read that bundle and use the referenced repos, branches, and SHAs for analysis.

   Baseline implemented: `knit bundle path`, `knit bundle print`, and `knit bundle validate` inspect the active bundle. Validation checks structural `ChangeGroup` invariants and deliberately does not perform git reachability or review analysis.

2. Gloss handoff

   Do not add `knit review` for now. Review/ranking/explanation belongs in Gloss. Gloss can either accept a bundle path explicitly or discover the active `.knit/config.json` from the current workspace. If a future frozen snapshot/export is needed, design that separately as a portability feature, not as the normal review path.

3. `knit checkpoint`

   Add a non-git ledger note.

   ```sh
   knit checkpoint "frontend wired, backend pending"
   ```

   This is useful for recording feature state when there is no commit yet.

   Baseline implemented: `knit checkpoint "<note>"` appends a `checkpoint` node to the active bundle, updates `headNodeId`, appears in `knit log`, and can be inspected with `knit show HEAD`. It does not touch git.

4. `knit close`

   Mark the bundle as closed without deleting git state.

   ```sh
   knit close
   knit close --reason "merged"
   ```

   This should append a `feature.closed` node.

   Baseline implemented: `knit close` and `knit close --reason <reason>` append a `feature.closed` node to the active bundle, update `headNodeId`, appear in `knit log`, and can be inspected with `knit show HEAD`. It does not delete or mutate git state.

5. `knit clean`

   Remove Knit-local generated state after explicit confirmation.

   ```sh
   knit clean --plans
   knit clean --worktrees
   knit clean --all
   ```

   This must be conservative. It should never delete source repos or branches by default.

## Git Parity Candidates

- `knit branch`: show tracked feature branches, base branches, and upstreams.
- `knit base`: show or update base branch per repo.
- `knit checkout`: likely unnecessary until there are multiple active bundles.
- `knit tag`: later, if releases become part of Knit.

## Open Design Questions

- Should `knit pull` default to original repo paths, feature worktrees, or both with separate phases?
- Should feature branch rebases be recorded as `git.observed diverged` or a dedicated `git.rebased` node?
- Should `knit push` require a clean bundle status before pushing?
- Should Gloss prefer an explicit bundle path, active Knit workspace discovery, or both?
- Should Knit later support a frozen bundle export/import command for portability across machines?
