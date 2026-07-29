#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root/native"
cargo ndk -t arm64-v8a -p 31 build --release --bin tb376-ota-helper-native
install -Dm755 \
  target/aarch64-linux-android/release/tb376-ota-helper-native \
  "$repo_root/app/src/main/jniLibs/arm64-v8a/libtb376_ota_helper_native.so"

