# Knit 0.1.0-alpha.57

Landing runs no longer leave their command checkouts on disk. A run that
succeeds, or whose recovery restores everything, removes the checkouts under
`.knit/land-runs/<run>.run.checkouts/` with `git worktree remove`, so they also
leave each repository's worktree list. A failed run keeps its checkouts for
debugging and resume until a newer run of the bundle replaces it, its bundle is
finished, or three days pass. Resume recreates any checkout that is gone.

`knit clean --land-runs`, also part of `knit clean --all`, removes checkouts
left behind by earlier versions and keeps those of runs that can still resume.
`knit doctor` lists them.

Cleanup now finishes a worktree removal that git started but could not
complete because a process such as a dev server kept writing into the
checkout. Such a worktree used to stay on disk, and `knit bundle prune` listed
its landed bundle under finished bundles with worktrees still on disk;
`knit clean --archived --worktrees` now removes it.
