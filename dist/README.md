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

# 3. Wait for the Release workflow to finish:
gh run watch --repo knit-cli/knit "$(gh run list --repo knit-cli/knit --workflow Release --limit 1 --json databaseId -q '.[0].databaseId')"

# 4. The Release workflow then calls .github/workflows/homebrew.yml, which
#    repackages the raw binaries into Homebrew bottles, publishes them plus
#    the complete generated formula to the same release, verifies the
#    published bytes, and opens the tap bump PR. Merge that PR.

# crates.io is deliberately not part of this flow: `knit-cli` there stops at
# the earliest alphas, and Homebrew plus source are the supported paths.
```

## Homebrew tap (`knit-cli/homebrew-tap`)

Users install with `brew install knit-cli/tap/knit` and get a real bottle
(`cellar: :any_skip_relocation`; macOS tagged `arm64_big_sur`/`big_sur`,
Linux tagged `arm64_linux`/`x86_64_linux`) instead of a source build that
triggers Xcode checks.

The formula is generated, never hand-edited. `scripts/homebrew_bottles.py`
reads the four raw release archives, verifies each `.sha256` sidecar before
extracting anything, and writes `bottles/*.bottle.tar.gz`, the complete tap
formula `knit.rb`, `SHA256SUMS`, and `manifest.json`.

```sh
# The workflow opens the formula PR automatically once HOMEBREW_TAP_TOKEN is
# configured with Contents:write and Pull-requests:write access to
# knit-cli/homebrew-tap. Without the token it FAILS (loudly, after the
# bottles are already published) so the tap can never silently stay stale.
#
# To backfill an already-published release without rebuilding or moving tags:
gh workflow run "Homebrew bottles" --repo knit-cli/knit -f release_tag=v0.1.0-alpha.21
#
# Fully manual fallback (same bytes the workflow produces):
gh release download v0.1.0-alpha.21 --repo knit-cli/knit \
  --pattern 'knit-v0.1.0-alpha.21-*.tar.gz' \
  --pattern 'knit-v0.1.0-alpha.21-*.sha256' --dir assets
python3 scripts/homebrew_bottles.py \
  --version 0.1.0-alpha.21 --assets-dir assets --output-dir homebrew-dist
gh release upload v0.1.0-alpha.21 --repo knit-cli/knit homebrew-dist/bottles/*.bottle.tar.gz
# Note: `gh release upload` rejects any existing same-name asset, even with
# identical bytes, and --clobber would defeat immutability — for verified
# idempotent re-publishing, re-run the workflow instead. Send
# homebrew-dist/knit.rb to the tap as Formula/knit.rb via a PR (never a
# direct push).
```

## Where each manifest goes

| File | Destination | How |
|---|---|---|
| generated `homebrew-dist/knit.rb` | `knit-cli/homebrew-tap` repo as `Formula/knit.rb` | Workflow PR (or the manual path above); users run `brew install knit-cli/tap/knit` |
| `scoop/knit.json` | `marc-merino/scoop-knit` repo as `bucket/knit.json` | Push to the bucket repo, users run `scoop bucket add marc-merino/knit <url> && scoop install knit` |
| `winget/marc-merino.knit.yaml` | PR to `microsoft/winget-pkgs` as `manifests/m/marc-merino/knit/<version>/marc-merino.knit.yaml` | Submit PR, Microsoft reviews and merges |

## Updating versions

1. Bump `version` in `Cargo.toml`, `crates/knit-runtime/Cargo.toml`, and the
   `knit-runtime` dependency entry; land it
2. Tag `v<version>` and push; wait for the Release workflow
3. Homebrew: merge the tap PR the workflow opened (bottles are already
   published and verified against the release)
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
