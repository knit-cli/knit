# Knit 0.1.0-alpha.34

Per-repo draft publishing. A project repo can set `publish.draft: true` so
`knit publish create` opens its review as a draft while the other repos in the
same run open ready for review. `knit project set-draft <repo> [true|false]`
writes or removes the setting; `--draft` still drafts every repo, and projects
without the setting publish as before.
