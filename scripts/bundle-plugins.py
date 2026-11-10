#!/usr/bin/env python3
"""Build the WASM plugins that ship with the desktop app and put them into app/src-tauri/resources/plugins/<id>/.

Needs the wasm32-wasip2 Rust target (rustup target add wasm32-wasip2). Run from anywhere:

    python3 scripts/bundle-plugins.py

The app loads these folders in place at startup (see docs/internals/bots.md, "Plugins").
"""

import shutil
import subprocess
import sys
from pathlib import Path

# Plugin folder under plugins/ -> crate name
BUNDLED = {"github": "yi-plugin-github", "feishu": "yi-plugin-feishu"}

root = Path(__file__).resolve().parent.parent
plugins = root / "plugins"
out = root / "app" / "src-tauri" / "resources" / "plugins"


def run(*cmd):
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=plugins, check=True)


def main():
    crates = [x for c in BUNDLED.values() for x in ("-p", c)]
    run("cargo", "build", "--release", "--target", "wasm32-wasip2", *crates)
    run("cargo", "run", "-q", "-p", "yi-plugin-pack", "--", *BUNDLED)
    out.mkdir(parents=True, exist_ok=True)
    for entry in out.iterdir():
        if entry.is_dir():
            shutil.rmtree(entry)
    for plugin_id in BUNDLED:
        shutil.copytree(plugins / "dist" / plugin_id, out / plugin_id)
        print("bundled", out / plugin_id)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        sys.exit(e.returncode)
