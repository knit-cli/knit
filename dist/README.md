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
