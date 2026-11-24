#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DEVELOPER_DIR=$(/usr/bin/xcode-select -p)
SWIFTC="$DEVELOPER_DIR/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc"
SDK=$(/usr/bin/xcrun --sdk macosx --show-sdk-path)
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
  "$SWIFTC" -swift-version 6 -parse-as-library \
  -sdk "$SDK" -target arm64-apple-macos26.0 \
  -module-cache-path "$AUDIT/swift-cache" \
  -o "$AUDIT/bin/fsk-probes" \
  "$ROOT/fsk/macos/FSKExtension/RustBackend.swift" \
  "$ROOT/fsk/macos/FSKExtensionTests/AuditIPCServer.swift" \
  "$AUDIT/ProbeRunner.swift"

rc=0
for mode in readdir-name-bounds write-count-overflow readlink-capacity; do
  printf '\n=== %s ===\n' "$mode"
  set +e
  (
    exec /usr/bin/env -i \
      PATH=/usr/bin:/bin HOME="$AUDIT/home" TMPDIR="$AUDIT/tmp" \
      FSK_AUDIT_TMP="$AUDIT/tmp" LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 \
      /usr/bin/sandbox-exec -f "$AUDIT/test-sandbox.sb" \
      "$AUDIT/bin/fsk-probes" "$mode"
  )
  code=$?
  set -e
  printf 'exit=%s\n' "$code"
  if [ "$code" -ne 0 ]; then rc=1; fi
done
exit "$rc"
