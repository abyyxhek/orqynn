"""Regenerate crates/director-app/tests/readme_example.rs from README.md.

The README's Code Example must compile and run against the real crate API.
This script extracts the ```rust block, renames main(), and wraps it in a
tokio test so `cargo test` fails if the README drifts from the code.
"""
import re
import sys
from pathlib import Path

root = Path(__file__).resolve().parent.parent
readme = (root / "README.md").read_text(encoding="utf-8")
m = re.search(r"## Code Example.*?```rust\n(.*?)```", readme, re.S)
if not m:
    sys.exit("no Code Example rust block found in README.md")

lines = m.group(1).strip("\n").split("\n")
mi = next(i for i, l in enumerate(lines) if l.strip() == "#[tokio::main]")
uses = [l for l in lines[:mi] if l.strip()]
body = "\n".join(lines[mi + 2 :]).rstrip()
assert body.endswith("}"), "example body does not end with main's closing brace"
body = body[:-1].rstrip()

DQ = '"'
HEADER = (
    "// AUTO-GENERATED from README.md's Code Example section by\n"
    "// tools/sync_readme_example.py. If this fails to compile or run, the\n"
    "// README's example has drifted from the real API.\n\n"
    + "\n".join(uses)
    + "\n\n"
    + "async fn readme_example_main() -> Result<(), Box<dyn std::error::Error>> {\n"
)
FOOTER = (
    "\n}\n\n"
    "#[tokio::test]\n"
    "async fn readme_example_compiles_and_runs() {\n"
    "    readme_example_main().await.unwrap();\n"
    "}\n"
)

out = root / "crates" / "director-app" / "tests" / "readme_example.rs"
out.write_text(HEADER + body + FOOTER, encoding="utf-8")
print(f"regenerated {out} ({len(uses)} use statements, {len(body.splitlines())} body lines)")
