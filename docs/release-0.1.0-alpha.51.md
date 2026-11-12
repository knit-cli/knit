# Knit 0.1.0-alpha.51

`knit push` now always syncs the bundle artifact and history to the sync
remotes after pushing branches, even when push-sync is off; `--no-remote`
skips it. The new `knit sign` re-signs a bundle's commits in place with the
repo's git signing config, keeps every commit group on the new SHAs, then
force-pushes with a lease and syncs the hosted copy (`--no-push` stays local).
It refuses branches that contain a merge commit. `knit sync` keeps commit
groups when commits are rewritten without changing their content, author or
message (re-signed or re-dated outside Knit), and `knit sync --repair-groups
[--apply]` restores groups an earlier sync dropped that way. `knit sync push
--bundles` reports one total that includes the current bundle.
