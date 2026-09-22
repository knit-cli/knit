#!/usr/bin/env bash
# Add release artifacts without silently replacing previously published bytes.
set -euo pipefail
[[ $# == 3 ]] || { echo 'usage: publish_linux_assets.sh REPO TAG PACKAGE_DIR' >&2; exit 2; }
repo=$1 tag=$2 dir=$3
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
shopt -s nullglob
files=("$dir"/*.deb "$dir"/*.rpm "$dir"/SHA256SUMS)
(( ${#files[@]} >= 5 )) || { echo 'Expected both architectures and checksums' >&2; exit 1; }
for file in "${files[@]}"; do
  name=$(basename "$file")
  # Separate from checksums belonging to other release packagers.
  [[ "$name" != SHA256SUMS ]] || name="knit-${tag}-linux-packages.sha256"
  if gh release view "$tag" --repo "$repo" --json assets | jq -e --arg name "$name" '.assets[] | select(.name == $name)' >/dev/null; then
    gh release download "$tag" --repo "$repo" --dir "$tmp" --pattern "$name"
    cmp "$file" "$tmp/$name" || { echo "Refusing to replace published $name" >&2; exit 1; }
  else
    cp "$file" "$tmp/$name"
    gh release upload "$tag" --repo "$repo" "$tmp/$name"
  fi
done
