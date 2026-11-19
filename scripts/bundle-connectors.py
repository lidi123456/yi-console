#!/usr/bin/env python3
"""Build the WASM connectors that ship with the desktop app and put them into app/src-tauri/resources/connectors/<id>/.

Needs the wasm32-wasip2 Rust target (rustup target add wasm32-wasip2). Run from anywhere:

    python3 scripts/bundle-connectors.py

The app loads these folders in place at startup (see docs/internals/bots.md, "Connectors").
"""

import shutil
import subprocess
import sys
from pathlib import Path

# Connector folder under connectors/ -> crate name
BUNDLED = {"github": "yi-connector-github", "feishu": "yi-connector-feishu"}

root = Path(__file__).resolve().parent.parent
plugins = root / "connectors"
out = root / "app" / "src-tauri" / "resources" / "connectors"


def run(*cmd):
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=plugins, check=True)


def main():
    crates = [x for c in BUNDLED.values() for x in ("-p", c)]
    run("cargo", "build", "--release", "--target", "wasm32-wasip2", *crates)
    run("cargo", "run", "-q", "-p", "yi-connector-pack", "--", *BUNDLED)
    out.mkdir(parents=True, exist_ok=True)
    for entry in out.iterdir():
        if entry.is_dir():
            shutil.rmtree(entry)
    for connector_id in BUNDLED:
        shutil.copytree(plugins / "dist" / connector_id, out / connector_id)
        print("bundled", out / connector_id)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        sys.exit(e.returncode)
