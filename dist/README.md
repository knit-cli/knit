# Release Distribution

Templates for publishing knit to package managers. The canonical release flow:

## Release flow

```sh
# 0. Everything you want in the release is landed on main; local main is current.

# 1. Bump `version` in Cargo.toml (package `knit-cli`), land that change.

# 2. Tag and push — triggers .github/workflows/release.yml, which builds
#    macOS (x64/arm64; both pinned to MACOSX_DEPLOYMENT_TARGET=11.0),
#    Linux (x64/arm64 musl), and Windows (x64) binaries and uploads them
#    (plus .sha256 files) to a GitHub release. A tag containing `-`
#    (e.g. v0.1.0-alpha.14) is marked as a pre-release.
git tag v0.1.0-alpha.14
git push origin v0.1.0-alpha.14

# 3. Select the Release run for this tag and wait for its
#    verified-homebrew-tap artifact, NOT for the workflow to finish.
#    The release explicitly uses manual tap publication. Download that artifact,
#    copy knit.rb unchanged into the tap's Formula/knit.rb in a Knit checkout,
#    and publish + land the tap PR through Knit after tap CI passes.
#    Complete this while the release waits (30 minutes maximum).

# 4. Watch that same Release run to completion. It succeeds only after the live
#    tap's default-branch formula matches the verified bytes exactly and all
#    other release jobs pass. An open tap PR is not completion.
gh run watch --repo knit-cli/knit RELEASE_RUN_ID --exit-status

# crates.io is deliberately not part of this flow: `knit-cli` there stops at
# the earliest alphas, and Homebrew plus source are the supported paths.
```

## Immutable raw release assets

Release runs serialize per tag without cancelling an active run. Before building
any matrix target, `scripts/release_assets.py` downloads its existing archive and
checksum. A complete, checksum-valid pair is reused without rebuilding. An
inconsistent complete pair fails immediately.

The Rust binary action runs in its supported `dry-run` mode (build and compress
only). Its normal upload path uses `--clobber` and is deliberately not used.
Our publisher validates the local archive/checksum pair, compares every existing
member's SHA-256 before uploading anything, and uploads only missing members,
without overwrite. It downloads the completed pair again to verify exact bytes.
A competing upload fails safely rather than replacing another publisher's bytes.

An interrupted upload may leave a partial pair. Recovery accepts only the same
bytes for every already-published member; if a rebuild differs (including archive
metadata), the run fails without replacing assets. Retry publication with the
original archive/checksum from the failing attempt's
`raw-release-<target>-<attempt>` Actions artifact using
`scripts/release_assets.py publish --repo knit-cli/knit --tag <tag> --target <target> --directory <download-directory>`, or
investigate the partial state; do not delete assets, use `--clobber`, or move the
tag to make a rerun green. Completed targets are never rebuilt on rerun.

