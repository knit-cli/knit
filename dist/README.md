# Release Distribution

Templates for publishing knit to package managers. After a GitHub release, fill in the `REPLACE_WITH_SHA256` placeholders with the actual checksums from the release assets.

## Release flow

```sh
# 1. Tag and push — triggers .github/workflows/release.yml
git tag v0.1.0
git push origin v0.1.0

# 2. Wait for the release workflow to build and upload binaries

# 3. Publish to crates.io (the crate is `knit-cli`; the installed binary stays `knit`)
cargo publish

# 4. Copy SHA256 checksums from the release assets into the manifests below

# 5. Update each package manager (see below)
```

## Where each manifest goes

| File | Destination | How |
|---|---|---|
| `homebrew/knit.rb` | `marc-merino/homebrew-knit` repo as `Formula/knit.rb` | Push to the tap repo, users run `brew tap marc-merino/knit && brew install knit` |
| `scoop/knit.json` | `marc-merino/scoop-knit` repo as `bucket/knit.json` | Push to the bucket repo, users run `scoop bucket add marc-merino/knit <url> && scoop install knit` |
| `winget/marc-merino.knit.yaml` | PR to `microsoft/winget-pkgs` as `manifests/m/marc-merino/knit/0.1.0/marc-merino.knit.yaml` | Submit PR, Microsoft reviews and merges |

## Updating versions

When releasing a new version:

1. Bump `version` in `Cargo.toml`
2. Tag and push
3. `cargo publish`
4. Update the Homebrew formula (URLs + sha256)
5. Update the Scoop manifest (version + hash, `autoupdate` handles URLs)
6. Submit a new winget manifest for the new version
