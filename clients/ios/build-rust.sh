#!/usr/bin/env bash
# Builds crates/client-core for iOS and packages StreetlightCore.xcframework.
# Run on macOS with Xcode + rustup:  rustup target add aarch64-apple-ios aarch64-apple-ios-sim
set -euo pipefail
cd "$(dirname "$0")"
ROOT=../..
export IPHONEOS_DEPLOYMENT_TARGET=17.0
for t in aarch64-apple-ios aarch64-apple-ios-sim; do
  cargo build --manifest-path "$ROOT/Cargo.toml" -p streetlight-client-core --release --target "$t"
done
rm -rf StreetlightCore.xcframework
xcodebuild -create-xcframework \
  -library "$ROOT/target/aarch64-apple-ios/release/libstreetlight_client_core.a" -headers include \
  -library "$ROOT/target/aarch64-apple-ios-sim/release/libstreetlight_client_core.a" -headers include \
  -output StreetlightCore.xcframework
echo "built clients/ios/StreetlightCore.xcframework"
