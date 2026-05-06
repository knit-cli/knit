# Knit Roadmap

Knit is the workspace and history tool for cross-repo feature work. The next commands should keep two ideas separate:

- Git-flow convenience: run familiar git operations across every tracked repo safely.
- Knit-flow intent: make the bundle ledger easier to inspect, review, close, and hand off to Gloss.

## Near-Term Priority

1. `knit diff`

   Show cross-repo diffs for the active bundle.

   ```sh
   knit diff
   knit diff --stat
   knit diff backend
   knit diff HEAD~1
   ```

   Default behavior should compare each tracked worktree against its recorded base/head as appropriate. `--stat` should be optimized for quick scanning.

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
