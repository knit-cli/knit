# Knit 0.1.0-alpha.38

`knit diff --published` compares the live head of each recorded PR/MR with
its local bundle checkout, in the direction **published → local**. It includes
committed, staged, and unstaged tracked-file changes; untracked files are excluded.
The comparison uses endpoint trees, with no merge-base or Knit history dependency,
so it works after squashing, rebasing, or cleaning up local history.

Repository ID and path selectors, `--stat`, and explicit `--bundle` selection are
supported. Output identifies the repository, review URL, full published SHA,
local HEAD, and checkout. Fork contributions support separate upstream fetch
and contributor push URLs. Missing publications, unavailable published heads,
and missing checkouts produce errors instead of falling back to the bundle base.

The command fetches the exact published commit for inspection. It does not push,
publish, edit reviews, switch branches, change the index or working tree, or
rewrite bundle/history metadata. Plain `knit diff` retains its existing behavior.

Forge access now respects the explicit review host for self-hosted API endpoints
and `gh`/`glab` API calls. GitLab reads can use saved `glab` authentication when
no token is configured, and Forgejo reads select the saved `tea` login matching
the review host, reporting missing or ambiguous matches.
