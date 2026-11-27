# Knit 0.1.0-alpha.40

Review text comes from files and stays in sync. `PR-<repo>.md` in the bundle
worktree root is that repository's review body when no flag or policy selects
one, else `PR.md` covers every repository, and a leading `Title:` line is the
title. `knit publish sync` applies a title or body file to open reviews when it
changed since Knit last applied it, so editing the file and running
`knit publish sync` updates the review, while text edited on the host stays
until the file changes. `knit project set-publish` sets title and body policy.

Contributions to repositories you cannot merge now have a landing path. A fork
review into a repository the host reports you cannot push to is planned as a
gate that waits for its maintainers: merged reviews are recorded as landed,
open ones pause the run until `knit land resume`, and Knit never merges or
reverts them. `knit land check` lists them as awaiting maintainers.
