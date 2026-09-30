# Knit 0.1.0-alpha.33

Fix Windows recovery-state persistence for `knit squash` and `knit rebase`.
Recovery files are flushed through a writable handle, preserving durable
resume/abort state without the `Access is denied` failure introduced in alpha.32.

This release includes the resumable landing gates, consumer updates, bundle
rewrites, and fork lease improvements from
[alpha.32](release-0.1.0-alpha.32.md).
