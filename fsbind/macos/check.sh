#!/bin/sh
set -eu

macos_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$macos_root/../.." && pwd)
cd "$repo_root"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cd "$macos_root"
xcodegen generate

check_derived=$(mktemp -d /tmp/fsbind-check.XXXXXX)
cleanup() {
  case "$check_derived" in
    /tmp/fsbind-check.*) rm -rf -- "$check_derived" ;;
  esac
}
trap cleanup EXIT HUP INT TERM

xcodebuild \
  -project FSBind.xcodeproj \
  -scheme FSBindApp \
  -configuration Debug \
  -derivedDataPath "$check_derived" \
  CODE_SIGNING_ALLOWED=NO \
  test
