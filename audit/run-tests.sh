#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DEVELOPER_DIR=$(/usr/bin/xcode-select -p)
SWIFTC="$DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc"
SDK=$(/usr/bin/xcrun --sdk macosx --show-sdk-path)
DEVFRAMEWORKS="$DEVELOPER_DIR/Platforms/MacOSX.platform/Developer/Library/Frameworks"
PLUGINS="$DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/host/plugins/testing"
AUDIT="$ROOT/audit"
mkdir -p "$AUDIT/bin" "$AUDIT/home" "$AUDIT/tmp" "$AUDIT/swift-cache" "$AUDIT/logs"
ALIAS=$(printf '%s' "$ROOT" | sed 's#^/private##')
cat > "$AUDIT/test-sandbox.sb" <<EOF
(version 1)
(allow default)
(deny network*)
(allow network-outbound (subpath "$ROOT/audit/tmp"))
(allow network-outbound (subpath "$ALIAS/audit/tmp"))
(allow network-inbound (subpath "$ROOT/audit/tmp"))
(allow network-inbound (subpath "$ALIAS/audit/tmp"))
(deny file-write*)
(allow file-write* (subpath "$ROOT"))
(allow file-write* (subpath "$ALIAS"))
(allow file-write-data (literal "/dev/null"))
EOF

/usr/bin/env -i \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin \
  HOME="$AUDIT/home" TMPDIR="$AUDIT/tmp" \
  LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 \
  DEVELOPER_DIR="$DEVELOPER_DIR" \
  CLANG_MODULE_CACHE_PATH="$AUDIT/swift-cache" \
  SWIFTPM_MODULECACHE_PATH="$AUDIT/swift-cache" \
  /usr/bin/sandbox-exec -f "$AUDIT/test-sandbox.sb" \
  "$SWIFTC" -swift-version 6 -D DEBUG -parse-as-library \
  -sdk "$SDK" -target arm64-apple-macos26.0 \
  -F "$DEVFRAMEWORKS" -plugin-path "$PLUGINS" -framework Testing \
  -Xlinker -rpath -Xlinker "$DEVFRAMEWORKS" \
  -module-cache-path "$AUDIT/swift-cache" \
  -o "$AUDIT/bin/fsk-tests" \
  "$ROOT/fsk/macos/FSKExtension/RustBackend.swift" \
  "$ROOT/fsk/macos/FSKExtension/FSKItem.swift" \
  "$ROOT/fsk/macos/FSKExtension/FSKVolume.swift" \
  "$ROOT/fsk/macos/FSKExtension/FSKFileSystem.swift" \
  "$ROOT/fsk/macos/FSKExtensionTests/RustBackendTests.swift" \
  "$ROOT/fsk/macos/FSKExtensionTests/AuditIPCServer.swift" \
  "$ROOT/fsk/macos/FSKExtensionTests/AuditRegressionTests.swift" \
  "$AUDIT/TestRunner.swift"

/usr/bin/env -i \
  PATH=/usr/bin:/bin HOME="$AUDIT/home" TMPDIR="$AUDIT/tmp" FSK_AUDIT_TMP="$AUDIT/tmp" \
  LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 \
  /usr/bin/sandbox-exec -f "$AUDIT/test-sandbox.sb" \
  "$AUDIT/bin/fsk-tests" "$@"
