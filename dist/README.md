# Release Distribution

Templates for publishing knit to package managers. The canonical release flow:

## Release flow

```sh
# 0. Everything you want in the release is landed on main; local main is current.

# 1. Bump `version` in Cargo.toml (package `knit-cli`), land that change.

# 2. Tag and push — triggers .github/workflows/release.yml, which builds
#    macOS (x64/arm64), Linux (x64/arm64 musl), and Windows (x64) binaries
#    and uploads them (plus .sha256 files) to a GitHub release. A tag
#    containing `-` (e.g. v0.1.0-alpha.2) is marked as a pre-release.
git tag v0.1.0-alpha.2
git push origin v0.1.0-alpha.2

# 3. Wait for the Release workflow to finish:
gh run watch --repo marc-merino/knit "$(gh run list --repo marc-merino/knit --workflow Release --limit 1 --json databaseId -q '.[0].databaseId')"

# 4. Publish to crates.io (the crate is `knit-cli`; the installed binary stays `knit`):
cargo publish

# 5. Update the Homebrew tap (see below).
```

## Homebrew tap (`marc-merino/homebrew-knit`)

Users install with `brew tap marc-merino/knit && brew install knit`. To release:

```sh
# Fill the formula from the release assets:
#   - bump the `version` stanza in homebrew/knit.rb (URLs derive from it)
#   - replace each sha256 with the matching .sha256 asset, e.g.:
gh release view v0.1.0-alpha.2 --repo marc-merino/knit --json assets -q '.assets[].name'
curl -sL https://github.com/marc-merino/knit/releases/download/v0.1.0-alpha.2/knit-v0.1.0-alpha.2-aarch64-apple-darwin.sha256

# Copy homebrew/knit.rb into the tap as Formula/knit.rb, commit, push:
#   github.com/marc-merino/homebrew-knit
```

## Where each manifest goes

| File | Destination | How |
|---|---|---|
| `homebrew/knit.rb` | `marc-merino/homebrew-knit` repo as `Formula/knit.rb` | Push to the tap repo, users run `brew tap marc-merino/knit && brew install knit` |
| `scoop/knit.json` | `marc-merino/scoop-knit` repo as `bucket/knit.json` | Push to the bucket repo, users run `scoop bucket add marc-merino/knit <url> && scoop install knit` |
| `winget/marc-merino.knit.yaml` | PR to `microsoft/winget-pkgs` as `manifests/m/marc-merino/knit/<version>/marc-merino.knit.yaml` | Submit PR, Microsoft reviews and merges |

## Updating versions

1. Bump `version` in `Cargo.toml`, land it
2. Tag `v<version>` and push; wait for the Release workflow
3. `cargo publish`
4. Homebrew: bump the formula `version` stanza, refresh the four sha256s, push to the tap
5. Scoop: bump version + hash (`autoupdate` handles URLs)
6. Winget: submit a new manifest for the new version
