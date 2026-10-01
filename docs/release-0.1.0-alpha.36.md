# Knit 0.1.0-alpha.36

Landing records retain the review URLs from successful merges, so a completed
landing passes bundle validation. Branch-only landings validate without invented
review URLs. Deployment-only plans record completion without claiming a source
merge. Finalization and resume preserve extension metadata. `knit migrate` repairs
older incomplete landing records from existing landing evidence without rerunning
merges or deployments.

Homebrew release delivery now has an explicit manual or automatic publication
mode. Both verify the published bottles and formula, and wait for the tap's live
formula to match the verified release before reporting completion. A verified
handoff supports publishing through Knit without storing a personal token in
release automation. Repeated release jobs preserve already published binary
archives and checksums.

This release retains per-repository draft publishing and the fix that keeps
outdated landing plans from blocking bundle and history synchronization.
