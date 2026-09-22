#!/usr/bin/env bash
# Add immutable packages to a flat, signed APT repository.
set -euo pipefail
if [[ $# != 3 ]]; then
  echo 'usage: build_apt_repository.sh PACKAGE_DIR SITE_DIR SIGNING_FINGERPRINT' >&2
  exit 2
fi
packages=$(realpath "$1")
mkdir -p "$2/apt/pool"
site=$(realpath "$2")
key=$3
[[ "$key" =~ ^[A-Fa-f0-9]{40}$ ]] || { echo 'Expected a full signing fingerprint' >&2; exit 1; }
shopt -s nullglob
files=("$packages"/*.deb)
(( ${#files[@]} > 0 )) || { echo 'No Debian packages found' >&2; exit 1; }
for package in "${files[@]}"; do
  target="$site/apt/pool/$(basename "$package")"
  if [[ -e "$target" ]]; then
    cmp "$package" "$target" || { echo "Refusing to replace $target" >&2; exit 1; }
  else
    cp "$package" "$target"
  fi
done
# Refuse accidental key rotation: installed clients already trust this key.
gpg --batch --export "$key" > "$site/knit.gpg.new"
test -s "$site/knit.gpg.new"
if [[ -f "$site/knit.gpg" ]]; then
  old_key=$(gpg --batch --show-keys --with-colons "$site/knit.gpg" | awk -F: '$1 == "fpr" { print $10; exit }')
  [[ "${old_key^^}" == "${key^^}" ]] || { echo 'Repository signing key changed; rotate explicitly' >&2; exit 1; }
fi
mv "$site/knit.gpg.new" "$site/knit.gpg"
(
  cd "$site/apt"
  dpkg-scanpackages --multiversion pool /dev/null > Packages
  gzip -n -9 -c Packages > Packages.gz
  # Keep Date current; no short Valid-Until because releases can be infrequent.
  rm -f Release InRelease Release.gpg Release.new
  apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=Knit \
    -o APT::FTPArchive::Release::Label=Knit \
    -o APT::FTPArchive::Release::Architectures='amd64 arm64' \
    -o APT::FTPArchive::Release::Description='Knit Linux packages' \
    release . > Release.new
  mv Release.new Release
  gpg --batch --yes --local-user "$key" --digest-algo SHA256 --clearsign --output InRelease Release
  gpg --batch --yes --local-user "$key" --digest-algo SHA256 --armor --detach-sign --output Release.gpg Release
  gpgv --keyring "$site/knit.gpg" InRelease
)
touch "$site/.nojekyll"
