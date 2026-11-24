#!/bin/bash
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${FSK_AUDIT_ROOT:-$(dirname "$script_dir")}"
versions=("$@")
if (( ${#versions[@]} == 0 )); then
    versions=(0.2.0 0.2.0+audit)
fi
python3 "$root/repros/extract_release_workflow_snippets.py"
snippets="$root/target/repros/release-version"
fixture="$root/repros/release-notes-fixture"
export PATH="$snippets/bin:$PATH"
failed=0

for version in "${versions[@]}"; do
    case "$version" in
        0.2.0) expected="- older release" ;;
        0.2.0+audit) expected="- curated build-metadata notes" ;;
        *) echo "unsupported fixture version: $version" >&2; exit 2 ;;
    esac

    echo "version: $version"
    export GITHUB_REF_NAME="v$version"
    export RUNNER_TEMP="$snippets/artifacts"
    mkdir -p "$RUNNER_TEMP"

    (
        cd "$fixture"
        bash "$snippets/release-notes-current.sh"
    )
    if grep -Fq -- "$expected" "$RUNNER_TEMP/release-notes.md"; then
        echo "release notes: PASS"
    else
        echo "release notes: FAIL (expected section was not selected)"
        echo "release-notes.md contents:"
        sed -n 'l' "$RUNNER_TEMP/release-notes.md"
        failed=1
    fi

    if (
        cd "$fixture"
        crate_version="$version" bash "$snippets/publish-version-current.sh"
    ); then
        echo "publish heading check: PASS"
    else
        echo "publish heading check: FAIL"
        failed=1
    fi

    if (
        cd "$root"
        bash "$snippets/exported-application-current.sh"
    ); then
        echo "exported application: PASS"
    else
        echo "exported application: FAIL"
        failed=1
    fi
done
exit "$failed"
