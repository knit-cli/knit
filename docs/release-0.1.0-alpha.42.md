# Knit 0.1.0-alpha.42

Landing plan sync no longer stops on one bad record. `knit sync pull --plans`
reads replies larger than 10 MB (up to 512 MB) instead of failing. `knit sync
push --plans` keeps a bundle's landing history local when one of its saved runs
fails today's validation, names those bundles once per reason, and syncs every
other bundle. A run that differs from the sync remote's copy only in local
finalization bookkeeping counts as synced, so it is no longer re-sent and
refused.
