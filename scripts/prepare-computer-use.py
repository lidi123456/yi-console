#!/usr/bin/env python3
"""Prepare the pinned Windows desktop driver for embedding in yi-agentd."""
import hashlib
import json
from pathlib import Path
import urllib.request
import zipfile
import io

root = Path(__file__).resolve().parent.parent
lock = json.loads((root / 'runtime/computer-use/manifest.json').read_text())
dest = root / 'runtime/computer-use/windows-x64'
dest.mkdir(parents=True, exist_ok=True)

def digest(data):
    return hashlib.sha256(data).hexdigest()

def fetch(url):
    with urllib.request.urlopen(url, timeout=120) as response:
        return response.read()

binary = dest / 'cua-driver.exe'
if not binary.exists() or digest(binary.read_bytes()) != lock['binarySha256']:
    archive = fetch(f"{lock['repository']}/releases/download/{lock['tag']}/{lock['asset']}")
    if digest(archive) != lock['archiveSha256']:
        raise SystemExit('Computer Use archive SHA-256 mismatch')
    with zipfile.ZipFile(io.BytesIO(archive)) as files:
        data = files.read('cua-driver.exe')
    if digest(data) != lock['binarySha256']:
        raise SystemExit('Computer Use executable SHA-256 mismatch')
    binary.write_bytes(data)
license_file = dest / 'LICENSE.md'
if not license_file.exists() or digest(license_file.read_bytes()) != lock['licenseSha256']:
    data = fetch(f"https://raw.githubusercontent.com/trycua/cua/{lock['tag']}/LICENSE.md")
    if digest(data) != lock['licenseSha256']:
        raise SystemExit('Computer Use license SHA-256 mismatch')
    license_file.write_bytes(data)
print(f"Prepared MIT cua-driver {lock['version']} (Windows x64), executable and license verified")
