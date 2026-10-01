# Homebrew release completion

The release preflight explicitly selects `TAP_PUBLISH_MODE: manual` and passes it
as `tap_publish_mode` to the Homebrew workflow. The standalone
Homebrew workflow and reusable workflow default to `automatic`. These are publishing
strategies, not different success criteria: **both require the live default-branch
`Formula/knit.rb` to equal the verified formula byte for byte**. An open PR, a matching
version string, or an uploaded handoff artifact does not complete the workflow.
Reusable callers expecting PR creation alone must adopt this completion contract.

Packaging verifies raw archive checksums, preserves existing release assets, and
re-downloads published bottles and the formula to verify their bytes. Only after
that verification does the workflow upload `verified-homebrew-tap`, containing
`knit.rb`, `manifest.json`, `SHA256SUMS`, and `verification.json`. The verification
receipt records the release tag, resolved release source SHA, workflow SHA,
repository, run ID/attempt and URL, and exact formula SHA-256. It is generated
only after the published-byte checks pass. The earlier `homebrew-bottles` artifact
is a packaging intermediate, not proof of successful published-byte verification.

In manual mode, download `verified-homebrew-tap` from the active release run, copy
its `knit.rb` unchanged to the tap's `Formula/knit.rb` in a Knit checkout, then publish
and land through Knit after tap CI passes. The workflow announces publication as
pending and polls the tap's default branch every 20 seconds for up to 30 minutes.
The formula comparison is read-only and uses the repository's `GITHUB_TOKEN` to
read the public tap through GitHub's contents API. No cross-repository write secret
is needed. Failure to observe exact bytes, including persistent read errors,
fails the run with the last observed reason. A final successful comparison records
publication as complete in the job summary.

In automatic mode, `HOMEBREW_TAP_TOKEN` needs Contents:write and Pull requests:write
on the tap. A dedicated App installation token can supply that credential; never
copy a personal token into Actions as a release workaround. The workflow checks
credential presence before uploading bottle/formula assets, creates the tap PR,
and then waits for its merge with the same completion gate. If the formula is
already byte-identical, no token or new PR is required. Missing credentials never
silently change automatic mode into manual mode. The tag release preflight checks automatic-mode credential presence before
`create-release`, because the reusable packager runs after raw assets exist.
It is deliberately stricter for a new release than the standalone recovery path.

The canonical operator sequence is in [Release Distribution](../dist/README.md).
Both Actions artifact uploads use overwrite on rerun, so a repeated run can
replace its packaging artifact and verified handoff after rechecking bytes. This
does not permit replacement of any GitHub release asset.

For timeout recovery, publish the verified formula and dispatch Homebrew bottles
again with the **same release tag** and the desired explicit mode. It repackages
existing binaries without recompiling them and refuses to replace any asset with
different bytes. A completed live tap therefore permits credential-free recovery
in either mode. No new version or tag is needed. Dispatch uses the workflow's
selected ref, so use the same packaging implementation to preserve deterministic
asset bytes. This is not a verification-only dispatch.
