# Knit 0.1.0-alpha.35

A landing plan generated before a bundle's latest commits no longer blocks
the bundle's sync. The sync remote refuses such a plan as a stale bundle
fingerprint; Knit now keeps it local, still syncs the bundle artifact and
history, and names the fix: `knit --bundle <bundle> land plan --force`. A
landing plan upload that fails for any other reason is reported as a warning
instead of as a skipped sync.