The action's [official metadata](https://github.com/taiki-e/upload-rust-binary-action/blob/v1/action.yml)
documents build-only `dry-run`; its [upload implementation](https://github.com/taiki-e/upload-rust-binary-action/blob/v1/main.sh)
explains why publication is handled separately.

## Homebrew tap (`knit-cli/homebrew-tap`)

Users install with `brew install knit-cli/tap/knit` and get a real bottle
(`cellar: :any_skip_relocation`; macOS tagged `arm64_big_sur`/`big_sur`,
Linux tagged `arm64_linux`/`x86_64_linux`) instead of a source build that
triggers Xcode checks.

The formula is generated, never hand-edited. `scripts/homebrew_bottles.py`
reads the four raw release archives, verifies each `.sha256` sidecar before
extracting anything, and writes `bottles/*.bottle.tar.gz`, the complete tap
formula `knit.rb`, `SHA256SUMS`, and `manifest.json`.

The tag release explicitly selects manual tap publication; no cross-repository
write credential is needed. While the workflow is waiting, download the verified
handoff from the exact release run:

```sh
gh run download RELEASE_RUN_ID --repo knit-cli/knit \
  --name verified-homebrew-tap --dir verified-homebrew-tap
```

Check `verification.json` for the intended release tag, source SHA, run ID/attempt,
and formula SHA-256. Copy `knit.rb` unchanged to `Formula/knit.rb` in the tap's Knit
checkout, publish the PR through Knit, and land after tap CI passes. Do this
**before waiting for the release to finish**: the workflow polls for the exact
live formula every 20 seconds and fails after 30 minutes without a match.

Standalone/backfill calls default to automatic mode: they require
`HOMEBREW_TAP_TOKEN` before uploading bottles unless the exact formula is already
live. Automatic mode opens a PR and still waits for its merge; it never silently
falls back to manual. The tag release also checks credentials before creating
release assets if its explicit policy is changed to automatic.

To backfill or recover a timeout without rebuilding binaries or moving tags,
select manual publication explicitly and follow the same handoff/merge sequence:

```sh
gh workflow run "Homebrew bottles" --repo knit-cli/knit \
  -f release_tag=v0.1.0-alpha.21 -f tap_publish_mode=manual
```

Use the same packaging implementation when recovering an existing release.
Reruns replace only the run's Actions artifacts; existing GitHub release assets
must have identical bytes and are never clobbered. A completed live tap can be
verified again without a publishing token. See [Homebrew release completion](../docs/homebrew-packaging.md)
for mode contracts, handoff metadata, and recovery details.

## Where each manifest goes

| File | Destination | How |
|---|---|---|
| generated `homebrew-dist/knit.rb` | `knit-cli/homebrew-tap` repo as `Formula/knit.rb` | Verified handoff via Knit PR (automatic mode opens a PR); users run `brew install knit-cli/tap/knit` |
| `scoop/knit.json` | `marc-merino/scoop-knit` repo as `bucket/knit.json` | Push to the bucket repo, users run `scoop bucket add marc-merino/knit <url> && scoop install knit` |
| `winget/marc-merino.knit.yaml` | PR to `microsoft/winget-pkgs` as `manifests/m/marc-merino/knit/<version>/marc-merino.knit.yaml` | Submit PR, Microsoft reviews and merges |

## Updating versions

1. Bump `version` in `Cargo.toml`, `crates/knit-runtime/Cargo.toml`, and the
   `knit-runtime` dependency entry; land it
2. Tag `v<version>` and push; wait for the verified handoff artifact
3. Homebrew: publish and land the verified tap formula through Knit while the
   Release workflow waits; then confirm the full Release run succeeds
4. Scoop: bump version + hash (`autoupdate` handles URLs)
5. Winget: submit a new manifest for the new version

There is deliberately no `cargo publish` step; see the note in the release flow
above.

## Linux packages and APT

`.github/workflows/linux-packages.yml` repackages the verified Linux release
binaries into `.deb` and `.rpm` files using nFPM 2.47.0. No Rust build is needed.
The packages install `/usr/bin/knit` and depend on Git 2.31+ and CA certificates.
Prerelease versions sort before their eventual stable versions. Both amd64 and
arm64 installs are tested through signed APT on Ubuntu 22.04 and 24.04 before
publication. A changed repository index must fail authentication.

The release workflow calls this automatically after the binaries are uploaded.
To backfill an existing release after this workflow is on the default branch:

```sh
gh workflow run linux-packages.yml --repo knit-cli/knit -f release_tag=v0.1.0-alpha.22
```

Each run uploads immutable `.deb`, `.rpm`, and Linux-specific checksum assets
to the existing GitHub release. Repeating a run accepts identical bytes and
refuses replacements. It then adds packages to the existing `gh-pages` branch,
regenerates and signs APT metadata, pushes without force, and explicitly requests
a Pages build. Old packages remain available; backfilling an older version does
not downgrade the latest version APT selects. Concurrent publications serialize.
GitHub Pages serves `https://knit-cli.github.io/knit/`; this branch is reserved
for the package site. RPM files support local `dnf install`; no DNF repository
is configured by this workflow.

### One-time maintainer setup

1. Generate a dedicated OpenPGP signing key in a private directory. Keep an
   offline backup. Use an unencrypted automation key; GitHub encrypts the secret
   at rest and only the publication job imports it into an ephemeral keyring.
2. Save its armored private export as the repository Actions secret
   `APT_SIGNING_KEY`. Never commit this export or place it in a release artifact.
3. Bootstrap `gh-pages` with the signed site generated below, commit and push it,
   and enable GitHub Pages using **Deploy from a branch → gh-pages → / (root)**.
   The workflow's `GITHUB_TOKEN` needs `contents: write` and `pages: write`.
4. Confirm the live signing key and install using the commands in the root README.

The initial public signing-key fingerprint is
`873E5910AB55CBA214E5F7F2258C3BB163EC0B9A` (expires September 21, 2029).
The builder refuses accidental signing-key changes once a public key exists.
Key rotation requires a deliberate client migration, not replacing the secret.
GitHub Pages is dedicated to packages for this repository; do not point the
publisher at an unrelated documentation site.

### Build and verify locally

Install nFPM 2.47.0, Python 3, `dpkg-dev`, `apt-utils`, and GnuPG on Linux.
Download the four Linux musl archives/checksum sidecars from the chosen release:

```sh
gh release download v0.1.0-alpha.22 --repo knit-cli/knit --dir assets \
  --pattern '*unknown-linux-musl.tar.gz' --pattern '*unknown-linux-musl.sha256'
python3 scripts/linux_packages.py --version v0.1.0-alpha.22 --assets-dir assets --output-dir linux-dist
python3 -m unittest discover -s scripts -p '*_test.py' -v
docker run --rm -v "$PWD:/src:ro" ubuntu:22.04 \
  bash /src/scripts/linux_install_smoke.sh /src/linux-dist
```

To prepare the public site with your existing signing key:

```sh
bash scripts/build_apt_repository.sh linux-dist site YOUR_FULL_SIGNING_FINGERPRINT
cp dist/linux-index.html site/index.html
```

`GNUPGHOME` must point to the private signing keyring. `site` may be a checkout
of the existing `gh-pages` branch, which retains previously published packages.
The smoke script generates a disposable test key and modifies APT configuration:
run it only in a disposable container, never directly on your workstation.
