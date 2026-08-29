# Release Distribution

Templates for publishing knit to package managers. The canonical release flow:

## Release flow

```sh
# 0. Everything you want in the release is landed on main; local main is current.

# 1. Bump `version` in Cargo.toml (package `knit-cli`), land that change.

# 2. Tag and push — triggers .github/workflows/release.yml, which builds
#    macOS (x64/arm64), Linux (x64/arm64 musl), and Windows (x64) binaries
#    and uploads them (plus .sha256 files) to a GitHub release. A tag
#    containing `-` (e.g. v0.1.0-alpha.13) is marked as a pre-release.
git tag v0.1.0-alpha.13
git push origin v0.1.0-alpha.13

# 3. Wait for the Release workflow to finish:
gh run watch --repo knit-cli/knit "$(gh run list --repo knit-cli/knit --workflow Release --limit 1 --json databaseId -q '.[0].databaseId')"

# 4. Update the Homebrew tap (see below). If HOMEBREW_TAP_TOKEN is configured,
#    the release workflow opens this PR automatically after assets upload;
#    otherwise bump Formula/knit.rb by hand.

# crates.io is deliberately not part of this flow: `knit-cli` there stops at
# the earliest alphas, and Homebrew plus source are the supported paths.
```

## Homebrew tap (`knit-cli/homebrew-tap`)

Users install with `brew install knit-cli/tap/knit`. To release:

```sh
# The release workflow opens a formula bump PR automatically when the
# HOMEBREW_TAP_TOKEN secret is configured with Contents:write and
# Pull requests:write access to knit-cli/homebrew-tap.
#
# If that secret is not configured, fill the formula from the release assets:
#   - bump the `version` stanza in homebrew/knit.rb (URLs derive from it)
#   - replace each sha256 with the matching .sha256 asset, e.g.:
gh release view v0.1.0-alpha.13 --repo knit-cli/knit --json assets -q '.assets[].name'
curl -sL https://github.com/knit-cli/knit/releases/download/v0.1.0-alpha.13/knit-v0.1.0-alpha.13-aarch64-apple-darwin.sha256

# Copy homebrew/knit.rb into the tap as Formula/knit.rb, commit, push:
#   github.com/knit-cli/homebrew-tap
```

## Where each manifest goes

| File | Destination | How |
|---|---|---|
| `homebrew/knit.rb` | `knit-cli/homebrew-tap` repo as `Formula/knit.rb` | Push to the tap repo, users run `brew install knit-cli/tap/knit` |
| `scoop/knit.json` | `marc-merino/scoop-knit` repo as `bucket/knit.json` | Push to the bucket repo, users run `scoop bucket add marc-merino/knit <url> && scoop install knit` |
| `winget/marc-merino.knit.yaml` | PR to `microsoft/winget-pkgs` as `manifests/m/marc-merino/knit/<version>/marc-merino.knit.yaml` | Submit PR, Microsoft reviews and merges |

## Updating versions

1. Bump `version` in `Cargo.toml`, `crates/knit-runtime/Cargo.toml`, and the
   `knit-runtime` dependency entry; land it
2. Tag `v<version>` and push; wait for the Release workflow
3. Homebrew: merge the automatically opened tap PR, or manually bump the formula `version` stanza, refresh the four sha256s, and push to the tap
4. Scoop: bump version + hash (`autoupdate` handles URLs)
5. Winget: submit a new manifest for the new version

There is deliberately no `cargo publish` step; see the note in the release flow
above.
