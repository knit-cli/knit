# Knit 0.1.0-alpha.54

`knit status` no longer says a bundle is ready to land while a recorded review
conflicts with its base. It stays offline: under `Publications:` it prints each
review's last recorded mergeability and verdict, with the age of that record,
from the gates `knit publish create` and `knit publish sync` store. The `Next:`
line follows that record:

- a review that conflicts with or is behind its base:
  `knit land update --push <repos>`;
- another blocker on you, such as a draft or failing checks: do not land yet;
- a review with no record, a record older than 60 minutes, or commits recorded
  in that repository since: check with `knit publish status --live` before
  `knit land`;
- otherwise the usual landing advice.

`knit status --json` gives each repository with a recorded review a `review`
object (`mergeable`, `verdict`, `blocked`, `openGates`, `recordedAt`,
`ageSeconds`, `stale`, ...), and `publications` gains `readiness` (`blocked`,
`unverified` or `ready`) and `freshForSeconds`. Without recorded reviews the
document is unchanged.

`knit publish status` shows the same recorded state under each review and gives
the same advice. With `--live`, a conflict or a gate blocked on you replaces the
landing advice.

The bundle `AGENTS.md` now names `knit publish status --live` as the way to
check review state and `knit land update [REPOS] --push` as the way to resolve a
conflict with a review's base.
