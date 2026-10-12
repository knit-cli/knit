# Knit 0.1.0-alpha.46

`knit revert landed --apply --land` undoes a landed bundle in one step, even
when later work has landed on top of it. Knit opens one revert review per
repository, merges them in the reverse of the executed landing plan (in
parallel wherever that plan had no order), runs the plan's deployments after
their reverted merges, and ends with a `Revert verified:` line once every
merged revert is an exact inverse of the original change on the base branch.

`knit init` and `knit agents` now generate a short agent guide that names the
everyday commands and points at `knit <command> --help` for the rest.

`knit land apply` and `knit land check` wait for a forge that is still
computing whether a fresh review is mergeable instead of reporting a conflict.

Forge remotes on custom ports and plain HTTP are accepted. Credentials are
keyed by host and port; tokens travel over plain HTTP only to loopback or to
an origin allowed with `knit auth allow-http <host[:port]>`.
