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
knit add ../backend
knit add ../backend --in-place
```

Baseline implemented:

- `knit add --in-place`
- per-repo `checkoutMode`
- in-place status labeling
- wrong-branch guardrails for mutating commands

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

   Baseline implemented: `knit diff`, `knit diff --stat`, and repo id/path filtering against each repo's recorded `baseSha`.

2. Improve `knit show`

   Expand `knit show` beyond commit group ids.

   ```sh
   knit show HEAD
   knit show HEAD~1
   knit show <node-id>
   knit show <git-sha>
   ```

   A git SHA should resolve to the owning bundle node, matching `knit revert` semantics.

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

4. `knit fetch`

   Fetch all tracked repos without merging.

   ```sh
   knit fetch
   knit fetch backend
   knit fetch --all
   ```

   This should be safer than `pull` and should probably land before or with `pull`.

5. `knit push`

   Push tracked feature branches.

   ```sh
   knit push
   knit push backend
   knit push --set-upstream
   ```

   This should not create PRs. It only coordinates git push for feature branches.

## Knit-Native Flow

1. `knit bundle`

   Inspect and validate the artifact that Gloss consumes.

   ```sh
   knit bundle path
   knit bundle print
   knit bundle validate
   ```

2. `knit review`

   Produce the reviewable bundle target for Gloss without invoking Gloss.

   ```sh
   knit review
   knit review HEAD
   knit review <node-id>
   ```

   Output should clearly identify the bundle path and node id. No LLM, GitHub, or UI behavior belongs here.

3. `knit checkpoint`

   Add a non-git ledger note.

   ```sh
   knit checkpoint "frontend wired, backend pending"
   ```

   This is useful for recording feature state when there is no commit yet.

4. `knit close`

   Mark the bundle as closed without deleting git state.

   ```sh
   knit close
   knit close --reason "merged"
   ```

   This should append a `feature.closed` node.

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
- Should `knit review` eventually create a frozen bundle snapshot, or only print the current bundle path and node?
- Should `knit review` optionally run `knit fetch` first, or only warn when referenced commits/remotes are unavailable?
