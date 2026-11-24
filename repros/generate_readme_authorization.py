#!/usr/bin/env python3
"""Extract the README's cross-platform authorization example for compilation."""
from pathlib import Path
import shutil

root = Path(__file__).resolve().parents[1]
readme = (root / "README.md").read_text()
section = readme.split("## Mount prerequisites", 1)[1]
block = section.split("```rust" + chr(10), 1)[1].split(chr(10) + "```", 1)[0]
visible = chr(10).join(
    line for line in block.splitlines()
    if line != "#" and not line.startswith("# ")
)
crate = root / "target" / "repros" / "readme-authorization"
(crate / "src").mkdir(parents=True, exist_ok=True)
(crate / "Cargo.toml").write_text(
    '[package]\n'
    'name = "fsk-audit-readme-authorization"\n'
    'version = "0.0.0"\n'
    'edition = "2024"\n\n'
    '[dependencies]\n'
    'fsk = { path = "../../../fsk" }\n\n'
    '[workspace]\n'
)
shutil.copyfile(root / "Cargo.lock", crate / "Cargo.lock")
(crate / "src/main.rs").write_text(
    "fn main() -> std::io::Result<()> {\n" + visible + "\n    Ok(())\n}\n"
)
print(crate / "src/main.rs")
