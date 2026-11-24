#!/bin/bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
source_root=${1:-$root}
probe="$root/repros/windows/static-import-probe"
rm -rf "$probe"
mkdir -p "$probe"

(
    cd "$source_root"
    CARGO_BUILD_RUSTC_WRAPPER='' CARGO_TARGET_DIR="$probe/target" cargo rustc --locked -p fsk \
        --target x86_64-pc-windows-msvc --lib -- \
        --emit=obj -o "$probe/fsk.o"
)

object=$(find "$probe" -maxdepth 1 -type f -name 'fsk*.o' -print -quit)
if [[ -z "$object" ]]; then
    echo "FAIL: rustc did not emit a COFF object" >&2
    exit 1
fi

imports=$(strings "$object" | grep '^__imp_Prj' | sort -u || true)
if [[ -n "$imports" ]]; then
    echo "FAIL: fsk has eager ProjectedFSLib.dll imports:"
    printf '%s\n' "$imports"
    echo "A process loading fsk therefore requires ProjectedFSLib.dll before authorization_status can report NeedsApproval."
    exit 1
fi

echo "PASS: no eager Prj* imports remain in the fsk COFF object"
