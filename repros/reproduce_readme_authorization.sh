#!/bin/bash
set -euo pipefail
root="${FSK_AUDIT_ROOT:-/private/var/folders/ft/fkt3bwqd01g7f54wvyz9wgjc0000gn/T/opencode/fsk-audit-core}"
python3 "$root/repros/generate_readme_authorization.py"
export HOME="$root/.home"
export CARGO_HOME=/Users/oscar/.cargo
export RUSTUP_HOME=/Users/oscar/.rustup
export CARGO_TARGET_DIR="$root/target"
target="${1:-x86_64-unknown-linux-gnu}"
cargo check --manifest-path "$root/repros/readme-authorization/Cargo.toml" --offline --target "$target"
