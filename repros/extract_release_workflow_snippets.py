#!/usr/bin/env python3
"""Extract release workflow shell and prepare deterministic gate fixtures."""
import plistlib
import re
import shutil
import textwrap
from pathlib import Path

root = Path(__file__).resolve().parents[1]
output = root / "target" / "repros" / "release-version"
output.mkdir(parents=True, exist_ok=True)
workflow = (root / ".github/workflows/release.yml").read_text()
release_start = workflow.index('          version="${GITHUB_REF_NAME#v}"')
release_end = workflow.index("          if grep -q", release_start)
release = textwrap.dedent(workflow[release_start:release_end])
(output / "release-notes-current.sh").write_text("#!/bin/bash\nset -euo pipefail\n" + release)

publish_start = workflow.index('          version_pattern="$(printf', workflow.index("  publish-crate:"))
publish_end_line = workflow.index("\n", workflow.index("          grep -Eq", publish_start)) + 1
publish = textwrap.dedent(workflow[publish_start:publish_end_line])
(output / "publish-version-current.sh").write_text("#!/bin/bash\nset -euo pipefail\n" + publish)

export_step = workflow.index("      - name: Verify exported application")
run_marker = "        run: |\n"
export_start = workflow.index(run_marker, export_step) + len(run_marker)
export_end = workflow.index("\n      - name:", export_start)
export = textwrap.dedent(workflow[export_start:export_end])
(output / "exported-application-current.sh").write_text("#!/bin/bash\n" + export)

protocol_source = (root / "fsk/macos/rust/ipc.rs").read_text()


def protocol_constant(name):
    match = re.search(
        rf"^pub const {name}: u[0-9]+ = ([0-9]+);$", protocol_source, re.MULTILINE
    )
    if match is None:
        raise RuntimeError(f"missing {name} in fsk/macos/rust/ipc.rs")
    return int(match.group(1))


app = output / "artifacts" / "export" / "FSK.app"
shutil.rmtree(app, ignore_errors=True)
extension = app / "Contents" / "Extensions" / "FSKExtension.appex"
(app / "Contents").mkdir(parents=True, exist_ok=True)
(extension / "Contents").mkdir(parents=True, exist_ok=True)
app_info = {
    "CFBundleShortVersionString": "0.2.0",
    "CFBundleVersion": "0",
}
extension_info = {
    **app_info,
    "FSKProtocolMajor": protocol_constant("PROTOCOL_MAJOR"),
    "FSKProtocolMinor": protocol_constant("PROTOCOL_MINOR"),
}
for info_path, info in (
    (app / "Contents" / "Info.plist", app_info),
    (extension / "Contents" / "Info.plist", extension_info),
):
    with info_path.open("wb") as file:
        plistlib.dump(info, file, sort_keys=False)

bin_dir = output / "bin"
bin_dir.mkdir(exist_ok=True)
codesign = bin_dir / "codesign"
codesign.write_text(
    """#!/bin/bash
set -euo pipefail
case "$1" in
    --verify)
        exit 0
        ;;
    --display)
        printf '%s\n' 'Identifier=dev.specta.fsk' 'TeamIdentifier=68994Z5PT3'
        ;;
    -d)
        cat <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.developer.fskit.fsmodule</key>
    <true/>
</dict>
</plist>
PLIST
        ;;
    *)
        echo "unexpected codesign invocation: $*" >&2
        exit 2
        ;;
esac
"""
)
codesign.chmod(0o755)
