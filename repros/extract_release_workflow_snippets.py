#!/usr/bin/env python3
"""Extract and execute the release workflow's actual version-matching shell."""
from pathlib import Path
import textwrap

root = Path(__file__).resolve().parents[1]
workflow = (root / ".github/workflows/release.yml").read_text()
release_start = workflow.index('          version="${GITHUB_REF_NAME#v}"')
release_end = workflow.index("          if grep -q", release_start)
release = textwrap.dedent(workflow[release_start:release_end])
(root / "repros/release-notes-current.sh").write_text("#!/bin/bash\nset -euo pipefail\n" + release)

publish_start = workflow.index('          version_pattern="$(printf', workflow.index("  publish-crate:"))
publish_end_line = workflow.index("\n", workflow.index("          grep -Eq", publish_start)) + 1
publish = textwrap.dedent(workflow[publish_start:publish_end_line])
(root / "repros/publish-version-current.sh").write_text("#!/bin/bash\nset -euo pipefail\n" + publish)
