# Knit 0.1.0-alpha.52

A landing killed before it finished no longer leaves locks that block every
later apply and resume. Each lock now records which step processes its owner
started; once the owner and all of those have exited, the next `knit land
apply` or `knit land resume` takes the lock over and says so. Locks that
cannot be verified still block, and the new `knit land unlock` lists every
lock and releases the safe ones (`--confirm-quiescent` also releases the
unverifiable ones after you have checked). A lock held by a running process
is never released.

Rewriting one repository's commits (re-signing, re-dating, or a rebase that
moves only that repository) no longer splits cross-repository commit groups
in two. `knit sync` reports the real number of commits moved, and `knit log`
names every group a rewrite produced.
