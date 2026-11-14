# Knit 0.1.0-alpha.53

`knit rebase` now rebases each repository onto the base of its recorded
review, the same base `knit land update` merges: the review's branch, fetched
from the upstream repository for a fork contribution. Before, it always used the
configured base branch, so a review opened with `--target release` was rebased
onto `main`. The output names the review base when it is not `origin/<base>`.

`knit rebase` accepts repository selectors, like `knit land update`:
`knit rebase api web` rebases only those. A selection cannot be combined with
`--squash`, and it is refused when a repository left out pins a selected one's
feature branch in `Cargo.lock`.

`knit workspace status` still compares each configured base with `origin`, and
now adds a note under any repository whose open review targets another base,
such as a fork's upstream, with how far `origin/<base>` and the feature branch
are behind it.

When a review lives in another repository than `origin` but the bundle records
no contribution target, `knit rebase` and `knit land update` refuse instead of
silently using `origin`, and print the `git remote add` and
`knit publish create --source-remote origin --target-remote upstream` commands
that record it.

`knit rebase --offline` on a fork repository whose review base was never
fetched now fails instead of falling back to the local base branch.
