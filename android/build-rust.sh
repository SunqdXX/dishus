#!/usr/bin/env bash
# Build the Rust core for Android and generate the Kotlin bindings.
# Needs: rustup targets aarch64-linux-android x86_64-linux-android, cargo-ndk, ANDROID_NDK_HOME.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/.."
cd "$root"
cargo ndk -t arm64-v8a -t x86_64 --platform 28 -o "$here/app/src/main/jniLibs" \
    build --profile release-android -p dishus-core --features ffi --lib
# cargo-ndk copies every .so it finds; some dependencies also emit cdylibs we don't load.
find "$here/app/src/main/jniLibs" -name '*.so' ! -name 'libdishus_core.so' -delete
# Bindings are generated from a host *debug* build of the same crate (library mode): the
# release profile strips symbols, which removes the UniFFI metadata bindgen reads.
cargo build -p dishus-core --features ffi --lib
cargo run -p dishus-core --features bindgen-cli --bin uniffi-bindgen -- \
    generate --library "$root/target/debug/libdishus_core.so" --language kotlin \
    --config "$here/uniffi.toml" \
    --out-dir "$here/app/src/main/java" --no-format
echo "Rust core + Kotlin bindings ready."
