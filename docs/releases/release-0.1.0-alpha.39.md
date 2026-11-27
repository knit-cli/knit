# Knit 0.1.0-alpha.39

`knit diff --published` now skips repositories with no recorded PR/MR when
no repository or path selectors are supplied. Explicit selectors targeting an
unpublished repository still produce an error. When the bundle has no recorded
publications, the command prints a clear message and succeeds.

Missing checkouts, unavailable local HEADs, and provider failures remain errors.
Provider tests are isolated from personal credentials.
