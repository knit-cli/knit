# Knit 0.1.0-alpha.44

A service running Knit from bundle artifacts can read repositories that belong
to several GitHub accounts in one run. `KNIT_GITHUB_TOKENS` maps an account
(`acme`) or a repository (`acme/widget`) to the token for it; Knit reads each
repository with the most specific match and falls back to `GH_TOKEN` or
`GITHUB_TOKEN` for the rest.
