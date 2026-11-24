#!/bin/bash
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${FSK_AUDIT_ROOT:-$(dirname "$script_dir")}"
python3 "$root/repros/generate_readme_authorization.py"
crate="$root/target/repros/readme-authorization"
export CARGO_TARGET_DIR="$crate/target"
target="${1:-x86_64-unknown-linux-gnu}"
cargo check --manifest-path "$crate/Cargo.toml" --offline --target "$target"
