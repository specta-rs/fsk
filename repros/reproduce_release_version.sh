#!/bin/bash
set -euo pipefail
root="${FSK_AUDIT_ROOT:-/private/var/folders/ft/fkt3bwqd01g7f54wvyz9wgjc0000gn/T/opencode/fsk-audit-core}"
version="${1:-0.2.0+audit}"
python3 "$root/repros/extract_release_workflow_snippets.py"
export GITHUB_REF_NAME="v$version"
export RUNNER_TEMP="$root/repros/artifacts/release-notes"
mkdir -p "$RUNNER_TEMP"
failed=0

sh "$root/repros/release-notes-current.sh"
if grep -Fq -- "- curated build-metadata notes" "$RUNNER_TEMP/release-notes.md"; then
    echo "release notes: PASS"
else
    echo "release notes: FAIL (curated section was not selected)"
    echo "release-notes.md contents:"
    sed -n 'l' "$RUNNER_TEMP/release-notes.md"
    failed=1
fi

if crate_version="$version" sh "$root/repros/publish-version-current.sh"; then
    echo "publish heading check: PASS"
else
    echo "publish heading check: FAIL"
    failed=1
fi
exit "$failed"
