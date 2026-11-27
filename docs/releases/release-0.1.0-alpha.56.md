# Knit 0.1.0-alpha.56

After a rebase, commits that no earlier commit group maps onto are now
recorded under their own commit subject. Before, they were put together in a
single group named after the bundle. Commits with the same subject in
different repositories stay one group, as `knit commit` records them.

`knit sync --repair-groups` no longer leaves the commits it restores listed
in the Git observation that once superseded their groups, so history shows
each of them once. It also removes such doubles left behind by an earlier
repair.
