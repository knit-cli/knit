# Knit 0.1.0-alpha.50

`knit doctor`, `knit migrate` and `knit project list` read only
`*.project.json` files in `.knit/projects/` as projects, so the known-repos
file a fresh clone writes there no longer makes `knit doctor` fail. `knit pull
--bundles` no longer fails open bundles with "Another Knit process is updating
this state" naming its own process: the landing plan syncs it runs for each
bundle now take turns instead of colliding on the workspace lock.
