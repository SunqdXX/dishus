#!/usr/bin/env bash
# Build release artifacts into dist/:
#   dishus-<ver>-linux-x86_64.tar.gz   PC daemon + CLI + installer
#   DiShus-<ver>.apk                   Android app, signed with the release key
#   SHA256SUMS
# Needs: the Android toolchain (see README "Building the app") and
# DISHUS_KEYSTORE_PROPERTIES pointing at the release signing key (kept outside the repo).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
: "${DISHUS_KEYSTORE_PROPERTIES:?set DISHUS_KEYSTORE_PROPERTIES to the release keystore properties file}"
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean; commit first"; exit 1; }
dist="$root/dist"; rm -rf "$dist"; mkdir -p "$dist"

# --- PC ---
cargo build --release --locked -p dishus-daemon -p dishus-cli
pc="dishus-$ver-linux-x86_64"; stage="$dist/$pc"
mkdir -p "$stage/bin"
install -m755 target/release/dishus-daemon target/release/dishus "$stage/bin/"
install -m755 packaging/install.sh "$stage/install.sh"
install -m755 packaging/hyprland/dishus-share-picker "$stage/bin/"
install -m644 packaging/systemd/dishus.service packaging/udev/70-dishus-uinput.rules \
  README.md LICENSE-MIT LICENSE-APACHE "$stage/"
tar -C "$dist" -czf "$dist/$pc.tar.gz" "$pc"
rm -rf "$stage"

# --- Android ---
./android/build-rust.sh
(cd android && gradle -q assembleRelease)
apk=android/app/build/outputs/apk/release/app-release.apk
"$(ls "$ANDROID_HOME"/build-tools/*/apksigner | tail -1)" verify "$apk"
cp "$apk" "$dist/DiShus-$ver.apk"

(cd "$dist" && sha256sum -- * > SHA256SUMS)
ls -la "$dist"
