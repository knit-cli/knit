#!/usr/bin/env bash
# Run only in a disposable Ubuntu container, as root.
set -euo pipefail
[[ $# == 1 ]] || { echo 'usage: linux_install_smoke.sh PACKAGE_DIR' >&2; exit 2; }
packages=$(realpath "$1")
scripts=$(cd "$(dirname "$0")" && pwd)
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends python3 gnupg dpkg-dev apt-utils ca-certificates
# Use the package manager's real ordering rules, not a test reimplementation.
dpkg --compare-versions 0.1.0~alpha.9 lt 0.1.0~alpha.22
dpkg --compare-versions 0.1.0~alpha.22 lt 0.1.0
root=$(mktemp -d)
chmod 755 "$root"
export GNUPGHOME="$root/gnupg"
mkdir -m 700 "$GNUPGHOME"
gpg --batch --passphrase '' --quick-generate-key 'Package smoke test <test@example.invalid>' rsa2048 sign 1d
key=$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10; exit }')
bash "$scripts/build_apt_repository.sh" "$packages" "$root/site" "$key"
printf 'deb [signed-by=%s] file:%s/apt ./\n' "$root/site/knit.gpg" "$root/site" > /etc/apt/sources.list.d/knit-test.list
apt-get update -qq
apt-get install -y -qq knit
knit --version
test "$(command -v knit)" = /usr/bin/knit
git --version
knit auth --help >/dev/null
# Prove the package is removable and a direct .deb install also resolves deps.
apt-get remove -y -qq knit
arch=$(dpkg --print-architecture)
package=$(find "$packages" -maxdepth 1 -name "*_${arch}.deb" -print -quit)
test -n "$package"
apt-get install -y -qq "$package"
knit --version
# A changed index must fail authentication, never fall back to trusting it.
printf '\nTampered: yes\n' >> "$root/site/apt/Packages"
rm -f /var/lib/apt/lists/*knit* /var/lib/apt/lists/*_Packages*
rm "$root/site/apt/Packages.gz"
if apt-get update -o APT::Update::Error-Mode=any >"$root/tamper.log" 2>&1; then
  cat "$root/tamper.log"
  echo 'Tampered repository was accepted' >&2
  exit 1
fi
grep -Eq 'Hash Sum mismatch|File has unexpected size' "$root/tamper.log" || { cat "$root/tamper.log"; exit 1; }
echo 'PASS: signed APT install, direct package install, removal, and tamper rejection'
